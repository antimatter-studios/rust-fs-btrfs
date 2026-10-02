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
//! The command-line tools' tarballs are held to the same promise by a job
//! of their own: signed by this workflow, from the artifacts the package
//! legs built, before they are attached beside the crate
//! ([`tarball_gaps`]).
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
        // The tools' tarballs are attested by a job of their own, held to
        // its own rules by `tarball_gaps`.
        if attests_tarballs(job) {
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

/// Whether a job attests the command-line tools' release tarballs.
fn attests_tarballs(job: &Yaml) -> bool {
    attest_step(job).is_some_and(|(_, _, subject)| subject.contains(".tar.gz"))
}

/// Everything wrong with how `yaml` attests the command-line tools'
/// tarballs; empty when nothing is.
///
/// The same promise as the crate's, for the other thing a release
/// publishes: each tarball is signed by this workflow before it is
/// attached, so `gh attestation verify` can say it was built here from a
/// commit here and not uploaded from somebody's machine. The job must
/// take the tarballs the package legs built (downloaded artifacts, not
/// something it made itself), sign them with a pinned action, attach
/// them to the release, and hold the three grants that takes.
fn tarball_gaps(yaml: &str) -> Vec<String> {
    let doc = load(yaml);
    let jobs = doc
        .as_mapping_get("jobs")
        .and_then(Yaml::as_mapping)
        .expect("the workflow has jobs");
    let mut gaps = Vec::new();
    let mut attesting = 0;
    for (name, job) in jobs {
        if !attests_tarballs(job) {
            continue;
        }
        attesting += 1;
        let name = name.as_str().unwrap_or("?");
        let steps = steps_of(job);
        let (at, uses, _) = attest_step(job).expect("a tarball job attests");
        if !is_full_sha(&uses[ATTEST.len()..]) {
            gaps.push(format!(
                "job {name} uses {uses}, which a moved tag can redirect; pin a full commit SHA"
            ));
        }
        if !steps[..at].iter().any(|s| {
            s.as_mapping_get("uses")
                .and_then(Yaml::as_str)
                .is_some_and(|u| u.starts_with("actions/download-artifact@"))
        }) {
            gaps.push(format!(
                "job {name} attests tarballs it did not take from the package legs"
            ));
        }
        if !steps[at + 1..].iter().any(|s| {
            runs(s, "gh release upload") && commands(s).iter().any(|c| c.contains(".tar.gz"))
        }) {
            gaps.push(format!(
                "job {name} does not attach the attested tarballs to the GitHub release"
            ));
        }
        let granted = write_grants(job.as_mapping_get("permissions"));
        for grant in GRANTS {
            if !granted.iter().any(|g| g == grant) {
                gaps.push(format!("job {name} attests without {grant}: write"));
            }
        }
    }
    if attesting == 0 {
        gaps.push(format!(
            "no job in the workflow uses {ATTEST}<sha> on the tools' .tar.gz tarballs"
        ));
    }
    gaps
}

#[test]
fn the_release_workflow_attests_the_tarballs_it_attaches() {
    let gaps = tarball_gaps(&workflow());
    assert!(
        gaps.is_empty(),
        "{WORKFLOW} must attest the command-line tarballs it attaches to the release: \
         {gaps:#?}"
    );
}

/// The tarball reader answers for the shapes it is meant to catch.
#[test]
fn the_tarball_reader_discriminates() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let good = format!(
        "permissions:\n  contents: read\n\
         jobs:\n  release-cli:\n    permissions:\n      id-token: write\n      attestations: write\n      contents: write\n\
         \x20   steps:\n      - uses: actions/download-artifact@{sha}\n        with:\n          path: dist\n\
         \x20     - uses: {ATTEST}{sha} # v4.2.2\n        with:\n          subject-path: dist/*.tar.gz\n\
         \x20     - run: |\n          assets=(dist/*.tar.gz)\n          gh release upload \"$GITHUB_REF_NAME\" \"${{assets[@]}}\" --clobber\n"
    );
    assert_eq!(tarball_gaps(&good), Vec::<String>::new(), "{good}");
    let expect = |yaml: String, want: &str| {
        let gaps = tarball_gaps(&yaml);
        assert!(
            gaps.iter().any(|g| g.contains(want)),
            "expected a gap mentioning {want:?}, got {gaps:#?} for\n{yaml}"
        );
    };
    expect(
        good.replace("subject-path: dist/*.tar.gz", "subject-path: dist/*.zip"),
        "no job in the workflow uses",
    );
    expect(good.replace(sha, "v4"), "pin a full commit SHA");
    expect(
        good.replace("actions/download-artifact@", "actions/checkout@"),
        "did not take from the package legs",
    );
    expect(
        good.replace("gh release upload", "echo gh-release-upload"),
        "does not attach the attested tarballs",
    );
    for grant in GRANTS {
        expect(
            good.replace(&format!("      {grant}: write\n"), ""),
            &format!("attests without {grant}: write"),
        );
    }
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
