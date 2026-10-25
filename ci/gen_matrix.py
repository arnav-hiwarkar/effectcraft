#!/usr/bin/env python3
"""Emit the focused-mac-ci Actions matrix from sanitized CI items.

Usage: gen_matrix.py ci/items   (prints GITHUB_OUTPUT lines in one run)
Input must be exactly the packets sanitize_items.py produces: keys subset of
{id, source_head, github_branch, focus, named_filters}, focus in
{draft, pending}. Any other key => the packet is skipped, so raw migration
packets (with notes/URLs) committed to ci/items/ can never enter CI. Output:
'matrix={...}' (single line; tests job parses with fromJson) and 'count=N',
where an entry carries only the item id, verified source head SHA and the
packet's path - nothing from notes ever becomes a command.
"""
import json
import sys
from pathlib import Path

ALLOWED = {"id", "source_head", "github_branch", "focus", "named_filters"}


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: gen_matrix.py ITEM_DIR", file=sys.stderr)
        return 2
    items_dir, prefix = Path(sys.argv[1]), sys.argv[1].rstrip("/")
    include = []
    for p in sorted(items_dir.glob("*.json"), key=lambda q: q.name):
        try:
            it = json.loads(p.read_text(encoding="utf-8"))
        except ValueError:
            continue
        if set(it) - ALLOWED or not {"id", "source_head", "github_branch"} <= set(it):
            continue  # not sanitized: skip rather than run
        if it.get("focus") not in ("draft", "pending"):
            continue
        include.append({"id": str(it["id"]), "head": it["source_head"], "item": f"{prefix}/{p.name}"})
    print(f"matrix={json.dumps({'include': include}, separators=(',', ':'))}")
    print(f"count={len(include)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
