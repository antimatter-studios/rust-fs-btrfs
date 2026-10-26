//! `fs.btrfs <target> <verb>`: an errand inside a Btrfs image or device,
//! without mounting it.
//!
//! The verbs are the shared set: `ls`, `stat`, `read`, `export`, `xattr`, `write`, `mkdir`,
//! `get`/`info`, `set`, `resize`, and the names and lengths this library
//! can change: `create`, `ln`, `rm`, `rmdir`, `mv`, `truncate`. Metadata is
//! JSON (or `--text`); file content is raw bytes. A case the library
//! refuses, or a verb it cannot do yet, answers with exit status 3 and its
//! reason, so a script moved between filesystems fails loudly instead of
//! meaning something else.
//!
//! `write` changes the bytes of a file that exists, and never shortens it:
//! a NODATACOW file in place at the same length, any other one
//! copy-on-write. A new file or a shorter one is refused naming the verb
//! that makes that change, `create` or `truncate`, and every other refusal
//! gives the library's own reason.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::sync::Arc;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use super::device;
use fs_btrfs::inode::{FileType, Inode};
use fs_btrfs::superblock::{compat_ro, incompat};
use fs_btrfs::{ChecksumType, Error, Filesystem, Superblock};
use fs_core::cli::{transfer, CliError, Format, Json, Outcome, Tool};

pub const TOOL: Tool = Tool {
    name: "fs.btrfs",
    verb: "fs",
    section: 1,
    usage_exit: fs_core::cli::output::EXIT_USAGE,
    about: "List, read and inspect a Btrfs image or device without mounting it",
    command,
    run,
};

/// The canonical keys every `fs.<fs>` answers, in the shared order.
/// Filesystem specifics are nested under `btrfs`.
pub const KEYS: &[&str] = &[
    "fs",
    "label",
    "total_bytes",
    "free_bytes",
    "block_size",
    "dirty",
    "btrfs",
];

fn command() -> Cmd {
    Cmd::new("fs.btrfs")
        .about("List, read and inspect a Btrfs image or device without mounting it")
        .long_about(
            "Work inside a Btrfs image or device directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (get a file out, read the label, check whether \
             it is dirty), not a place to do real filesystem work: for that, mount it.\n\n\
             Metadata is JSON on stdout (--text for people); `read` and a named `xattr` \
             write raw bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being the \
             exit status: 1 failed, 2 wrong command line, 3 not implemented or refused.\n\n\
             Writing is limited to what this library can do safely: `write` overwrites \
             an existing file at the same length, a NODATACOW file in place and any other \
             copy-on-write, committed as one transaction. What that cannot do yet answers \
             with exit status 3.",
        )
        .arg(
            Arg::new("target")
                .value_name("TARGET")
                .help("The image file or device")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("BYTES")
                .help(
                    "Where the filesystem starts in TARGET, for a partition in a whole-disk image",
                )
                .value_parser(value_parser!(u64))
                .global(true),
        )
        .args(fs_core::cli::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(
            Cmd::new("ls")
                .about(
                    "List a directory: name, type, size, mode, mtime (a symlink's target, \
                     and which entries are subvolumes)",
                )
                .arg(Arg::new("recursive").long("recursive").short('R')
                    .action(ArgAction::SetTrue)
                    .help("Walk depth first, names sorted as bytes; never follow a symlink"))
                .arg(Arg::new("max-depth").long("max-depth").value_name("N")
                    .value_parser(value_parser!(u64)).requires("recursive")
                    .help("How deep --recursive may go (default: 64); a directory's entries are depth 1"))
                .arg(Arg::new("limit").long("limit").value_name("N")
                    .value_parser(value_parser!(u64)).requires("recursive")
                    .help("How many entries --recursive may list (default: 100000)"))
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img ls /home\n  \
                     fs.btrfs disk.img ls / | jq -r '.[] | select(.subvolume) | .name'\n  \
                     fs.btrfs disk.img ls --text /\n  \
                     fs.btrfs disk.img ls -R --max-depth 8 --limit 1000 /home\n\n\
                     With --recursive each entry adds its path from the volume's root and \
                     its depth from PATH. A starting file is listed alone at depth 0. \
                     Subvolumes and snapshots are entered and marked; a directory whose \
                     (subvolume, inode) identity was visited is marked revisited: true and \
                     not entered again. If one more entry would pass either bound, or a \
                     directory cannot be read, exit 1 keeps the collected entries on stdout \
                     and names the bound or path on stderr. An exact fit succeeds; \
                     --limit 0 allows no entry, --max-depth 0 none below PATH.",
                ),
        )
        .subcommand(
            Cmd::new("stat")
                .about("Report one path: identity, type, mode, owner, links, sizes and every time")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img stat /etc/hostname\n  \
                     fs.btrfs disk.img stat /sub/notes.txt | jq .btrfs.subvolume\n  \
                     fs.btrfs disk.img stat --text /bin/sh\n\n\
                     A symlink is reported, not followed. Times are signed seconds since \
                     the epoch plus nanoseconds. The compression property is null when \
                     absent; it does not describe the compression of each data extent.",
                ),
        )
        .subcommand(
            Cmd::new("read")
                .about("Write a file's bytes to stdout, or to a file with -o")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .help("Write here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img read /etc/hostname\n  \
                     fs.btrfs disk.img read /snapshots/home/notes.txt | less\n  \
                     fs.btrfs disk.img read /backup.tar -o backup.tar",
                ),
        )
        .subcommand(
            Cmd::new("export")
                .about("Copy a regular file out through the shared bounded transfer")
                .args(transfer::export_args())
                .after_help(
                    "Examples:\n  fs.btrfs disk.img export /backup.tar backup.tar\n  \
                     fs.btrfs disk.img export --replace /etc/hostname hostname\n  \
                     fs.btrfs disk.img export /backup.tar - | tar tf -\n\n\
                     Files are copied in fixed chunks. A local destination is published only \
                     when complete; an existing name is refused unless --replace is given. \
                     The report includes the byte count and SHA-256; DEST - writes only raw \
                     bytes to stdout. Directories, symlinks and special files are refused.",
                ),
        )
        .subcommand(
            Cmd::new("xattr")
                .about("List a path's extended attributes, or read one value as raw bytes")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("name")
                        .value_name("NAME")
                        .help("The fully qualified attribute name, including its namespace")
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .requires("name")
                        .value_parser(value_parser!(OsString))
                        .help("Write the named value here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img xattr /notes.txt\n  \
                     fs.btrfs disk.img xattr /notes.txt --text\n  \
                     fs.btrfs disk.img xattr /notes.txt user.comment\n  \
                     fs.btrfs disk.img xattr /bin/tool security.capability -o capability.bin\n\n\
                     Listings are JSON names and byte lengths (--text for people). A \
                     non-UTF-8 name is shown lossily with its exact bytes in name_hex. \
                     A named value is always raw bytes, including with --text. Inspection \
                     only: attributes are never set or removed.",
                ),
        )
        .subcommand(
            Cmd::new("write")
                .about(
                    "Overwrite an existing file with at least as many bytes as it holds, from stdin",

                )
                .arg(
                    Arg::new("at")
                        .long("at")
                        .value_name("BYTES")
                        .value_parser(value_parser!(u64))
                        .help("Position inside the file; positioned writes are not implemented (exit 3)"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img write /vm/disk.raw < disk.raw\n  \
                     fs.btrfs disk.img read /db/data | fix-up | fs.btrfs disk.img write /db/data\n  \
                     fs.btrfs disk.img write --at 0 /f < payload.bin\n\n\
                     --at is a byte position inside the file: positioned writes are not \
                     implemented and exit 3 before reading stdin or opening TARGET, even \
                     at zero. --offset remains the filesystem's position inside TARGET.\n\n\
                     The file must exist and get at least as many bytes as it already holds. \
                     A NODATACOW file (chattr +C) is overwritten in place, at the same length; \
                     any other is written copy-on-write into new extents, growing it where the \
                     library can, and committed as one transaction. A new file is `create`'s \
                     to make and a shorter one `truncate`'s to cut, so both are refused with \
                     exit status 3, as is anything the library cannot write yet, with the \
                     reason (rust-fs-btrfs#261).",
                ),
        )
        .subcommand(
            name_command(
                "mkdir",
                "Create an empty directory, mode 0755 unless --mode says otherwise \
                 (its parent must exist, or with -p every one on the way)",
                "fs.btrfs disk.img mkdir /backup\n  fs.btrfs disk.img mkdir --mode 700 /private",
                Some("755"),
            )
            .arg(
                Arg::new("parents")
                    .short('p')
                    .long("parents")
                    .action(ArgAction::SetTrue)
                    .help("Make every missing directory on the way; one already there is success"),
            )
            .after_help(format!(
                "Examples:\n  fs.btrfs disk.img mkdir /backup\n  \
                 fs.btrfs disk.img mkdir --mode 700 /private\n  \
                 fs.btrfs disk.img mkdir -p /backup/2026/10/17\n\n\
                 {NAMES_NOTE}\n\n\
                 With --parents every missing directory on the way to PATH is made, mode \
                 0755 (--mode is PATH's own), and the report lists those it made: none, and \
                 nothing written, when the whole chain is already there, a subvolume's top \
                 included. A file, a symlink or a name too long anywhere on the way is \
                 refused with exit 1, and a directory inside a subvolume other than the top \
                 level with exit 3, before the image is opened for writing, so a refused \
                 command makes no part of the chain. Each directory is its own committed \
                 transaction; should one still fail, those made before it are removed \
                 again. A symlink is never followed, even one naming a directory."
            )),
        )
        .subcommand(name_command(
            "create",
            "Create an empty regular file, mode 0644 unless --mode says otherwise",
            "fs.btrfs disk.img create /notes.txt",
            Some("644"),
        ))
        .subcommand(name_command(
            "rm",
            "Remove a name; the file goes with its last name",
            "fs.btrfs disk.img rm /notes.txt",
            None,
        ))
        .subcommand(name_command(
            "rmdir",
            "Remove an empty directory",
            "fs.btrfs disk.img rmdir /backup",
            None,
        ))
        .subcommand(
            Cmd::new("ln")
                .about("Add a name for a file (a hard link), or with -s make a symbolic link")
                .arg(
                    Arg::new("symbolic")
                        .short('s')
                        .long("symbolic")
                        .action(ArgAction::SetTrue)
                        .help("Make a symbolic link whose target is TARGET, as given"),
                )
                .arg(
                    Arg::new("target")
                        .value_name("TARGET")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img ln /data/a /data/also-a\n  \
                     fs.btrfs disk.img ln -s ../etc/hosts /hosts\n\n\
                     A new name is committed as one transaction. Only the top-level \
                     subvolume is written, and a directory whose full leaf cannot be split \
                     is refused with exit 3.",
                ),
        )
        .subcommand(
            Cmd::new("mv")
                .about("Move a name, replacing whatever is at the destination")
                .arg(
                    Arg::new("source")
                        .value_name("SOURCE")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img mv /notes.txt /archive/notes.txt\n  \
                     fs.btrfs disk.img mv /draft /final\n\n\
                     PATH is the new name itself, not a directory to move into. A name \
                     already there is replaced, as rename(2) does: a file by a file, an empty \
                     directory by a directory. Committed as one transaction; only the \
                     top-level subvolume is written. A replaced file's last name takes its \
                     data with it; one whose data a snapshot may share is refused with exit \
                     3 (rust-fs-btrfs#426).",
                ),
        )
        .subcommand(
            Cmd::new("truncate")
                .about("Set a file's length, releasing what lies past a shorter one")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("size")
                        .value_name("BYTES")
                        .required(true)
                        .value_parser(value_parser!(u64)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img truncate /log.txt 0\n  \
                     fs.btrfs disk.img truncate /sparse.img 1073741824\n\n\
                     Committed as one transaction. Shrinking releases the extents wholly past \
                     the new end, or drops the file's reference of one a reflink shares; \
                     growing records the new range as a hole. What the library still refuses \
                     answers with exit 3 and its reason (rust-fs-btrfs#262).",
                ),
        )
        .subcommand(key_command(
            "get",
            "Report the filesystem's properties, or one of them",
        ))
        .subcommand(key_command(
            "info",
            "The same as get: every property, or one of them",
        ))
        .subcommand(
            Cmd::new("set")
                .about("Change a property: the label")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.btrfs disk.img set label BACKUP\n\n\
                     The label is written to every superblock copy; at most 255 bytes.",
                ),
        )
        .subcommand(
            Cmd::new("resize")
                .about("Grow or shrink the filesystem, and an image file with it")
                .arg(
                    Arg::new("size")
                        .value_name("SIZE")
                        .required(true)
                        .allow_hyphen_values(true)
                        .help(
                            "The new size (20G), a change to the current one (+1G, -512M), \
                             or max: all of the device",
                        ),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Shrink, cutting an image file to the new size; or grow at \
                             --offset, where the partition's end cannot be seen",
                        ),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img resize 20G\n  \
                     fs.btrfs disk.img resize +1G\n  \
                     fs.btrfs disk.img resize 448M --force\n  \
                     truncate -s 8G disk.img && fs.btrfs disk.img resize max\n\n\
                     K, M, G and T are powers of 1024; the size is rounded down to a whole \
                     sector. Committed as one transaction: the device item, the superblock's \
                     size and its copy of the device item. An image file is made longer \
                     first when it is too short, and cut to the new size after a shrink; a \
                     device or a partition must already be as large as the filesystem is to \
                     be. A shrink needs --force. A chunk past the new end, which only a \
                     balance could move, and a filesystem of several devices are refused \
                     with exit 3 (rust-fs-btrfs#264), leaving the image as it was.",
                ),
        )
        .after_help(
            "Examples:\n  fs.btrfs disk.img ls /\n  \
             fs.btrfs disk.img read /etc/fstab > fstab\n  \
             fs.btrfs disk.img get label --text\n  \
             fs.btrfs --offset 1048576 whole-disk.img info",
        )
}

