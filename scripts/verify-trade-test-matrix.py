#!/usr/bin/env python3
"""CI guard for the split maxplayer-trade jobs in .github/workflows/ci.yml.

1. MATRIX COVERAGE. The trade tests run as a matrix (`maxplayer-trade-test`) so the suites run in
   parallel. A split like that can drop coverage silently: a new tests/*.rs file nobody adds to
   the matrix compiles and never runs, and the check list stays green. For each suite (`default`,
   `lab`) this requires:
     * the lib unit tests (`--lib`), the bin unit tests (`--bins`) and the doc tests (`doc: true`)
       to be run by exactly one entry;
     * every test target cargo reports for the crate to be run either whole by exactly one entry,
       or by shards: one or more `only:` entries plus exactly one `skip:` entry whose skip list is
       exactly the union of the `only:` lists. A test is then in exactly one shard, and a test
       added to that binary later lands in the `skip:` (rest) shard by construction;
     * every tests/*.rs file to be a cargo test target.
   A name in an `only:`/`skip:` list that no longer exists is caught by the test job itself, which
   checks each name against `--list` before running its shard.

2. CHANGE-FILTER COVERAGE. `trade-changes` skips the trade jobs unless a path in TRADE_PATHS
   changed. Every local (path) package in maxplayer-trade's dependency graph, and the workspace
   manifest each one is resolved against, must be covered by TRADE_PATHS — otherwise a new
   `path =` dependency would let a PR that breaks trading skip the trade tests.

Usage (repo root): scripts/verify-trade-test-matrix.py — needs python3, yq (v4) and cargo.
"""
import glob
import json
import os
import subprocess
import sys

WORKFLOW = ".github/workflows/ci.yml"
MANIFEST = "crates/maxplayer-trade/Cargo.toml"
TEST_JOB = "maxplayer-trade-test"
CHANGES_JOB = "trade-changes"
SUITES = ("default", "lab")
ROOT = os.path.realpath(".")

errors = []


def err(msg):
    errors.append(msg)


def run(*argv):
    return subprocess.run(argv, check=True, capture_output=True, text=True).stdout


def yq(expr):
    return json.loads(run("yq", "-o=json", expr, WORKFLOW))


def rel(path):
    return os.path.relpath(os.path.realpath(path), ROOT)


def label(e):
    return f"matrix entry ({e.get('suite')}, {e.get('group')})"


def parse_targets(entry):
    """Split an entry's `targets` cargo args into ({lib,bins} flags, [test names])."""
    toks = str(entry.get("targets") or "").split()
    flags, tests, i = set(), [], 0
    while i < len(toks):
        t = toks[i]
        if t in ("--lib", "--bins"):
            flags.add(t[2:])
        elif t == "--test" and i + 1 < len(toks):
            tests.append(toks[i + 1])
            i += 1
        else:
            err(f"{label(entry)}: unsupported token {t!r} in targets (only --lib, --bins, --test <name>)")
        i += 1
    return flags, tests


