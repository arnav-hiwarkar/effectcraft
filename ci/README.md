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
| `runner.py` | `ci/runner.py` |
| `offline_check.py` | stays local; create/verify-only helper |
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

- Trigger: push to `perf/ci-migration` only; concurrency cancels older runs.
- Job 1 `matrix`: generates sha+item pairs from `ci/items/*.json` (draft or
  pending-validation items only; existing validated evidence is retained).
- Job 2 `tests`: macos-latest, max-parallel 3, 45 min cap; verifies the branch
  head equals the item's `source_head`, diffs vs a verified merge-base (stable
  local ref, never stale origin/main), maps changed files to the closest
  `Cargo.toml` packages, runs `cargo test -p <pkg>` with explicit argv, and
  honestly reports no-run crates/notes. Registry/git cache only — no target
  dir, so no foreign head's test binary is reused.
- Report: final step holds GH_TOKEN only there (statuses:write, contents:read)
  and sets commit status context `focused-mac-ci` on the tested SHA; GitHub
  statuses have no neutral state, so neutral results are reported as failure
  with the honest description from `ci-out/status.json` (limit-surviving <=140
  chars: truncation happens at the start, not over the limits suffix).

## Limits (enforced by wording in every status description)

- Tests run with `cargo test --locked` and `CARGO_BUILD_JOBS=2`, using the
  tested head's own lockfile; stable toolchain selected via rustup.
- A named filter or selected crate that collects zero tests is reported
  neutral ("zero-test selection"), never a green pass.
- No GPU/UI app runs, no criterion benchmarks, no performance acceptance.
- Source-head checks only; never claims current-base integration or upstream gates.
- Commit statuses carry item id, diff mode and crate list; details in logs.

Offline verification: `python3 ci/offline_check.py` (no network needed).
