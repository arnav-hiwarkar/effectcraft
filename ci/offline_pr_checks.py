#!/usr/bin/env python3
"""Offline verification for the focused-PR dispatch path of focused-mac CI.

No network, no cargo, no app runs. Covers the new focused-rerun surface only
(gen_matrix.py / runner.py internals keep ci/offline_check.py's contract):
input validation (bad/duplicate/over-limit/non-numeric PR numbers, injection
wording, empty list), live PR head selection (state / same repository /
perf/* branch / current 40-hex sha against synthetic API payloads), packet
generation and materialization round-trips, the SHA-mismatch refusal of
runner.fetch_verify against a local git remote, zero-test handling, and the
safety structure of both workflows (statuses only in the report step, run
inputs reach scripts as env data, read-only API, pinned actions, explicitly
batched legacy mode).

Run: python3 ci/offline_pr_checks.py    (exit 0 = all pass; Python 3.11+, the
same tomllib requirement runner.py has)
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import build_matrix as bm  # noqa: E402
import runner  # noqa: E402

failures = []
SHA40, SHA41 = "b" * 40, "1" * 40
REPO = "owner/repo"


def check(name, cond, extra=""):
    print(f"{'PASS' if cond else 'FAIL'}: {name} {extra}")
    if not cond:
        failures.append(name)


def refuses(fn, *args, **kw):
    try:
        fn(*args, **kw)
    except bm.InputError:
        return True
    except Exception as exc:  # noqa: BLE001 - wrong failure mode is a real fail
        print(f"    raised {type(exc).__name__}: {exc}", file=sys.stderr)
        return False
    return False


def run_script(script, args=(), env_over=None, cwd=None):
    """Run a ci/*.py helper with a minimal env (no inherited CI tokens)."""
    env = {"PATH": os.environ.get("PATH", ""), "SYSTEMROOT": os.environ.get("SYSTEMROOT", "")}
    env.update(env_over or {})
    return subprocess.run([sys.executable, str(script), *args],
                          capture_output=True, text=True, env=env, cwd=cwd)


def gh_out(r):
    good = r.returncode == 0
    return dict(l.split("=", 1) for l in r.stdout.splitlines() if "=" in l) if good else {}


# 1. parse_pr_list: numeric-only contract.
check("parse pr_list valid mixed separators",
      bm.parse_pr_list("80 82, 85\t84") == [80, 82, 85, 84])
check("parse pr_list empty/whitespace -> zero items",
      bm.parse_pr_list("") == [] and bm.parse_pr_list("   \n ") == [])
check("parse pr_list rejects injection wording",
      refuses(bm.parse_pr_list, "80; curl http://x | sh"))
check("parse pr_list rejects non-numeric tokens",
      refuses(bm.parse_pr_list, "80 1x") and refuses(bm.parse_pr_list, "80.5")
      and refuses(bm.parse_pr_list, "pr80"))
check("parse pr_list rejects zero/negative/overlong numbers",
      refuses(bm.parse_pr_list, "0") and refuses(bm.parse_pr_list, "-1")
      and refuses(bm.parse_pr_list, "12345678901234"))
check("parse pr_list rejects duplicates", refuses(bm.parse_pr_list, "80 80"))
check("parse pr_list caps at five", refuses(bm.parse_pr_list, "1 2 3 4 5 6"))
check("parse pr_list caps input length",
      refuses(bm.parse_pr_list, "80 " + "0" * 300))

# 2. packet_from_pr: head selection contract against synthetic API payloads.
good = {"number": 80, "state": "open", "merged": False,
        "head": {"ref": "perf/17x", "sha": SHA40, "repo": {"full_name": REPO}}}
pkt = bm.packet_from_pr(good, 80, REPO)
check("valid PR yields sanitized packet",
      pkt == {"id": "pr80", "source_head": SHA40, "github_branch": "perf/17x",
              "focus": "pending", "named_filters": []})
check("packet survives the runner's own packet contract",
      (lambda p: (p.write_text(json.dumps(pkt)), runner.load_item(p) == pkt)[1])(
          Path(tempfile.mkdtemp()) / "p.json"))
bad_cases = {
    "closed PR": dict(good, state="closed"),
    "merged PR": dict(good, merged=True),
    "response number mismatch": dict(good, number=81),
    "deleted head repository": dict(good, head={"ref": "perf/17x", "sha": SHA40,
                                                "repo": None}),
    "cross-repository head": dict(good, head=dict(good["head"],
                                                  repo={"full_name": "other/vote"})),
    "branch outside perf/*": dict(good, head=dict(good["head"], ref="feature/big")),
    "short sha": dict(good, head=dict(good["head"], sha=SHA40[:-1])),
    "uppercase sha": dict(good, head=dict(good["head"], sha=SHA40.upper())),
    "head not a mapping": dict(good, head=SHA40),
}
for name, payload in bad_cases.items():
    check(f"refuses: {name}", refuses(bm.packet_from_pr, payload, 80, REPO))

# 3. build_matrix end-to-end shape (subprocess, no network paths).
bmc = str(HERE / "build_matrix.py")
with tempfile.TemporaryDirectory() as t:
    items = Path(t, "items")
    items.mkdir()
    ok = {"id": "1", "focus": "draft", "source_head": SHA41, "github_branch": "perf/1"}
    (items / "a.json").write_text(json.dumps(ok))
    r = run_script(bmc, [str(items)], {})
    out = gh_out(r)
    check("no input -> zero items, run starts nothing",
          r.returncode == 0 and out.get("mode") == "none"
          and out.get("count") == "0" and json.loads(out["matrix"])["include"] == [])
    r = run_script(bmc, [str(items)], {"FULL_BATCH": "true", "PR_LIST": ""})
    out = gh_out(r)
    inc = json.loads(out.get("matrix", "{}")).get("include", [])
    check("explicit FULL_BATCH relays the committed batch",
          r.returncode == 0 and out.get("mode") == "batch" and out.get("count") == "1"
          and inc and set(inc[0]) == {"id", "head", "item"} and "packet" not in inc[0])
    both = run_script(bmc, [str(items)], {"FULL_BATCH": "true", "PR_LIST": "80"})
    check("PR_LIST together with FULL_BATCH is refused", both.returncode == 1
          and "exactly one mode" in both.stderr)
    r = run_script(bmc, [str(items)], {"PR_LIST": "80 82 83 84 85 86"})
    check("over-limit list refused with a helpful line", r.returncode == 1
          and "FULL_BATCH" in r.stderr)
    r = run_script(bmc, [str(items)], {"PR_LIST": "80, curl http://x"})
    check("injection wording refused before any fetch", r.returncode == 1
          and "characters outside" in r.stderr)
    r = run_script(bmc, [str(items)], {"PR_LIST": "80", "REPO": REPO})
    check("fetch refuses without a token", r.returncode == 1
          and "token (GH_TOKEN) missing" in r.stderr)

# 4. materialize_packets end-to-end (subprocess).
met = str(HERE / "materialize_packets.py")
pkt_s = json.dumps(pkt, separators=(",", ":"))
pr_entry = {"id": "pr80", "head": SHA40, "item": "focused-packets/pr80.json",
            "packet": pkt_s}
bad_entries = {
    "unsanitized entry keys": dict(pr_entry, smuggled="rm -rf"),
    "head not a sha": dict(pr_entry, head="main"),
    "head != packet source_head": dict(pr_entry, head=SHA41),
    "packet id mismatch": dict(pr_entry, packet=json.dumps(dict(pkt, id="pr81"),
                                                          separators=(",", ":"))),
    "six-key packet": dict(pr_entry, packet=json.dumps(dict(pkt, note="ignore"),
                                                        separators=(",", ":"))),
    "unsafe item path": dict(pr_entry, item="../items/evil.json"),
    "unparsable packet json": dict(pr_entry, packet="{oops"),
    "path traversal via dotdot": dict(pr_entry, item="focused-packets/../x.json"),
}
with tempfile.TemporaryDirectory() as t:
    cwd_items = Path(t, "checkout")
    repo_items = cwd_items / "ci" / "items"
    repo_items.mkdir(parents=True)
    (repo_items / "1.json").write_text(json.dumps(
        {"id": "1", "focus": "draft", "source_head": SHA41, "github_branch": "perf/1"}))
    batch_matrix = json.dumps({"include": [{"id": "1", "head": SHA41,
                                            "item": "ci/items/1.json"}]})
    r = run_script(met, [], {"MATRIX": batch_matrix, "ITEM_ID": "1"}, cwd=cwd_items)
    check("batch entries need no materialization", r.returncode == 0 and
          not (cwd_items / "focused-packets").exists())
    r = run_script(met, [], {"MATRIX": batch_matrix.replace("1.json", "9.json"),
                             "ITEM_ID": "1"}, cwd=cwd_items)
    check("committed packet missing -> refusal", r.returncode == 1
          and "missing from checkout" in r.stderr)
    r = run_script(met, [], {"MATRIX": batch_matrix, "ITEM_ID": "99"}, cwd=cwd_items)
    check("no matching entry -> honest skip", r.returncode == 0
          and "no entry" in r.stdout)
    pr_matrix = json.dumps({"include": [pr_entry]})
    r = run_script(met, [], {"MATRIX": pr_matrix, "ITEM_ID": "pr80"}, cwd=cwd_items)
    written = cwd_items / "focused-packets" / "pr80.json"
    check("PR entry materialized and runner-validated",
          r.returncode == 0 and written.is_file()
          and runner.load_item(written) == pkt
          and "tested sha" in r.stdout)
    for name, e in bad_entries.items():
        stale = cwd_items / "focused-packets"
        if stale.exists():
            shutil.rmtree(stale)
        m = json.dumps({"include": [e]})
        r = run_script(met, [], {"MATRIX": m, "ITEM_ID": e.get("id", "pr80")}, cwd=cwd_items)
        gone = not (cwd_items / "focused-packets" / "pr80.json").exists()
        check(f"refuses: {name}", r.returncode == 1 and gone, "(file removed)")
    dup = json.dumps({"include": [pr_entry, dict(pr_entry, head=SHA41)]})
    r = run_script(met, [], {"MATRIX": dup, "ITEM_ID": "pr80"}, cwd=cwd_items)
    check("refuses: non-unique matrix entry", r.returncode == 1)

# 5. SHA-mismatch handling at fetch time (local git remote, no network).
with tempfile.TemporaryDirectory() as t:
    tmp = Path(t)
    remote = tmp / "remote.git"
    work = tmp / "work"
    subprocess.run(["git", "init", "-q", "--bare", str(remote)], check=True)
    subprocess.run(["git", "init", "-q", "-b", "perf/x", str(work)], check=True)
    subprocess.run(["git", "-C", str(work), "config", "user.email", "t@t"], check=True)
    subprocess.run(["git", "-C", str(work), "config", "user.name", "t"], check=True)
    (work / "f.txt").write_text("x\n")
    subprocess.run(["git", "-C", str(work), "add", "f.txt"], check=True)
    subprocess.run(["git", "-C", str(work), "commit", "-qm", "one"], check=True)
    subprocess.run(["git", "-C", str(work), "remote", "add", "origin", str(remote)],
                   check=True)
    subprocess.run(["git", "-C", str(work), "push", "-q", "origin", "perf/x"], check=True)
    tip = subprocess.run(["git", "-C", str(work), "rev-parse", "HEAD"],
                         capture_output=True, text=True).stdout.strip()
    got = runner.fetch_verify(work, "refs/heads/perf/x", "refs/focused-mac/head", tip)
    moved = runner.fetch_verify(work, "refs/heads/perf/x", "refs/focused-mac/head", SHA41)
    check("fetch_verify verifies a matching branch tip", got == tip)
    check("fetch_verify refuses a moved branch (SHA mismatch -> failure)",
          moved is None, "runner reports 'branch tip does not match source_head'")

# 6. Zero-test handling stays honest (structural, no cargo here).
src = (HERE / "runner.py").read_text()
check("zero-test selections tracked and never green",
      "zero_test_selections" in src
      and src.index("zero collected tests => neutral") < src.index("def run(")
      and src.index("if missing or passed == 0") < src.index('status(out_dir, "success"'))

# 7. build_matrix safety shape.
src = (HERE / "build_matrix.py").read_text()
check("no shell/eval paths in PR fetch script",
      "shell=True" not in src and "os.system" not in src and "eval(" not in src
      and "subprocess.run(cmd, shell" not in src)
check("build_matrix regex contract mirrors runner.py verbatim",
      (bm.SHA_RE.pattern, bm.HASH_RE.pattern, bm.ID_RE.pattern)
      == (runner.SHA_RE.pattern, runner.HASH_RE.pattern, runner.ID_RE.pattern))
check("build_matrix runs on the stock python3 (imports only stdlib; no tomllib "
      "user before the tests job's setup-python)",
      re.search(r"^\s*(import|from)\s+(runner|tomllib)\b", src, re.M) is None)
check("single explicit argv subprocess (batch relay only)",
      src.count("subprocess.run(") == 1 and 'gen_matrix.py' in src)
check("read-only HTTP GET, no write verb, token only in Authorization header",
      'method="GET"' in src and src.count("Authorization") == 1
      and "resp.read(MAX_PR_BODY)" in src)
check("GITHUB_OUTPUT-bounded emit", "break GITHUB_OUTPUT" in src)

# 8. Workflow structure: ci.yml (gate intact, dispatch inputs, env-plumbing).
cif = (HERE.parent / ".github" / "workflows" / "ci.yml").read_text()
check("default main gate untouched",
      "uses: storytold/craft-common-actions/.github/workflows/gate.yml@main" in cif
      and "packages: >-" in cif
      and "if: github.ref != 'refs/heads/perf/ci-migration'" in cif)
check("workflow_dispatch inputs are typed and defaulted",
      "pr_list:" in cif and "type: string" in cif and "full_batch:" in cif
      and "type: boolean" in cif and "default: false" in cif)
check("focused-mac call: read-only + statuses:write, inputs passed as data",
      "pull-requests: read" in cif and "statuses: write" in cif
      and "pr_list: ${{ inputs.pr_list || '' }}" in cif
      and "full_batch: ${{ inputs.full_batch || false }}" in cif)
check("no run input interpolated into any ci.yml command",
      "run:" in cif and cif.count("${{ inputs.") == 2)
check("focused-mac path stays branch-gated",
      "if: github.ref == 'refs/heads/perf/ci-migration'" in cif
      and "workflow_dispatch" in cif)

# 9. Workflow structure: focused-mac.yml (same safety envelope, added steps).
yml = (HERE.parent / ".github" / "workflows" / "focused-mac.yml").read_text()
check("workflow_call accepts the two focused inputs",
      "workflow_call" in yml and yml.count("default: ''") == 1
      and yml.count("default: false") == 1)
check("permissions: read-only API + status write, creds never persisted",
      "pull-requests: read" in yml and "statuses: write" in yml
      and yml.count("persist-credentials: false") == 2)
check("status step still reports on the tested sha",
      "SHA: ${{ matrix.head }}" in yml and yml.count("GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}")
      == 2 and "focused-mac-ci" in yml)
check("matrix gen is env-plumbed and output-file based",
      "PR_LIST: ${{ inputs.pr_list }}" in yml and "FULL_BATCH: ${{ inputs.full_batch }}"
      in yml and "python3 ci/build_matrix.py ci/items >> \"$GITHUB_OUTPUT\"" in yml)
check("GH_TOKEN in matrix step is for read GETs only",
      yml.count("secrets.GITHUB_TOKEN") == 2)
pos_mat = yml.find("python3 ci/materialize_packets.py")
pos_run = yml.find("python3 ci/runner.py --item")
check("materialize step before runner step, env data only",
      -1 < pos_mat < pos_run
      and "MATRIX: ${{ needs.matrix.outputs.matrix }}" in yml
      and "ITEM_ID: ${{ matrix.id }}" in yml)
check("zero-selection runs skip the tests job", "needs.matrix.outputs.count != '0'" in yml)
check("pinned actions unchanged", all(sha in yml for sha in (
      "3d3c42e5aac5ba805825da76410c181273ba90b1",   # checkout v7.0.1
      "0057852bfaa89a56745cba8c7296529d2fc39830",   # cache v4
      "ea165f8d65b6e75b540449e92b4886f43607fa02",   # upload-artifact v4
      "a26af69be951a213d495a4c3e4e4022e16d87065")))  # setup-python v5
check("limits wording intact (45min, parallel 3, no fail-fast, registry cache)",
      "timeout-minutes: 45" in yml and "max-parallel: 3" in yml
      and "fail-fast: false" in yml
      and "focused-mac-registry-${{ runner.os }}" in yml)

print("OFFLINE PR CHECKS:", "ALL PASS" if not failures
      else f"{len(failures)} FAILURES {failures}")
sys.exit(1 if failures else 0)