/// What every namespace verb's help says about how it writes.
const NAMES_NOTE: &str = "Committed as one transaction. What the library still refuses \
     answers with exit 3 and its reason (rust-fs-btrfs#262).";

/// A verb taking one PATH, and for one that creates, a --mode.
fn name_command(
    name: &'static str,
    about: &'static str,
    examples: &'static str,
    mode: Option<&'static str>,
) -> Cmd {
    let mut cmd = Cmd::new(name)
        .about(about)
        .arg(
            Arg::new("path")
                .value_name("PATH")
                .required(true)
                .value_parser(value_parser!(OsString)),
        )
        .after_help(format!("Examples:\n  {examples}\n\n{NAMES_NOTE}"));
    if let Some(default) = mode {
        cmd = cmd.arg(
            Arg::new("mode")
                .long("mode")
                .value_name("OCTAL")
                .default_value(default)
                .help("Permission bits, in octal"),
        );
    }
    cmd
}

fn key_command(name: &'static str, about: &'static str) -> Cmd {
    Cmd::new(name)
        .about(about)
        .arg(
            Arg::new("key")
                .value_name("KEY")
                .help(format!("One of: {} (or btrfs.<field>)", KEYS.join(", "))),
        )
        .after_help(format!(
            "Examples:\n  fs.btrfs disk.img {name}\n  \
             fs.btrfs disk.img {name} label --text\n  \
             fs.btrfs disk.img {name} btrfs.csum_type"
        ))
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let target = matches
        .get_one::<OsString>("target")
        .expect("clap requires the target");
    let (verb, sub) = matches.subcommand().expect("clap requires a verb");
    let offset = sub
        .get_one::<u64>("offset")
        .or_else(|| matches.get_one::<u64>("offset"))
        .copied()
        .unwrap_or(0);
    match verb {
        "ls" if sub.get_flag("recursive") => ls_recursive(
            device::mount(target, offset)?,
            path_arg(sub),
            sub,
            Format::of(matches, &std::env::args_os().collect::<Vec<_>>()),
        ),
        "ls" => ls(&device::mount(target, offset)?, path_arg(sub)),
        "stat" => stat(&device::mount(target, offset)?, path_arg(sub)),
        "read" => read(
            &device::mount(target, offset)?,
            path_arg(sub),
            sub.get_one("output"),
        ),
        "export" => {
            let request = transfer::ExportRequest::from_matches(sub)?;
            export(&device::mount(target, offset)?, &request)
        }
        "write" if sub.get_one::<u64>("at").is_some() => Err(CliError::not_implemented(
            "positioned write (--at) is not supported",
        )),
        "xattr" => xattr(
            &device::mount(target, offset)?,
            path_arg(sub),
            sub.get_one::<OsString>("name").map(os_bytes),
            sub.get_one("output"),
        ),
        "write" => write(target, offset, path_arg(sub)),
        "mkdir" if sub.get_flag("parents") => mkdir_parents(target, offset, sub),
        "mkdir" | "create" | "rm" | "rmdir" | "ln" | "mv" => names(target, offset, verb, sub),
        "truncate" => truncate(target, offset, sub),
        "get" | "info" => get(
            target,
            offset,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        "set" => set(target, offset, sub),
        "resize" => resize(target, offset, sub),
        other => unreachable!("clap knows no verb {other}"),
    }
}

fn path_arg(sub: &ArgMatches) -> &[u8] {
    let path = sub
        .get_one::<OsString>("path")
        .expect("clap requires or defaults the path");
    os_bytes(path)
}

#[cfg(unix)]
fn os_bytes(s: &OsString) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(s: &OsString) -> &[u8] {
    s.to_str().map(str::as_bytes).unwrap_or_default()
}

/// The namespace verbs: each opens the image read-write, makes one change
/// and commits it.
fn names(
    target: &OsString,
    offset: u64,
    verb: &str,
    sub: &ArgMatches,
) -> Result<Outcome, CliError> {
    let path = path_arg(sub);
    let dev = device::open_rw(target, offset)?;
    let mut fs = Filesystem::mount_rw(dev).map_err(|e| {
        btrfs_error(
            format!("{} (read-write)", target.to_string_lossy()).as_bytes(),
            e,
        )
    })?;
    let (parent, name) = split_parent(&fs, path)?;
    let mode = mode_arg(sub)?;
    let fail = |e| btrfs_error(path, e);
    let (what, ino) = match verb {
        "mkdir" => (
            "created",
            Some(
                fs.mkdir(parent.ino, name, mode, parent.uid, parent.gid)
                    .map_err(fail)?,
            ),
        ),
        "create" => (
            "created",
            Some(
                fs.create(parent.ino, name, mode, parent.uid, parent.gid)
                    .map_err(fail)?,
            ),
        ),
        "rm" => {
            fs.unlink(parent.ino, name).map_err(fail)?;
            ("removed", None)
        }
        "rmdir" => {
            fs.rmdir(parent.ino, name).map_err(fail)?;
            ("removed", None)
        }
        "ln" => {
            let source = os_bytes(sub.get_one::<OsString>("target").expect("clap requires it"));
            if sub.get_flag("symbolic") {
                let ino = fs
                    .symlink(parent.ino, name, source, parent.uid, parent.gid)
                    .map_err(fail)?;
                ("linked", Some(ino))
            } else {
                let file = fs
                    .lookup_path_bytes(source)
                    .map_err(|e| btrfs_error(source, e))?;
                fs.link(file.ino, parent.ino, name).map_err(fail)?;
                ("linked", Some(file.ino))
            }
        }
        "mv" => {
            let source = os_bytes(sub.get_one::<OsString>("source").expect("clap requires it"));
            let (from, from_name) = split_parent(&fs, source)?;
            fs.rename(from.ino, from_name, parent.ino, name)
                .map_err(|e| btrfs_error(source, e))?;
            ("moved", None)
        }
        other => unreachable!("not a namespace verb: {other}"),
    };
    let mut fields = vec![("path", Json::from(show(path))), (what, Json::from(true))];
    if let Some(ino) = ino {
        fields.push(("inode", Json::from(ino)));
    }
    Ok(Outcome::report(Json::object(fields)).with_text(format!("{what} {}", show(path))))
}

/// A creating verb's `--mode`, in octal; 0 for a verb that has none.
fn mode_arg(sub: &ArgMatches) -> Result<u32, CliError> {
    match sub.try_get_one::<String>("mode") {
        Ok(Some(m)) => u32::from_str_radix(m, 8)
            .ok()
            .filter(|m| *m <= 0o7777)
            .ok_or_else(|| CliError::usage(format!("--mode {m}: not octal permission bits"))),
        _ => Ok(0),
    }
}

/// Create every missing directory on the way to `path`, as `mkdir -p`
/// does, through the library's `mkdir`, all in one writable open: mode
/// 0755, or `--mode` for the directory `path` itself names, each owned as
/// the directory already there that the chain starts from. Every refusal
/// is decided first, on a read-only mount, so a refused command never
/// opens the image for writing and makes no part of the chain; a chain
/// already there all the way is success without opening it for writing
/// either. Each directory is one committed transaction, and should one be
/// refused all the same (a full leaf that cannot be split), the ones made
/// before it are removed again, last first.
fn mkdir_parents(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let path = path_arg(sub);
    let mode = mode_arg(sub)?;
    let chain = chain(&device::mount(target, offset)?, path)?;
    if !chain.is_empty() {
        let dev = device::open_rw(target, offset)?;
        let mut fs = Filesystem::mount_rw(dev).map_err(|e| {
            btrfs_error(
                format!("{} (read-write)", target.to_string_lossy()).as_bytes(),
                e,
            )
        })?;
        let mut made: Vec<u64> = Vec::with_capacity(chain.len());
        let parent = |step: &Step, made: &[u64]| match step.parent {
            Dir::Existing(ino) => ino,
            Dir::Made(i) => made[i],
        };
        for step in &chain {
            let bits = if step.named { mode } else { 0o755 };
            let (uid, gid) = step.owner;
            match fs.mkdir(parent(step, &made), step.name, bits, uid, gid) {
                Ok(ino) => made.push(ino),
                Err(e) => {
                    // Last made, first removed, so each is empty when its
                    // turn comes. The error reported is the one that
                    // stopped the chain, and any removal that fails too.
                    let mut refusal = btrfs_error(step.shown, e);
                    for done in chain[..made.len()].iter().rev() {
                        if let Err(undo) = fs.rmdir(parent(done, &made), done.name) {
                            refusal.message = format!(
                                "{}; and removing {} again failed: {undo}",
                                refusal.message,
                                show(done.shown)
                            );
                        }
                    }
                    return Err(refusal);
                }
            }
        }
    }
    let text = if chain.is_empty() {
        format!("{} exists; nothing created", show(path))
    } else {
        let lines: Vec<String> = chain
            .iter()
            .map(|step| format!("created {}", show(step.shown)))
            .collect();
        lines.join("\n")
    };
    let created: Vec<Json> = chain
        .iter()
        .map(|step| Json::from(show(step.shown)))
        .collect();
    let mut fields = vec![("path", Json::from(show(path)))];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.push(("created", Json::from(created)));
    Ok(Outcome::report(Json::object(fields)).with_text(text))
}

/// The directory a directory of a chain is made in: one already on the
/// volume, in the top-level subvolume, or the one the chain makes at that
/// index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Existing(u64),
    Made(usize),
}

