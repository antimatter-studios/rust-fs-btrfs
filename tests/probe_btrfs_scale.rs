//! Scratch probe (not for commit): many in-place writes across many files
//! and commits, with `btrfs check` and a kernel mount judging each round.

use fs_btrfs::Filesystem;
use fs_core::FileDevice;
use std::process::Command;
use std::sync::Arc;

const FILES: usize = 40;
const FILE_BYTES: u64 = 256 * 1024;

fn sudo(script: &str) -> String {
    let out = Command::new("sudo")
        .args([
            "-n",
            "env",
            &format!("PATH={}", std::env::var("PATH").unwrap()),
            "bash",
            "-c",
            script,
        ])
        .output()
        .expect("sudo bash");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Mount with the kernel, read every file's digest, unmount, then check.
fn kernel_verdict(image: &str) -> String {
    sudo(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop {image} "$m"; then
            md5sum "$m"/nc/f* | md5sum | cut -d' ' -f1 | sed 's/^/DIGEST /'
            umount "$m"
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -6
        fi
        rmdir "$m"
        btrfs check {image} > /tmp/bcheck.$$ 2>&1 && echo CHECK_RC=0 || echo CHECK_RC=$?
        grep -iE "error|warning|corrupt" /tmp/bcheck.$$ | head -10
        rm -f /tmp/bcheck.$$
        "#
    ))
}

fn run(tag: &str, mkfs_args: &str, size: &str) {
    let image = format!(
        "{}/btrfs-probe-{tag}-{}.img",
        std::env::temp_dir().display(),
        std::process::id()
    );
    let built = sudo(&format!(
        r#"
        set -e
        truncate -s {size} {image}
        mkfs.btrfs -f {mkfs_args} {image} > /dev/null
        m=$(mktemp -d)
        mount -o loop {image} "$m"
        mkdir "$m/nc"
        chattr +C "$m/nc"
        for i in $(seq 0 {last}); do
            dd if=/dev/urandom of="$m/nc/f$i" bs=4096 count={blocks} status=none
        done
        sync
        umount "$m"
        rmdir "$m"
        chmod 666 {image}
        echo BUILT
        "#,
        size = size,
        last = FILES - 1,
        blocks = FILE_BYTES / 4096,
    ));
    assert!(built.contains("BUILT"), "building {tag} failed:\n{built}");

    let mut round = 0;
    let mut last_digest = String::new();
    let mut refusals: std::collections::BTreeMap<String, usize> = Default::default();
    // Each round is its own mount, so each is its own commit.
    for step in 0..500u64 {
        {
            let dev = FileDevice::open_rw(&image).expect("open rw");
            let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount_rw");
            for k in 0..5u64 {
                let file = format!("/nc/f{}", (step * 5 + k) as usize % FILES);
                let ino = match fs.lookup_path(&file) {
                    Ok(i) => i.ino,
                    Err(e) => {
                        *refusals.entry(format!("lookup {e}")).or_default() += 1;
                        continue;
                    }
                };
                let offset = ((step * 7 + k * 13) % 60) * 4096;
                let len = 512 + ((step + k) % 8) as usize * 512;
                let payload = vec![(step as u8).wrapping_add(k as u8) | 1; len];
                if let Err(e) = fs.write_at(ino, offset, &payload) {
                    *refusals.entry(format!("write {e}")).or_default() += 1;
                }
            }
        }
        if step % 25 == 24 {
            round += 1;
            let out = kernel_verdict(&image);
            assert!(
                out.contains("MOUNTED") && out.contains("CHECK_RC=0"),
                "[{tag}] after round {round} (step {step}) the kernel or btrfs check \
                 rejected the volume:\n{out}"
            );
            // The oracle has to be able to fail: if the kernel reads the same
            // bytes after 50 writes as before them, nothing is being written
            // and a clean check means nothing.
            let digest = out
                .lines()
                .find_map(|l| l.strip_prefix("DIGEST "))
                .expect("the kernel reported no digest")
                .to_string();
            assert_ne!(
                digest, last_digest,
                "[{tag}] round {round}: the kernel read back the same bytes as last \
                 round, so these writes did not land"
            );
            last_digest = digest;
        }
    }
    eprintln!("[{tag}] 2500 writes over {round} checked rounds; refusals: {refusals:?}");
    let _ = sudo(&format!("rm -f {image}"));
}

#[test]
fn probe_default() {
    run("default", "", "1G");
}

#[test]
fn probe_node4k_dup() {
    run("n4k-dup", "-n 4096 -m dup -d single", "1G");
}

#[test]
fn probe_mixed() {
    run("mixed", "-M", "512M");
}
