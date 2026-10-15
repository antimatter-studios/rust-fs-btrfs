#!/usr/bin/env bash
# Exercise the workflow's actual planner and aggregate, without a VM or fixtures.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
python3 - "$repo/.github/workflows/ci.yml" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import yaml

doc = yaml.safe_load(Path(sys.argv[1]).read_text())
events = doc.get("on", doc.get(True))
assert {"pull_request", "merge_group", "workflow_dispatch"} <= set(events)
assert not (events.get("pull_request") or {}).get("branches"), "all stack bases need PR checks"
jobs = doc["jobs"]
planner = next(s for s in jobs["changes"]["steps"] if s.get("id") == "c")["run"]
aggregate = next(s for s in jobs["ci-ok"]["steps"] if "NEEDS" in s.get("env", {}))
full = set(json.loads(aggregate["env"]["FULL_JOBS"]))
assert full <= set(jobs), "the aggregate must not exempt nonexistent jobs"
assert set(jobs["ci-ok"]["needs"]) == set(jobs) - {"ci-ok"}

# Derive every intentionally skipped job from the dependency graph. A new
# full job must be declared, and a quick job must never receive an exemption.
expected = {n for n, j in jobs.items()
            if j.get("if") == "needs.changes.outputs.code == 'true'"}
while True:
    expanded = set(expected)
    for name, job in jobs.items():
        needs = job.get("needs", [])
        needs = [needs] if isinstance(needs, str) else needs
        if name != "ci-ok" and set(needs) & expected:
            expanded.add(name)
    if expanded == expected:
        break
    expected = expanded
assert full == expected, (full, expected)

for event, ref, decision in [
    ("pull_request", "refs/pull/1/merge", "false"),
    ("merge_group", "refs/heads/gh-readonly-queue/main/pr-1", "true"),
    ("workflow_dispatch", "refs/heads/ci/oracle-red", "true"),
    ("push", "refs/heads/main", "false"),
    ("push", "refs/tags/v1.0.0", "true"),
    ("unexpected", "", "true"),
]:
    with tempfile.TemporaryDirectory() as tmp:
        output = Path(tmp) / "output"
        env = dict(os.environ, EVENT=event, REF=ref, GITHUB_OUTPUT=str(output))
        result = subprocess.run(["bash", "-c", planner], env=env, capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        assert output.read_text().splitlines() == ["code=" + decision], (event, result.stdout)

def verdict(code, results):
    env = dict(os.environ, CODE=code, NEEDS=json.dumps(results), FULL_JOBS=json.dumps(sorted(full)))
    return subprocess.run(["bash", "-c", aggregate["run"]], env=env,
                          capture_output=True, text=True)

success = {n: {"result": "success"} for n in jobs["ci-ok"]["needs"]}
quick = {n: {"result": "skipped" if n in full else "success"} for n in success}
for code, results in [("true", success), ("false", quick)]:
    result = verdict(code, results)
    assert result.returncode == 0, result.stdout + result.stderr

for code in ["", "invalid"]:
    assert verdict(code, success).returncode != 0, "a missing decision must fail closed"
for name in success:
    for state in ["failure", "cancelled", "skipped"]:
        bad = dict(success)
        bad[name] = {"result": state}
        assert verdict("true", bad).returncode != 0, ("full", name, state)
        if name not in full or state != "skipped":
            bad = dict(quick)
            bad[name] = {"result": state}
            assert verdict("false", bad).returncode != 0, ("quick", name, state)
print("ci-depth: event planning, stack triggers, and aggregate failure cases passed")
PY
