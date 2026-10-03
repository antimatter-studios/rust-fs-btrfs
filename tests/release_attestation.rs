//! The release workflow attests the crate it publishes.
//!
//! A version on crates.io says nothing about where it was built: anyone
//! holding a publish token could have uploaded it from their own machine.
//! `release.yml` therefore packages the crate, publishes it, checks that
//! the file it packaged is byte-for-byte the one crates.io serves, and
//! signs a build-provenance attestation over that file with the
//! workflow's own identity. The same `.crate` is attached to the GitHub
//! release for the tag, so anyone can check a download with
//!
//! ```text
//! gh attestation verify <crate> --repo <owner>/<repo> \
//!     --signer-workflow <owner>/<repo>/.github/workflows/release.yml
//! ```
//!
//! Nothing else notices if that step goes. The workflow runs only on a
//! version tag, and a release without an attestation publishes exactly
//! as green as one with it; the gap would surface the first time someone
//! tried to verify a download, long after the version was taken. This
//! file makes the loss loud on the pull request that causes it.
//!
//! It also keeps the privileges where they are needed. The attesting job
//! must be able to mint an OIDC token, write an attestation and attach a
//! release asset; no other job, and not the workflow as a whole, may
//! hold any of those grants.
//!
//! The command-line tools' tarballs are packaged, attested and attached
//! by rust-fs-core's reusable release-cli workflow, which this one calls
//! at a pinned commit; no copy of it is kept here ([`release_cli_gaps`]).
//!
//! The workflow is PARSED rather than scanned, so a step name, a comment
//! or a quoted string cannot satisfy a check meant for a real step.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::Path;

const WORKFLOW: &str = ".github/workflows/release.yml";

/// The action that signs the attestation, up to its `@`.
const ATTEST: &str = "actions/attest-build-provenance@";

/// The grants the attesting job needs, each at `write`: an OIDC token
/// to sign with, the attestation store, and the release to attach to.
const GRANTS: &[&str] = &["id-token", "attestations", "contents"];

fn load(yaml: &str) -> Yaml<'static> {
    let mut docs = Yaml::load_from_str(yaml).expect("the workflow parses as YAML");
    assert_eq!(docs.len(), 1, "one YAML document");
    docs.remove(0)
}

fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(WORKFLOW);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {WORKFLOW}: {e}"))
}

/// The lines of a `run:` script that are commands, not comments.
fn commands(step: &Yaml) -> Vec<String> {
    let Some(run) = step.as_mapping_get("run").and_then(Yaml::as_str) else {
        return Vec::new();
    };
    run.replace("\\\n", " ")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The commands in a step that start with `program`, so an `echo` or
/// a string mentioning it does not count.
fn invocations(step: &Yaml, program: &str) -> Vec<String> {
    commands(step)
        .into_iter()
        .filter(|c| c.starts_with(program))
        .collect()
}

fn runs(step: &Yaml, program: &str) -> bool {
    !invocations(step, program).is_empty()
}

/// Every grant in `permissions` that is `write`, by name. `write-all`
/// grants every one.
fn write_grants(permissions: Option<&Yaml>) -> Vec<String> {
    let Some(permissions) = permissions else {
        return Vec::new();
    };
    if permissions.as_str() == Some("write-all") {
        return GRANTS.iter().map(|g| (*g).to_owned()).collect();
    }
    let Some(map) = permissions.as_mapping() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(_, v)| v.as_str() == Some("write"))
        .filter_map(|(k, _)| k.as_str().map(str::to_owned))
        .filter(|k| GRANTS.contains(&k.as_str()))
        .collect()
}

