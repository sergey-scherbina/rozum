#!/usr/bin/env python3
"""rag-eval runner: ask a question set through a live rozum MCP endpoint, score the ranks.

  run.py <questions.json> <project-root> [--url http://127.0.0.1:8779/mcp] [--k 10]

A hit answers a question when its id's path matches `path` (substring; empty = any) and its
text matches the `def` regex. Prints each question's rank and time, then top-1, top-5 and MRR —
and, for sets with paths, the same three for FILE rank (the first hit from the right file, any
chunk of it): a pointer to the right file is most of what an agent needs, and the gap between
the two says whether a miss is the wrong file or the wrong chunk of the right one.
One MCP session for the whole set, so the numbers are what an agent's session sees.
"""
import argparse, json, re, sys, time, urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("questions"); ap.add_argument("project")
ap.add_argument("--url", default="http://127.0.0.1:8779/mcp"); ap.add_argument("--k", type=int, default=10)
a = ap.parse_args()
U = f"{a.url}?project={a.project}"
H = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}

def post(body, sid=None):
    h = dict(H)
    if sid: h["Mcp-Session-Id"] = sid
    r = urllib.request.urlopen(urllib.request.Request(U, json.dumps(body).encode(), h), timeout=120)
    return r.headers.get("mcp-session-id"), r.read().decode()

sid, _ = post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rag-eval", "version": "0"}}})
post({"jsonrpc": "2.0", "method": "notifications/initialized"}, sid)
qs = json.load(open(a.questions))["questions"]
top1 = top5 = 0; rr = 0.0; f1 = f5 = 0; frr = 0.0
for i, q in enumerate(qs):
    t = time.time()
    _, raw = post({"jsonrpc": "2.0", "id": 10 + i, "method": "tools/call", "params": {"name": "rag.search", "arguments": {"query": q["q"], "top_k": a.k}}}, sid)
    dt = time.time() - t
    res = json.loads(json.loads(next(l[6:] for l in raw.splitlines() if l.startswith("data: {")))["result"]["content"][0]["text"])["results"]
    rank = next((n + 1 for n, h in enumerate(res) if q["path"] in h["id"].split("#")[0] and re.search(q["def"], h["text"])), None)
    top1 += rank == 1; top5 += bool(rank and rank <= 5); rr += 1 / rank if rank else 0
    frank = next((n + 1 for n, h in enumerate(res) if q["path"] and q["path"] in h["id"].split("#")[0]), None)
    f1 += frank == 1; f5 += bool(frank and frank <= 5); frr += 1 / frank if frank else 0
    first = res[0]["id"] if res else "-"
    print(f"{('miss' if rank is None else rank):>4} {('-' if frank is None else frank):>3}f  {dt:4.1f}s  {q['q'][:70]:70s}  first: {first[:60]}")
urllib.request.urlopen(urllib.request.Request(U, method="DELETE", headers={"Mcp-Session-Id": sid}))
n = len(qs)
print(f"\ntop-1 {top1}/{n}   top-5 {top5}/{n}   MRR {rr / n:.3f}")
if any(q["path"] for q in qs):
    print(f"file: top-1 {f1}/{n}   top-5 {f5}/{n}   MRR {frr / n:.3f}")
