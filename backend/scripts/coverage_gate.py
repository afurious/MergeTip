#!/usr/bin/env python3
"""Fails when a crate's line coverage is under its threshold.

Reads `cargo llvm-cov report --json --summary-only` (whole-workspace test run) on stdin and sums
lines per crate, counting only the crate's own `src/` files (not its tests).

Usage: cargo llvm-cov report --json --summary-only | python3 scripts/coverage_gate.py
"""

import json
import sys

# Line coverage floors (milestone 5.10): pure logic and money code high, the GitHub adapter lower.
THRESHOLDS = {"parser": 90.0, "core": 90.0, "payer": 90.0, "github": 70.0}

report = json.load(sys.stdin)
totals = {crate: [0, 0] for crate in THRESHOLDS}
for file in report["data"][0]["files"]:
    name = file["filename"].replace("\\", "/")
    for crate in THRESHOLDS:
        if f"/crates/{crate}/src/" in name:
            lines = file["summary"]["lines"]
            totals[crate][0] += lines["covered"]
            totals[crate][1] += lines["count"]

failed = False
for crate, threshold in THRESHOLDS.items():
    covered, count = totals[crate]
    percent = 100.0 * covered / count if count else 0.0
    ok = percent >= threshold
    failed |= not ok
    print(f"{crate:8} {percent:6.2f}% lines (needs {threshold:.0f}%) {'ok' if ok else 'FAIL'}")
sys.exit(1 if failed else 0)