/// One directory `mkdir --parents` makes: the entry `name` in `parent`,
/// owned by `owner`, reported as `shown`, the path as given up to and
/// including the name. `named` when it is the last name of the path, the
/// directory the path itself names.
struct Step<'p> {
    parent: Dir,
    owner: (u32, u32),
    name: &'p [u8],
    shown: &'p [u8],
    named: bool,
}

/// Where the walk of `mkdir --parents` is: a directory already on the
/// volume, by its path with no `.` or `..` in it, and whether it is in
/// the top-level subvolume, the only one written; or a directory the
/// chain makes.
enum Here {
    Existing {
        at: Vec<u8>,
        inode: Inode,
        top: bool,
    },
    Made(usize),
}

/// The directories `mkdir --parents` makes for `path`, in order, once each
/// refusal the guest's `mkdir -p` gives on the kernel's mount has been
/// ruled out, for the same reason and, where two apply, at the first name
/// on the way that fails (`tests/cli_mkdir_kernel.rs` holds the two to the
/// same answers):
///
/// - an empty path: no such file or directory;
/// - a name over 255 bytes: a name too long;
/// - a name already there that is not a directory: not a directory when
///   more of the path follows it, and already there when it is the last.
///
/// A subvolume is a directory on the way, as on a mount, and `..` at its
/// top climbs back out. Where the kernel would make a directory inside a
/// subvolume other than the top level, this refuses with exit 3: only the
/// top level is written. `.` stays where the walk is and `..` goes back
/// to where it came from, through a directory the chain makes as well as
/// one already there, so `a/../b` makes `a` and `b` as `mkdir -p` does.
/// Nothing is made until every name has been decided: where `mkdir -p`
/// makes the start of a chain before it stops, this makes none of it. No
/// symlink is followed, so one on the way is not a directory and one at
/// `path` is already there, even one naming a directory, which `mkdir -p`
/// would follow.
fn chain<'p>(fs: &Filesystem, path: &'p [u8]) -> Result<Vec<Step<'p>>, CliError> {
    if path.is_empty() {
        return Err(CliError::failed("an empty path: no such file or directory"));
    }
    // Each name, with where it ends in `path`.
    let mut names = Vec::new();
    let mut end = 0;
    for name in path.split(|&b| b == b'/') {
        end += name.len();
        if !name.is_empty() {
            names.push((name, end));
        }
        end += 1;
    }
    let root = fs.root_inode().map_err(|e| btrfs_error(b"/", e))?;
    let mut walk = vec![Here::Existing {
        at: Vec::new(),
        inode: root,
        top: true,
    }];
    let mut steps: Vec<Step<'p>> = Vec::new();
    for (i, &(name, end)) in names.iter().enumerate() {
        let shown = &path[..end];
        let named = i + 1 == names.len();
        if name == b"." {
            continue;
        }
        if name == b".." {
            if walk.len() > 1 {
                walk.pop();
            }
            continue;
        }
        if name.len() > fs_btrfs::dir::MAX_NAME_LEN {
            return Err(CliError::failed(format!("{}: name too long", show(shown))));
        }
        let step = |parent: Dir, owner: (u32, u32)| Step {
            parent,
            owner,
            name,
            shown,
            named,
        };
        let next = match walk.last().expect("the top is never popped") {
            &Here::Made(j) => {
                let owner = steps[j].owner;
                plan(&mut steps, step(Dir::Made(j), owner))
            }
            Here::Existing { at, inode, top } => {
                let here = Dir::Existing(inode.ino);
                let owner = (inode.uid, inode.gid);
                // A name the chain already makes here is not looked up:
                // it is not on the volume yet.
                if *top && steps.iter().any(|s| s.parent == here && s.name == name) {
                    plan(&mut steps, step(here, owner))
                } else {
                    let mut below = at.clone();
                    below.push(b'/');
                    below.extend_from_slice(name);
                    match fs.resolve_path_bytes(&below) {
                        Ok(found) if found.inode.is_dir() => Here::Existing {
                            at: below,
                            top: *top
                                && found.tree.is_none()
                                && found.inode.ino != fs_btrfs::subvol::EMPTY_SUBVOL_DIR_OBJECTID,
                            inode: found.inode,
                        },
                        Ok(_) if named => {
                            return Err(CliError::failed(format!(
                                "{}: already exists",
                                show(shown)
                            )))
                        }
                        Ok(_) => {
                            return Err(CliError::failed(format!(
                                "{}: not a directory",
                                show(shown)
                            )))
                        }
                        Err(Error::NotFound) if *top => plan(&mut steps, step(here, owner)),
                        Err(Error::NotFound) => {
                            return Err(CliError::refused(format!(
                                "{}: inside a subvolume other than the top level, and only the \
                                 top level is written (rust-fs-btrfs#262)",
                                show(shown)
                            )))
                        }
                        Err(e) => return Err(btrfs_error(shown, e)),
                    }
                }
            }
        };
        walk.push(next);
    }
    Ok(steps)
}

