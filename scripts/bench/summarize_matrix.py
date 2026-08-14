#!/usr/bin/env python3
"""Summarize full agentic-matrix output — HONESTLY.

Usage:
  summarize_matrix.py /tmp/full_matrix-<stamp>.log
  summarize_matrix.py scripts/bench/results/full-matrix-<stamp>/per-run.csv
  summarize_matrix.py scripts/bench/results/full-matrix-<stamp>

Why this exists / what changed (2026-07-05, matrix-hygiene):
  A blended "TOTAL X/Y green" is MISLEADING when a run mixes three very different
  things into one average:
    1. CAPABLE tier   — the curated agentic coders we actually ship/measure.
    2. PROBE tier      — small / experimental models kept for context; most only
                          clear `greet` (the known 7B→27B capability cliff).
    3. BROKEN backends — a spec the gateway cannot even serve (e.g. an `ollama:`
                          model with no ollama running) fails every cell with
                          rc=1 (agent error) or rc=2 (infra). That is an
                          INTEGRATION failure, NOT a capability signal, and it
                          must never drag the headline down as if the model
                          "scored 20%".
  It is also misleading to average the three DRIVERS (claude / codex / opencode)
  into one number — they differ by ~2x. So the honest read is:
    • CAPABILITY headline = claude × the curated tier only.
    • DRIVERS compared over the curated tier, side by side.
    • BROKEN backends listed and EXCLUDED from every rate.
    • PROBE models shown for context, separately.
"""
from __future__ import annotations

import csv
import re
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

TASK_ORDER = {name: i for i, name in enumerate(["greet", "build", "fix", "test", "debug", "rpn"])}

# The curated CAPABLE tier — keep in sync with DEFAULT_MODELS in agentic.sh. A model
# is "capable tier" if its display name contains any of these substrings. These are the
# models whose score is a real agentic-capability signal; the headline is computed over
# them (claude driver). Everything else is PROBE (context only) unless it is BROKEN.
CAPABLE_SUBSTRINGS = (
    "Qwen3.6-35B-A3B",
    "Qwen3-Coder-30B-A3B",
    "Devstral-Small-2507",
    "GLM-4.7-Flash",
    "GLM-4-32B-0414",
    "gpt-oss-20b",
)

# rc codes that mean "this was NOT a capability measurement":
#   1 = agent error (tool error / segfault / the runner itself failed)
#   2 = infra failure (gateway crash / clients_gone)
# A model whose non-greet cells are DOMINATED by these is a broken/unsupported backend.
NON_CAPABILITY_RC = {"1", "2"}
BROKEN_RC_SHARE = 0.5  # >= this share of cells in {1,2} → BROKEN (for non-curated models)


def display_model(model: str) -> str:
    return model.replace("mlx-community:", "")


def is_capable(model: str) -> bool:
    return any(s in model for s in CAPABLE_SUBSTRINGS)


