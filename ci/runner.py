#!/usr/bin/env python3
"""Focused macOS libtest runner for one SANITIZED migration item.

--item ci/items/<id>.json  --out ci-out
The item must be a sanitize_items.py packet (id/source_head/github_branch/
focus/named_filters only); any other key is refused. Flow, verified order:

  1. unshallow this fetch if it is shallow (no silently incomplete diffs);
  2. fetch main and the item branch into stable local refs, verifying the
     branch tip equals item.source_head (no stale origin/main trusted);
  3. check out the exact verified head FIRST, so package mapping reads the
     tested tree, not the CI branch's tree;
  4. diff: verified merge-base(main, head)..head only - no partial fallback,
     because anything narrower could silently miss a multi-commit head;
  5. map changed paths to packages via tomllib on the checked-out manifests
     (root Cargo.toml/Cargo.lock => workspace-level; docs/tools => ignored);
  6. run tests: packet named filters (per package) when present, otherwise the
     package's libtests. Explicit argv only, no shell. A nonzero cargo exit is
     a failure even when test-result lines exist; the only broadening allowed
     is an explicit "no library targets" diagnostic (never on build failure);
  7. write status.json + logs; the workflow's report step (which holds
     GH_TOKEN) turns this into a commit status. No token needed here.

Honesty: no GPU/UI apps, no criterion benchmarks, no performance acceptance;
zero collected tests => neutral, never a fake pass. Cache: the workflow
caches ~/.cargo/registry and ~/.cargo/git with a host-only key; item sources
are always compiled fresh (no target-dir cache, no foreign head binaries,
independent of each head's lockfile). Requires Python 3.11+ (tomllib).
Exit codes: 0 = success/neutral, 1 = failure.
"""
import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

RESULT = re.compile(r"^test result: \w+\. (\d+) passed; (\d+) failed", re.M)
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
HASH_RE = re.compile(r"^perf/[A-Za-z0-9._-]+$")
ID_RE = re.compile(r"^[A-Za-z0-9_-]{1,16}$")
PKG_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_-]*$")
TEST_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_:]*$")
LIB_MISSING = "no library targets found"
LIMITS = "; limit: no GPU/UI/criterion benchmarks ran; not a performance accept."
ALLOWED_KEYS = {"id", "source_head", "github_branch", "focus", "named_filters"}


def run(cmd, cwd):
    """Explicit argv only; never a shell; stdout+stderr captured for logs."""
    return subprocess.run(cmd, shell=False, capture_output=True, text=True, cwd=cwd)


def status(out_dir, state, desc, **extra):
    # GitHub commit-status descriptions cap at 140 chars; truncate from the
    # START of the free-form part so the LIMITS suffix always survives.
    desc = (desc[:140 - len(LIMITS)].rstrip() + LIMITS)[:140]
    (out_dir / "status.json").write_text(json.dumps({"state": state, "desc": desc, **extra}, indent=1))
    print(f"status: {state}: {desc}")


def load_item(path):
    """Read and validate a sanitized packet; refuse anything else."""
    it = json.loads(Path(path).read_text(encoding="utf-8"))
    extra = set(it) - ALLOWED_KEYS
    if extra:
        raise ValueError(f"unsanitized item keys: {sorted(extra)}")
    if not SHA_RE.match(str(it.get("source_head", ""))):
        raise ValueError("bad/missing source_head")
    if not HASH_RE.match(str(it.get("github_branch", ""))):
        raise ValueError("bad/missing github_branch")
    if not ID_RE.match(str(it.get("id", ""))):
        raise ValueError("bad/missing id")
    filters, seen = [], set()
    for nf in it.get("named_filters") or []:
        if (not isinstance(nf, dict) or not PKG_RE.match(str(nf.get("package", "")))
                or not TEST_RE.match(str(nf.get("test", "")))):
            raise ValueError(f"bad named filter: {nf!r}")
        key = nf["package"], nf["test"]
        if key not in seen:
            seen.add(key)
            filters.append({"package": nf["package"], "test": nf["test"]})
    it["named_filters"] = filters
    it["source_head"] = str(it["source_head"])
    it["github_branch"] = str(it["github_branch"])
    it["id"] = str(it["id"])
    return it


def fetch_verify(top, remote_ref, local_ref, expect):
    """Fetch one remote ref into a stable local ref and verify its SHA."""
    if run(["git", "fetch", "--force", "--no-tags", "origin", f"{remote_ref}:{local_ref}"], top).returncode:
        return None
    sha = run(["git", "rev-parse", local_ref], top).stdout.strip()
    if not SHA_RE.match(sha) or (expect is not None and sha != expect):
        return None
    return sha


def full_history(top):
    """Unshallow when needed; without it the merge-base diff is unsafe."""
    if run(["git", "rev-parse", "--is-shallow-repository"], top).stdout.strip() != "true":
        return True
    return run(["git", "fetch", "--no-tags", "--unshallow", "origin"], top).returncode == 0


def diff_paths(top, head):
    """Changed paths + provenance mode; (None, reason) when main unavailable.

    Only the verified merge-base(main, head)..head diff is accepted. There is
    deliberately NO parent-diff fallback: a single-commit diff would silently
    miss earlier commits of a multi-commit head.
    """
    main = fetch_verify(top, "refs/heads/main", "refs/focused-mac/main", None)
    if not main:
        return None, "verified main unavailable; refusing any partial diff"
    mb = run(["git", "merge-base", main, head], top)
    if mb.returncode != 0:
        return None, "no merge-base with verified main; refusing any partial diff"
    r = run(["git", "diff", "--name-only", mb.stdout.strip(), head], top)
    if r.returncode != 0:
        return None, "git diff failed against verified merge-base"
    return sorted(set(p.strip() for p in r.stdout.splitlines() if p.strip())), "merge-base diff"


