//! `fs.btrfs <target> <verb>`: an errand inside a Btrfs image or device,
//! without mounting it.
//!
//! The verbs are the shared set: `ls`, `read`, `write`, `mkdir`,
//! `get`/`info`, `set`, `resize`. Metadata is JSON (or `--text`); file
//! content is raw bytes. A verb the library cannot do yet still exists and
//! answers `not implemented` with exit status 3, so a script moved between
//! filesystems fails loudly instead of meaning something else.
//!
//! What this library can write is narrow, and `write` says so rather than
//! pretending otherwise: the bytes of an existing NODATACOW file, in
//! place, at the same length. Everything that needs a copy-on-write
//! transaction -- a new file, a directory, a different length, an ordinary
//! file -- is refused, by the library's own reason where it has one.

use std::ffi::OsString;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use crate::common::{CliError, Outcome, Tool};

pub const TOOL: Tool = Tool {
    name: "fs.btrfs",
    verb: "fs",
    section: 1,
    usage_exit: crate::common::output::EXIT_USAGE,
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

/// Why every copy-on-write change is refused: the one issue it waits on.
const COW_BLOCKED: &str =
    "blocked on rust-fs-btrfs#61: this library has no copy-on-write write path yet";

fn command() -> Cmd {
    Cmd::new("fs.btrfs")
        .about("List, read and inspect a Btrfs image or device without mounting it")
        .long_about(
            "Work inside a Btrfs image or device directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (get a file out, read the label, check whether \
             it is dirty), not a place to do real filesystem work: for that, mount it.\n\n\
             Metadata is JSON on stdout (--text for people); `read` writes the file's raw \
             bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being the \
             exit status: 1 failed, 2 wrong command line, 3 not implemented or refused.\n\n\
             Writing is limited to what this library can do safely: `write` overwrites \
             an existing NODATACOW file in place, at the same length. Everything that \
             needs a copy-on-write transaction answers with exit status 3.",
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
        .args(crate::common::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(
            Cmd::new("ls")
                .about(
                    "List a directory: name, type, size, mode, mtime (a symlink's target, \
                     and which entries are subvolumes)",
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img ls /home\n  \
                     fs.btrfs disk.img ls / | jq -r '.[] | select(.subvolume) | .name'\n  \
                     fs.btrfs disk.img ls --text /",
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
            Cmd::new("write")
                .about(
                    "Overwrite an existing NODATACOW file in place with the same number of \
                     bytes from stdin",
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img write /vm/disk.raw < disk.raw\n  \
                     fs.btrfs disk.img read /db/data | fix-up | fs.btrfs disk.img write /db/data\n\n\
                     Only a file marked NODATACOW (chattr +C) can be written, and only with \
                     exactly as many bytes as it already holds: this library cannot allocate. \
                     A new file, an ordinary copy-on-write file, a snapshotted or compressed \
                     extent, or a different length is refused with exit status 3 and the \
                     reason.",
                ),
        )
        .subcommand(
            Cmd::new("mkdir")
                .about("Create a directory (not implemented: needs copy-on-write writes)")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img mkdir /backup\n\n\
                     Answers `not implemented` (exit 3) until this library can make a \
                     copy-on-write change (rust-fs-btrfs#61).",
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
                .about("Change a property (label: not implemented)")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.btrfs disk.img set label BACKUP\n\n\
                     Answers `not implemented` (exit 3): this library has no label writer.",
                ),
        )
        .subcommand(
            Cmd::new("resize")
                .about("Grow or shrink the filesystem (not implemented)")
                .arg(Arg::new("size").value_name("SIZE").required(true))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Do it without asking"),
                )
                .after_help(
                    "Examples:\n  fs.btrfs disk.img resize 20G --force\n\n\
                     Answers `not implemented` (exit 3): this library has no resize.",
                ),
        )
        .after_help(
            "Examples:\n  fs.btrfs disk.img ls /\n  \
             fs.btrfs disk.img read /etc/fstab > fstab\n  \
             fs.btrfs disk.img get label --text\n  \
             fs.btrfs --offset 1048576 whole-disk.img info",
        )
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
    let (verb, sub) = matches.subcommand().expect("clap requires a verb");
    match verb {
        // The verbs this binary will carry, answering until they do.
        "ls" | "read" | "write" | "get" | "info" => Err(CliError::not_implemented(format!(
            "{verb}: not in this build of fs.btrfs yet"
        ))),
        "mkdir" => Err(CliError::not_implemented(format!("mkdir: {COW_BLOCKED}"))),
        "set" => set(sub),
        "resize" => Err(CliError::not_implemented(
            "resize: this library cannot resize a Btrfs filesystem",
        )),
        other => unreachable!("clap knows no verb {other}"),
    }
}

fn set(sub: &ArgMatches) -> Result<Outcome, CliError> {
    let key = sub.get_one::<String>("key").expect("clap requires the key");
    match key.as_str() {
        "label" => Err(CliError::not_implemented(
            "set label: this library has no writer for the Btrfs label",
        )),
        k if KEYS.contains(&k) || k.starts_with("btrfs.") => {
            Err(CliError::refused(format!("{k} is read-only")))
        }
        other => Err(CliError::usage(format!(
            "no key {other:?}; the settable key is label"
        ))),
    }
}
