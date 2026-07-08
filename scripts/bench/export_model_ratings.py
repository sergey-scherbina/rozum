#!/usr/bin/env python3
"""Export per-model star ratings for the UCC model pickers from real matrix results.

Reads matrix-like non-archived scripts/bench/results/*/per-run.csv, aggregates the
CLAUDE-driver pass-rate per model (the capability headline — see
summarize_matrix.py's matrix-hygiene rationale: drivers differ ~2x, so blending
them is misleading), and writes ~/.rozum/ucc/model-ratings.json which
control-serve consults before its built-in static table.

Honesty rules (mirrors summarize_matrix.py):
  - claude driver only — the capability headline, not a driver blend;
  - rc=2 rows (infra/gateway-not-ready) are excluded — integration failures,
    not capability signal;
  - `greet` rows are excluded — greet measures liveness, not coding capability,
    and inflates probe-tier models (the known 7B cliff clears only greet);
  - narrow diagnostic/slice result dirs are excluded by default — they are for
    repro/verification and otherwise stale old failures keep depressing a model
    after the gateway or parser bug has been fixed;
  - a model needs >= MIN_RUNS counted rows to be rated at all;
  - stars = 5 (>=0.90), 4 (>=0.70), 3 (>=0.50), 2 (>=0.25), 1 (>0 or 0).

Usage:
  python3 scripts/bench/export_model_ratings.py                 # scan + write
  python3 scripts/bench/export_model_ratings.py --dry-run       # print, don't write
  python3 scripts/bench/export_model_ratings.py --include-slices # include narrow diag/slice dirs
  python3 scripts/bench/export_model_ratings.py <results-dir>   # explicit results dir
    (per-run.csv artifacts are local-only, so run from the checkout that ran the
     matrix — or pass its results dir explicitly)
"""
from __future__ import annotations

import csv
import json
import sys
import time
from collections import defaultdict
from pathlib import Path

MIN_RUNS = 5
MIN_RESULT_TASKS = 4
RESULTS = Path(__file__).resolve().parent / "results"
OUT = Path.home() / ".rozum/ucc/model-ratings.json"


def stars_for(rate: float) -> int:
    if rate >= 0.90:
        return 5
    if rate >= 0.70:
        return 4
    if rate >= 0.50:
        return 3
    if rate >= 0.25:
        return 2
    return 1


def read_rows(path: Path) -> list[dict[str, str]]:
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def is_matrix_like(rows: list[dict[str, str]]) -> bool:
    tasks = {
        (row.get("task") or "").strip()
        for row in rows
        if (row.get("task") or "").strip() and (row.get("task") or "").strip() != "greet"
    }
    return len(tasks) >= MIN_RESULT_TASKS


def main() -> int:
    dry = "--dry-run" in sys.argv
    include_slices = "--include-slices" in sys.argv
    dirs = [a for a in sys.argv[1:] if not a.startswith("--")]
    results = Path(dirs[0]) if dirs else RESULTS
    agg: dict[str, list[int]] = defaultdict(lambda: [0, 0])  # model -> [pass, runs]
    csvs = sorted(results.glob("*/per-run.csv")) if results.is_dir() else []
    csvs = [p for p in csvs if not p.parent.name.startswith("_archive")]
    included = 0
    skipped_slices = 0
    for p in csvs:
        try:
            rows = read_rows(p)
            if not include_slices and not is_matrix_like(rows):
                skipped_slices += 1
                continue
            included += 1
            for row in rows:
                if (row.get("agent") or "").strip() != "claude":
                    continue
                if (row.get("rc") or "").strip() == "2":
                    continue  # infra, not capability
                if (row.get("task") or "").strip() == "greet":
                    continue  # liveness probe, not coding capability
                model = (row.get("model") or "").strip()
                if not model:
                    continue
                agg[model][1] += 1
                if (row.get("pass") or "").strip() == "1":
                    agg[model][0] += 1
        except Exception as e:  # a malformed CSV must not kill the export
            print(f"  (skip {p}: {e})", file=sys.stderr)
    models = {}
    for model, (passed, runs) in sorted(agg.items()):
        if runs < MIN_RUNS:
            continue
        rate = passed / runs
        models[model] = {
            "stars": stars_for(rate),
            "pass": passed,
            "runs": runs,
            "rate": round(rate, 3),
        }
    doc = {
        "generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "source": "scripts/bench/export_model_ratings.py (claude driver, rc!=2, matrix-like non-archived results)",
        "inputs": {
            "included_result_dirs": included,
            "skipped_slice_result_dirs": skipped_slices,
            "include_slices": include_slices,
            "min_result_tasks": MIN_RESULT_TASKS,
        },
        "models": models,
    }
    body = json.dumps(doc, indent=1, ensure_ascii=False)
    if dry:
        print(body)
    else:
        OUT.parent.mkdir(parents=True, exist_ok=True)
        OUT.write_text(body)
        print(
            f"wrote {OUT} ({len(models)} rated models from {included} result dirs"
            f", skipped {skipped_slices} slices)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