/// Where the walk goes for the directory `step` makes: the step already
/// planned for the same name in the same directory, if the walk has been
/// there before (`a/../a/b`), or `step`, planned now.
fn plan<'p>(steps: &mut Vec<Step<'p>>, step: Step<'p>) -> Here {
    let k = steps
        .iter()
        .position(|s| s.parent == step.parent && s.name == step.name)
        .unwrap_or_else(|| {
            steps.push(step);
            steps.len() - 1
        });
    Here::Made(k)
}

/// Set a file's length, as one committed transaction.
fn truncate(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let path = path_arg(sub);
    let size = *sub.get_one::<u64>("size").expect("clap requires it");
    let dev = device::open_rw(target, offset)?;
    let mut fs = Filesystem::mount_rw(dev).map_err(|e| {
        btrfs_error(
            format!("{} (read-write)", target.to_string_lossy()).as_bytes(),
            e,
        )
    })?;
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| btrfs_error(path, e))?;
    fs.truncate(inode.ino, size)
        .map_err(|e| btrfs_error(path, e))?;
    let report = Json::object([("path", Json::from(show(path))), ("size", Json::from(size))]);
    Ok(Outcome::report(report).with_text(format!("{} is {size} bytes", show(path))))
}

/// What `resize SIZE` asks for: a size, a change to the current one, or
/// all of the device.
enum NewSize {
    To(u64),
    Plus(u64),
    Minus(u64),
    Max,
}

fn new_size(spec: &str) -> Result<NewSize, CliError> {
    if spec.eq_ignore_ascii_case("max") {
        return Ok(NewSize::Max);
    }
    let bytes = |v: &str| {
        super::mkfs::parse_bytes(v)
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "resize {spec}: not a size (20G, +1G, -512M or max)"
                ))
            })
    };
    Ok(match spec.as_bytes().first() {
        Some(b'+') => NewSize::Plus(bytes(&spec[1..])?),
        Some(b'-') => NewSize::Minus(bytes(&spec[1..])?),
        _ => NewSize::To(bytes(spec)?),
    })
}

/// Grow or shrink the filesystem, as one committed transaction.
///
/// TARGET itself, when it is an image file (not a partition at
/// `--offset`, not a device node), follows the filesystem: made longer
/// first when it is too short, given its length back if the resize is
/// then refused, and cut to the new end after a shrink. Everything that
/// can be refused without writing is refused before the file is touched.
fn resize(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let spec = sub.get_one::<String>("size").expect("clap requires it");
    let force = sub.get_flag("force");
    let wanted = new_size(spec)?;
    let name = target.to_string_lossy();
    let dev = device::open_rw(target, offset)?;
    let sb = fs_btrfs::superblock::read_superblock(&*dev)
        .map(|(sb, _copy)| sb)
        .map_err(|e| CliError::failed(format!("{name} has no readable Btrfs superblock: {e}")))?;
    let current = sb.total_bytes;
    let device_len = dev.size_bytes();
    let size = match wanted {
        NewSize::To(n) => n,
        NewSize::Plus(n) => current.checked_add(n).ok_or_else(|| {
            CliError::usage(format!("resize {spec}: past the largest size there is"))
        })?,
        NewSize::Minus(n) => current.checked_sub(n).ok_or_else(|| {
            CliError::usage(format!(
                "resize {spec}: more than the filesystem's {current} bytes"
            ))
        })?,
        NewSize::Max => device_len,
    };
    let size = size - size % u64::from(sb.sectorsize);
    let report = |total: u64, device: u64| {
        Json::object([
            ("total_bytes", Json::from(total)),
            ("previous_total_bytes", Json::from(current)),
            ("device_bytes", Json::from(device)),
        ])
    };
    if size == current {
        return Ok(Outcome::report(report(current, device_len))
            .with_text(format!("{name} is already {current} bytes")));
    }
    let growing = size > current;
    if !growing && !force {
        return Err(CliError::refused(format!(
            "{name}: shrinking from {current} to {size} bytes cuts off what lies past the \
             new end; give --force"
        )));
    }
    if growing && offset != 0 && !force {
        return Err(CliError::refused(format!(
            "{name}: at --offset {offset} the partition's end cannot be seen from here, so \
             growing could reach into whatever follows it; grow the partition first, then \
             give --force"
        )));
    }

    // An image file too short for the new size is made longer first: the
    // library grows only into bytes the device already has.
    let extended = growing && size > device_len && dev.can_grow();
    if extended {
        dev.set_len(size)
            .map_err(|e| CliError::failed(format!("{name}: making it {size} bytes: {e}")))?;
    }
    let resized = Filesystem::mount_rw(dev.clone()).and_then(|mut fs| fs.resize(size));
    if let Err(e) = resized {
        let mut refusal = btrfs_error(name.as_bytes(), e);
        if extended {
            if let Err(undo) = dev.set_len(device_len) {
                refusal = CliError::failed(format!(
                    "{}; and giving {name} back its {device_len} bytes failed: {undo}",
                    refusal.message
                ));
            }
        }
        return Err(refusal);
    }
    // After a shrink, an image file is cut to the filesystem's new end.
    if !growing && dev.can_grow() {
        dev.set_len(size)
            .map_err(|e| CliError::failed(format!("{name}: cutting it to {size} bytes: {e}")))?;
    }

    // The report is what the superblock now says, read back.
    let total = fs_btrfs::superblock::read_superblock(&*dev)
        .map(|(sb, _copy)| sb.total_bytes)
        .map_err(|e| CliError::failed(format!("{name}: reading the size back: {e}")))?;
    Ok(Outcome::report(report(total, dev.size_bytes()))
        .with_text(format!("{name} resized from {current} to {total} bytes")))
}

