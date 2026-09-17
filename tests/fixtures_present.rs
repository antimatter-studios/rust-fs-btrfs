//! The fixtures CI builds must exist when CI runs the suite.
//!
//! # Why a separate test rather than a check inside each oracle
//!
//! Every oracle here already looks for its fixture and returns early
//! when it is missing, printing a line to stderr. On a developer's
//! machine that is right: the fixtures are gitignored, several need a
//! Linux VM to build, and a contributor without them should still be
//! able to run everything that does not need them.
//!
//! In CI it is exactly wrong. The workflow has eight separate steps
//! whose only purpose is to produce these images, and if one of them
//! silently produced nothing, the suite that depends on it would skip
//! and report success. The repository already knows this happens — the
//! workflow says so beside two of those steps:
//!
//! > Without this, tests/subvol_oracle.rs finds no fixture and skips,
//! > which reads exactly like passing. It did, on the run that added it.
//!
//! So the hazard is documented, has occurred, and was still unguarded.
//!
//! # Why one test rather than an assert in every oracle
//!
//! `am-fs-squashfs` and `am-fs-erofs` put the assertion inside the
//! helper every oracle calls, which is better where such a helper
//! exists: suites added later are covered without anyone maintaining a
//! list. Here there is no such helper — each test opens its own fixture
//! by name — so the equivalent would mean editing every suite and
//! remembering to edit the next one.
//!
//! One preflight test instead. It is less precise (it says the fixtures
//! are missing, not which suite wanted them) and it fails FIRST and
//! ALONE, with a sentence naming the cause, rather than as a screen of
//! unrelated test names whose common reason is a screen further down.
//! For the failure that actually happens — a build step breaking, so
//! nothing is there at all — that is the more useful shape.
//!
//! # Why not `CI`, which is what the sibling crates use
//!
//! Because this repository has two jobs and only one of them builds
//! fixtures. `test-${{ matrix.os }}` runs a plain `cargo test --release`
//! with none, and `kernel-gate` builds them across eight steps and runs
//! the oracles by name.
//!
//! Keying on `CI` therefore failed the wrong job — the first version of
//! this file did exactly that, and `test-ubuntu-latest` went red for a
//! condition that is correct there. The job that intends to have
//! fixtures has to say so itself, which is what `AM_FIXTURES_REQUIRED`
//! is for.
//!
//! That the plain test job runs these suites without fixtures at all is
//! its own matter: every oracle in it skips, and the job reports success
//! having run the unit tests only. `am-fs-xfs` has the same split and
//! tracks it as antimatter-studios/rust-fs-xfs#108 and #109.

use std::path::{Path, PathBuf};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// The job that builds fixtures must end up with them.
///
/// A FLOOR RATHER THAN A LIST, and rather than merely "not empty".
///
/// A list of expected filenames is a second place to remember a
/// fixture, and it goes stale the moment a step is added. But "at least
/// one image" is too weak in the other direction: it catches "no step
/// ran" and misses "one of several broke while the others worked",
/// which is the likelier failure. A single stray image satisfies it.
///
/// A floor catches both. It goes stale only downwards — add a fixture
/// step and the guard is merely weaker until someone raises the number,
/// which is a silent weakening rather than a false failure, and the
/// right direction for a check nobody wants to fight.
///
/// WHERE 30 COMES FROM, since a number nobody can re-derive is a number
/// nobody will dare change. It is what the workflow's own steps build,
/// counted as distinct `.img` names in `.vm-share`:
///
///   build-fixtures-native.sh      10 geometries + 2 populated + 1 rich = 13
///   build-subvol-fixtures.sh       1
///   build-xattr-fixtures.sh        1
///   build-nodatacow-fixtures.sh    2   (the second holds a snapshot, #63)
///   build-commit-fixtures.sh       1   (the SHA-256 DUP rerun rewrites the same name)
///   build-cow-fixtures.sh          3 + 3 with BTRFS_FIXTURE_SUFFIX=-sha256-dup
///   build-split-fixtures.sh        2 + 2 with BTRFS_SPLIT_SUFFIX=-vary
///                                    (its work and prev images are removed)
///   build-pool-fixtures.sh         2
///
/// THE COUNT ONLY MEANS SOMETHING AFTER THE LAST OF THOSE STEPS. This
/// constant was first 25, a correct total for the workflow of the day,
/// but the step running this test sat before the cow, split and pool
/// builds. It counted 16 on the run that added it, so it failed a job
/// in which every fixture step had succeeded, and read as "a build step
/// produced nothing" when none had. The step now runs after the last
/// build in `ci.yml`; moving a fixture step below it lowers this count
/// the same way.
///
/// An earlier draft took its number from one developer's `.vm-share`,
/// which holds images from other work as well. That is the wrong source:
/// this test runs in CI, so the number has to describe what CI produces.
///
/// If the matrix script SKIPs a geometry its `mkfs.btrfs` rejects, this
/// will fail rather than pass quietly. That is the intended direction:
/// a visible failure that names the count is worth more than a silent
/// pass, and lowering the number with a reason is a one-line change.
const FIXTURES_EXPECTED: usize = 30;

#[test]
fn the_fixture_job_has_its_fixtures() {
    if std::env::var_os("AM_FIXTURES_REQUIRED").is_none() {
        // A developer, or the plain test job, which builds none.
        eprintln!("AM_FIXTURES_REQUIRED is not set — fixtures are optional here");
        return;
    }

    let dir = share();
    let images: Vec<_> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("img"))
                .collect()
        })
        .unwrap_or_default();

    assert!(
        images.len() >= FIXTURES_EXPECTED,
        "the fixture job has {} .img files in {}, and expects at least {}. Eight steps \
         exist only to build them; if one broke, the oracles depending on it would skip \
         and this suite would report success having compared this driver against \
         nothing. If a fixture was deliberately removed, lower FIXTURES_EXPECTED and \
         say why.",
        images.len(),
        dir.display(),
        FIXTURES_EXPECTED
    );

    eprintln!(
        "{} fixtures present (floor {})",
        images.len(),
        FIXTURES_EXPECTED
    );
}
