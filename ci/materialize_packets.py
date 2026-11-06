#!/usr/bin/env python3
"""Materialize runtime-generated focused packets (workflow_dispatch PR mode)
for exactly one matrix entry, keeping runner.py's single --item contract.

Reads the run's matrix JSON from $MATRIX (data only, never shell-evaluated)
and this entry's id from $ITEM_ID:

  - batch-mode entries (no 'packet' key): the committed ci/items/ packet is
    read straight from this checkout; check it exists, write nothing;
  - PR-mode entries ('packet' key): build_matrix.py produced the packet from
    the live GitHub API. It is validated here with runner.load_item (the
    same sanitized five-key contract the runner enforces) and cross-checked
    against the entry: entry keys subset, 40-hex head, head ==
    packet.source_head (the status step reports on this sha), packet.id ==
    entry id, and a fixed focused-packets/ path. Any refusal deletes the
    written file and fails honestly.

Exit codes: 0 = materialized or nothing to do, 1 = refused (job fails).
"""
import json
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import runner  # noqa: E402

ENTRY_KEYS = {"id", "head", "item", "packet"}
ITEM_PATH = re.compile(r"^focused-packets/[A-Za-z0-9._-]+\.json$")
BATCH_PATH = re.compile(r"^ci/items/[A-Za-z0-9._-]+\.json$")


def fail(msg):
    print(f"materialize_packets: refused: {msg}", file=sys.stderr)
    return 1


def main() -> int:
    raw = os.environ.get("MATRIX", "").strip()
    item_id = os.environ.get("ITEM_ID", "")
    if not raw:
        print("materialize_packets: no matrix payload; skipping")
        return 0
    try:
        entries = json.loads(raw).get("include", [])
    except ValueError:
        return fail("matrix output is not JSON")
    if not isinstance(entries, list):
        return fail("matrix 'include' is not a list")
    matches = [e for e in entries if isinstance(e, dict) and e.get("id") == item_id]
    if not matches:
        print(f"materialize_packets: no entry for id {item_id!r}; skipping")
        return 0
    if len(matches) != 1:
        return fail(f"matrix entry id {item_id!r} is not unique")
    e = matches[0]
    extra = set(e) - ENTRY_KEYS
    if extra:
        return fail(f"unsanitized matrix entry keys: {sorted(extra)}")
    path = str(e.get("item", ""))
    if "packet" in e:
        head = str(e.get("head", ""))
        if not runner.SHA_RE.match(head):
            return fail("matrix head is not a 40-hex sha")
        if not ITEM_PATH.match(path):
            return fail(f"unsafe item path {path!r}")
        try:
            pkt = json.loads(e["packet"])
        except ValueError:
            return fail("packet payload is not JSON")
        if not isinstance(pkt, dict) or set(pkt) != runner.ALLOWED_KEYS:
            return fail("packet is not the sanitized five-key shape")
        if pkt.get("id") != e["id"]:
            return fail("packet id does not match the matrix entry id")
        if pkt.get("source_head") != head:
            return fail("packet source_head does not match the tested sha")
        p = Path(path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(e["packet"])
        try:
            runner.load_item(path)  # round-trip: the exact runner contract
        except (ValueError, OSError) as err:
            p.unlink(missing_ok=True)
            return fail(f"packet rejected by runner: {err}")
        print(f"materialize_packets: wrote {path} (tested sha {head})")
        return 0
    if not BATCH_PATH.match(path):
        return fail(f"unsafe batch item path {path!r}")
    if not Path(path).is_file():
        return fail(f"committed packet missing from checkout: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
