#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["chromadb>=1.0"]
# ///
"""Compare two ChromaDB collections for parity.

Usage: compare.py <py_collection> <rs_collection>

Exits 0 on full parity, non-zero on any mismatch.
Always deletes BOTH collections before exiting (cleanup).
"""
from __future__ import annotations

import sys
import chromadb

MAX_REPORT = 20


def get_all(col) -> dict[str, dict]:
    """Fetch all id→metadata from a collection, paginating."""
    result: dict[str, dict] = {}
    limit = 5000
    offset = 0
    while True:
        got = col.get(include=["metadatas"], limit=limit, offset=offset)
        ids = got.get("ids") or []
        metas = got.get("metadatas") or []
        for cid, meta in zip(ids, metas):
            result[cid] = meta or {}
        if len(ids) < limit:
            break
        offset += len(ids)
    return result


def normalise(meta: dict) -> dict:
    """Normalise metadata for comparison: treat missing/empty scope as equal."""
    out = dict(meta)
    if not out.get("scope"):
        out.pop("scope", None)
    return out


def compare(py_col_name: str, rs_col_name: str) -> bool:
    client = chromadb.HttpClient(host="192.168.1.2", port=8000)

    py_col = client.get_collection(py_col_name)
    rs_col = client.get_collection(rs_col_name)

    py_data = get_all(py_col)
    rs_data = get_all(rs_col)

    py_ids = set(py_data)
    rs_ids = set(rs_data)

    only_py = py_ids - rs_ids
    only_rs = rs_ids - py_ids
    shared = py_ids & rs_ids

    ok = True

    if only_py:
        ok = False
        print(f"PARITY FAIL: {len(only_py)} ids only in Python collection:")
        for cid in list(only_py)[:MAX_REPORT]:
            print(f"  PY-ONLY  {cid}  {py_data[cid]}")

    if only_rs:
        ok = False
        print(f"PARITY FAIL: {len(only_rs)} ids only in Rust collection:")
        for cid in list(only_rs)[:MAX_REPORT]:
            print(f"  RS-ONLY  {cid}  {rs_data[cid]}")

    field_diffs: list[str] = []
    fields = ("path", "line", "type", "scope", "lang")
    for cid in shared:
        py_n = normalise(py_data[cid])
        rs_n = normalise(rs_data[cid])
        for f in fields:
            pv = py_n.get(f)
            rv = rs_n.get(f)
            if pv != rv:
                field_diffs.append(
                    f"  FIELD_MISMATCH  id={cid}  field={f}  py={pv!r}  rs={rv!r}"
                )

    if field_diffs:
        ok = False
        print(f"PARITY FAIL: {len(field_diffs)} field mismatches (showing up to {MAX_REPORT}):")
        for line in field_diffs[:MAX_REPORT]:
            print(line)

    total = len(shared)
    if ok:
        print(f"PARITY OK: {total} chunks identical (py={len(py_ids)}, rs={len(rs_ids)})")
    else:
        print(
            f"PARITY SUMMARY: py={len(py_ids)} rs={len(rs_ids)} "
            f"only_py={len(only_py)} only_rs={len(only_rs)} "
            f"field_diffs={len(field_diffs)}"
        )

    return ok


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: compare.py <py_collection> <rs_collection>", file=sys.stderr)
        return 1

    py_col_name, rs_col_name = sys.argv[1], sys.argv[2]

    try:
        result = compare(py_col_name, rs_col_name)
    finally:
        # Always clean up both collections.
        client = chromadb.HttpClient(host="192.168.1.2", port=8000)
        for name in (py_col_name, rs_col_name):
            try:
                client.delete_collection(name)
                print(f"cleanup: deleted {name}")
            except Exception as e:
                print(f"cleanup: could not delete {name}: {e}", file=sys.stderr)

    return 0 if result else 1


if __name__ == "__main__":
    sys.exit(main())
