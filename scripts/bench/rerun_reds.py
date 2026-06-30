#!/usr/bin/env python3
"""Rerun only non-green agentic matrix cells and build a latest-wins merged CSV.

Examples:
  scripts/bench/rerun_reds.py scripts/bench/results/full-matrix-20260629-203836 --dry-run
  scripts/bench/rerun_reds.py scripts/bench/results/full-matrix-20260629-203836 --nctx 8192

The script intentionally runs exact cells one at a time instead of passing broad AGENTS/TASKS sets to
agentic.sh. That avoids accidentally rerunning a cross-product of already-green cells.
"""
from __future__ import annotations

import argparse
import csv
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

DEFAULT_HEADER = [
    "agent",
    "model",
    "task",
    "difficulty",
    "seconds",
    "pass",
    "rc",
    "timeout",
    "turns",
    "tool_uses",
    "agent_peak_mb",
    "peak_cpu_pct",
    "model_footprint_mb",
    "repairs",
]


@dataclass(frozen=True, order=True)
class CellKey:
    agent: str
    model: str
    task: str


@dataclass
class CellPlan:
    key: CellKey
    rows: list[dict[str, str]]
    passed: int
    total: int


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def resolve_csv_path(path: Path) -> Path:
    if path.is_dir():
        path = path / "per-run.csv"
    if not path.exists():
        raise SystemExit(f"{path}: no such per-run.csv")
    return path


def read_rows(path: Path) -> tuple[list[str], list[dict[str, str]]]:
    with path.open(newline="", errors="replace") as f:
        reader = csv.DictReader(f)
        if not reader.fieldnames:
            raise SystemExit(f"{path}: empty CSV")
        rows = [{k: (v or "") for k, v in row.items()} for row in reader]
    return list(reader.fieldnames), rows


def parse_log_rows(path: Path) -> tuple[list[str], list[dict[str, str]]]:
    model = ""
    rows: list[dict[str, str]] = []
    cell_re = re.compile(r"^\s*\[(\w+)\]\s+(\w+)\s+([\d.]+)s(\s+\(RUN_TIMEOUT\))?\s+pass=(\d)")
    for line in path.read_text(errors="replace").splitlines():
        m = re.search(r"^=+ model:\s+(\S+)", line)
        if m:
            model = m.group(1).replace(",", "+")
            continue
        c = cell_re.match(line)
        if c and model:
            row = {name: "" for name in DEFAULT_HEADER}
            row.update(
                {
                    "agent": c.group(1),
                    "model": model,
                    "task": c.group(2),
                    "seconds": c.group(3),
                    "pass": c.group(5),
                    "timeout": "1" if c.group(4) else "0",
                }
            )
            rows.append(row)
    if not rows:
        raise SystemExit(f"{path}: no matrix cell rows found")
    return DEFAULT_HEADER, rows


def load_source(path: Path) -> tuple[Path, list[str], list[dict[str, str]]]:
    if path.is_dir() or path.name == "per-run.csv" or path.suffix == ".csv":
        csv_path = resolve_csv_path(path)
        header, rows = read_rows(csv_path)
        return csv_path, header, rows
    if not path.exists():
        raise SystemExit(f"{path}: no such input")
    header, rows = parse_log_rows(path)
    return path, header, rows


def write_rows(path: Path, header: list[str], rows: list[dict[str, str]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=header, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)


def row_key(row: dict[str, str]) -> CellKey:
    return CellKey(row.get("agent", ""), row.get("model", ""), row.get("task", ""))


def row_passed(row: dict[str, str]) -> bool:
    # `pass` is the verifier truth. Some agent CLIs return a non-zero rc after a valid answer
    # (notably greet/no-tool cells), and agentic.sh intentionally still records pass=1.
    return row.get("pass") == "1" and row.get("timeout") != "1"


def red_cell_plans(rows: list[dict[str, str]]) -> list[CellPlan]:
    by_key: dict[CellKey, list[dict[str, str]]] = {}
    order: list[CellKey] = []
    for row in rows:
        key = row_key(row)
        if key not in by_key:
            by_key[key] = []
            order.append(key)
        by_key[key].append(row)

    plans: list[CellPlan] = []
    for key in order:
        reps = by_key[key]
        passed = sum(1 for row in reps if row_passed(row))
        total = len(reps)
        if passed < total:
            plans.append(CellPlan(key=key, rows=reps, passed=passed, total=total))
    return plans


def model_csv_to_spec(model: str) -> str:
    # agentic.sh writes comma pipeline specs as '+' in CSV to keep rows comma-safe. Model ids do not
    # use '+mlx-community:' today, so this reverses the pipeline label without corrupting single models.
    return model.replace("+mlx-community:", ",mlx-community:")


def safe_name(text: str, limit: int = 120) -> str:
    safe = re.sub(r"[^A-Za-z0-9._-]+", "_", text).strip("_")
    return safe[:limit] or "cell"


def default_out_dir() -> Path:
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    return repo_root() / "scripts" / "bench" / "results" / f"rerun-reds-{stamp}"


def agentic_env(args: argparse.Namespace, cell: CellKey, out_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "AGENTIC_MODELS": model_csv_to_spec(cell.model),
            "AGENTS": cell.agent,
            "TASKS": cell.task,
            "REPS": str(args.reps),
            "KEEP": "1" if args.keep else "0",
            "REPAIR": str(args.repair),
            "BENCH_OUT": str(out_dir),
        }
    )
    if args.run_timeout is not None:
        env["RUN_TIMEOUT"] = str(args.run_timeout)
    if args.gen_timeout is not None:
        env["GEN_TIMEOUT"] = str(args.gen_timeout)
    if args.nctx is not None:
        env["NCTX"] = str(args.nctx)
    if args.bench_bin:
        env["BENCH_BIN"] = str(Path(args.bench_bin).resolve())
    if args.seed is not None:
        env["ROZUM_SAMPLING_SEED"] = str(args.seed)
    return env


