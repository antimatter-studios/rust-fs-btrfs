//! `rust-fs-btrfs`: the command-line tools for Btrfs, one multi-call
//! binary.
//!
//! Installed as `rust-fs-btrfs` and linked as each dotted name; see
//! `common` for the dispatch and the output contract every tool shares,
//! and `btrfs` for the tools themselves.

// The shared plumbing is a library in waiting (see its module docs): its
// API is whole, and a piece Btrfs does not call yet is not dead, it is the
// part another driver's tools will.
mod btrfs;
#[allow(dead_code)]
mod common;

use std::process::ExitCode;

static FAMILY: common::Family = common::Family {
    repo: "rust-fs-btrfs",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "Btrfs tools: work on a Btrfs image or device directly, without mounting it",
    install_hints: &[
        "`chore cli:install` from a checkout of this repository",
        "`brew install antimatter-studios/tap/rust-fs-btrfs`",
    ],
    tools: &[btrfs::fs::TOOL],
};

fn main() -> ExitCode {
    common::main(&FAMILY)
}