fn is_full_sha(pin: &str) -> bool {
    pin.len() == 40
        && pin
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Everything wrong with how `yaml` attests what it publishes; empty
/// when nothing is.
fn attestation_gaps(yaml: &str) -> Vec<String> {
    let doc = load(yaml);
    let mut gaps = Vec::new();
    for grant in write_grants(doc.as_mapping_get("permissions")) {
        gaps.push(format!(
            "the workflow-level permissions grant {grant}: write to every job"
        ));
    }
    let jobs = doc
        .as_mapping_get("jobs")
        .and_then(Yaml::as_mapping)
        .expect("the workflow has jobs");
    let mut attesting = 0;
    for (name, job) in jobs {
        // The tools' tarballs are attested by rust-fs-core's release-cli
        // workflow, which this job calls; `release_cli_gaps` holds it.
        if calls_core_release_cli(job) {
            continue;
        }
        let name = name.as_str().unwrap_or("?");
        let steps: Vec<&Yaml> = job
            .as_mapping_get("steps")
            .and_then(Yaml::as_sequence)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        let granted = write_grants(job.as_mapping_get("permissions"));
        let attest_at = steps.iter().position(|s| {
            s.as_mapping_get("uses")
                .and_then(Yaml::as_str)
                .is_some_and(|u| u.starts_with(ATTEST))
        });
        let Some(at) = attest_at else {
            for grant in granted {
                gaps.push(format!(
                    "job {name} attests nothing but holds {grant}: write"
                ));
            }
            continue;
        };
        attesting += 1;
        let step = steps[at];
        let uses = step
            .as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .unwrap_or("");
        let pin = &uses[ATTEST.len()..];
        if !is_full_sha(pin) {
            gaps.push(format!(
                "job {name} uses {uses}, which a moved tag can redirect; pin a full commit SHA"
            ));
        }
        let subject = step
            .as_mapping_get("with")
            .and_then(|w| w.as_mapping_get("subject-path"))
            .and_then(Yaml::as_str)
            .unwrap_or("");
        if !subject.contains(".crate") {
            gaps.push(format!(
                "job {name} attests {subject:?}, not the packaged .crate"
            ));
        }
        if !steps[..at].iter().any(|s| runs(s, "cargo package")) {
            gaps.push(format!("job {name} attests before any `cargo package`"));
        }
        if !steps[..at].iter().any(|s| runs(s, "cargo publish")) {
            gaps.push(format!(
                "job {name} attests before `cargo publish`, so what it signs is not \
                 known to be what was published"
            ));
        }
        if !steps[at + 1..].iter().any(|s| {
            invocations(s, "gh release upload")
                .iter()
                .any(|c| c.contains(".crate"))
        }) {
            gaps.push(format!(
                "job {name} does not attach the attested .crate to the GitHub release"
            ));
        }
        for grant in GRANTS {
            if !granted.iter().any(|g| g == grant) {
                gaps.push(format!("job {name} attests without {grant}: write"));
            }
        }
    }
    if attesting == 0 {
        gaps.push(format!("no job in the workflow uses {ATTEST}<sha>"));
    }
    gaps
}

/// The steps of a job.
fn steps_of<'a>(job: &'a Yaml<'a>) -> Vec<&'a Yaml<'a>> {
    job.as_mapping_get("steps")
        .and_then(Yaml::as_sequence)
        .map(|s| s.iter().collect())
        .unwrap_or_default()
}

/// Where a job's attestation step is, and what it attests.
fn attest_step<'a>(job: &'a Yaml<'a>) -> Option<(usize, &'a str, String)> {
    let steps = steps_of(job);
    let at = steps.iter().position(|s| {
        s.as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .is_some_and(|u| u.starts_with(ATTEST))
    })?;
    let uses = steps[at]
        .as_mapping_get("uses")
        .and_then(Yaml::as_str)
        .unwrap_or("");
    let subject = steps[at]
        .as_mapping_get("with")
        .and_then(|w| w.as_mapping_get("subject-path"))
        .and_then(Yaml::as_str)
        .unwrap_or("")
        .to_string();
    Some((at, uses, subject))
}

/// Whether a job attests the command-line tools' release tarballs itself.
fn attests_tarballs(job: &Yaml) -> bool {
    attest_step(job).is_some_and(|(_, _, subject)| subject.contains(".tar.gz"))
}

