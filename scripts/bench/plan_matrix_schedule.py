#!/usr/bin/env python3
"""Plan a RAM-aware matrix model order using `rozum-gateway gateway --dry-run`.

This does not load model weights. It asks the gateway's existing admission math whether each model
would load at the requested context, then orders admitted models by estimated footprint.

Examples:
  scripts/bench/plan_matrix_schedule.py --models "$MODELS" --nctx 8192
  MODELS="$(scripts/bench/plan_matrix_schedule.py --models "$MODELS" --nctx 8192 --emit-models)"
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass, asdict
from datetime import datetime, timezone
from pathlib import Path


DEFAULT_MODELS = (
    "mlx-community:Qwen3.6-35B-A3B-4bit-DWQ "
    "mlx-community:GLM-4-32B-0414-4bit,mlx-community:gpt-oss-20b-MXFP4-Q4"
)


@dataclass
class ModelPlan:
    model: str
    verdict: str
    footprint_gib: float | None
    command_rc: int
    dry_run_excerpt: str


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def default_bin() -> Path:
    root = repo_root()
    for candidate in [root / "target" / "release" / "rozum-gateway", root / "target" / "debug" / "rozum-gateway"]:
        if candidate.exists():
            return candidate
    return root / "target" / "release" / "rozum-gateway"


def split_models(models: str) -> list[str]:
    return [m for m in models.split() if m.strip()]


def parse_footprint_gib(text: str) -> float | None:
    m = re.search(r"est\. footprint:\s+([\d.]+)\s+GiB", text)
    return float(m.group(1)) if m else None


def parse_verdict(text: str, rc: int) -> str:
    if "WOULD LOAD" in text:
        return "load"
    if "WOULD REFUSE" in text:
        return "refuse"
    if rc != 0:
        return "error"
    return "unknown"


def dry_run_model(bin_path: Path, model: str, nctx: int | None) -> ModelPlan:
    cmd = [str(bin_path), "gateway", "--model", model, "--dry-run"]
    if nctx is not None:
        cmd.extend(["--n-ctx", str(nctx)])
    proc = subprocess.run(cmd, cwd=repo_root(), text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    text = proc.stdout
    return ModelPlan(
        model=model,
        verdict=parse_verdict(text, proc.returncode),
        footprint_gib=parse_footprint_gib(text),
        command_rc=proc.returncode,
        dry_run_excerpt="\n".join(text.splitlines()[:40]),
    )


def sort_plans(plans: list[ModelPlan]) -> list[ModelPlan]:
    order = {"load": 0, "unknown": 1, "refuse": 2, "error": 3}
    return sorted(plans, key=lambda p: (order.get(p.verdict, 9), p.footprint_gib or 10**9, p.model))


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--models", default=os.environ.get("MODELS", DEFAULT_MODELS), help="space-separated model specs")
    p.add_argument("--nctx", type=int, default=None, help="context window for dry-run admission")
    p.add_argument("--bench-bin", type=Path, default=default_bin(), help="rozum-gateway binary")
    p.add_argument("--out", type=Path, default=None, help="write JSON plan")
    p.add_argument("--emit-models", action="store_true", help="print only admitted/unknown model specs in planned order")
    return p.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    models = split_models(args.models)
    if not models:
        raise SystemExit("no models")
    if not args.bench_bin.exists():
        raise SystemExit(f"{args.bench_bin}: missing; build rozum-gateway first")

    plans = sort_plans([dry_run_model(args.bench_bin, model, args.nctx) for model in models])
    if args.emit_models:
        runnable = [p.model for p in plans if p.verdict in ("load", "unknown")]
        print(" ".join(runnable))
        return 0 if runnable else 2

    data = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "nctx": args.nctx,
        "summary": dict(sorted(Counter(p.verdict for p in plans).items())),
        "models": [asdict(p) for p in plans],
    }
    rendered = json.dumps(data, indent=2, sort_keys=True) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered)
        print(f"wrote {args.out}")
    else:
        print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
