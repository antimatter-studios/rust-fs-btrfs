//! Send streams, judged in both directions by the implementation that is
//! not this crate (#273).
//!
//! **Reading.** The kernel's `btrfs send` writes a stream of a read-only
//! snapshot it populated. This crate parses it: every command's checksum
//! verifies, the commands are the ones `btrfs receive --dump` lists, in
//! the same order and on the same paths, and replaying them in memory
//! rebuilds the snapshot the kernel shows -- every type, mode, owner,
//! size, digest, link target, device number, link count, modification
//! time and extended attribute.
//!
//! **Writing.** This crate writes a stream of the same snapshot, read off
//! the image with its own reader. The guest's `btrfs receive` replays it
//! into a fresh filesystem, and the kernel must then show the received
//! subvolume exactly as it shows the source, with the source's UUID
//! recorded as the one it was received from.
//!
//! The snapshot holds what a stream has to carry: inline and multi-chunk
//! data, a hole, a preallocated range, an empty file, nested directories,
//! a hard link, symbolic links (one dangling), a FIFO, a character
//! device, extended attributes, a set-user-ID file, a foreign owner and
//! a timestamp with nanoseconds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_btrfs::fs::Filesystem;
use fs_btrfs::send::{attr, cmd, command_name, parse_send_stream, Command};
use fs_btrfs_test_support::{
    fixture, guest_kernel_read_ok, guest_kernel_write_ok, guest_quote, sha256_hex,
};
use fs_core::FileDevice;

/// The guest's report of one tree, one line per path, sorted:
/// `path type mode uid gid nlink size mtime content xattrs`, tab-separated.
/// `content` is a regular file's SHA-256, a symlink's target, a device's
/// `major:minor` in hex, or `-`.
const REPORT_FN: &str = r#"
report() {
    ( cd "$1" && find . -mindepth 1 -printf '%P\n' | LC_ALL=C sort | while IFS= read -r p; do
        type="$(stat -c '%F' "$p")"
        size=-
        content=-
        case "$type" in
            "regular file" | "regular empty file")
                size="$(stat -c '%s' "$p")"
                content="$(sha256sum "$p" | cut -d' ' -f1)" ;;
            "symbolic link") content="$(readlink "$p")" ;;
            "character special file" | "block special file") content="$(stat -c '%t:%T' "$p")" ;;
        esac
        xattrs="$( (getfattr -h -d -m - -e hex "$p" 2>/dev/null || true) | grep -v '^#' | grep -v '^$' | LC_ALL=C sort | paste -sd, - || true)"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$p" "$type" \
            "$(stat -c '%a' "$p")" "$(stat -c '%u' "$p")" "$(stat -c '%g' "$p")" \
            "$(stat -c '%h' "$p")" "$size" "$(stat -c '%.9Y' "$p")" "$content" "${xattrs:--}"
    done )
}
"#;

/// The guest builds the snapshot, sends it, and reports it.
const POPULATE: &str = r#"
btrfs subvolume create "$MNT/src" >/dev/null
cd "$MNT/src"
printf 'hello\n' > small.txt
head -c 300000 /dev/urandom > big.bin
truncate -s 1M sparse.bin
printf 'tail' | dd of=sparse.bin bs=1 seek=900000 conv=notrunc status=none
fallocate -l 64K prealloc.bin
: > empty
mkdir -p d1/d2
printf 'deep' > d1/d2/deep.txt
ln d1/d2/deep.txt hard.txt
ln -s d1/d2/deep.txt link
ln -s /nowhere/at/all dangling
mkfifo fifo
mknod cdev c 1 3
setfattr -n user.color -v blue small.txt
setfattr -n user.bytes -v 0x00ff7f d1
chmod 0640 small.txt
chmod 4755 big.bin
chmod 0700 d1
chown 1000:1001 d1/d2/deep.txt
touch -h -d '2001-02-03 04:05:06.123456789' empty link
cd /
sync
btrfs subvolume snapshot -r "$MNT/src" "$MNT/snap" >/dev/null
btrfs send -q -f "$OUT/kernel.stream" "$MNT/snap"
btrfs receive --dump -f "$OUT/kernel.stream" > "$OUT/kernel.dump"
report "$MNT/snap" > "$OUT/kernel.report"
"#;

