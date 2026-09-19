//! Shared setup for the fuzz targets.
//!
//! The helpers live in `fuzz/shared/helpers.rs`, which
//! `tests/fuzz_decoders.rs` includes too -- see that file for why they
//! are shared textually rather than as a dependency.

use std::sync::OnceLock;

/// Where the shared helpers look for the corpus, from this crate.
fn corpus_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

include!("../shared/helpers.rs");
