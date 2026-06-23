#!/usr/bin/env python3
"""Parse a full-matrix stdout log into model × agent × task → result · time · REASON.
Usage: summarize_matrix.py /tmp/full_matrix-<stamp>.log"""
import re, sys

log = sys.argv[1] if len(sys.argv) > 1 else "/dev/stdin"
lines = open(log, errors="replace").read().splitlines()

model = None
refused = {}          # model -> True if gateway never came up
cells = []            # (model, agent, task, secs, pass, [reasons])
cur = None
cell_re = re.compile(r"^\s*\[(\w+)\]\s+(\w+)\s+([\d.]+)s(\s+\(RUN_TIMEOUT\))?\s+pass=(\d)")
detail_re = re.compile(r"^\s+(PASS|FAIL)\s+(.*)$")

for ln in lines:
    m = re.search(r"^=+ model:\s+(\S+)", ln)
    if m:
        model = m.group(1).split(":")[-1]
        refused.setdefault(model, False)
        cur = None
        continue
    if "gateway not ready" in ln and model:
        refused[model] = True
        continue
    c = cell_re.match(ln)
    if c:
        cur = [model, c.group(1), c.group(2), c.group(3), c.group(5), bool(c.group(4)), []]
        cells.append(cur)
        continue
    d = detail_re.match(ln)
    if d and cur is not None:
        cur[6].append(f"{d.group(1)} {d.group(2)}")

# Report
order_t = ["greet", "build", "fix", "test", "debug"]
models = list(refused.keys())
print("=" * 78)
for mdl in models:
    if refused.get(mdl) and not any(c[0] == mdl for c in cells):
        print(f"\n■ {mdl}:  ⛔ NOT LOADED (gateway refused/failed to start — RAM admission or load error).")
        print("   → every cell for this model was skipped (a clean refusal, not a crash).")
        continue
    print(f"\n■ {mdl}")
    rows = [c for c in cells if c[0] == mdl]
    rows.sort(key=lambda c: (c[1], order_t.index(c[2]) if c[2] in order_t else 9))
    npass = sum(1 for c in rows if c[4] == "1")
    print(f"   {npass}/{len(rows)} green")
    for _m, agent, task, secs, ok, tmo, reasons in rows:
        mark = "✅" if ok == "1" else "❌"
        why = ""
        if ok != "1":
            fails = [r for r in reasons if r.startswith("FAIL")] or reasons
            why = "  ·  " + "; ".join(fails)
            if tmo:
                why += "  [hit RUN_TIMEOUT]"
        print(f"   {mark} {agent:9s} {task:6s} {secs:>7s}s{why}")
print("\n" + "=" * 78)
tot = len(cells); g = sum(1 for c in cells if c[4] == "1")
print(f"TOTAL ran: {g}/{tot} green   ({sum(1 for v in refused.values() if v)} model(s) not loaded)")