/// The guest receives this crate's stream into a fresh filesystem and
/// reports both trees.
const RECEIVE: &str = r#"
work="$(mktemp -d /var/tmp/fs-btrfs-receive.XXXXXX)"
truncate -s 512M "$work/img"
mkfs.btrfs -q "$work/img"
mkdir "$work/mnt"
mount -o loop "$work/img" "$work/mnt"
trap 'umount "$work/mnt" 2>/dev/null || true; rm -rf "$work"' EXIT
btrfs receive -f "$OUT/ours.stream" "$work/mnt"
report "$MNT/snap" > "$OUT/source.report"
report "$work/mnt/snap" > "$OUT/received.report"
btrfs subvolume show "$MNT/snap" > "$OUT/source.show"
btrfs subvolume show "$work/mnt/snap" > "$OUT/received.show"
"#;

fn scratch() -> PathBuf {
    let dir = PathBuf::from(fs_btrfs_test_support::temp_path!(
        "send-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    dir
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn guest_script(out: &Path, body: &str) -> String {
    format!(
        "OUT={}\n{REPORT_FN}\n{body}",
        guest_quote(out.to_str().expect("a UTF-8 scratch path"))
    )
}

/// One node of the tree a stream rebuilds.
#[derive(Debug, Clone, Default)]
struct Node {
    kind: &'static str,
    mode: u64,
    uid: u64,
    gid: u64,
    rdev: u64,
    data: Vec<u8>,
    target: Vec<u8>,
    mtime: (i64, u32),
    xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// A stream replayed in memory: the receiver, without a filesystem.
#[derive(Default)]
struct Replay {
    nodes: Vec<Node>,
    paths: BTreeMap<Vec<u8>, usize>,
}

impl Replay {
    fn node(&mut self, c: &Command) -> &mut Node {
        let path = c.path().expect("a path");
        let id = *self.paths.get(path).unwrap_or_else(|| {
            panic!(
                "{:?} names {:?}, which does not exist",
                command_name(c.cmd),
                String::from_utf8_lossy(path)
            )
        });
        &mut self.nodes[id]
    }

    fn create(&mut self, c: &Command, kind: &'static str) {
        let path = c.path().unwrap().to_vec();
        assert!(!self.paths.contains_key(&path), "{path:?} created twice");
        let mut node = Node {
            kind,
            ..Default::default()
        };
        if kind == "symbolic link" {
            node.target = c.attr(attr::PATH_LINK).unwrap().to_vec();
            node.mode = 0o777;
        }
        if let Some(mode) = c.attr(attr::MODE) {
            node.mode = u64::from_le_bytes(mode.try_into().unwrap()) & 0o7777;
            node.rdev = c.u64(attr::RDEV).unwrap_or(0);
        }
        self.nodes.push(node);
        self.paths.insert(path, self.nodes.len() - 1);
    }

    fn apply(&mut self, c: &Command) {
        match c.cmd {
            cmd::SUBVOL => {
                self.nodes.push(Node {
                    kind: "directory",
                    ..Default::default()
                });
                self.paths.insert(Vec::new(), 0);
            }
            cmd::MKFILE => self.create(c, "regular file"),
            cmd::MKDIR => self.create(c, "directory"),
            cmd::MKNOD => self.create(c, "character special file"),
            cmd::MKFIFO => self.create(c, "fifo"),
            cmd::MKSOCK => self.create(c, "socket"),
            cmd::SYMLINK => self.create(c, "symbolic link"),
            cmd::RENAME => {
                let from = c.path().unwrap().to_vec();
                let to = c.attr(attr::PATH_TO).unwrap().to_vec();
                let moved: Vec<Vec<u8>> = self
                    .paths
                    .keys()
                    .filter(|p| {
                        **p == from || (p.starts_with(&from) && p.get(from.len()) == Some(&b'/'))
                    })
                    .cloned()
                    .collect();
                assert!(
                    !moved.is_empty(),
                    "rename of {from:?}, which does not exist"
                );
                for p in moved {
                    let id = self.paths.remove(&p).unwrap();
                    let mut q = to.clone();
                    q.extend_from_slice(&p[from.len()..]);
                    self.paths.insert(q, id);
                }
            }
            cmd::LINK => {
                let existing = c.attr(attr::PATH_LINK).unwrap();
                let id = self.paths[existing];
                self.paths.insert(c.path().unwrap().to_vec(), id);
            }
            cmd::UNLINK | cmd::RMDIR => {
                self.paths.remove(c.path().unwrap());
            }
            cmd::SET_XATTR => {
                let name = c.attr(attr::XATTR_NAME).unwrap().to_vec();
                let value = c.attr(attr::XATTR_DATA).unwrap().to_vec();
                self.node(c).xattrs.insert(name, value);
            }
            cmd::REMOVE_XATTR => {
                let name = c.attr(attr::XATTR_NAME).unwrap().to_vec();
                self.node(c).xattrs.remove(&name);
            }
            cmd::WRITE => {
                let offset = c.u64(attr::FILE_OFFSET).unwrap() as usize;
                let data = c.attr(attr::DATA).unwrap().to_vec();
                let node = self.node(c);
                if node.data.len() < offset + data.len() {
                    node.data.resize(offset + data.len(), 0);
                }
                node.data[offset..offset + data.len()].copy_from_slice(&data);
            }
            cmd::CLONE => {
                let offset = c.u64(attr::FILE_OFFSET).unwrap() as usize;
                let len = c.u64(attr::CLONE_LEN).unwrap() as usize;
                let from = c.u64(attr::CLONE_OFFSET).unwrap() as usize;
                let src = self.paths[c.attr(attr::CLONE_PATH).unwrap()];
                let mut bytes = self.nodes[src].data.clone();
                bytes.resize(bytes.len().max(from + len), 0);
                let bytes = bytes[from..from + len].to_vec();
                let node = self.node(c);
                if node.data.len() < offset + len {
                    node.data.resize(offset + len, 0);
                }
                node.data[offset..offset + len].copy_from_slice(&bytes);
            }
            cmd::TRUNCATE => {
                let size = c.u64(attr::SIZE).unwrap() as usize;
                self.node(c).data.resize(size, 0);
            }
            cmd::CHMOD => {
                let mode = c.u64(attr::MODE).unwrap() & 0o7777;
                self.node(c).mode = mode;
            }
            cmd::CHOWN => {
                let (uid, gid) = (c.u64(attr::UID).unwrap(), c.u64(attr::GID).unwrap());
                let node = self.node(c);
                node.uid = uid;
                node.gid = gid;
            }
            cmd::UTIMES => {
                let t = c.timestamp(attr::MTIME).unwrap();
                self.node(c).mtime = (t.sec, t.nsec);
            }
            cmd::END => {}
            other => panic!(
                "the replay has no rule for command {other} ({:?})",
                command_name(other)
            ),
        }
    }

    /// The tree as the guest's `report` prints it.
    fn report(&self) -> String {
        let mut nlink: BTreeMap<usize, usize> = BTreeMap::new();
        for id in self.paths.values() {
            *nlink.entry(*id).or_default() += 1;
        }
        let mut out = String::new();
        for (path, id) in &self.paths {
            if path.is_empty() {
                continue;
            }
            let n = &self.nodes[*id];
            let kind = if n.kind == "regular file" && n.data.is_empty() {
                "regular empty file"
            } else {
                n.kind
            };
            let links = if n.kind == "directory" { 1 } else { nlink[id] };
            let (size, content) = match n.kind {
                "regular file" => (n.data.len().to_string(), sha256_hex(&n.data)),
                "symbolic link" => ("-".into(), String::from_utf8_lossy(&n.target).into_owned()),
                "character special file" => {
                    let major = ((n.rdev >> 8) & 0xfff) | ((n.rdev >> 32) & !0xfff);
                    let minor = (n.rdev & 0xff) | ((n.rdev >> 12) & !0xff);
                    ("-".into(), format!("{major:x}:{minor:x}"))
                }
                _ => ("-".into(), "-".into()),
            };
            let xattrs = if n.xattrs.is_empty() {
                "-".to_string()
            } else {
                n.xattrs
                    .iter()
                    .map(|(k, v)| {
                        let hex: String = v.iter().map(|b| format!("{b:02x}")).collect();
                        format!("{}=0x{hex}", String::from_utf8_lossy(k))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            };
            out.push_str(&format!(
                "{}\t{kind}\t{:o}\t{}\t{}\t{links}\t{size}\t{}.{:09}\t{content}\t{xattrs}\n",
                String::from_utf8_lossy(path),
                n.mode,
                n.uid,
                n.gid,
                n.mtime.0,
                n.mtime.1,
            ));
        }
        out
    }
}

/// `btrfs receive --dump`'s path, as the stream carries it: the dump
/// joins every path but the subvolume's own onto `./snap/`.
fn dump_path(field: &str, subvol: bool) -> &str {
    let p = field.strip_prefix("./").unwrap_or(field);
    if subvol {
        return p;
    }
    let p = p.strip_prefix("snap").unwrap_or(p);
    p.strip_prefix('/').unwrap_or(p)
}

/// The snapshot named `snap` on `image`, through this crate's reader.
fn snapshot_id(fs: &Filesystem) -> u64 {
    fs.subvolumes()
        .expect("list the subvolumes")
        .into_iter()
        .find(|s| s.name == b"snap")
        .expect("the guest made a subvolume named snap")
        .id
}

fn show_field(show: &str, name: &str) -> String {
    show.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(name).map(|v| v.trim().to_string()))
        .unwrap_or_else(|| panic!("`btrfs subvolume show` printed no {name:?}:\n{show}"))
}

#[test]
fn send_streams_agree_with_btrfs_send_and_btrfs_receive() {
    let out = scratch();
    let image = out.join("btrfs-send.img");
    std::fs::copy(fixture("btrfs-default.img"), &image).expect("copy the fixture");
    let image_str = image.to_str().expect("a UTF-8 scratch path").to_string();

    // The kernel populates, snapshots and sends.
    guest_kernel_write_ok(
        &image_str,
        "populate and send",
        &guest_script(&out, POPULATE),
    );

    // --- Reading what the kernel wrote ---
    let kernel_bytes = std::fs::read(out.join("kernel.stream")).expect("the kernel's stream");
    let kernel = parse_send_stream(&kernel_bytes).expect("parse the kernel's stream");
    assert_eq!(kernel.version, 1);
    assert_eq!(kernel.commands.first().map(|c| c.cmd), Some(cmd::SUBVOL));
    assert_eq!(kernel.commands.last().map(|c| c.cmd), Some(cmd::END));
    assert_eq!(kernel.commands[0].path().unwrap(), b"snap");

    let dump = read(&out.join("kernel.dump"));
    let dumped: Vec<(&str, &str)> = dump
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut f = l.split_whitespace();
            (f.next().unwrap(), f.next().unwrap_or(""))
        })
        .collect();
    let ours: Vec<(&str, String)> = kernel
        .commands
        .iter()
        .filter(|c| c.cmd != cmd::END)
        .map(|c| {
            (
                command_name(c.cmd).unwrap_or("?"),
                String::from_utf8_lossy(c.path().unwrap_or_default()).into_owned(),
            )
        })
        .collect();
    assert_eq!(
        ours.len(),
        dumped.len(),
        "btrfs receive --dump lists {} commands, the parser found {}:\n{dump}",
        dumped.len(),
        ours.len()
    );
    for (i, ((name, path), (dname, dpath))) in ours.iter().zip(&dumped).enumerate() {
        assert_eq!(name, dname, "command {i}:\n{dump}");
        assert_eq!(
            path,
            dump_path(dpath, i == 0),
            "command {i}'s path:\n{dump}"
        );
    }

    let mut replay = Replay::default();
    for c in &kernel.commands {
        replay.apply(c);
    }
    let kernel_report = read(&out.join("kernel.report"));
    assert!(
        kernel_report.lines().count() == 13,
        "the snapshot holds 13 paths, and the guest reported:\n{kernel_report}"
    );
    assert_eq!(
        replay.report(),
        kernel_report,
        "replaying the kernel's stream does not rebuild the snapshot the kernel shows"
    );

    // --- Writing one the kernel reads ---
    let dev = FileDevice::open(&image).expect("open the image");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount the image");
    let ours_bytes = fs
        .send_subvolume(snapshot_id(&fs))
        .expect("send the snapshot");
    std::fs::write(out.join("ours.stream"), &ours_bytes).expect("write our stream");

    let parsed = parse_send_stream(&ours_bytes).expect("our stream parses");
    assert_eq!(
        parsed.commands[0].attr(attr::UUID),
        kernel.commands[0].attr(attr::UUID),
        "the stream names the snapshot by a different UUID than the kernel's does"
    );
    assert_eq!(
        parsed.commands[0].u64(attr::CTRANSID).unwrap(),
        kernel.commands[0].u64(attr::CTRANSID).unwrap(),
        "the stream gives a different change transaction than the kernel's does"
    );

    guest_kernel_read_ok(
        &image_str,
        "receive our stream",
        &guest_script(&out, RECEIVE),
    );
    let source = read(&out.join("source.report"));
    let received = read(&out.join("received.report"));
    assert_eq!(
        source, kernel_report,
        "the source snapshot changed between mounts"
    );
    assert_eq!(
        received, source,
        "btrfs receive rebuilt something other than the snapshot from this crate's stream"
    );
    assert_eq!(
        show_field(&read(&out.join("received.show")), "Received UUID:"),
        show_field(&read(&out.join("source.show")), "UUID:"),
        "the received subvolume does not name the source as where it came from"
    );
}
