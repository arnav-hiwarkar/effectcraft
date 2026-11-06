#!/usr/bin/env python3
"""Emit the focused-mac-ci Actions matrix, chosen by the run's inputs.

workflow_dispatch chooses ONE mode (the legacy 59-item batch now runs only
when explicitly requested; a run with no focused input starts nothing):

  PR_LIST='80 82      85'  -> one item per numeric fork PR number, max 5
                              distinct numbers. Every PR is fetched read-only
                              (one JSON GET /repos/<REPO>/pulls/<n> with the
                              run token) and must verify as: open state,
                              head branch from this repository, a perf/*
                              branch name, and a head sha that is CURRENT
                              lowercase 40-hex at retrieval time. The packet
                              is the sanitized five-key shape; runner.py then
                              re-verifies branch tip == source_head at test
                              time, and the report step posts the commit
                              status on exactly that verified sha.
  FULL_BATCH=true           -> relay gen_matrix.py over the committed
                              ci/items/ packets (the previous full-batch
                              behaviour, now explicit only).
  neither                   -> zero items: the tests job skips; dispatching
                              with no focused input never re-runs all 59.

Both modes set at once, any invalid input (non-numeric token, duplicate,
more than 5 numbers) or any unverifyable PR => exit 1 with NO matrix: the
dispatch fails loudly instead of silently skipping an item or testing an
unverified head.

Safety: argv-only subprocess + one JSON-only HTTP GET; no shell, no eval;
PR titles, bodies and notes are never parsed into commands (ids are bounded
numbers, heads 40-hex shas, branches the perf/* charset, packets JSON-
escaped single lines). Output is one key=value line per GITHUB_OUTPUT key.
"""
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent

MAX_PRS = 5
PR_NUM = re.compile(r"^[1-9][0-9]{0,8}$")            # bounded positive integers
PR_LIST_TEXT = re.compile(r"^[,\s0-9]{0,256}$")       # whole-input charset cap
REPO_RE = re.compile(r"^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$")
API = "https://api.github.com"
MAX_PR_BODY = 1 << 20                                 # 1 MiB cap on one PR JSON

# The packet contract, verbatim from runner.py (offline_pr_checks.py asserts
# the mirror). Not imported here on purpose: the matrix job runs on the
# runner's stock python3 (no setup-python step), which may lack tomllib —
# the only tomllib user is runner.py itself, inside the tests job.
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
HASH_RE = re.compile(r"^perf/[A-Za-z0-9._-]+$")
ID_RE = re.compile(r"^[A-Za-z0-9_-]{1,16}$")


class InputError(Exception):
    """Deliberate, honest refusal of an input or a PR."""


def parse_pr_list(text) -> list:
    """Strict list of <=MAX_PRS distinct positive PR numbers; [] when empty."""
    text = text or ""
    if not PR_LIST_TEXT.match(text):
        raise InputError("pr_list contains characters outside [0-9], ',' and whitespace")
    nums, seen = [], set()
    for tok in (t for t in re.split(r"[,\s]+", text) if t):
        if not PR_NUM.match(tok):
            raise InputError(f"not a positive numeric PR number: {tok!r}")
        n = int(tok)
        if n in seen:
            raise InputError(f"duplicate PR number: {n}")
        seen.add(n)
        nums.append(n)
    if len(nums) > MAX_PRS:
        raise InputError(f"more than {MAX_PRS} PR numbers ({len(nums)} given); "
                         "batching all items is the explicit FULL_BATCH=true mode")
    return nums


def fetch_pr(repo, n, token) -> dict:
    """One read-only GET of the PR object; a bounded JSON document only."""
    req = urllib.request.Request(
        f"{API}/repos/{repo}/pulls/{n}",
        headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "effectcraft focused-mac-ci",
            "Authorization": f"Bearer {token}",
        },
        method="GET",
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = resp.read(MAX_PR_BODY)
    except urllib.error.HTTPError as e:
        raise InputError(f"PR {n} not retrievable (HTTP {e.code}; "
                         "verify the number is an open PR on this repository)") from e
    except urllib.error.URLError as e:
        raise InputError(f"PR {n} request failed ({e.reason})") from e
    try:
        pr = json.loads(body)
    except ValueError as e:
        raise InputError(f"PR {n} response was not JSON") from e
    if not isinstance(pr, dict):
        raise InputError(f"PR {n} response was not a JSON object")
    return pr


