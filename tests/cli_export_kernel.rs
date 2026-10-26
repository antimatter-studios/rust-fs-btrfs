//! Shared export compared with files and digests made by the Linux kernel.
mod cli_support;
use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_write_ok, oracle, sha256_hex,
};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

fn source(tag: &str) -> (PathBuf, Vec<(String, String)>) {
    let image = scratch_dir(tag).join("fs.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 << 20)
        .unwrap();
    let out = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384"])
        .arg(&image)
        .output();
    assert!(out.status.success(), "mkfs.btrfs: {}", stderr(&out));
    let report = guest_kernel_write_ok(
        image.to_str().unwrap(),
        "export source and SHA-256 manifest",
        r#"
cd "$MNT"
: > empty
head -c 3145729 /dev/urandom > large
mkdir dir
ln -s large link
mkfifo fifo
btrfs subvolume create sub >/dev/null
printf snapshot > sub/file
btrfs subvolume snapshot sub snap >/dev/null
printf live > sub/file
for file in empty large sub/file snap/file; do sha256sum "$file"; done
"#,
    );
    assert_btrfs_check_clean(&image, "kernel export source");
    let manifest: Vec<_> = report
        .lines()
        .filter_map(|line| {
            let (digest, name) = line.split_once("  ")?;
            (digest.len() == 64).then(|| (format!("/{name}"), digest.to_string()))
        })
        .collect();
    assert_eq!(manifest.len(), 4, "kernel manifest: {report}");
    (image, manifest)
}

#[test]
fn empty_multichunk_and_subvolume_exports_equal_the_kernel_digests() {
    let (image, manifest) = source("export-kernel");
    let outdir = scratch_dir("export-destinations");
    for (index, (path, digest)) in manifest.iter().enumerate() {
        let dest = outdir.join(index.to_string());
        let out = ok(tool("fs.btrfs")
            .arg(&image)
            .args(["export", path])
            .arg(&dest));
        let bytes = std::fs::read(&dest).unwrap_or_else(|error| {
            panic!(
                "read {}: {error}; export stdout: {}; directory: {:?}",
                dest.display(),
                stdout(&out),
                std::fs::read_dir(&outdir)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>()
            )
        });
        assert_eq!(&sha256_hex(&bytes), digest, "{path}");
        assert_eq!(json_field(&stdout(&out), "sha256"), *digest);
        assert_eq!(json_field(&stdout(&out), "bytes"), bytes.len().to_string());
        assert_eq!(
            bytes.len(),
            match path.as_str() {
                "/empty" => 0,
                "/large" => 3145729,
                "/sub/file" => 4,
                _ => 8,
            }
        );
        let raw = ok(tool("fs.btrfs")
            .arg(&image)
            .args(["export", path, "-", "--text"]));
        assert_eq!(raw.stdout, bytes);
        assert!(raw.stderr.is_empty());
        refused(
            tool("fs.btrfs")
                .arg(&image)
                .args(["export", path])
                .arg(&dest),
            1,
        );
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
        std::fs::write(&dest, b"old destination").unwrap();
        ok(tool("fs.btrfs")
            .arg(&image)
            .args(["export", "--replace", path])
            .arg(&dest));
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    }
    for path in ["/dir", "/link", "/fifo", "/missing"] {
        let dest = outdir.join("refused");
        refused(
            tool("fs.btrfs")
                .arg(&image)
                .args(["export", path])
                .arg(&dest),
            1,
        );
        assert!(!dest.exists());
    }
    // A partition inside a larger image must use the same positioned source.
    let embedded = outdir.join("embedded.img");
    let mut file = std::fs::File::create(&embedded).unwrap();
    file.set_len(4096).unwrap();
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(4096)).unwrap();
    std::io::copy(&mut std::fs::File::open(&image).unwrap(), &mut file).unwrap();
    let dest = outdir.join("offset");
    ok(tool("fs.btrfs")
        .arg(&embedded)
        .args(["--offset", "4096", "export", "/large"])
        .arg(&dest));
    assert_eq!(sha256_hex(&std::fs::read(dest).unwrap()), manifest[1].1);
}

#[test]
fn damaged_source_data_never_publishes_or_replaces_a_destination() {
    let (image, _) = source("export-damaged");
    // The independent tools locate data and its mirrors; no driver parser
    // participates in choosing the byte that the test damages.
    let dump = dump_tree(&image, "5");
    let logical: u64 = dump
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("extent data disk byte ")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .filter(|n| *n != 0)
        })
        .expect("the kernel's large file has a regular data extent");
    let mapped = oracle("btrfs-map-logical")
        .args(["-l", &logical.to_string()])
        .arg(&image)
        .output();
    assert!(mapped.status.success(), "{}", stderr(&mapped));
    let copies: Vec<u64> = stdout(&mapped)
        .lines()
        .filter(|line| line.starts_with("mirror"))
        .filter_map(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            let at = words.iter().position(|word| *word == "physical")?;
            words.get(at + 1)?.parse().ok()
        })
        .collect();
    assert!(!copies.is_empty(), "{}", stdout(&mapped));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image)
        .unwrap();
    for physical in copies {
        let mut byte = [0];
        file.read_exact_at(&mut byte, physical).unwrap();
        byte[0] ^= 0xff;
        file.write_all_at(&byte, physical).unwrap();
    }
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert!(
        !checked.status.success(),
        "the independent checker must detect the damaged data"
    );
    let outdir = scratch_dir("export-failure-dest");
    let dest = outdir.join("file");
    refused(
        tool("fs.btrfs")
            .arg(&image)
            .args(["export", "/large"])
            .arg(&dest),
        1,
    );
    assert!(!dest.exists());
    std::fs::write(&dest, b"keep this file").unwrap();
    refused(
        tool("fs.btrfs")
            .arg(&image)
            .args(["export", "--replace", "/large"])
            .arg(&dest),
        1,
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep this file");
    assert_eq!(
        std::fs::read_dir(outdir).unwrap().count(),
        1,
        "no staging file survives"
    );
}
