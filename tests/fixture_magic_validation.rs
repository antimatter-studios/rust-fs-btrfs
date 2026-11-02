use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[path = "common/fixture_magic.rs"]
mod fixture_magic;

const MAGIC_AT: u64 = 65536 + 64;
const SMALL_IMAGE: u64 = MAGIC_AT + 9;

fn image_directory() -> PathBuf {
    fs_btrfs_test_support::create_unique_temp_dir(fs_btrfs_test_support::temp_dir()).unwrap()
}

fn write_image(dir: &Path, name: &str, length: u64, magic: &[u8; 8]) {
    let mut image = File::create(dir.join(name)).unwrap();
    image.set_len(length).unwrap();
    if length >= MAGIC_AT + 8 {
        image.seek(SeekFrom::Start(MAGIC_AT)).unwrap();
        image.write_all(magic).unwrap();
    }
}

fn names<const COUNT: usize>(names: [&str; COUNT]) -> Vec<String> {
    names.into_iter().map(str::to_owned).collect()
}

fn fails_about(action: impl FnOnce(), expected: &[&str]) {
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
        .expect_err("invalid image was accepted");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .expect("the failure must have a diagnostic");
    for text in expected {
        assert!(message.contains(text), "{message:?} does not name {text:?}");
    }
}

#[test]
fn truncated_images_fail_by_name_including_the_existing_length_boundary() {
    let dir = image_directory();
    for length in [0, MAGIC_AT + 7, MAGIC_AT + 8] {
        write_image(&dir, "truncated.img", length, b"_BHRfS_M");
        fails_about(
            || {
                fixture_magic::check_fixture_magic(&dir, names(["truncated.img"]));
            },
            &[
                "truncated.img",
                &length.to_string(),
                "shorter than one superblock",
            ],
        );
    }
}

#[test]
fn wrong_magic_fails_by_name() {
    let dir = image_directory();
    write_image(&dir, "wrong.img", SMALL_IMAGE, b"notbtrfs");
    fails_about(
        || {
            fixture_magic::check_fixture_magic(&dir, names(["wrong.img"]));
        },
        &["wrong.img", "no btrfs superblock magic"],
    );
}

#[test]
fn an_unreadable_existing_image_cannot_hide_behind_a_valid_image() {
    let dir = image_directory();
    write_image(&dir, "valid.img", SMALL_IMAGE, b"_BHRfS_M");
    write_image(&dir, "unreadable.img", SMALL_IMAGE, b"_BHRfS_M");
    assert!(dir.join("unreadable.img").is_file());
    fails_about(
        || {
            fixture_magic::check_fixture_magic_with(
                &dir,
                names(["valid.img", "unreadable.img"]),
                |path| {
                    if path.ends_with("unreadable.img") {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "permission denied",
                        ));
                    }
                    let image = File::open(path)?;
                    let length = image.metadata()?.len();
                    Ok((image, length))
                },
            );
        },
        &["unreadable.img", "permission denied"],
    );
}

struct Reader {
    image: File,
    fail_read: bool,
    fail_seek: bool,
    bounded: bool,
    read_bytes: Rc<Cell<usize>>,
    seeks: Rc<Cell<usize>>,
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.fail_read {
            return Err(io::Error::other("injected read failure"));
        }
        if self.bounded {
            assert!(
                buffer.len() <= 8 - self.read_bytes.get(),
                "requested {} bytes beyond the eight-byte read budget",
                buffer.len()
            );
        }
        let count = self.image.read(buffer)?;
        self.read_bytes.set(self.read_bytes.get() + count);
        Ok(count)
    }
}

impl Seek for Reader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.fail_seek {
            return Err(io::Error::other("injected seek failure"));
        }
        if self.bounded {
            assert_eq!(position, SeekFrom::Start(MAGIC_AT));
        }
        self.seeks.set(self.seeks.get() + 1);
        self.image.seek(position)
    }
}

fn reader(path: &Path) -> io::Result<(Reader, u64)> {
    let image = File::open(path)?;
    let length = image.metadata()?.len();
    Ok((
        Reader {
            image,
            fail_read: false,
            fail_seek: false,
            bounded: false,
            read_bytes: Rc::default(),
            seeks: Rc::default(),
        },
        length,
    ))
}

#[test]
fn a_read_failure_cannot_hide_behind_a_valid_image() {
    let dir = image_directory();
    write_image(&dir, "valid.img", SMALL_IMAGE, b"_BHRfS_M");
    write_image(&dir, "read-error.img", SMALL_IMAGE, b"_BHRfS_M");
    fails_about(
        || {
            fixture_magic::check_fixture_magic_with(
                &dir,
                names(["valid.img", "read-error.img"]),
                |path| {
                    let (mut image, length) = reader(path)?;
                    image.fail_read = path.ends_with("read-error.img");
                    Ok((image, length))
                },
            );
        },
        &["read-error.img", "injected read failure"],
    );
}

#[test]
fn a_seek_failure_fails_by_name() {
    let dir = image_directory();
    write_image(&dir, "seek-error.img", SMALL_IMAGE, b"_BHRfS_M");
    fails_about(
        || {
            fixture_magic::check_fixture_magic_with(&dir, names(["seek-error.img"]), |path| {
                let (mut image, length) = reader(path)?;
                image.fail_seek = true;
                Ok((image, length))
            });
        },
        &["seek-error.img", "injected seek failure"],
    );
}

#[test]
fn a_valid_one_tib_sparse_image_needs_only_eight_bytes_and_one_seek() {
    let dir = image_directory();
    write_image(&dir, "large.img", 1 << 40, b"_BHRfS_M");
    let read_bytes = Rc::new(Cell::new(0));
    let seeks = Rc::new(Cell::new(0));
    let checked = fixture_magic::check_fixture_magic_with(&dir, names(["large.img"]), |path| {
        let (mut image, length) = reader(path)?;
        image.bounded = true;
        image.read_bytes = Rc::clone(&read_bytes);
        image.seeks = Rc::clone(&seeks);
        Ok((image, length))
    });
    assert_eq!(checked, 1);
    assert_eq!(read_bytes.get(), 8);
    assert_eq!(seeks.get(), 1);
}

#[test]
fn missing_images_remain_the_existence_checks_failure_and_non_images_are_not_read() {
    let dir = image_directory();
    write_image(&dir, "valid.img", SMALL_IMAGE, b"_BHRfS_M");
    std::fs::write(dir.join("not-an-image.manifest"), b"not an image").unwrap();
    assert_eq!(
        fixture_magic::check_fixture_magic(
            &dir,
            names(["missing.img", "not-an-image.manifest", "valid.img"])
        ),
        1
    );
}

#[test]
fn checking_no_image_still_fails() {
    let dir = image_directory();
    fails_about(
        || {
            fixture_magic::check_fixture_magic(&dir, names(["missing.img", "absent.manifest"]));
        },
        &["no fixture image was checked, so this compared nothing"],
    );
}