def check_matrix():
    matrix = yq(f'.jobs["{TEST_JOB}"].strategy.matrix.include')
    if not isinstance(matrix, list) or not matrix:
        sys.exit(f"verify-trade-test-matrix: no matrix.include for job {TEST_JOB} in {WORKFLOW} "
                 "— INSTRUMENT FAILURE, not a pass")
    meta = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1",
                          "--manifest-path", MANIFEST))
    (pkg,) = [p for p in meta["packages"] if p["name"] == "maxplayer-trade"]
    targets = pkg["targets"]
    lib = [t for t in targets if "lib" in t["kind"]]
    have = {
        "lib": bool(lib),
        "doc": any(t.get("doctest") for t in lib),
        "bins": any("bin" in t["kind"] for t in targets),
    }
    tests = {t["name"]: os.path.realpath(t["src_path"]) for t in targets if "test" in t["kind"]}
    if not tests:
        sys.exit("verify-trade-test-matrix: cargo reports ZERO test targets — INSTRUMENT FAILURE")

    for path in sorted(glob.glob("crates/maxplayer-trade/tests/*.rs")):
        if os.path.realpath(path) not in tests.values():
            err(f"{path} is not a cargo test target of maxplayer-trade — it would never run")

    suites = {e.get("suite") for e in matrix}
    if suites != set(SUITES):
        err(f"matrix suites are {sorted(map(str, suites))}, expected {list(SUITES)}")

    for suite in SUITES:
        whole = {"lib": 0, "bins": 0, "doc": 0}
        whole_tests = {}
        shards = {}  # test -> {"only": [(entry, names)], "skip": [(entry, names)]}
        for e in (e for e in matrix if e.get("suite") == suite):
            flags, ts = parse_targets(e)
            only, skip = str(e.get("only") or "").split(), str(e.get("skip") or "").split()
            for t in ts:
                if t not in tests:
                    err(f"{label(e)}: --test {t} is not a test target of maxplayer-trade")
            if e.get("doc"):
                whole["doc"] += 1
            if only or skip:
                if only and skip:
                    err(f"{label(e)}: set only: or skip:, not both")
                if flags or len(ts) != 1 or e.get("doc"):
                    err(f"{label(e)}: a sharded entry must select exactly one --test target and nothing else")
                    continue
                names = only or skip
                if len(set(names)) != len(names):
                    err(f"{label(e)}: duplicate test name in its only:/skip: list")
                s = shards.setdefault(ts[0], {"only": [], "skip": []})
                s["only" if only else "skip"].append((e, set(names)))
                continue
            for f in flags:
                whole[f] += 1
            for t in ts:
                whole_tests[t] = whole_tests.get(t, 0) + 1

        for kind in ("lib", "bins", "doc"):
            if have[kind] and whole[kind] != 1:
                err(f"suite {suite}: the {kind} tests are run by {whole[kind]} entries, expected exactly 1")

        for t, s in shards.items():
            if len(s["skip"]) != 1 or not s["only"]:
                err(f"suite {suite}: --test {t} needs one or more only: shards and exactly one skip: "
                    f"(rest) shard, has {len(s['only'])} and {len(s['skip'])}")
                continue
            union = set()
            for e, names in s["only"]:
                if union & names:
                    err(f"{label(e)}: {sorted(union & names)} already in another shard of {t}")
                union |= names
            skipped = s["skip"][0][1]
            if skipped != union:
                err(f"suite {suite}: --test {t} rest shard must skip exactly the only: shards' union; "
                    f"differs on {sorted(skipped ^ union)}")

        for t in sorted(tests):
            n = whole_tests.get(t, 0) + (1 if t in shards else 0)
            if n != 1:
                err(f"suite {suite}: test target {t} is run by {n} matrix entries, expected exactly 1")
    return len(tests), len(matrix)


def covered(path, patterns):
    """Same semantics as the trade-changes job: `dir/` is a prefix, anything else is exact."""
    return any(path.startswith(p) if p.endswith("/") else path == p for p in patterns)


def check_change_filter():
    patterns = str(yq(f'.jobs["{CHANGES_JOB}"].env.TRADE_PATHS') or "").split()
    if not patterns:
        sys.exit(f"verify-trade-test-matrix: no TRADE_PATHS in job {CHANGES_JOB} — INSTRUMENT FAILURE")
    meta = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1",
                          "--manifest-path", MANIFEST))
    local = [p for p in meta["packages"] if p["source"] is None]
    if not any(p["name"] == "maxplayer-trade" for p in local):
        sys.exit("verify-trade-test-matrix: maxplayer-trade missing from its own metadata — INSTRUMENT FAILURE")
    for p in local:
        manifest = p["manifest_path"]
        crate_dir = rel(os.path.dirname(manifest)) + "/"
        if not covered(crate_dir, patterns):
            err(f"path package {p['name']} ({crate_dir}) is not covered by {CHANGES_JOB} TRADE_PATHS")
        ws = run("cargo", "locate-project", "--workspace", "--message-format", "plain",
                 "--manifest-path", manifest).strip()
        if os.path.realpath(ws) != os.path.realpath(manifest) and not covered(rel(ws), patterns):
            err(f"{rel(ws)} (workspace root of path package {p['name']}) is not covered by "
                f"{CHANGES_JOB} TRADE_PATHS")
    return len(local)


def main():
    n_tests, n_entries = check_matrix()
    n_local = check_change_filter()
    if errors:
        print("verify-trade-test-matrix: FAILED", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        sys.exit(1)
    print(f"verify-trade-test-matrix: OK — lib/bins/doc + {n_tests} test targets each run exactly once "
          f"in each of {', '.join(SUITES)} ({n_entries} matrix entries); {n_local} local packages "
          f"and their workspace manifests covered by TRADE_PATHS")


if __name__ == "__main__":
    main()