def packet_from_pr(pr, n, repo) -> dict:
    """Sanitized packet from one verified PR object; refuse anything else."""
    if not isinstance(pr.get("number"), int) or pr.get("number") != n:
        raise InputError(f"PR {n}: API returned a different number")
    if pr.get("state") != "open":
        raise InputError(f"PR {n}: not an open PR (state {pr.get('state')!r})")
    if pr.get("merged"):
        raise InputError(f"PR {n}: already merged")
    head = pr.get("head")
    if not isinstance(head, dict) or not isinstance(head.get("repo"), dict):
        raise InputError(f"PR {n}: head repository is deleted or gone (no "
                         "branch to fetch and verify)")
    if head["repo"].get("full_name") != repo:
        raise InputError(f"PR {n}: head is from {head['repo'].get('full_name')!r}; "
                         "only fork-repository PR heads are accepted")
    ref = str(head.get("ref", ""))
    sha = str(head.get("sha", ""))
    if not HASH_RE.match(ref):
        raise InputError(f"PR {n}: head branch {ref!r} is outside the perf/* namespace")
    if not SHA_RE.match(sha):
        raise InputError(f"PR {n}: head is not a lowercase 40-hex sha")
    it = {"id": f"pr{n}", "source_head": sha, "github_branch": ref,
          "focus": "pending", "named_filters": []}
    for key, rx in (("id", ID_RE), ("github_branch", HASH_RE),
                    ("source_head", SHA_RE)):
        if not rx.match(str(it[key])):
            raise InputError(f"PR {n}: packet {key} failed its charset contract")
    return it


def emit(stream, include, mode):
    """GITHUB_OUTPUT-proof key=value lines; refuse any multi-line payload."""
    lines = [f"mode={mode}",
             f"matrix={json.dumps({'include': include}, separators=(',', ':'))}",
             f"count={len(include)}"]
    for ln in lines:
        if "\n" in ln or "\r" in ln:
            raise InputError("internal: payload would break GITHUB_OUTPUT")
        stream.write(ln + "\n")


def relay_batch(items_dir, out) -> None:
    """Relay gen_matrix.py over the committed packets (explicit batch mode)."""
    r = subprocess.run([sys.executable, str(HERE / "gen_matrix.py"), str(items_dir)],
                       capture_output=True, text=True)
    if r.returncode != 0 or r.stderr.strip().startswith("usage:"):
        print("build_matrix: gen_matrix.py failed", file=sys.stderr)
        raise InputError("batch relay failed")
    lines = r.stdout.splitlines()
    got = {ln.split("=", 1)[0] for ln in lines if "=" in ln}
    if not {"matrix", "count"} <= got:
        raise InputError("batch relay produced no matrix/count output")
    for ln in lines:
        if ln.split("=", 1)[0] in ("matrix", "count"):
            out.write(ln + "\n")
    out.write("mode=batch\n")


def main() -> int:
    args = sys.argv[1:]
    if len(args) != 1:
        print("usage: build_matrix.py ITEM_DIR", file=sys.stderr)
        return 2
    items_dir = Path(args[0])
    pr_text = os.environ.get("PR_LIST", "")
    full = os.environ.get("FULL_BATCH", "").strip().lower() == "true"
    try:
        nums = parse_pr_list(pr_text)
        if nums and full:
            raise InputError("choose exactly one mode: PR_LIST or FULL_BATCH=true")
        if full:
            relay_batch(items_dir, sys.stdout)
        elif nums:
            repo = os.environ.get("REPO", "")
            token = os.environ.get("GH_TOKEN", "")
            if not REPO_RE.match(repo):
                raise InputError("GITHUB repository id (REPO) missing or malformed")
            if not token:
                raise InputError("read-only run token (GH_TOKEN) missing")
            inc, problems = [], []
            for n in nums:
                try:
                    pkt = packet_from_pr(fetch_pr(repo, n, token), n, repo)
                except InputError as e:
                    problems.append(str(e))
                    continue
                inc.append({"id": pkt["id"], "head": pkt["source_head"],
                            "item": f"focused-packets/pr{n}.json",
                            "packet": json.dumps(pkt, separators=(",", ":"))})
            if problems:  # loud refusal: nothing runs for a partially bad input
                print("build_matrix: refused: " + "; ".join(problems), file=sys.stderr)
                return 1
            emit(sys.stdout, inc, "pr")
        else:
            emit(sys.stdout, [], "none")
    except InputError as e:
        print(f"build_matrix: refused: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