def cell(
    model: str,
    agent: str,
    task: str,
    seconds: str,
    passed: str,
    timeout: bool = False,
    reasons: list[str] | None = None,
    rc: str | None = None,
) -> dict[str, Any]:
    return {
        "model": display_model(model),
        "agent": agent,
        "task": task,
        "seconds": float(seconds or 0),
        "pass": passed == "1",
        "timeout": timeout,
        "reasons": reasons or [],
        "rc": rc,
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
                    rc=row.get("rc"),
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


def classify(model: str, rows: list[dict[str, Any]]) -> str:
    """Return 'capable' | 'probe' | 'broken'.

    Curated models are always 'capable' (their infra flakes are surfaced but they are
    never demoted). Any other model whose cells are dominated by rc∈{1,2} is 'broken'
    (unsupported/crashing backend — not a capability measurement). The rest are 'probe'.
    """
    if is_capable(model):
        return "capable"
    if rows:
        bad = sum(1 for r in rows if r.get("rc") in NON_CAPABILITY_RC)
        if bad / len(rows) >= BROKEN_RC_SHARE:
            return "broken"
    return "probe"


def measured(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """The cells that actually MEASURED the model — everything except `NON_CAPABILITY_RC`.

    A cell where the agent process died (rc=1) or the gateway crashed (rc=2) says nothing about the
    model, and counting it as a miss is not a small distortion. Measured 2026-08-15 on this
    repository's own results: reading `pass` alone put the resident model at 73/88 with `fix` at
    10/18, and the true figure is 51/52 — fourteen of the fifteen "failures" were an agent error, an
    infra crash or a timeout, most of them against a model build deleted a month earlier. The
    conclusion drawn from the wrong number was "the model is weak at editing code", which is exactly
    the mis-attribution `NON_CAPABILITY_RC` exists to prevent.

    The constant and the promise were already here — the module docstring says broken backends are
    "EXCLUDED from every rate" — but `rate` counted every row, so the exclusion only ever applied to
    whole models, never to a bad cell inside a good one.

    A PASSING cell is never excluded, whatever its `rc`. That is a fact about the data, not caution:
    13 of the 105 rows in `agentic-ucc-1782922643` are `pass=1` carrying `rc=1`, from before the
    structured codes existed and that column meant something else. A cell that passed measured the
    model by definition, so this rule only ever removes a false NEGATIVE — excluding historical green
    cells would have quietly restated every old number, the same class of error this exists to fix.
    """
    return [r for r in rows if r["pass"] or r.get("rc") not in NON_CAPABILITY_RC]


def rate(rows: list[dict[str, Any]]) -> tuple[int, int]:
    m = measured(rows)
    return sum(1 for r in m if r["pass"]), len(m)


def pct(p: int, t: int) -> str:
    return f"{100 * p / t:3.0f}%" if t else "  -%"


def summarize(cells: list[dict[str, Any]], refused: dict[str, bool]) -> None:
    models = list(refused.keys())
    for c in cells:
        if c["model"] not in models:
            models.append(c["model"])

    tier: dict[str, str] = {m: classify(m, [c for c in cells if c["model"] == m]) for m in models}
    capable_models = [m for m in models if tier[m] == "capable"]
    probe_models = [m for m in models if tier[m] == "probe"]
    broken_models = [m for m in models if tier[m] == "broken"]

    agents = sorted({c["agent"] for c in cells})
    infra_rc = sum(1 for c in cells if c.get("rc") in NON_CAPABILITY_RC)
    to = sum(1 for c in cells if c["timeout"])

    # ── HONEST HEADLINE ───────────────────────────────────────────────────────
    print("=" * 78)
    print("AGENTIC MATRIX — honest read (capability tier ≠ probe ≠ broken backend)")
    print("=" * 78)

    cap_claude = [c for c in cells if tier[c["model"]] == "capable" and c["agent"] == "claude"]
    p, t = rate(cap_claude)
    print(f"\n▶ CAPABILITY  (claude × curated tier)   {p}/{t}  {pct(p, t)}   ← the headline")
    # Printed even when zero. An exclusion nobody can see is the same trap one line further on:
    # the reader cannot tell "51/52" from "51/52, and 14 cells were dropped".
    dropped = len(cap_claude) - t
    print(f"    excluded from it: {dropped} cell(s) that did not measure the model "
          f"(rc {'/'.join(sorted(NON_CAPABILITY_RC))} — agent error / infra), of "
          f"{len(cap_claude)} run")
    for m in capable_models:
        rows = [c for c in cells if c["model"] == m and c["agent"] == "claude"]
        if not rows:
            continue
        mp, mt = rate(rows)
        # per-task mini-breakdown, ordered
        bytask = defaultdict(list)
        for r in rows:
            bytask[r["task"]].append(r)
        cells_str = " ".join(
            f"{tk}:{rate(bytask[tk])[0]}/{rate(bytask[tk])[1]}"
            for tk in sorted(bytask, key=lambda x: TASK_ORDER.get(x, 99))
        )
        print(f"    {mp:>2}/{mt:<2} {pct(mp, mt)}  {m:34s} {cells_str}")

    # DRIVERS over the curated tier (the fair driver comparison)
    print("\n▶ DRIVERS  (over curated tier only)  —  fail modes: "
          "deliver=wrote no files (rc11), partial=manifest without src (rc12), "
          "untouched=seeded tree unchanged (rc13), "
          "wrong=verify red (rc10), timeout (124), broke=runner/infra (rc1/2)")
    for a in agents:
        rows = [c for c in cells if tier[c["model"]] == "capable" and c["agent"] == a]
        p, t = rate(rows)
        fails = [c for c in rows if not c["pass"]]
        # rc is mutually exclusive per agentic.sh (tmo→124, rc2→2, pass→0, non-zero→raw, no-files→11,
        # manifest-without-src→12, else→10), so these buckets partition the failures with no
        # double-count. `partial` is counted SEPARATELY from `deliver` rather than added to it: both
        # are delivery-shaped, but "wrote nothing" and "wrote half" point at different fixes, and
        # summing them would hide which one a driver actually does.
        deliver = sum(1 for c in fails if c.get("rc") == "11")
        partial = sum(1 for c in fails if c.get("rc") == "12")
        untouched = sum(1 for c in fails if c.get("rc") == "13")
        wrong = sum(1 for c in fails if c.get("rc") == "10")
        tout = sum(1 for c in fails if c.get("rc") == "124" or c["timeout"])
        broke = sum(1 for c in fails if c.get("rc") in NON_CAPABILITY_RC)
        other = len(fails) - deliver - partial - untouched - wrong - tout - broke
        modes = [f"{lbl} {n}" for lbl, n in
                 (("deliver", deliver), ("partial", partial), ("untouched", untouched),
                  ("wrong", wrong), ("timeout", tout), ("broke", broke), ("other", other))
                 if n]
        modestr = ("   fails: " + ", ".join(modes)) if modes else ""
        print(f"    {a:9s} {p:>3}/{t:<3} {pct(p, t)}{modestr}")

    # BROKEN backends — excluded from every rate above
    if broken_models:
        print("\n▶ BROKEN / unsupported backends  (EXCLUDED from rates — not a capability signal)")
        for m in broken_models:
            rows = [c for c in cells if c["model"] == m]
            bad = sum(1 for r in rows if r.get("rc") in NON_CAPABILITY_RC)
            rcs = defaultdict(int)
            for r in rows:
                if r.get("rc") in NON_CAPABILITY_RC:
                    rcs[r["rc"]] += 1
            rcstr = ", ".join(f"{n}× rc={k}" for k, n in sorted(rcs.items()))
            print(f"    ⛔ {m:34s} {bad}/{len(rows)} cells failed at the runner/gateway ({rcstr})")
        print("    → fix or drop these specs from the matrix selection; they measure integration, not skill.")

    # PROBE models — context only
    if probe_models:
        print("\n▶ PROBE / small models  (context only — most clear `greet` and little else)")
        for m in probe_models:
            rows = [c for c in cells if c["model"] == m and c["agent"] == "claude"]
            p, t = rate(rows)
            ng_p, ng_t = rate([r for r in rows if r["task"] != "greet"])
            print(f"    {m:34s} claude {p}/{t} {pct(p, t)}  (non-greet {ng_p}/{ng_t})")

    # ── PER-MODEL DETAIL (unchanged, still useful for drill-down) ──────────────
    print("\n" + "=" * 78)
    print("PER-MODEL DETAIL")
    for model in models:
        rows = [c for c in cells if c["model"] == model]
        if refused.get(model) and not rows:
            print(f"\n■ {model}:  ⛔ NOT LOADED (gateway refused/failed to start)")
            print("   every cell for this model was skipped")
            continue
        badge = {"capable": "", "probe": "  [probe]", "broken": "  [BROKEN backend]"}[tier[model]]
        print(f"\n■ {model}{badge}")
        npass, ntot = rate(rows)
        print(f"   {npass}/{ntot} green")

        grouped: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
        for row in rows:
            grouped[(row["agent"], row["task"])].append(row)
        for (agent, task), reps in sorted(
            grouped.items(), key=lambda item: (item[0][0], TASK_ORDER.get(item[0][1], 99), item[0][1])
        ):
            passed, total = rate(reps)
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

    # ── TIERED FOOTER ──────────────────────────────────────────────────────────
    print("\n" + "=" * 78)
    cap_p, cap_t = rate([c for c in cells if tier[c["model"]] == "capable"])
    all_p, all_t = rate(cells)
    not_loaded = sum(
        1
        for model, is_refused in refused.items()
        if is_refused and not any(c["model"] == model for c in cells)
    )
    print(f"CAPABILITY tier (all drivers): {cap_p}/{cap_t} {pct(cap_p, cap_t)}   "
          f"|  claude-only: " + pct(*rate(cap_claude)))
    print(f"ALL runs incl. probe/broken:   {all_p}/{all_t} {pct(all_p, all_t)}  "
          f"← do NOT quote this as 'capability'")
    if broken_models:
        print(f"  ⚠ {len(broken_models)} broken backend(s) excluded from the capability rate: "
              + ", ".join(broken_models))
    print(f"  infra/runner-error cells (rc∈{{1,2}}): {infra_rc}   timeouts: {to}   "
          f"models not loaded: {not_loaded}")


def main() -> None:
    arg = sys.argv[1] if len(sys.argv) > 1 else "-"
    summarize(*read_input(arg))


if __name__ == "__main__":
    main()
