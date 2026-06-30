#!/usr/bin/env python3
"""Build a machine-readable capability registry from agentic matrix CSVs.

Status policy:
  green  = all runs pass and evidence count >= --green-min-runs
  yellow = at least one pass, but evidence is single-run/partial
  red    = no passing runs

Examples:
  scripts/bench/matrix_capabilities.py scripts/bench/results/full-matrix-*/per-run.csv
  scripts/bench/matrix_capabilities.py scripts/bench/results/full-matrix-20260629-203836 --out docs/model-matrix-capabilities.json
"""
from __future__ import annotations

import argparse
import csv
import json
import statistics
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def resolve_inputs(paths: list[str]) -> list[Path]:
    out: list[Path] = []
    for raw in paths:
        path = Path(raw)
        if path.is_dir():
            path = path / "per-run.csv"
        if not path.exists():
            raise SystemExit(f"{path}: missing")
        out.append(path)
    return out


def read_csv(path: Path) -> list[dict[str, str]]:
    with path.open(newline="", errors="replace") as f:
        rows = []
        for row in csv.DictReader(f):
            item = {k: (v or "") for k, v in row.items()}
            item["_source"] = str(path)
            rows.append(item)
        return rows


def as_float(value: str) -> float | None:
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def as_int(value: str) -> int | None:
    try:
        return int(float(value))
    except (TypeError, ValueError):
        return None


def passed(row: dict[str, str]) -> bool:
    return row.get("pass") == "1" and row.get("timeout") != "1"


def status_for(passes: int, total: int, green_min_runs: int) -> str:
    if total == 0:
        return "gray"
    if passes == 0:
        return "red"
    if passes == total and total >= green_min_runs:
        return "green"
    return "yellow"


def compact_row(row: dict[str, str]) -> dict[str, Any]:
    return {
        "source": row.get("_source", ""),
        "seconds": as_float(row.get("seconds", "")),
        "pass": passed(row),
        "rc": as_int(row.get("rc", "")),
        "timeout": row.get("timeout") == "1",
        "repairs": as_int(row.get("repairs", "")),
        "agent_peak_mb": as_int(row.get("agent_peak_mb", "")),
        "model_footprint_mb": as_int(row.get("model_footprint_mb", "")),
    }


def build_registry(rows: list[dict[str, str]], green_min_runs: int) -> dict[str, Any]:
    grouped: dict[tuple[str, str, str], list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        grouped[(row.get("agent", ""), row.get("model", ""), row.get("task", ""))].append(row)

    cells = []
    for (agent, model, task), reps in sorted(grouped.items()):
        pass_count = sum(1 for row in reps if passed(row))
        total = len(reps)
        seconds = [v for row in reps if (v := as_float(row.get("seconds", ""))) is not None]
        footprints = [v for row in reps if (v := as_int(row.get("model_footprint_mb", ""))) is not None]
        repairs = [as_int(row.get("repairs", "")) or 0 for row in reps]
        latest = reps[-1]
        cells.append(
            {
                "agent": agent,
                "model": model,
                "task": task,
                "status": status_for(pass_count, total, green_min_runs),
                "passes": pass_count,
                "runs": total,
                "pass_rate": pass_count / total if total else 0.0,
                "mean_seconds": statistics.fmean(seconds) if seconds else None,
                "latest_seconds": as_float(latest.get("seconds", "")),
                "model_footprint_mb": max(footprints) if footprints else None,
                "total_repairs": sum(repairs),
                "latest": compact_row(latest),
            }
        )

    counts: dict[str, int] = defaultdict(int)
    for cell in cells:
        counts[cell["status"]] += 1

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "policy": {
            "green": f"all runs pass and runs >= {green_min_runs}",
            "yellow": "at least one pass but evidence is partial or below green threshold",
            "red": "zero passing runs",
            "gray": "known but not run/refused (reserved for scheduler/UI inputs)",
            "green_min_runs": green_min_runs,
        },
        "summary": dict(sorted(counts.items())),
        "cells": cells,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("inputs", nargs="+", help="per-run.csv files or result dirs")
    p.add_argument("--out", type=Path, default=None, help="write JSON here instead of stdout")
    p.add_argument("--green-min-runs", type=int, default=3, help="minimum all-pass runs for green")
    return p.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    inputs = resolve_inputs(args.inputs)
    rows: list[dict[str, str]] = []
    for path in inputs:
        rows.extend(read_csv(path))
    registry = build_registry(rows, args.green_min_runs)
    data = json.dumps(registry, indent=2, sort_keys=True) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(data)
        print(f"wrote {args.out}")
    else:
        print(data, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
