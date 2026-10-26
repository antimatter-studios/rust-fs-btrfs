//! The shared export verb's help and argument contract, without an image.
mod cli_support;
use cli_support::*;

#[test]
fn export_help_and_generated_docs_describe_bounded_complete_file_copying() {
    let help = stdout(&ok(tool("fs.btrfs").args([
        "absent.img",
        "export",
        "--help",
    ])));
    for text in [
        "export /backup.tar backup.tar",
        "--replace",
        "SHA-256",
        "stdout",
    ] {
        assert!(help.contains(text), "export help omits {text}: {help}");
    }
    for args in [
        vec!["absent.img", "export", "/file"],
        vec!["absent.img", "export", "--recursive", "/", "out"],
    ] {
        refused(tool("fs.btrfs").args(args), 2);
    }
    let share = scratch_dir("export-docs");
    ok(entry().args(["generate", "man"]).arg(&share));
    ok(entry().args(["generate", "completions"]).arg(&share));
    let page = std::fs::read_to_string(share.join("man/man1/fs.btrfs-export.1"))
        .unwrap()
        .replace("\\-", "-");
    for text in ["--replace", "SHA-256", "export /backup.tar backup.tar"] {
        assert!(page.contains(text), "export man page omits {text}");
    }
    for file in [
        "zsh/site-functions/_fs.btrfs",
        "bash-completion/completions/fs.btrfs",
        "fish/vendor_completions.d/fs.btrfs.fish",
    ] {
        let text = std::fs::read_to_string(share.join(file)).unwrap();
        assert!(
            text.contains("export") && text.contains("replace"),
            "{file}"
        );
    }
}