/// The reusable workflow that packages, attests and attaches the tools'
/// tarballs for every repository in the family, up to its `@`.
const CORE_RELEASE_CLI: &str = "antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml@";

/// Whether a job is a call to rust-fs-core's release-cli workflow.
fn calls_core_release_cli(job: &Yaml) -> bool {
    job.as_mapping_get("uses")
        .and_then(Yaml::as_str)
        .is_some_and(|u| u.starts_with(CORE_RELEASE_CLI))
}

/// The names in a job's `needs`, whether written as one or as a list.
fn needs_of(job: &Yaml) -> Vec<String> {
    let Some(needs) = job.as_mapping_get("needs") else {
        return Vec::new();
    };
    if let Some(one) = needs.as_str() {
        return vec![one.to_owned()];
    }
    needs
        .as_sequence()
        .map(|s| {
            s.iter()
                .filter_map(Yaml::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Everything wrong with how `yaml` ships the command-line tools'
/// tarballs; empty when nothing is.
///
/// The tarballs are rust-fs-core's to package: one reusable workflow,
/// `release-cli.yml`, builds them natively per platform, checks them and,
/// from the one job that holds write grants, attests and attaches them.
/// This repository once carried its own copy of all of that -- a
/// `package-cli` matrix, an attest-and-attach job and
/// `scripts/package-cli.sh` -- and the family's copies drifted (#251).
///
/// So the workflow must call core's at a full commit SHA (a tag can be
/// moved), after the gate and after `publish` (one job creates the
/// release), granting the three writes the attach job needs, with
/// `core-ref` the core tag `pin` names and `toolchain` the one
/// `toolchain` names -- and no job here may attest tarballs of its own.
fn release_cli_gaps(yaml: &str, pin: &str, toolchain: &str) -> Vec<String> {
    let doc = load(yaml);
    let jobs = doc
        .as_mapping_get("jobs")
        .and_then(Yaml::as_mapping)
        .expect("the workflow has jobs");
    let mut gaps = Vec::new();
    let mut calls = 0;
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or("?");
        if attests_tarballs(job) {
            gaps.push(format!(
                "job {name} attests tarballs itself, a copy of what rust-fs-core's \
                 release-cli workflow does"
            ));
        }
        if !calls_core_release_cli(job) {
            continue;
        }
        calls += 1;
        let uses = job
            .as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .unwrap_or("");
        if !is_full_sha(&uses[CORE_RELEASE_CLI.len()..]) {
            gaps.push(format!(
                "job {name} calls {uses}, which a moved tag can redirect; pin a full commit SHA"
            ));
        }
        let needs = needs_of(job);
        for after in ["test", "publish"] {
            if !needs.iter().any(|n| n == after) {
                gaps.push(format!("job {name} does not wait for `{after}`"));
            }
        }
        let granted = write_grants(job.as_mapping_get("permissions"));
        for grant in GRANTS {
            if !granted.iter().any(|g| g == grant) {
                gaps.push(format!(
                    "job {name} calls release-cli without {grant}: write"
                ));
            }
        }
        let input = |key: &str| {
            job.as_mapping_get("with")
                .and_then(|w| w.as_mapping_get(key))
                .and_then(Yaml::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let core_ref = input("core-ref");
        if core_ref != format!("v{pin}") {
            gaps.push(format!(
                "job {name} passes core-ref {core_ref:?}, not v{pin}, the am-fs-core Cargo.toml pins"
            ));
        }
        let called_with = input("toolchain");
        if called_with != toolchain {
            gaps.push(format!(
                "job {name} passes toolchain {called_with:?}, not {toolchain}, the one \
                 rust-toolchain.toml pins"
            ));
        }
    }
    if calls != 1 {
        gaps.push(format!(
            "{calls} jobs call {CORE_RELEASE_CLI}<sha>; the tarballs need exactly one"
        ));
    }
    gaps
}

/// The `version` of the `am-fs-core` dependency in Cargo.toml.
fn am_fs_core_pin() -> String {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read Cargo.toml");
    manifest
        .lines()
        .filter(|l| l.trim_start().starts_with("am-fs-core"))
        .find_map(|l| {
            l.split_once("version = \"")
                .and_then(|(_, r)| r.split_once('"'))
        })
        .map(|(v, _)| v.to_owned())
        .expect("Cargo.toml pins am-fs-core by version")
}

/// The `channel` rust-toolchain.toml pins.
fn pinned_toolchain() -> String {
    let file =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("rust-toolchain.toml"))
            .expect("read rust-toolchain.toml");
    file.lines()
        .find_map(|l| l.trim().strip_prefix("channel = \""))
        .and_then(|r| r.split_once('"'))
        .map(|(v, _)| v.to_owned())
        .expect("rust-toolchain.toml pins a channel")
}

#[test]
fn the_release_workflow_ships_the_tarballs_through_cores_release_cli() {
    let gaps = release_cli_gaps(&workflow(), &am_fs_core_pin(), &pinned_toolchain());
    assert!(
        gaps.is_empty(),
        "{WORKFLOW} must ship the command-line tarballs by calling rust-fs-core's \
         release-cli workflow, pinned by SHA, and keep no copy of it: {gaps:#?}"
    );
}

/// The copies the reusable workflow replaced are gone, and nothing a run
/// reads calls them.
#[test]
fn no_local_copy_of_the_tarball_packaging_remains() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for copy in [
        "scripts/package-cli.sh",
        "tests/scripts/test-package-cli.sh",
    ] {
        assert!(
            !root.join(copy).exists(),
            "{copy} is a copy of rust-fs-core's packaging; run `scripts/core.sh package-cli`"
        );
    }
    let workflows = root.join(".github/workflows");
    for entry in std::fs::read_dir(&workflows).expect("read .github/workflows") {
        let path = entry.expect("a workflow").path();
        let text = std::fs::read_to_string(&path).expect("read a workflow");
        let calls: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter(|l| l.contains("scripts/package-cli.sh"))
            .collect();
        assert!(
            calls.is_empty(),
            "{} runs a local scripts/package-cli.sh: {calls:#?}",
            path.display()
        );
    }
}

/// The release-cli reader answers for the shapes it is meant to catch.
#[test]
fn the_release_cli_reader_discriminates() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let good = format!(
        "permissions:\n  contents: read\n\
         jobs:\n  test:\n    steps:\n      - run: cargo test\n\
         \x20 publish:\n    needs: test\n    steps:\n      - run: cargo publish\n\
         \x20 cli:\n    needs: [test, publish]\n\
         \x20   permissions:\n      contents: write\n      id-token: write\n      attestations: write\n\
         \x20   uses: {CORE_RELEASE_CLI}{sha} # v0.2.23\n\
         \x20   with:\n      core-ref: v0.2.23\n      toolchain: 1.95.0\n"
    );
    let gaps = |yaml: &str| release_cli_gaps(yaml, "0.2.23", "1.95.0");
    assert_eq!(gaps(&good), Vec::<String>::new(), "{good}");
    let expect = |yaml: String, want: &str| {
        let found = gaps(&yaml);
        assert!(
            found.iter().any(|g| g.contains(want)),
            "expected a gap mentioning {want:?}, got {found:#?} for\n{yaml}"
        );
    };
    expect(
        good.replace("rust-fs-core/.github", "rust-fs-other/.github"),
        "0 jobs call",
    );
    expect(good.replace(sha, "v0.2.23"), "pin a full commit SHA");
    expect(
        good.replace("[test, publish]", "test"),
        "does not wait for `publish`",
    );
    expect(
        good.replace("[test, publish]", "publish"),
        "does not wait for `test`",
    );
    for grant in GRANTS {
        expect(
            good.replace(&format!("      {grant}: write\n"), ""),
            &format!("without {grant}: write"),
        );
    }
    expect(
        good.replace("core-ref: v0.2.23", "core-ref: v0.2.18"),
        "passes core-ref",
    );
    expect(
        good.replace("toolchain: 1.95.0", "toolchain: stable"),
        "passes toolchain",
    );
    // A local attest-and-attach job beside the call is a copy.
    expect(
        good.replace(
            "  publish:\n",
            &format!(
                "  release-cli:\n    steps:\n      - uses: {ATTEST}{sha}\n        with:\n          subject-path: dist/*.tar.gz\n  publish:\n"
            ),
        ),
        "job release-cli attests tarballs itself",
    );
}

#[test]
fn the_release_workflow_attests_the_crate_it_publishes() {
    let gaps = attestation_gaps(&workflow());
    assert!(
        gaps.is_empty(),
        "{WORKFLOW} must attest the .crate it publishes, from the one job that \
         publishes it, with only that job privileged: {gaps:#?}"
    );
}

/// The reader answers for the inputs it is meant to catch, and not for
/// the ones it is not.
#[test]
fn the_reader_discriminates() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let good = format!(
        "permissions:\n  contents: read\n\
         jobs:\n  test:\n    steps:\n      - run: cargo test\n\
         \x20 publish:\n    permissions:\n      id-token: write\n      attestations: write\n      contents: write\n\
         \x20   steps:\n      - run: cargo package --no-verify\n      - run: cargo publish\n\
         \x20     - uses: {ATTEST}{sha} # v4.2.2\n        with:\n          subject-path: target/package/*.crate\n\
         \x20     - run: gh release upload \"$GITHUB_REF_NAME\" target/package/*.crate --clobber\n"
    );
    assert_eq!(attestation_gaps(&good), Vec::<String>::new(), "{good}");

    let expect = |yaml: String, want: &str| {
        let gaps = attestation_gaps(&yaml);
        assert!(
            gaps.iter().any(|g| g.contains(want)),
            "expected a gap mentioning {want:?}, got {gaps:#?} for\n{yaml}"
        );
    };
    // The step gone entirely, or only named in a comment.
    let no_step = good.replace(&format!("      - uses: {ATTEST}{sha} # v4.2.2\n        with:\n          subject-path: target/package/*.crate\n"), "      # uses: actions/attest-build-provenance\n");
    expect(no_step, "no job in the workflow uses");
    // Pinned to a tag.
    expect(good.replace(sha, "v4.2.2"), "pin a full commit SHA");
    // Each grant dropped in turn.
    for grant in GRANTS {
        expect(
            good.replace(&format!("      {grant}: write\n"), ""),
            &format!("attests without {grant}: write"),
        );
    }
    // A grant hoisted to the whole workflow.
    expect(
        good.replace(
            "permissions:\n  contents: read\n",
            "permissions:\n  id-token: write\n",
        ),
        "workflow-level permissions grant id-token",
    );
    expect(
        good.replace(
            "permissions:\n  contents: read\n",
            "permissions: write-all\n",
        ),
        "workflow-level permissions grant attestations",
    );
    // A job that attests nothing, holding a grant.
    expect(
        good.replace(
            "  test:\n    steps:",
            "  test:\n    permissions:\n      id-token: write\n    steps:",
        ),
        "job test attests nothing but holds id-token: write",
    );
    // Signing before publishing, or something other than the crate.
    expect(
        good.replace("      - run: cargo publish\n", "")
            .replace("--clobber\n", "--clobber\n      - run: cargo publish\n"),
        "attests before `cargo publish`",
    );
    expect(
        good.replace(
            "subject-path: target/package/*.crate",
            "subject-path: Cargo.toml",
        ),
        "not the packaged .crate",
    );
    expect(
        good.replace(
            "      - run: cargo package --no-verify\n",
            "      - run: echo '# cargo package'\n",
        ),
        "attests before any `cargo package`",
    );
    // Not attached to the release.
    expect(
        good.replace("gh release upload", "echo gh-release-upload"),
        "does not attach the attested .crate",
    );
}
