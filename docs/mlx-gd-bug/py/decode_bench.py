"""Apples-to-apples Python decode bench, mirroring the Rust mlx_qwen35_prefill_bench:
512 synthetic-token prefill, then 64 greedy decode steps, measured serial vs pipelined.
Run: /tmp/mlxoracle/bin/python docs/mlx-gd-bug/py/decode_bench.py
"""
import time, glob, os, sys
import mlx.core as mx
from mlx_lm.utils import load

# Resolve the same HF-cache model the Rust bench uses. Arg picks the repo.
repo = sys.argv[1] if len(sys.argv) > 1 else "mlx-community--Qwen3.6-27B-4bit"
cands = glob.glob(os.path.expanduser(
    f"~/.cache/huggingface/hub/models--{repo}/snapshots/*"))
assert cands, f"{repo} not in HF cache"
model_path = cands[0]
print("model:", model_path)

model, tokenizer = load(model_path)

def synth(n):
    return mx.array([[1000 + i % 5000 for i in range(n)]])

STEPS = 64

def make_cache():
    from mlx_lm.models.cache import make_prompt_cache
    return make_prompt_cache(model)

# Context-growth probe (mirror of the Rust ROZUM_CTXSWEEP path).
if os.environ.get("CTXSWEEP"):
    prompt = synth(8)
    cache = make_cache()
    y = model(prompt, cache=cache)[:, -1, :]
    mx.eval(y)
    total, win = 1024, 64
    t = time.time()
    for s in range(total):
        inp = mx.argmax(y, axis=-1, keepdims=True)
        y = model(inp, cache=cache)[:, -1, :]
        mx.eval(y)
        if (s + 1) % win == 0:
            print(f"CTX ~{s+1:>5}: {win/(time.time()-t):6.1f} t/s")
            t = time.time()
    raise SystemExit

for n in (128, 512):
    prompt = synth(n)

    # Prefill (timed once)
    cache = make_cache()
    t = time.time()
    logits = model(prompt, cache=cache)
    mx.eval(logits)
    prefill = time.time() - t

    # --- Serial decode: forward, eval(block), repeat ---
    y = logits[:, -1, :]
    cache_s = make_cache()
    mx.eval(model(prompt, cache=cache_s))  # re-prime
    serial_ids = []
    td = time.time()
    for _ in range(STEPS):
        inp = mx.argmax(y, axis=-1, keepdims=True)
        serial_ids.append(inp)
        y = model(inp, cache=cache_s)[:, -1, :]
        mx.eval(y)
    serial = STEPS / (time.time() - td)
    serial_ids = [int(x.item()) for x in serial_ids]

    # --- Pipelined decode: async_eval next before blocking on current ---
    cache_p = make_cache()
    cur = model(prompt, cache=cache_p)[:, -1, :]
    mx.async_eval(cur)
    pipe_ids = []
    tp = time.time()
    for _ in range(STEPS):
        inp = mx.argmax(cur, axis=-1, keepdims=True)
        pipe_ids.append(inp)
        nxt = model(inp, cache=cache_p)[:, -1, :]
        mx.async_eval(nxt)
        mx.eval(cur)
        cur = nxt
    pipelined = STEPS / (time.time() - tp)
    pipe_ids = [int(x.item()) for x in pipe_ids]

    match = "MATCH" if serial_ids == pipe_ids else "DIVERGE"
    print(f"BENCH n={n:>4}  prefill={prefill:6.2f}s ({n/prefill:6.1f} tok/s)  "
          f"decode serial={serial:5.1f}  pipelined={pipelined:5.1f} t/s  "
          f"({pipelined/serial:.2f}x)  serial==pipe:{match}")
    print(f"SERIAL_IDS   n={n}: {serial_ids[:24]}")
    print(f"PIPELINE_IDS n={n}: {pipe_ids[:24]}")