def append_csv(src: Path, dst: Path, header: list[str]) -> list[dict[str, str]]:
    _, rows = read_rows(src)
    exists = dst.exists()
    with dst.open("a", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=header, extrasaction="ignore")
        if not exists:
            writer.writeheader()
        writer.writerows(rows)
    return rows


def merge_latest(
    original_rows: list[dict[str, str]],
    rerun_rows: list[dict[str, str]],
    rerun_keys: set[CellKey],
) -> list[dict[str, str]]:
    merged = [row for row in original_rows if row_key(row) not in rerun_keys]
    merged.extend(rerun_rows)
    return merged


def write_plan(path: Path, plans: list[CellPlan]) -> None:
    with path.open("w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["agent", "model", "task", "passed", "total"])
        for plan in plans:
            writer.writerow([plan.key.agent, plan.key.model, plan.key.task, plan.passed, plan.total])


def run_summary(csv_path: Path, out_dir: Path) -> None:
    summary = out_dir / "summary.txt"
    cmd = [sys.executable, str(repo_root() / "scripts" / "bench" / "summarize_matrix.py"), str(csv_path)]
    with summary.open("w") as f:
        subprocess.run(cmd, cwd=repo_root(), stdout=f, stderr=subprocess.STDOUT, check=False)
    print(f"summary: {summary}")


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("input", help="result dir or per-run.csv to inspect")
    p.add_argument("--out", type=Path, default=None, help="output dir (default: results/rerun-reds-<ts>)")
    p.add_argument("--dry-run", action="store_true", help="write plan/merged CSV without launching models")
    p.add_argument("--limit", type=int, default=None, help="rerun at most N red cells")
    p.add_argument("--reps", type=int, default=1, help="REPS for each rerun cell")
    p.add_argument("--keep", type=int, default=1, choices=[0, 1], help="KEEP workdirs for rerun cells")
    p.add_argument("--repair", type=int, default=1, help="REPAIR attempts passed to agentic.sh")
    p.add_argument("--run-timeout", type=int, default=None, help="RUN_TIMEOUT override")
    p.add_argument("--gen-timeout", type=int, default=None, help="GEN_TIMEOUT override")
    p.add_argument("--nctx", type=int, default=None, help="NCTX override")
    p.add_argument("--seed", default=1234, help="ROZUM_SAMPLING_SEED override; empty string disables")
    p.add_argument("--bench-bin", default=None, help="BENCH_BIN override")
    p.add_argument("--keep-going", action="store_true", help="continue when a cell command exits non-zero")
    p.add_argument("--no-summary", action="store_true", help="skip summarize_matrix.py")
    return p.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    source_input, header, original_rows = load_source(Path(args.input))
    for required in DEFAULT_HEADER[:3] + ["pass", "rc", "timeout"]:
        if required not in header:
            raise SystemExit(f"{source_input}: missing required column {required!r}")

    plans = red_cell_plans(original_rows)
    if args.limit is not None:
        plans = plans[: args.limit]

    out_dir = (args.out or default_out_dir()).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    source_snapshot = out_dir / "source-per-run.csv"
    write_rows(source_snapshot, header, original_rows)
    write_plan(out_dir / "rerun-plan.csv", plans)

    print(f"source: {source_input}")
    print(f"out:    {out_dir}")
    print(f"red cells: {len(plans)}")
    for i, plan in enumerate(plans, 1):
        print(f"  {i:02d}. {plan.key.agent} {plan.key.model} {plan.key.task} ({plan.passed}/{plan.total})")

    rerun_rows: list[dict[str, str]] = []
    rerun_keys = {plan.key for plan in plans}
    rerun_csv = out_dir / "rerun-per-run.csv"

    if not args.dry_run:
        for i, plan in enumerate(plans, 1):
            cell_dir = out_dir / f"{i:02d}-{safe_name(plan.key.agent)}-{safe_name(plan.key.task)}-{safe_name(plan.key.model)}"
            cmd = ["bash", "scripts/bench/agentic.sh"]
            print(f"\n[{i}/{len(plans)}] rerun {plan.key.agent} × {plan.key.model} × {plan.key.task}")
            print(f"  out: {cell_dir}")
            result = subprocess.run(cmd, cwd=repo_root(), env=agentic_env(args, plan.key, cell_dir))
            if result.returncode != 0 and not args.keep_going:
                raise SystemExit(result.returncode)
            cell_csv = cell_dir / "per-run.csv"
            if cell_csv.exists():
                rerun_rows.extend(append_csv(cell_csv, rerun_csv, header))
            elif not args.keep_going:
                raise SystemExit(f"{cell_csv}: missing after rerun")

    merged_rows = merge_latest(original_rows, rerun_rows, rerun_keys if rerun_rows else set())
    merged_csv = out_dir / "merged-per-run.csv"
    write_rows(merged_csv, header, merged_rows)

    print(f"\nplan:   {out_dir / 'rerun-plan.csv'}")
    if rerun_rows:
        print(f"rerun:  {rerun_csv}")
    print(f"merged: {merged_csv}")
    if not args.no_summary:
        run_summary(merged_csv, out_dir)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
