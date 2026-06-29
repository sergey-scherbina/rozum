#!/usr/bin/env python3
"""Summarize full agentic-matrix output.

Usage:
  summarize_matrix.py /tmp/full_matrix-<stamp>.log
  summarize_matrix.py scripts/bench/results/full-matrix-<stamp>/per-run.csv
  summarize_matrix.py scripts/bench/results/full-matrix-<stamp>
"""
from __future__ import annotations

import csv
import re
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

TASK_ORDER = {name: i for i, name in enumerate(["greet", "build", "fix", "test", "debug", "rpn"])}


def display_model(model: str) -> str:
    return model.replace("mlx-community:", "")


def cell(
    model: str,
    agent: str,
    task: str,
    seconds: str,
    passed: str,
    timeout: bool = False,
    reasons: list[str] | None = None,
) -> dict[str, Any]:
    return {
        "model": display_model(model),
        "agent": agent,
        "task": task,
        "seconds": float(seconds or 0),
        "pass": passed == "1",
        "timeout": timeout,
        "reasons": reasons or [],
    }


def parse_log(lines: list[str]) -> tuple[list[dict[str, Any]], dict[str, bool]]:
    model = None
    refused: dict[str, bool] = {}
    cells: list[dict[str, Any]] = []
    cur: dict[str, Any] | None = None
    cell_re = re.compile(r"^\s*\[(\w+)\]\s+(\w+)\s+([\d.]+)s(\s+\(RUN_TIMEOUT\))?\s+pass=(\d)")
    detail_re = re.compile(r"^\s+(PASS|FAIL)\s+(.*)$")

    for ln in lines:
        m = re.search(r"^=+ model:\s+(\S+)", ln)
        if m:
            model = display_model(m.group(1))
            refused.setdefault(model, False)
            cur = None
            continue
        if "gateway not ready" in ln and model:
            refused[model] = True
            continue
        c = cell_re.match(ln)
        if c and model:
            cur = cell(model, c.group(1), c.group(2), c.group(3), c.group(5), bool(c.group(4)))
            cells.append(cur)
            continue
        d = detail_re.match(ln)
        if d and cur is not None:
            cur["reasons"].append(f"{d.group(1)} {d.group(2)}")
    return cells, refused


def parse_csv(path: Path) -> tuple[list[dict[str, Any]], dict[str, bool]]:
    cells: list[dict[str, Any]] = []
    with path.open(newline="", errors="replace") as f:
        for row in csv.DictReader(f):
            reasons = []
            if row.get("rc") not in ("", "0", None):
                reasons.append(f"rc={row['rc']}")
            if row.get("timeout") == "1":
                reasons.append("RUN_TIMEOUT")
            if row.get("repairs") not in ("", "0", None):
                reasons.append(f"repairs={row['repairs']}")
            if row.get("pass") != "1" and not reasons:
                reasons.append("failed row (CSV has no verifier detail; inspect log/kept workdir)")
            cells.append(
                cell(
                    row.get("model", ""),
                    row.get("agent", ""),
                    row.get("task", ""),
                    row.get("seconds", "0"),
                    row.get("pass", "0"),
                    row.get("timeout") == "1",
                    reasons,
                )
            )
    return cells, {}


def read_input(arg: str) -> tuple[list[dict[str, Any]], dict[str, bool]]:
    if arg == "-":
        return parse_log(sys.stdin.read().splitlines())
    path = Path(arg)
    if path.is_dir():
        csv_path = path / "per-run.csv"
        if csv_path.exists():
            return parse_csv(csv_path)
        raise SystemExit(f"{path}: no per-run.csv")
    if path.name == "per-run.csv" or path.suffix == ".csv":
        return parse_csv(path)
    return parse_log(path.read_text(errors="replace").splitlines())


def summarize(cells: list[dict[str, Any]], refused: dict[str, bool]) -> None:
    models = list(refused.keys())
    for c in cells:
        if c["model"] not in models:
            models.append(c["model"])

    print("=" * 78)
    for model in models:
        rows = [c for c in cells if c["model"] == model]
        if refused.get(model) and not rows:
            print(f"\n■ {model}:  ⛔ NOT LOADED (gateway refused/failed to start)")
            print("   every cell for this model was skipped")
            continue
        print(f"\n■ {model}")
        npass = sum(1 for c in rows if c["pass"])
        print(f"   {npass}/{len(rows)} green")

        grouped: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
        for row in rows:
            grouped[(row["agent"], row["task"])].append(row)
        for (agent, task), reps in sorted(
            grouped.items(), key=lambda item: (item[0][0], TASK_ORDER.get(item[0][1], 99), item[0][1])
        ):
            passed = sum(1 for r in reps if r["pass"])
            total = len(reps)
            mark = "✅" if passed == total else ("❌" if passed == 0 else "◐")
            avg = sum(r["seconds"] for r in reps) / total if total else 0
            why = ""
            if passed != total:
                failed_reasons = []
                for r in reps:
                    if not r["pass"]:
                        failed_reasons.extend(r["reasons"])
                why = "  ·  " + "; ".join(failed_reasons[:4])
            print(f"   {mark} {agent:9s} {task:6s} {passed}/{total}  avg={avg:7.1f}s{why}")

    print("\n" + "=" * 78)
    total = len(cells)
    passed = sum(1 for c in cells if c["pass"])
    not_loaded = sum(
        1
        for model, is_refused in refused.items()
        if is_refused and not any(c["model"] == model for c in cells)
    )
    print(f"TOTAL ran: {passed}/{total} green   ({not_loaded} model(s) not loaded)")


def main() -> None:
    arg = sys.argv[1] if len(sys.argv) > 1 else "-"
    summarize(*read_input(arg))


if __name__ == "__main__":
    main()