def manifest_pkg(man):
    """[package].name via tomllib; None for virtual or unreadable manifests."""
    try:
        data = tomllib.loads(man.read_text(encoding="utf-8", errors="replace"))
    except (tomllib.TOMLDecodeError, ValueError):
        return None
    return data.get("package", {}).get("name") or None


def closest_pkg(top, rel):
    """Package for a path: pkg name, 'ws' (root Cargo.toml/Cargo.lock), or ''."""
    rel = Path(rel)
    if rel.name in ("Cargo.toml", "Cargo.lock") and str(rel.parent) in (".", ""):
        return "ws"
    d = (top / rel).parent
    while top <= d and d != d.parent:
        man = d / "Cargo.toml"
        if man.exists() and manifest_pkg(man):
            return manifest_pkg(man)
        d = d.parent
    return ""


def map_packages(top, paths):
    """(packages, workspace-level?) for the changed path set."""
    pkgs, workspace = set(), False
    for rel in paths:
        p = closest_pkg(top, rel)
        if p == "ws":
            workspace = True
        elif p:
            pkgs.add(p)
    return pkgs, workspace


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--item", required=True)
    ap.add_argument("--out", default="ci-out")
    args = ap.parse_args()
    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    name, it = None, None
    try:
        it = load_item(args.item)
    except ValueError as e:  # unsanitized or malformed packet: honest refusal
        status(out_dir, "failure", f"item rejected: {e}")
        return 1
    name = it["id"]
    (out_dir / "item.json").write_text(json.dumps(it, indent=1))  # snapshot survives checkout
    top = Path(run(["git", "rev-parse", "--show-toplevel"], ".").stdout.strip() or ".")

    if not full_history(top):
        status(out_dir, "failure", f"item {name}: shallow checkout and unshallow failed")
        return 1
    h = fetch_verify(top, f"refs/heads/{it['github_branch']}", "refs/focused-mac/head", it["source_head"])
    if not h:
        status(out_dir, "failure", f"item {name}: branch tip does not match source_head (stale/moved)")
        return 1
    # Checkout the verified head BEFORE deriving packages from the tree.
    if run(["git", "checkout", "--detach", h], top).returncode:
        status(out_dir, "failure", f"item {name}: cannot check out source_head")
        return 1
    paths, mode = diff_paths(top, h)
    if paths is None:
        status(out_dir, "failure", f"item {name}: {mode}")
        return 1
    pkgs, workspace = map_packages(top, paths)
    if workspace:
        status(out_dir, "neutral", f"item {name}: workspace-level change ({mode}); no focused crate set")
        return 0
    if not pkgs:
        status(out_dir, "neutral", f"item {name}: no Rust packages in {mode} ({','.join(paths[:3])})")
        return 0

    passed, failed, crashed, missing = 0, 0, [], []
    for pkg in sorted(pkgs):
        filters = [nf["test"] for nf in it["named_filters"] if nf["package"] == pkg]
        if filters:  # focused packet filters exist -> run them, not the lib suite
            for filt in filters:
                r = run(["cargo", "test", "--locked", "-p", pkg, filt], top)
                (out_dir / f"log-{pkg}-{filt}.log").write_text(r.stdout + r.stderr)
                lines = RESULT.findall(r.stdout + r.stderr)
                for a, b in lines:
                    passed, failed = passed + int(a), failed + int(b)
                if r.returncode:  # nonzero exit fails even if result lines exist
                    crashed.append(f"{pkg}/{filt}")
                elif sum(int(a) + int(b) for a, b in lines) == 0:  # zero collection never green
                    missing.append(f"named {pkg}/{filt}")
        else:
            r = run(["cargo", "test", "--locked", "-p", pkg, "--lib"], top)
            lines = RESULT.findall(r.stdout + r.stderr)
            if r.returncode and LIB_MISSING in (r.stdout + r.stderr):
                # Broaden ONLY on an explicit no-lib-target diagnostic.
                r = run(["cargo", "test", "--locked", "-p", pkg], top)
                lines = RESULT.findall(r.stdout + r.stderr)
            (out_dir / f"log-{pkg}.log").write_text(r.stdout + r.stderr)
            for a, b in lines:
                passed, failed = passed + int(a), failed + int(b)
            if r.returncode:
                crashed.append(pkg)
            elif sum(int(a) + int(b) for a, b in lines) == 0:  # zero collection never green
                missing.append(pkg)

    (out_dir / "summary.json").write_text(json.dumps(
        {"item": name, "mode": mode, "paths": paths, "packages": sorted(pkgs),
         "filters": it["named_filters"], "passed": passed, "failed": failed,
         "crashed": crashed, "zero_test_selections": missing}, indent=1))
    if crashed or failed:
        status(out_dir, "failure", f"item {name}: {failed or 'build'} failure(s), {passed} passed "
                                   f"({mode}, {','.join(sorted(pkgs))})")
        return 1
    if missing or passed == 0:
        sel = ','.join(missing[:3])
        status(out_dir, "neutral", f"item {name}: zero-test selection(s) [{sel}]; {passed} passed "
                                   f"({mode}, {','.join(sorted(pkgs))})")
        return 0
    status(out_dir, "success", f"item {name}: {passed} test(s) passed ({mode}, {','.join(sorted(pkgs))})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