/// `path` as its directory, resolved, and its last component.
fn split_parent<'a>(fs: &Filesystem, path: &'a [u8]) -> Result<(Inode, &'a [u8]), CliError> {
    let end = path
        .iter()
        .rposition(|&b| b != b'/')
        .ok_or_else(|| CliError::usage(format!("{}: names no entry", show(path))))?;
    let trimmed = &path[..=end];
    let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
        None => (&b""[..], trimmed),
    };
    let parent = fs.lookup_path_bytes(dir).map_err(|e| btrfs_error(dir, e))?;
    if !parent.is_dir() {
        return Err(CliError::failed(format!("{}: not a directory", show(dir))));
    }
    Ok((parent, name))
}

/// A byte string for a person to read. Never fed back into a lookup:
/// two distinct names can show the same.
fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A library error about `what`: exit 3 when the library refused (it
/// cannot do this, or will not), exit 1 when something failed.
fn btrfs_error(what: &[u8], e: Error) -> CliError {
    let message = format!("{}: {e}", show(what));
    match e {
        Error::UnsupportedFeature(_) | Error::ReadOnly => CliError::refused(message),
        _ => CliError::failed(message),
    }
}

fn type_name(t: Option<FileType>) -> &'static str {
    match t {
        Some(FileType::Regular) => "file",
        Some(FileType::Directory) => "dir",
        Some(FileType::Symlink) => "symlink",
        Some(FileType::CharDevice) => "char",
        Some(FileType::BlockDevice) => "block",
        Some(FileType::Fifo) => "fifo",
        Some(FileType::Socket) => "socket",
        None => "unknown",
    }
}

fn type_char(name: &str) -> char {
    match name {
        "file" => '-',
        "dir" => 'd',
        "symlink" => 'l',
        "char" => 'c',
        "block" => 'b',
        "fifo" => 'p',
        "socket" => 's',
        _ => '?',
    }
}

/// `path` joined with one more component.
fn child_path(path: &[u8], name: &[u8]) -> Vec<u8> {
    let mut full = path.to_vec();
    if !full.ends_with(b"/") {
        full.push(b'/');
    }
    full.extend_from_slice(name);
    full
}

