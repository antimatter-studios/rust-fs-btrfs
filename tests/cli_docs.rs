//! The man pages and shell completions the binary writes for packaging
//! (`rust-fs-btrfs generate man|completions SHARE`): one page per name in
//! its section, each tool's page naming every one of its subcommands, and
//! a zsh, bash and fish completion per name.

mod cli_support;

use cli_support::*;
use std::path::PathBuf;

fn generated(tag: &str) -> PathBuf {
    let share = scratch_dir(&format!("docs-{tag}"));
    let share_arg = share.to_string_lossy().into_owned();
    ok(entry().args(["generate", "man", &share_arg]));
    ok(entry().args(["generate", "completions", &share_arg]));
    share
}

fn section(name: &str) -> u8 {
    if name.starts_with("mkfs.") || name.starts_with("fsck.") {
        8
    } else {
        1
    }
}

#[test]
fn every_name_has_a_man_page_in_its_section_naming_its_subcommands() {
    let share = generated("man");
    let mut names = dotted_names();
    names.push("rust-fs-btrfs".to_string());
    for name in &names {
        let s = section(name);
        let page = share.join(format!("man/man{s}/{name}.{s}"));
        let text =
            std::fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
        assert!(text.contains(".TH "), "{name}: not a man page");
        assert!(text.contains("Examples:"), "{name}: no example");
        // Every subcommand --help lists is named on the page.
        let help = stdout(&ok(tool(name).arg("--help")));
        let verbs: Vec<&str> = help
            .lines()
            .skip_while(|l| !l.starts_with("Commands:"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .filter_map(|l| l.split_whitespace().next())
            .filter(|v| *v != "help")
            .collect();
        for verb in verbs {
            assert!(text.contains(verb), "{name}'s page does not mention {verb}");
        }
    }
    // Section 8 holds the two system-administration tools (#260).
    for page in ["mkfs.btrfs.8", "fsck.btrfs.8"] {
        assert!(share.join("man/man8").join(page).is_file(), "no {page}");
    }
    // fs.btrfs's subcommands have pages of their own, which its list names.
    for verb in [
        "ls", "stat", "read", "write", "mkdir", "get", "info", "set", "resize",
    ] {
        assert!(
            share.join(format!("man/man1/fs.btrfs-{verb}.1")).is_file(),
            "no fs.btrfs-{verb}.1"
        );
    }
    // roff escapes every hyphen (`\-\-max\-depth`), so the page is read
    // with those escapes turned back into the hyphens a reader sees.
    let ls = std::fs::read_to_string(share.join("man/man1/fs.btrfs-ls.1"))
        .unwrap()
        .replace("\\-", "-");
    for option in ["recursive", "max-depth", "limit", "64", "100000"] {
        assert!(ls.contains(option), "ls man page omits {option}");
    }
    let mkdir = std::fs::read_to_string(share.join("man/man1/fs.btrfs-mkdir.1")).unwrap();
    for option in ["parents", "0755"] {
        assert!(mkdir.contains(option), "mkdir man page omits {option}");
    }
    let write_page = std::fs::read_to_string(share.join("man/man1/fs.btrfs-write.1")).unwrap();
    assert!(write_page.replace("\\-", "-").contains("--at"));
    assert!(write_page.contains("positioned write"));
    let help = stdout(&ok(tool("fs.btrfs").args(["disk.img", "write", "--help"])));
    assert!(help.contains("--at <BYTES>"), "{help}");
    assert!(help.contains("write --at 0 /f"), "{help}");
}

#[test]
fn every_name_has_a_zsh_bash_and_fish_completion() {
    let share = generated("completions");
    let mut names = dotted_names();
    names.push("rust-fs-btrfs".to_string());
    for name in &names {
        let zsh = std::fs::read_to_string(share.join(format!("zsh/site-functions/_{name}")))
            .unwrap_or_else(|e| panic!("zsh {name}: {e}"));
        assert!(
            zsh.starts_with(&format!("#compdef {name}")),
            "zsh {name}: {}",
            &zsh[..40.min(zsh.len())]
        );
        let bash =
            std::fs::read_to_string(share.join(format!("bash-completion/completions/{name}")))
                .unwrap_or_else(|e| panic!("bash {name}: {e}"));
        assert!(bash.contains("complete -F"), "bash {name}");
        let fish =
            std::fs::read_to_string(share.join(format!("fish/vendor_completions.d/{name}.fish")))
                .unwrap_or_else(|e| panic!("fish {name}: {e}"));
        assert!(fish.contains(&format!("complete -c {name}")), "fish {name}");
        if name == "fs.btrfs" {
            for completion in [&zsh, &bash, &fish] {
                for option in ["recursive", "max-depth", "limit", "parents"] {
                    assert!(
                        completion.contains(option),
                        "{name} completion omits {option}"
                    );
                }
            }
            assert!(zsh.contains("--at"), "zsh does not complete --at");
            assert!(bash.contains("--at"), "bash does not complete --at");
            assert!(fish.contains("-l at"), "fish does not complete --at");
        }
    }
}
