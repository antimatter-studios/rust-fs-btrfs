//! The harness holds a seed pool for one guest operation at a time: a
//! checker of the same devices waits while the kernel has them mounted.
//!
//! THIS BINARY HOLDS ONE TEST. It runs two guest operations at once on
//! purpose, and cargo runs one test binary at a time, so alone here no other
//! test adds guest work beside them (#822).

use fs_btrfs_test_support::{
    copy_sparse, fixture, guest_kernel_pool_check, guest_kernel_pool_read, guest_quote, temp_path,
};

#[test]
fn pool_check_waits_for_kernel_readback_using_the_same_device_identities() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let seed = std::path::PathBuf::from(temp_path!("seed-registry-seed.img"));
    let sprout = std::path::PathBuf::from(temp_path!("seed-registry-sprout.img"));
    for (name, copy) in [("btrfs-seed.img", &seed), ("btrfs-sprout.img", &sprout)] {
        copy_sparse(fixture(&format!("seed/{name}")), copy).unwrap();
    }
    let ready = std::path::PathBuf::from(temp_path!("seed-registry.ready"));
    let release = std::path::PathBuf::from(temp_path!("seed-registry.release"));
    let images = [sprout.to_str().unwrap(), seed.to_str().unwrap()];
    let script = format!(
        "touch {}\nfor attempt in $(seq 1 6000); do\n\
         [ ! -f {} ] || exit 0\nsleep 0.01\ndone\n\
         echo 'the registry test never released its kernel mount' >&2\nexit 1",
        guest_quote(ready.to_str().unwrap()),
        guest_quote(release.to_str().unwrap()),
    );
    std::thread::scope(|threads| {
        let reader = threads.spawn(|| guest_kernel_pool_read(&images, &script));
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ready.is_file() && !reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !ready.is_file() {
            std::fs::write(&release, b"release").unwrap();
            reader.join().unwrap();
            panic!("the kernel oracle never held the seed pool mounted");
        }
        let (finished, completion) = mpsc::channel();
        let checker = threads.spawn(move || {
            let out = guest_kernel_pool_check(&images);
            let _ = finished.send(());
            out
        });
        let blocked = matches!(
            completion.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        // Always release the reader before asserting, so a regression
        // cannot leave a test thread holding the mount during unwinding.
        std::fs::write(&release, b"release").unwrap();
        let read = reader.join().unwrap();
        let check = checker.join().unwrap();
        assert!(blocked, "a checker ran while the kernel held the same pool");
        assert_eq!(read.check_status, 0, "{}", read.check);
        assert!(read.out.status.success(), "{:?}", read.out);
        assert!(check.status.success(), "{:?}", check);
    });
}
