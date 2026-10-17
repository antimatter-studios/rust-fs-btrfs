//! Fixture-free CLI contracts for extended-attribute inspection (#416).

mod cli_support;

use cli_support::*;

#[test]
fn xattr_help_man_page_and_completions_describe_the_verb() {
    let help = stdout(&ok(tool("fs.btrfs").args(["image.img", "xattr", "--help"])));
    for word in ["PATH", "NAME", "--output", "Examples:", "user.comment"] {
        assert!(help.contains(word), "missing {word}: {help}");
    }
    let share = scratch_dir("xattr-docs");
    ok(entry().args(["generate", "man"]).arg(&share));
    ok(entry().args(["generate", "completions"]).arg(&share));
    let man = std::fs::read_to_string(share.join("man/man1/fs.btrfs-xattr.1")).unwrap();
    assert!(man.contains("user.comment"), "{man}");
    for file in [
        "zsh/site-functions/_fs.btrfs",
        "bash-completion/completions/fs.btrfs",
        "fish/vendor_completions.d/fs.btrfs.fish",
    ] {
        let text = std::fs::read_to_string(share.join(file)).unwrap();
        assert!(text.contains("xattr"), "{file} omits xattr");
    }
}

#[test]
fn xattr_output_requires_a_name_before_the_target_is_opened() {
    let dir = scratch_dir("xattr-usage");
    let image = dir.join("absent.img");
    let dest = dir.join("value");
    std::fs::write(&dest, b"keep me").unwrap();
    let message = refused(
        tool("fs.btrfs")
            .arg(&image)
            .args(["xattr", "/f", "-o"])
            .arg(&dest),
        2,
    );
    assert!(message.contains("NAME"), "{message}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
    assert!(!image.exists());
}