/// One `ls` entry: the fields every `fs.<fs>` reports, typed the same way
/// everywhere -- name (string), type (string), size (number), mode (octal
/// string), mtime (seconds since the epoch, number), inode (number) and
/// target (string) for a symlink -- plus `subvolume` (boolean), which is
/// Btrfs's own: the entry is the top directory of another subvolume or a
/// snapshot. A name that is not UTF-8 is shown lossily, with its exact
/// bytes in `name_hex`.
fn entry(fs: &Filesystem, name: &[u8], inode: &Inode, subvolume: bool) -> Json {
    let kind = type_name(inode.file_type());
    let mut fields = vec![("name", Json::from(show(name)))];
    if std::str::from_utf8(name).is_err() {
        fields.push((
            "name_hex",
            Json::from(name.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("type", Json::from(kind)),
        ("size", Json::from(inode.size)),
        ("mode", Json::from(format!("{:04o}", inode.permissions()))),
        ("mtime", Json::from(inode.mtime.sec)),
        ("inode", Json::from(inode.ino)),
        ("subvolume", Json::from(subvolume)),
    ]);
    if inode.is_symlink() {
        fields.push((
            "target",
            Json::from(fs.read_link(inode.ino).map(|t| show(&t)).ok()),
        ));
    }
    Json::object(fields)
}

fn entry_text(e: &Json) -> String {
    let field = |k: &str| e.get(k).map(Json::to_text).unwrap_or_default();
    let mut line = format!(
        "{}{} {:>12} {}",
        type_char(&field("type")),
        field("mode"),
        field("size"),
        if e.get("path").is_some() {
            field("path")
        } else {
            field("name")
        }
    );
    if e.get("subvolume") == Some(&Json::Bool(true)) {
        line.push_str(" (subvolume)");
    }
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    if e.get("revisited") == Some(&Json::Bool(true)) {
        line.push_str(" [revisited]");
    }
    line
}

/// List `path`. A path crossing into a subvolume or a snapshot is
/// followed into it, as a mount would show it.
fn ls(root: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let found = root
        .resolve_path_bytes(path)
        .map_err(|e| btrfs_error(path, e))?;
    let fs = found.fs(root);
    let entries = if found.inode.is_dir() {
        let mut listed = Vec::new();
        for d in fs
            .read_dir(found.inode.ino)
            .map_err(|e| btrfs_error(path, e))?
        {
            let full = child_path(path, &d.name);
            if d.is_inode() {
                let child = fs.read_inode(d.ino).map_err(|e| btrfs_error(&full, e))?;
                listed.push(entry(fs, &d.name, &child, false));
            } else {
                // A subvolume's entry names a tree, not an inode; its top
                // directory is what a mount shows at this name.
                let inner = root
                    .resolve_path_bytes(&full)
                    .map_err(|e| btrfs_error(&full, e))?;
                listed.push(entry(inner.fs(root), &d.name, &inner.inode, true));
            }
        }
        listed.sort_by(|a, b| {
            a.get("name")
                .map(Json::to_text)
                .cmp(&b.get("name").map(Json::to_text))
        });
        listed
    } else {
        let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
        vec![entry(fs, name, &found.inode, false)]
    };
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Outcome::report(Json::Arr(entries)).with_text(text))
}

/// The shared finite defaults for a recursive listing.
const MAX_DEPTH: u64 = 64;
const LIMIT: u64 = 100_000;

fn recursive_entry(
    fs: &Filesystem,
    path: &[u8],
    depth: u64,
    inode: &Inode,
    subvolume: bool,
    revisited: bool,
) -> Json {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    let Json::Obj(mut fields) = entry(fs, name, inode, subvolume) else {
        unreachable!("an ls entry is an object")
    };
    fields.push(("path".into(), Json::from(show(path))));
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex".into(),
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.push(("depth".into(), Json::from(depth)));
    if revisited {
        fields.push(("revisited".into(), Json::from(true)));
    }
    Json::Obj(fields)
}

fn sorted_children(
    fs: &Filesystem,
    ino: u64,
    path: &[u8],
) -> Result<std::vec::IntoIter<fs_btrfs::dir::DirEntry>, CliError> {
    // Snapshot copies of nested subvolume entries have no backing tree.
    let mut children = if ino == fs_btrfs::subvol::EMPTY_SUBVOL_DIR_OBJECTID {
        Vec::new()
    } else {
        fs.read_dir(ino).map_err(|e| btrfs_error(path, e))?
    };
    children.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(children.into_iter())
}

/// One heap frame per directory, with its containing tree kept alive.
/// Revisit identities include the tree: unrelated subvolume roots all use 256.
fn walk(
    root: &Arc<Filesystem>,
    found: fs_btrfs::subvol::PathTarget,
    start: Vec<u8>,
    (max_depth, limit): (u64, u64),
    entries: &mut Vec<Json>,
) -> Result<(), CliError> {
    let reached = |bound: &str, value: u64, at: &[u8]| {
        CliError::failed(format!("{bound} {value} reached at {}", show(at)))
    };
    let fs = found.tree.map(Arc::new).unwrap_or_else(|| Arc::clone(root));
    if !found.inode.is_dir() {
        if limit == 0 {
            return Err(reached("--limit", limit, &start));
        }
        entries.push(recursive_entry(&fs, &start, 0, &found.inode, false, false));
        return Ok(());
    }
    let tree = fs.subvolume_at(b"/").map_err(|e| btrfs_error(&start, e))?;
    let mut visited = HashSet::from([(tree, found.inode.ino)]);
    let first = sorted_children(&fs, found.inode.ino, &start)?;
    let mut stack = vec![(fs, tree, start, 0u64, first)];
    while let Some((fs, tree, parent, parent_depth, children)) = stack.last_mut() {
        let Some(child) = children.next() else {
            stack.pop();
            continue;
        };
        let path = child_path(parent, &child.name);
        let depth = *parent_depth + 1;
        if entries.len() as u64 >= limit {
            return Err(reached("--limit", limit, &path));
        }
        if depth > max_depth {
            return Err(reached("--max-depth", max_depth, &path));
        }
        let subvolume = !child.is_inode();
        let (fs, tree, inode) = if subvolume {
            // ROOT_REF validation makes a snapshot's copied entries empty,
            // rather than following them back into the ancestor's live tree.
            let target = root
                .resolve_path_bytes(&path)
                .map_err(|e| btrfs_error(&path, e))?;
            let next = target
                .tree
                .map(Arc::new)
                .unwrap_or_else(|| Arc::clone(root));
            let id = if target.inode.ino == fs_btrfs::subvol::EMPTY_SUBVOL_DIR_OBJECTID {
                *tree
            } else {
                child.ino // A subvolume entry's location is its tree id.
            };
            (next, id, target.inode)
        } else {
            (
                Arc::clone(fs),
                *tree,
                fs.read_inode(child.ino)
                    .map_err(|e| btrfs_error(&path, e))?,
            )
        };
        // Empty inode-2 placeholders are distinct synthetic directories,
        // with no children and no real inode to put in the revisit set.
        let revisited = inode.is_dir()
            && inode.ino != fs_btrfs::subvol::EMPTY_SUBVOL_DIR_OBJECTID
            && !visited.insert((tree, inode.ino));
        entries.push(recursive_entry(
            &fs, &path, depth, &inode, subvolume, revisited,
        ));
        if inode.is_dir() && !revisited {
            let next = sorted_children(&fs, inode.ino, &path)?;
            stack.push((fs, tree, path, depth, next));
        }
    }
    Ok(())
}

fn ls_recursive(
    root: Filesystem,
    path: &[u8],
    sub: &ArgMatches,
    format: Format,
) -> Result<Outcome, CliError> {
    let found = root
        .resolve_path_bytes(path)
        .map_err(|e| btrfs_error(path, e))?;
    let bounds = (
        sub.get_one::<u64>("max-depth")
            .copied()
            .unwrap_or(MAX_DEPTH),
        sub.get_one::<u64>("limit").copied().unwrap_or(LIMIT),
    );
    let mut start = Vec::new();
    for component in path.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
        start.push(b'/');
        start.extend_from_slice(component);
    }
    if start.is_empty() {
        start.push(b'/');
    }
    let mut entries = Vec::new();
    let walked = walk(&Arc::new(root), found, start, bounds, &mut entries);
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    let report = Outcome::report(Json::Arr(entries)).with_text(text);
    match walked {
        Ok(()) => Ok(report),
        Err(error) => {
            // Preserve a complete partial report through the shared renderer.
            let partial = fs_core::cli::output::render(TOOL.name, format, Ok(report));
            let mut out = std::io::stdout().lock();
            out.write_all(partial.stdout.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            Err(CliError::failed(error.message))
        }
    }
}

/// The on-disk inode flags, with unknown bits preserved by `flag_names`.
const INODE_FLAGS: &[(u64, &str)] = &[
    (fs_btrfs::inode::INODE_NODATASUM, "NODATASUM"),
    (fs_btrfs::inode::INODE_NODATACOW, "NODATACOW"),
    (1 << 2, "READONLY"),
    (1 << 3, "NOCOMPRESS"),
    (1 << 4, "PREALLOC"),
    (1 << 5, "SYNC"),
    (1 << 6, "IMMUTABLE"),
    (1 << 7, "APPEND"),
    (1 << 8, "NODUMP"),
    (1 << 9, "NOATIME"),
    (1 << 10, "DIRSYNC"),
    (1 << 11, "COMPRESS"),
    (1 << 31, "ROOT_ITEM_INIT"),
    (fs_btrfs::verity::INODE_RO_VERITY, "RO_VERITY"),
];

#[cfg(test)]
#[test]
fn inode_flags_keep_names_and_unknown_bits() {
    assert_eq!(flag_names(0, INODE_FLAGS), Json::Arr(Vec::new()));
    assert_eq!(
        flag_names((1 << 0) | (1 << 1) | (1 << 11) | (1 << 60), INODE_FLAGS),
        Json::Arr(vec![
            Json::from("NODATASUM"),
            Json::from("NODATACOW"),
            Json::from("COMPRESS"),
            Json::from("0x1000000000000000"),
        ])
    );
}

/// An inode item's device number as major and minor.
///
/// The inode item packs the major above bit 20 and the minor below it, not
/// in the `mknod(2)` layout a send stream carries. Observed against the
/// kernel: a `c 1 3` node is `0x100003` on disk.
fn device_numbers(rdev: u64) -> (u64, u64) {
    (rdev >> 20, rdev & 0xf_ffff)
}

#[cfg(test)]
#[test]
fn device_numbers_split_at_bit_20() {
    assert_eq!(device_numbers(0x0010_0003), (1, 3));
    assert_eq!(device_numbers((7 << 20) | 300), (7, 300));
    assert_eq!(device_numbers(0), (0, 0));
}

/// One path's metadata, with the shared keys first and Btrfs's own last.
/// Resolve through subvolumes before reading attributes or a symlink's target.
fn stat(root: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let found = root
        .resolve_path_bytes(path)
        .map_err(|e| btrfs_error(path, e))?;
    let fs = found.fs(root);
    let inode = &found.inode;
    let subvolume = fs.subvolume_at(b"/").map_err(|e| btrfs_error(path, e))?;
    let compression = fs
        .get_xattr(inode.ino, b"btrfs.compression")
        .map_err(|e| btrfs_error(path, e))?;
    // The kernel's stat accounts in whole filesystem sectors, including
    // inline data; nbytes itself can hold a partial sector.
    let sector = u64::from(fs.superblock().sectorsize);
    let allocated = inode
        .nbytes
        .checked_add(sector - 1)
        .ok_or_else(|| CliError::failed(format!("{}: allocated size overflows", show(path))))?
        & !(sector - 1);
    let mut fields = vec![("path", Json::from(show(path)))];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("inode", Json::from(inode.ino)),
        ("type", Json::from(type_name(inode.file_type()))),
        ("mode", Json::from(format!("{:04o}", inode.permissions()))),
        ("uid", Json::from(inode.uid)),
        ("gid", Json::from(inode.gid)),
        ("nlink", Json::from(inode.nlink)),
        ("size", Json::from(inode.size)),
        ("allocated_bytes", Json::from(allocated)),
        ("atime", Json::from(inode.atime.sec)),
        ("atime_nsec", Json::from(inode.atime.nsec)),
        ("mtime", Json::from(inode.mtime.sec)),
        ("mtime_nsec", Json::from(inode.mtime.nsec)),
        ("ctime", Json::from(inode.ctime.sec)),
        ("ctime_nsec", Json::from(inode.ctime.nsec)),
        ("btime", Json::from(inode.otime.sec)),
        ("btime_nsec", Json::from(inode.otime.nsec)),
    ]);
    match inode.file_type() {
        Some(FileType::Symlink) => fields.push((
            "target",
            Json::from(
                fs.read_link(inode.ino)
                    .map(|t| show(&t))
                    .map_err(|e| btrfs_error(path, e))?,
            ),
        )),
        Some(FileType::CharDevice | FileType::BlockDevice) => {
            let (major, minor) = device_numbers(inode.rdev);
            fields.push(("major", Json::from(major)));
            fields.push(("minor", Json::from(minor)));
        }
        _ => {}
    }
    fields.push((
        "btrfs",
        Json::object([
            ("subvolume", Json::from(subvolume)),
            ("flags", flag_names(inode.flags, INODE_FLAGS)),
            ("generation", Json::from(inode.generation)),
            ("transid", Json::from(inode.transid)),
            ("sequence", Json::from(inode.sequence)),
            ("compression", Json::from(compression.map(|v| show(&v)))),
        ]),
    ));
    Ok(Outcome::report(Json::object(fields)))
}

