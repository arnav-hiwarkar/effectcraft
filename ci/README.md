# Focused Mac CI — supplemental migration infrastructure (ci/ only)

For source heads that cannot run the standard merge gate (merge conflicts,
pending validation). Never a substitute for `cargo xtask ci` or the standard
PR gate; claims no performance acceptance.

## Where each file goes (publishing is done by root, not by item workers)

| File | Placement on branch `perf/ci-migration` of the fork |
|---|---|
| `focused-mac.yml` | `.github/workflows/focused-mac.yml` |
| `sanitize_items.py` | stays local (it reads raw packets, which are not committed) |
| `gen_matrix.py` | `ci/gen_matrix.py` |
| `build_matrix.py` | `ci/build_matrix.py` (workflow_dispatch input modes) |
| `materialize_packets.py` | `ci/materialize_packets.py` (runtime packet handoff) |
| `runner.py` | `ci/runner.py` |
| `offline_check.py` | stays local; create/verify-only helper |
| `offline_pr_checks.py` | `ci/offline_pr_checks.py` (committed offline verification) |
| sanitized item/*.json | `ci/items/` in the same branch (matrix inputs) |

Regenerate sanitized packets (offline, from the local raw packet dir):

```
python3 ci/sanitize_items.py /Users/ash/memaudit/github-migration/items ci/items
```

Only draft/pending-validation packets are emitted; validated items retain their recorded evidence. Older branches may lack the
standard PR workflow, so no automatic gate is assumed. Sanitized packets carry only id/source_head/github_branch/
focus/named_filters — no raw notes, bodies, PR objects, diffstats, or URLs
ever enter the CI branch.

## Behaviour

- Trigger: manual `workflow_dispatch` on `perf/ci-migration` (routed through
  `ci.yml`), run as a `workflow_call` from `.github/workflows/ci.yml`;
  concurrency cancels older runs. Two mutually exclusive focused inputs:
  - `pr_list` (string): up to five distinct numeric fork PR numbers, separated
    by spaces or commas. The matrix job fetches each PR with one read-only
    API GET (`ci/build_matrix.py`, `pull-requests: read`) and verifies it:
    open state, head branch from this repository, `perf/*` branch name, and a
    current lowercase 40-hex head sha at retrieval time. Each verified PR
    becomes one sanitized five-key packet (focus "pending"); `runner.py`
    re-verifies branch tip == `source_head` at test time, so a force-push
    between dispatch and test fails honestly instead of testing a stale head.
    The report step posts the commit status on the exact tested sha.
  - `full_batch=true`: relays `gen_matrix.py` over the committed
    `ci/items/*.json` (draft or pending-validation items only; existing
    validated evidence is retained). The legacy full batch now runs ONLY via
    this explicit switch; dispatching with no focused input starts nothing
    (`count=0`, tests job skips) instead of re-running all 59.
  - Any invalid input (non-numeric/duplicate/over-limit numbers, both inputs
    set, an unverifyable PR: closed, merged, cross-repository head, deleted
    head repository, non-`perf/*` branch, wrong-sha) fails the whole dispatch
    loudly with no items run — it never silently skips an item.
- Job 1 `matrix`: emits the GITHUB_OUTPUT matrix lines (`mode`/`matrix`/
  `count`); PR-mode entries carry a runtime-made packet, batch entries point
  at committed `ci/items/` files.
- Job 2 `tests`: macos-latest, max-parallel 3, 45 min cap. PR-mode packets are
  materialized per entry by `ci/materialize_packets.py` (env data only; the
  packet is revalidated with `runner.load_item` and cross-checked: entry head
  == packet `source_head`, packet id == entry id, fixed `focused-packets/`
  path — a refusal deletes the file and fails honestly). The runner then
  verifies the branch head equals the item's `source_head`, diffs vs a
  verified merge-base (stable local ref, never stale origin/main), maps
  changed files to the closest `Cargo.toml` packages, runs `cargo test -p
  <pkg>` with explicit argv, and honestly reports no-run crates/notes.
  Registry/git cache only — no target dir, so no foreign head's test binary
  is reused.
- Report: final step holds GH_TOKEN only there (statuses:write) and sets
  commit status context `focused-mac-ci` on the tested SHA; GitHub statuses
  have no neutral state, so neutral results are reported as failure with the
  honest description from `ci-out/status.json` (limit-surviving <=140 chars:
  truncation happens at the start, not over the limits suffix).

## Safety (PR mode)

- The PR fetch is one bounded read-only JSON GET per PR (1 MiB cap, 30 s
  timeout); no shell interpolation of any input or API text, no eval:
  PR titles/bodies/notes are never parsed as data beyond the verified
  number/state/ref/sha fields, and packets travel as JSON.
- IDs are bounded numbers (`pr<N>`), heads 40-hex shas, branches the
  `perf/[A-Za-z0-9._-]+` charset (`runner.py`'s own contract); filenames are
  derived only from validated numbers.
- `pr_list` parsing refuses anything outside `[0-9], whitespace` up to a
  256-char cap, before any network call.


## Limits (enforced by wording in every status description)

- Tests run with `cargo test --locked` and `CARGO_BUILD_JOBS=2`, using the
  tested head's own lockfile; stable toolchain selected via rustup.
- A named filter or selected crate that collects zero tests is reported
  neutral ("zero-test selection"), never a green pass.
- No GPU/UI app runs, no criterion benchmarks, no performance acceptance.
- Source-head checks only; never claims current-base integration or upstream gates.
- Commit statuses carry item id, diff mode and crate list; details in logs.

Offline verification: `python3 ci/offline_pr_checks.py` (focused-PR dispatch
path: input validation, head selection, packet materialization, SHA-mismatch
refusal, zero-test handling, workflow safety shape; no network needed) and
`python3 ci/offline_check.py` (stays local; verifies the batch artifacts:
sanitizer, gen_matrix, package mapping, status truncation).
