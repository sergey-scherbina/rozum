#!/usr/bin/env python
"""Does the bug reproduce in PURE Python at real-model scale, SERIAL, no per-call
eval? Loads the real Qwen3.6-27B via mlx_lm (for weights/model only), then runs MY
OWN decode loop — exactly our Rust pattern: forward -> eval(logits) -> argmax ->
repeat. NO per-call eval (mlx_lm's gated_delta has none), NO pipelining (no
async_eval). If this diverges from the known-correct reference, Python SERIAL also
breaks (⇒ mlx_lm is correct only because it PIPELINES). If it stays correct, Python
genuinely differs from our Rust.
"""
import sys
import mlx.core as mx
from mlx_lm import load

REPO = "mlx-community/Qwen3.6-27B-4bit"
print(f"loading {REPO} ...", flush=True)
model, tokenizer = load(REPO)

prompt = "What is the capital of France? Reply in one short sentence. /no_think"
msgs = [{"role": "user", "content": prompt}]
ids = tokenizer.apply_chat_template(msgs, add_generation_prompt=True)
toks = mx.array(ids)[None]
MAXT = 16


def greedy_serial(pipeline: bool):
    cache = model.make_cache()
    out = []
    # prefill
    logits = model(toks, cache=cache)
    y = mx.argmax(logits[:, -1, :], axis=-1)
    if pipeline:
        mx.async_eval(y)
    else:
        mx.eval(y)
    for n in range(MAXT):
        if pipeline:
            nxt_logits = model(y[None], cache=cache)
            nxt = mx.argmax(nxt_logits[:, -1, :], axis=-1)
            mx.async_eval(nxt)
            tid = y.item()
        else:
            tid = y.item()  # forces eval of the current token (serial)
        if tid == tokenizer.eos_token_id:
            break
        out.append(tid)
        if pipeline:
            y = nxt
        else:
            logits = model(y[None], cache=cache)
            y = mx.argmax(logits[:, -1, :], axis=-1)
            mx.eval(y)
    return tokenizer.decode(out)


print("=== SERIAL (no per-call eval, no pipeline) — our Rust pattern ===", flush=True)
ser = greedy_serial(pipeline=False)
print(f"SERIAL OUTPUT: {ser!r}", flush=True)
print("=== PIPELINED (async_eval) — mlx_lm pattern ===", flush=True)
pipe = greedy_serial(pipeline=True)
print(f"PIPELINE OUTPUT: {pipe!r}", flush=True)
print("MATCH" if ser.strip().startswith("Here's a thinking process") else "SERIAL DIVERGED/garbage")