/// Inspect attributes in the resolved path's own tree, including inside
/// subvolumes and snapshots. A value is read completely before any output
/// is opened, so lookup failures cannot truncate a destination file.
fn xattr(
    root: &Filesystem,
    path: &[u8],
    name: Option<&[u8]>,
    output: Option<&OsString>,
) -> Result<Outcome, CliError> {
    let found = root
        .resolve_path_bytes(path)
        .map_err(|e| btrfs_error(path, e))?;
    let fs = found.fs(root);
    if let Some(name) = name {
        let value = fs
            .get_xattr(found.inode.ino, name)
            .map_err(|e| btrfs_error(path, e))?
            .ok_or_else(|| {
                CliError::failed(format!(
                    "{}: no such extended attribute {}",
                    show(path),
                    show(name)
                ))
            })?;
        if let Some(file) = output {
            let dest = std::path::Path::new(file);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let written =
                std::fs::write(&partial, &value).and_then(|()| std::fs::rename(&partial, dest));
            if let Err(e) = written {
                let _ = std::fs::remove_file(&partial);
                return Err(CliError::failed(format!("write {}: {e}", dest.display())));
            }
        } else {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&value)
                .and_then(|()| stdout.flush())
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
        }
        return Ok(Outcome::done());
    }
    let mut attrs = fs
        .list_xattrs(found.inode.ino)
        .map_err(|e| btrfs_error(path, e))?;
    attrs.sort_by(|a, b| a.name.cmp(&b.name));
    let text = attrs
        .iter()
        .map(|attr| format!("{}\t{}", show(&attr.name), attr.value.len()))
        .collect::<Vec<_>>()
        .join("\n");
    let entries = attrs
        .into_iter()
        .map(|attr| {
            let mut fields = vec![
                ("name", Json::from(show(&attr.name))),
                ("size", Json::from(attr.value.len() as u64)),
            ];
            if std::str::from_utf8(&attr.name).is_err() {
                fields.push((
                    "name_hex",
                    Json::from(
                        attr.name
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                    ),
                ));
            }
            Json::object(fields)
        })
        .collect();
    Ok(Outcome::report(Json::Arr(entries)).with_text(text))
}

/// A positioned source in the resolved path's own subvolume or snapshot.
struct ExportSource<'a> {
    fs: &'a Filesystem,
    inode: &'a Inode,
    path: &'a [u8],
}

impl transfer::FileSource for ExportSource<'_> {
    fn size(&self) -> u64 {
        self.inode.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, CliError> {
        self.fs
            .read_at(self.inode.ino, offset, buf)
            .map_err(|e| btrfs_error(self.path, e))
    }
}

/// Resolve the source here; core owns copying, digesting and publication.
fn export(root: &Filesystem, request: &transfer::ExportRequest) -> Result<Outcome, CliError> {
    let path = os_bytes(&request.path);
    let found = root
        .resolve_path_bytes(path)
        .map_err(|e| btrfs_error(path, e))?;
    let inode = &found.inode;
    if inode.is_dir() {
        return Err(CliError::failed(format!("{}: is a directory", show(path))));
    }
    if inode.is_symlink() {
        return Err(CliError::failed(format!(
            "{}: is a symlink; export the target instead",
            show(path)
        )));
    }
    if !inode.is_regular_file() {
        return Err(CliError::failed(format!(
            "{}: not a regular file",
            show(path)
        )));
    }
    transfer::export(
        request,
        &mut ExportSource {
            fs: found.fs(root),
            inode,
            path,
        },
    )
}

/// Stream a regular file's bytes. Each chunk is read before it is
/// written, so a file whose blocks turn out unreadable part-way stops with
/// status 1 and what came before stays on stdout; everything that can be
/// refused up front (no such path, a directory, a symlink) is refused
/// before a byte is written. `-o FILE` writes `FILE.partial` and renames
/// it, so FILE is never left half written.
fn read(root: &Filesystem, path: &[u8], output: Option<&OsString>) -> Result<Outcome, CliError> {
    let found = root
        .resolve_path_bytes(path)
        .map_err(|e| btrfs_error(path, e))?;
    let fs = found.fs(root);
    let inode = &found.inode;
    if inode.is_dir() {
        return Err(CliError::failed(format!("{}: is a directory", show(path))));
    }
    if inode.is_symlink() {
        let target = fs
            .read_link(inode.ino)
            .map(|t| show(&t))
            .unwrap_or_default();
        return Err(CliError::failed(format!(
            "{}: is a symlink to {target}; read the target instead",
            show(path)
        )));
    }
    if !inode.is_regular_file() {
        return Err(CliError::failed(format!(
            "{}: not a regular file",
            show(path)
        )));
    }
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    let mut copy = |sink: &mut dyn Write| -> Result<(), CliError> {
        let mut offset = 0u64;
        while offset < inode.size {
            let want = CHUNK.min((inode.size - offset) as usize);
            let got = fs
                .read_at(inode.ino, offset, &mut buf[..want])
                .map_err(|e| btrfs_error(path, e))?;
            if got == 0 {
                return Err(CliError::failed(format!(
                    "{}: short read at byte {offset} of {}",
                    show(path),
                    inode.size
                )));
            }
            sink.write_all(&buf[..got])
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            offset += got as u64;
        }
        sink.flush()
            .map_err(|e| CliError::failed(format!("write: {e}")))
    };
    match output {
        None => copy(&mut std::io::stdout().lock())?,
        Some(file) => {
            let dest = std::path::Path::new(file);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let mut f = std::fs::File::create(&partial)
                .map_err(|e| CliError::failed(format!("create {}: {e}", partial.display())))?;
            if let Err(e) = copy(&mut f) {
                drop(f);
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            std::fs::rename(&partial, dest)
                .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))?;
        }
    }
    Ok(Outcome::done())
}

/// A UUID in its standard 8-4-4-4-12 form.
fn uuid_text(u: &[u8; 16]) -> String {
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The checksum algorithm, by the name btrfs-progs gives it.
pub fn csum_name(t: ChecksumType) -> &'static str {
    match t {
        ChecksumType::Crc32c => "crc32c",
        ChecksumType::XxHash64 => "xxhash64",
        ChecksumType::Sha256 => "sha256",
        ChecksumType::Blake2b256 => "blake2",
    }
}

/// The names of the bits set in `bits`, spelled as the table says;
/// a bit this table does not know is given in hex.
fn flag_names(bits: u64, table: &[(u64, &str)]) -> Json {
    let mut names = Vec::new();
    let mut known = 0u64;
    for &(bit, name) in table {
        known |= bit;
        if bits & bit != 0 {
            names.push(Json::from(name));
        }
    }
    let unknown = bits & !known;
    if unknown != 0 {
        names.push(Json::from(format!("{unknown:#x}")));
    }
    Json::Arr(names)
}

