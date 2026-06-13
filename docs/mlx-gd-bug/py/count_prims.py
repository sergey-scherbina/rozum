"""Count MLX graph primitives in ONE decode-step (T=1) for a model.
Prefill a few tokens, make everything concrete, then build a single decode step
lazily and export its graph. Primitive count = the work `eval` dispatches per token.
Run: /tmp/mlxoracle/bin/python count_prims.py <repo>
"""
import sys, glob, os, re
import mlx.core as mx
from mlx_lm.utils import load
from mlx_lm.models.cache import make_prompt_cache

repo = sys.argv[1] if len(sys.argv) > 1 else "mlx-community--Qwen3.6-35B-A3B-4bit"
path = glob.glob(os.path.expanduser(f"~/.cache/huggingface/hub/models--{repo}/snapshots/*"))[0]
model, _ = load(path)

cache = make_prompt_cache(model)
prompt = mx.array([[1000 + i for i in range(8)]])
y = model(prompt, cache=cache)
# Make prefill + all cache state concrete so the next step's graph is single-token.
leaves = [y]
for c in cache:
    leaves += [s for s in getattr(c, "state", []) if isinstance(s, mx.array)]
mx.eval(leaves)

inp = mx.array([[123]])
y2 = model(inp, cache=cache)[:, -1, :]

dot = "/tmp/step.dot"
with open(dot, "w") as f:
    mx.export_to_dot(f, y2)

text = open(dot).read()
# DOT: each node line is `<id> [label="..."];`. Primitives carry an op label;
# input arrays are labelled "array" (or empty). Count by label.
labels = re.findall(r'\[label\s*="([^"]*)"', text)
from collections import Counter
cnt = Counter(labels)
prim = sum(n for l, n in cnt.items() if l and l.lower() != "array")
arr = sum(n for l, n in cnt.items() if not l or l.lower() == "array")
print(f"{repo}: total_nodes={len(labels)}  primitives={prim}  arrays={arr}")
print("top primitives:", cnt.most_common(20))
