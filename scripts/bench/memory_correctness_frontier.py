#!/usr/bin/env python3
"""Report the model×driver Pareto frontier for correctness, memory, and memory-time cost."""

from __future__ import annotations

import argparse
import csv
import json
import math
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


def as_float(value: str | None) -> float | None:
    try:
        return float(value or "")
    except ValueError:
        return None


def passed(row: dict[str, str]) -> bool:
    verdict = row.get("verdict", "")
    if verdict:
        return verdict == "pass"
    return row.get("pass") == "1" and row.get("timeout") != "1"


def wilson_lower(passes: int, runs: int, z: float = 1.959963984540054) -> float:
    if runs <= 0:
        return 0.0
    p = passes / runs
    z2 = z * z
    center = p + z2 / (2 * runs)
    radius = z * math.sqrt((p * (1 - p) + z2 / (4 * runs)) / runs)
    return max(0.0, (center - radius) / (1 + z2 / runs))


def memory_mb(row: dict[str, str]) -> float | None:
    # MLX unified-memory peak is more faithful than process RSS. `/usr/bin/time` footprint is the
    # portable fallback and is backfilled after the model exits.
    return as_float(row.get("mlx_peak_mb")) or as_float(row.get("model_footprint_mb"))


def build_candidates(rows: list[dict[str, str]], min_correctness: float) -> list[dict[str, Any]]:
    groups: dict[tuple[str, str], list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        groups[(row.get("agent", ""), row.get("model", ""))].append(row)

    out: list[dict[str, Any]] = []
    for (agent, model), reps in sorted(groups.items()):
        pass_count = sum(passed(row) for row in reps)
        runs = len(reps)
        seconds = [v for row in reps if (v := as_float(row.get("seconds"))) is not None]
        peaks = [v for row in reps if (v := memory_mb(row)) is not None]
        peak_mb = max(peaks) if peaks else None
        # A failed attempt consumed memory and time too. Missing per-row memory inherits the
        # candidate high-water so old CSVs remain comparable after footprint backfill.
        gib_seconds_total = 0.0
        have_cost = peak_mb is not None
        if have_cost:
            for row in reps:
                secs = as_float(row.get("seconds")) or 0.0
                mem = memory_mb(row) or peak_mb or 0.0
                gib_seconds_total += mem / 1024.0 * secs
        gib_seconds_per_solve = (
            gib_seconds_total / pass_count if have_cost and pass_count > 0 else None
        )
        lower = wilson_lower(pass_count, runs)
        out.append(
            {
                "agent": agent,
                "model": model,
                "passes": pass_count,
                "runs": runs,
                "pass_rate": pass_count / runs if runs else 0.0,
                "correctness_lower_95": lower,
                "eligible": lower >= min_correctness,
                "peak_memory_mb": peak_mb,
                "mean_seconds": sum(seconds) / len(seconds) if seconds else None,
                "gib_seconds_per_solve": gib_seconds_per_solve,
                "pareto": False,
            }
        )

    for candidate in out:
        cmem = candidate["peak_memory_mb"]
        ccost = candidate["gib_seconds_per_solve"]
        if cmem is None or ccost is None:
            continue
        dominated = False
        for other in out:
            if other is candidate:
                continue
            omem = other["peak_memory_mb"]
            ocost = other["gib_seconds_per_solve"]
            if omem is None or ocost is None:
                continue
            no_worse = (
                other["correctness_lower_95"] >= candidate["correctness_lower_95"]
                and omem <= cmem
                and ocost <= ccost
            )
            strict = (
                other["correctness_lower_95"] > candidate["correctness_lower_95"]
                or omem < cmem
                or ocost < ccost
            )
            if no_worse and strict:
                dominated = True
                break
        candidate["pareto"] = not dominated
    return out


def read_rows(paths: list[Path]) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []
    for path in paths:
        if path.is_dir():
            path = path / "per-run.csv"
        if not path.exists():
            raise SystemExit(f"{path}: missing")
        with path.open(newline="", errors="replace") as handle:
            rows.extend({k: (v or "") for k, v in row.items()} for row in csv.DictReader(handle))
    return rows


def print_text(candidates: list[dict[str, Any]], min_correctness: float) -> None:
    print(f"memory × correctness frontier (eligible lower-95 ≥ {min_correctness:.0%})")
    print("P  ok  pass   lower95  peak-GiB  GiB·s/solve  seconds  agent   model")
    ordered = sorted(
        candidates,
        key=lambda c: (
            not c["pareto"],
            not c["eligible"],
            -(c["correctness_lower_95"]),
            c["peak_memory_mb"] or float("inf"),
        ),
    )
    for c in ordered:
        peak = c["peak_memory_mb"]
        cost = c["gib_seconds_per_solve"]
        mean = c["mean_seconds"]
        print(
            f"{'*' if c['pareto'] else '-'}  {'✓' if c['eligible'] else '-'}  "
            f"{c['passes']:>2}/{c['runs']:<2}  {c['correctness_lower_95']:>7.1%}  "
            f"{peak / 1024:>8.2f}  " if peak is not None else
            f"{'*' if c['pareto'] else '-'}  {'✓' if c['eligible'] else '-'}  "
            f"{c['passes']:>2}/{c['runs']:<2}  {c['correctness_lower_95']:>7.1%}  {'n/a':>8}  ",
            end="",
        )
        print(
            f"{cost:>11.1f}  " if cost is not None else f"{'n/a':>11}  ",
            end="",
        )
        print(f"{mean:>7.1f}  " if mean is not None else f"{'n/a':>7}  ", end="")
        print(f"{c['agent']:<7} {c['model']}")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inputs", nargs="+", type=Path, help="per-run.csv files or result dirs")
    parser.add_argument("--min-correctness", type=float, default=0.80, help="minimum lower-95 bound")
    parser.add_argument("--json", action="store_true", help="emit JSON")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if not 0.0 <= args.min_correctness <= 1.0:
        raise SystemExit("--min-correctness must be in [0,1]")
    candidates = build_candidates(read_rows(args.inputs), args.min_correctness)
    if args.json:
        print(json.dumps({"min_correctness": args.min_correctness, "candidates": candidates}, indent=2))
    else:
        print_text(candidates, args.min_correctness)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