const INCOMPAT: &[(u64, &str)] = &[
    (incompat::MIXED_BACKREF, "mixed_backref"),
    (incompat::DEFAULT_SUBVOL, "default_subvol"),
    (incompat::MIXED_GROUPS, "mixed_groups"),
    (incompat::COMPRESS_LZO, "compress_lzo"),
    (incompat::COMPRESS_ZSTD, "compress_zstd"),
    (incompat::BIG_METADATA, "big_metadata"),
    (incompat::EXTENDED_IREF, "extended_iref"),
    (incompat::RAID56, "raid56"),
    (incompat::SKINNY_METADATA, "skinny_metadata"),
    (incompat::NO_HOLES, "no_holes"),
    (incompat::METADATA_UUID, "metadata_uuid"),
    (incompat::RAID1C34, "raid1c34"),
    (incompat::ZONED, "zoned"),
    (incompat::EXTENT_TREE_V2, "extent_tree_v2"),
    (incompat::RAID_STRIPE_TREE, "raid_stripe_tree"),
    (incompat::SIMPLE_QUOTA, "simple_quota"),
    (incompat::REMAP_TREE, "remap_tree"),
];

const COMPAT_RO: &[(u64, &str)] = &[
    (compat_ro::FREE_SPACE_TREE, "free_space_tree"),
    (compat_ro::FREE_SPACE_TREE_VALID, "free_space_tree_valid"),
    (compat_ro::VERITY, "verity"),
    (compat_ro::BLOCK_GROUP_TREE, "block_group_tree"),
];

/// Whether the volume needs attention before it can be trusted: a log
/// tree waiting to be replayed, or the error flag the kernel sets when it
/// forced the volume read-only.
pub fn is_dirty(sb: &Superblock) -> bool {
    sb.has_dirty_log() || sb.has_error_flag()
}

/// The envelope: the shared keys first, Btrfs's own under `btrfs`.
///
/// From the superblock alone, so a volume that will not mount -- a log
/// waiting for replay -- still answers, and says it is dirty.
pub fn envelope(sb: &Superblock) -> Json {
    Json::object([
        ("fs", Json::from("btrfs")),
        (
            "label",
            if sb.label.is_empty() {
                Json::Null
            } else {
                Json::from(sb.label.as_str())
            },
        ),
        ("total_bytes", Json::from(sb.total_bytes)),
        (
            "free_bytes",
            Json::from(sb.total_bytes.saturating_sub(sb.bytes_used)),
        ),
        ("block_size", Json::from(sb.sectorsize)),
        ("dirty", Json::from(is_dirty(sb))),
        (
            "btrfs",
            Json::object([
                ("fsid", Json::from(uuid_text(&sb.fsid))),
                ("metadata_uuid", Json::from(uuid_text(&sb.node_uuid()))),
                ("node_size", Json::from(sb.nodesize)),
                ("sector_size", Json::from(sb.sectorsize)),
                ("csum_type", Json::from(csum_name(sb.csum_type))),
                ("device_count", Json::from(sb.num_devices)),
                ("bytes_used", Json::from(sb.bytes_used)),
                ("generation", Json::from(sb.generation)),
                ("log_root", Json::from(sb.log_root)),
                (
                    "features",
                    Json::object([
                        ("compat", Json::from(format!("{:#x}", sb.compat_flags))),
                        ("compat_ro", flag_names(sb.compat_ro_flags, COMPAT_RO)),
                        ("incompat", flag_names(sb.incompat_flags, INCOMPAT)),
                    ]),
                ),
            ]),
        ),
    ])
}

fn get(target: &OsString, offset: u64, key: Option<&str>) -> Result<Outcome, CliError> {
    let sb = device::superblock(target, offset)?;
    let all = envelope(&sb);
    let Some(key) = key else {
        return Ok(Outcome::report(all));
    };
    let mut value = Some(&all);
    for part in key.split('.') {
        value = value.and_then(|v| v.get(part));
    }
    let Some(value) = value else {
        return Err(CliError::usage(format!(
            "no key {key:?}; the keys are {} (and btrfs.<field>)",
            KEYS.join(", ")
        )));
    };
    let text = value.to_text();
    Ok(Outcome::report(Json::object([(key, value.clone())])).with_text(text))
}

fn set(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let key = sub.get_one::<String>("key").expect("clap requires the key");
    match key.as_str() {
        "label" => {
            let value = sub
                .get_one::<String>("value")
                .expect("clap requires the value");
            if value.len() > fs_btrfs::super_write::MAX_LABEL_BYTES {
                return Err(CliError::failed(format!(
                    "set label: {} bytes, and a Btrfs label holds at most {}",
                    value.len(),
                    fs_btrfs::super_write::MAX_LABEL_BYTES
                )));
            }
            let dev = device::open_rw(target, offset)?;
            fs_btrfs::super_write::set_label(&*dev, value)
                .map_err(|e| btrfs_error(target.to_string_lossy().as_bytes(), e))?;
            Ok(
                Outcome::report(Json::object([("label", Json::from(value.as_str()))]))
                    .with_text(format!("label set to {value:?}")),
            )
        }
        k if KEYS.contains(&k) || k.starts_with("btrfs.") => {
            Err(CliError::refused(format!("{k} is read-only")))
        }
        other => Err(CliError::usage(format!(
            "no key {other:?}; the settable key is label"
        ))),
    }
}

/// Overwrite an existing file with everything on stdin, which must be at
/// least as long as the file: a NODATACOW file in place, any other one
/// copy-on-write through `Filesystem::write`, which grows it where it can. The whole input is read
/// before the image is opened, so a failing producer
/// (`false | fs.btrfs img write /f`) leaves the image as it was; the
/// library writes the whole range or none of it.
fn write(target: &OsString, offset: u64, path: &[u8]) -> Result<Outcome, CliError> {
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut data)
        .map_err(|e| CliError::failed(format!("read stdin: {e}")))?;
    let dev = device::open_rw(target, offset)?;
    let mut fs = Filesystem::mount_rw(dev).map_err(|e| {
        btrfs_error(
            format!("{} (read-write)", target.to_string_lossy()).as_bytes(),
            e,
        )
    })?;
    let found = match fs.resolve_path_bytes(path) {
        Ok(found) => found,
        Err(Error::NotFound) => {
            return Err(CliError::refused(format!(
                "write {}: no such file; write changes the bytes of a file that \
                 exists, and create makes an empty one",
                show(path)
            )))
        }
        Err(e) => return Err(btrfs_error(path, e)),
    };
    if found.tree.is_some() {
        return Err(CliError::refused(format!(
            "{}: is inside a subvolume or snapshot, which this library opens read-only",
            show(path)
        )));
    }
    let inode = &found.inode;
    if inode.is_dir() {
        return Err(CliError::failed(format!("{}: is a directory", show(path))));
    }
    if !inode.is_regular_file() {
        return Err(CliError::failed(format!(
            "{}: not a regular file",
            show(path)
        )));
    }
    if (data.len() as u64) < inode.size {
        return Err(CliError::refused(format!(
            "{}: {} bytes on stdin for a {}-byte file; write keeps a file's length, \
             and truncate makes it shorter",
            show(path),
            data.len(),
            inode.size
        )));
    }
    let in_place = inode.flags & fs_btrfs::write::INODE_NODATACOW != 0;
    let ino = inode.ino;
    let written = fs.write(ino, 0, &data).map_err(|e| match e {
        // Each refusal is a case the copy-on-write path does not cover
        // yet; say where that work is tracked.
        Error::UnsupportedFeature(why) => {
            CliError::refused(format!("{}: {why} (rust-fs-btrfs#261)", show(path)))
        }
        e => btrfs_error(path, e),
    })?;
    let report = Json::object([
        ("path", Json::from(show(path))),
        ("bytes", Json::from(written as u64)),
        ("created", Json::from(false)),
    ]);
    let how = if in_place {
        "in place"
    } else {
        "copy-on-write"
    };
    let text = format!("overwrote {} ({written} bytes, {how})", show(path));
    Ok(Outcome::report(report).with_text(text))
}
