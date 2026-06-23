#!/usr/bin/env python3
"""Parse a planner's solution and WRITE each file — the deterministic forward-output handoff of the
planner→executor pipeline. The planner is asked to emit a STRUCTURED format (robust, not free markdown):

    === FILE: <path> ===
    <full raw file contents>
    === END ===

For create-from-scratch the bottleneck is landing CORRECT code reliably, not an agent loop; the planner
(GLM) supplies correct code, this lands it byte-exact. Falls back to ```fenced blocks if no FILE markers.
Usage: write_solution.py <target_dir> <solution_file>"""
import os, re, sys

target, text = sys.argv[1], open(sys.argv[2]).read()
files = []  # (path, content)

# Primary: structured `=== FILE: path ===  …  === END ===` (or up to the next FILE marker / EOF).
parts = re.split(r"(?m)^\s*===\s*FILE:\s*(.+?)\s*===\s*$", text)
if len(parts) > 1:
    # parts = [preamble, path1, body1, path2, body2, …]
    for i in range(1, len(parts), 2):
        path = parts[i].strip().strip("`\"'")
        body = parts[i + 1] if i + 1 < len(parts) else ""
        body = re.sub(r"(?m)^\s*===\s*END\s*===\s*$.*", "", body, count=1, flags=re.S)
        files.append((path, body.strip("\n") + "\n"))
else:
    # Fallback: a `path` line (or a bare ``` fence holding just the path) then a ```lang block.
    lines = text.splitlines()
    pending_path = None
    i = 0
    while i < len(lines):
        fence = re.match(r"^\s*```(\w*)\s*$", lines[i])
        if fence:
            body, i = [], i + 1
            while i < len(lines) and not re.match(r"^\s*```\s*$", lines[i]):
                body.append(lines[i]); i += 1
            i += 1  # consume closing fence
            joined = "\n".join(body).strip()
            if len(body) == 1 and re.search(r"\.\w+$", joined) and " " not in joined:
                pending_path = joined  # a fence that just names the next file
            elif pending_path:
                files.append((pending_path, "\n".join(body) + "\n")); pending_path = None
            continue
        t = lines[i].strip().strip("`*:#").strip().strip('"').strip("'")
        if t and re.search(r"\.\w+$", t) and " " not in t and len(t) < 80:
            pending_path = t
        i += 1

written = []
for path, content in files:
    if not re.search(r"\.(rs|toml|md|txt|json|lock|cfg|py|sh|yaml|yml)$", path):
        continue
    full = os.path.join(target, path.lstrip("/"))
    os.makedirs(os.path.dirname(full) or target, exist_ok=True)
    with open(full, "w") as f:
        f.write(content if content.endswith("\n") else content + "\n")
    written.append(path)
print("\n".join(written) if written else "(no files parsed)")
