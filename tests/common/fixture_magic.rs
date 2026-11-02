use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub(crate) fn check_fixture_magic(dir: &Path, names: impl IntoIterator<Item = String>) -> usize {
    check_fixture_magic_with(dir, names, |path| {
        let image = File::open(path)?;
        let length = image.metadata()?.len();
        Ok((image, length))
    })
}

pub(crate) fn check_fixture_magic_with<Reader: Read + Seek>(
    dir: &Path,
    names: impl IntoIterator<Item = String>,
    mut open: impl FnMut(&Path) -> io::Result<(Reader, u64)>,
) -> usize {
    const MAGIC_AT: u64 = 0x1_0000 + 0x40;
    let mut checked = 0;
    for name in names {
        if !name.ends_with(".img") {
            continue;
        }
        let path = dir.join(&name);
        let (mut image, length) = match open(&path) {
            Ok(image) => image,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => panic!("cannot read fixture {name}: {error}"),
        };
        assert!(
            length > MAGIC_AT + 8,
            "{name} is {length} bytes, which is shorter than one superblock"
        );
        image
            .seek(SeekFrom::Start(MAGIC_AT))
            .unwrap_or_else(|error| panic!("cannot seek to fixture {name}'s btrfs magic: {error}"));
        let mut magic = [0; 8];
        image
            .read_exact(&mut magic)
            .unwrap_or_else(|error| panic!("cannot read fixture {name}'s btrfs magic: {error}"));
        assert_eq!(
            &magic, b"_BHRfS_M",
            "{name} has no btrfs superblock magic: it is not a btrfs filesystem, \
             so whichever oracle reads it first will fail about itself instead"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no fixture image was checked, so this compared nothing"
    );
    checked
}
