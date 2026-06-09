#!/usr/bin/env python3
"""Side-by-side MLX reference oracle for the Qwen3.6 mistralrs integration.

Canonical ground-truth generator referenced by
`docs/specs/mlx-weight-layout-and-afq.md` (sections 8 and 10). It loads the
exact same MLX checkpoint our Rust runtime loads, renders the exact same
fixed prompt, and dumps the values we diff against the env-gated Rust dumps
(`ROZUM_FWD_DEBUG=1`):

  * prompt token ids and count
  * embedding row for the last position (first 8 values + stats)
  * per-decoder-layer output at the last position (first 8 values + L2 norm),
    tagged linear/full so we can localize which block first diverges
  * final top-10 logits with token strings

Usage:
    python scripts/mlx_ref.py [--model REPO] [--prompt TEXT] [--layers]

Defaults match the Qwen3.6 P0 parity gate: model
`mlx-community/Qwen3.6-35B-A3B-4bit`, prompt "Hello" through the chat
template. `--layers` enables the per-layer dump (slower, more output).
"""

import argparse
import math

import mlx.core as mx
import mlx.nn as nn
from mlx_lm import load

DEFAULT_MODEL = "mlx-community/Qwen3.6-35B-A3B-4bit"
DEFAULT_PROMPT = "Hello"


def stats(vec):
    v = [float(x) for x in vec]
    n = len(v)
    mean = sum(v) / n
    norm = math.sqrt(sum(x * x for x in v))
    return min(v), max(v), mean, norm


def fmt_head(arr, k=8):
    return [round(float(x), 6) for x in arr[:k]]


def install_layer_taps(model, records):
    """Patch the decoder-layer class __call__ to record each layer's output at
    the last position. MLX dispatches `layer(x)` through `type(layer).__call__`,
    so we patch the class once and resolve the layer index by identity."""
    layers = model.language_model.model.layers
    index_of = {id(layer): i for i, layer in enumerate(layers)}
    linear_of = {id(layer): getattr(layer, "is_linear", None) for layer in layers}
    cls = type(layers[0])
    orig = cls.__call__

    def tapped(self, x, *args, **kwargs):
        out = orig(self, x, *args, **kwargs)
        i = index_of.get(id(self))
        if i is not None:
            mx.eval(out)
            last = out[0, -1, :]
            mn, mx_, mean, norm = stats(last.tolist())
            records.append((i, linear_of[id(self)], fmt_head(last.tolist()), norm, mn, mx_, mean))
        return out

    cls.__call__ = tapped


def install_attn_taps(model, records):
    """Tap each layer's attention sub-module (linear_attn / self_attn) to record
    its raw output `r` (before the residual add). Localizes GDN vs MoE bugs."""
    layers = model.language_model.model.layers

    def sub_of(layer):
        return layer.linear_attn if getattr(layer, "is_linear", False) else layer.self_attn

    index_of = {id(sub_of(l)): i for i, l in enumerate(layers)}
    linear_of = {id(sub_of(l)): getattr(l, "is_linear", False) for l in layers}

    classes = {type(sub_of(l)) for l in layers}
    for cls in classes:
        orig = cls.__call__

        def shared(self, *args, _orig=orig, **kwargs):
            out = _orig(self, *args, **kwargs)
            i = index_of.get(id(self))
            if i is not None:
                mx.eval(out)
                last = out[0, -1, :]
                _, _, _, norm = stats(last.tolist())
                records.append((i, linear_of[id(self)], fmt_head(last.tolist()), norm))
            return out

        cls.__call__ = shared


def install_router_taps(model, records):
    """Tap each SparseMoeBlock to record the top-k expert indices and renormalized
    weights at the last position. Compares against the Rust ROUTER dump."""
    layers = model.language_model.model.layers
    moe_layers = [l for l in layers if hasattr(l.mlp, "switch_mlp")]
    if not moe_layers:
        return
    index_of = {id(l.mlp): i for i, l in enumerate(layers) if hasattr(l.mlp, "switch_mlp")}
    cls = type(moe_layers[0].mlp)
    orig = cls.__call__

    def tapped(self, x, *args, **kwargs):
        gates = mx.softmax(self.gate(x), axis=-1, precise=True)
        k = self.top_k
        inds = mx.argpartition(gates, kth=-k, axis=-1)[..., -k:]
        scores = mx.take_along_axis(gates, inds, axis=-1)
        if self.norm_topk_prob:
            scores = scores / scores.sum(axis=-1, keepdims=True)
        mx.eval(inds, scores)
        i = index_of.get(id(self))
        if i is not None:
            y_experts = self.switch_mlp(x, inds)  # (B, S, k, hidden), before scores
            mx.eval(y_experts)
            per = y_experts[0, -1]  # (k, hidden)
            norms = [round(float(mx.sqrt(mx.sum(per[e] * per[e]))), 4) for e in range(per.shape[0])]
            records.append(
                (i, [int(v) for v in inds[0, -1].tolist()], [round(float(v), 4) for v in scores[0, -1].tolist()], norms)
            )
        return orig(self, x, *args, **kwargs)

    cls.__call__ = tapped


def install_mlp_taps(model, records):
    """Tap each layer's mlp (SparseMoeBlock / MLP) to record its output, before
    the residual add. Localizes MoE-expert bugs."""
    layers = model.language_model.model.layers
    index_of = {id(l.mlp): i for i, l in enumerate(layers)}
    classes = {type(l.mlp) for l in layers}
    for cls in classes:
        orig = cls.__call__

        def shared(self, *args, _orig=orig, **kwargs):
            out = _orig(self, *args, **kwargs)
            i = index_of.get(id(self))
            if i is not None:
                mx.eval(out)
                last = out[0, -1, :]
                _, _, _, norm = stats(last.tolist())
                records.append((i, True, fmt_head(last.tolist()), norm))
            return out

        cls.__call__ = shared


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--prompt", default=DEFAULT_PROMPT)
    ap.add_argument("--layers", action="store_true", help="dump per-layer output")
    ap.add_argument("--attn", action="store_true", help="dump per-layer attention (r) output")
    ap.add_argument("--mlp", action="store_true", help="dump per-layer mlp/moe output")
    ap.add_argument("--router", action="store_true", help="dump per-layer router top-k")
    args = ap.parse_args()

    print(f"loading {args.model} ...", flush=True)
    model, tokenizer = load(args.model)

    messages = [{"role": "user", "content": args.prompt}]
    ids = tokenizer.apply_chat_template(messages, add_generation_prompt=True)
    print(f"prompt = {args.prompt!r}")
    print(f"token ids ({len(ids)}): {ids}")
    print("decoded per-token:")
    for i, t in enumerate(ids):
        print(f"  [{i}] {t} -> {tokenizer.decode([t])!r}")

    x = mx.array([ids])

    # Embedding at the last position (matches the Rust EMBED dump).
    embed = model.language_model.model.embed_tokens(x)
    mx.eval(embed)
    last_embed = embed[0, -1, :]
    mn, mx_, mean, norm = stats(last_embed.tolist())
    print("\n=== embedding ===")
    print(f"EMBED last-pos[0..8] = {fmt_head(last_embed.tolist())}")
    print(f"EMBED last-pos stats: min={mn:.4f} max={mx_:.4f} mean={mean:.4f} ||x||={norm:.4f}")

    records = []
    attn_records = []
    if args.layers:
        install_layer_taps(model, records)
    if args.attn:
        install_attn_taps(model, attn_records)
    mlp_records = []
    if args.mlp:
        install_mlp_taps(model, mlp_records)
    router_records = []
    if args.router:
        install_router_taps(model, router_records)

    logits = model(x)
    mx.eval(logits)

    if args.router:
        print("\n=== per-layer router top-k (idx, renorm weights, per-expert ||.||) ===")
        for i, idx, w, norms in sorted(router_records):
            print(f"layer {i:>2} idx={idx} wsum={sum(w):.4f} w={w}")
            print(f"          per-expert ||.|| = {norms}")

    if args.attn:
        print("\n=== per-layer attention (r) output, before residual add ===")
        for i, linear, head, norm in sorted(attn_records):
            kind = "linear" if linear else "FULL  "
            print(f"layer {i:>2} [{kind}] ||r||={norm:9.4f} head={head}")

    if args.mlp:
        print("\n=== per-layer mlp/moe output, before residual add ===")
        for i, _linear, head, norm in sorted(mlp_records):
            print(f"layer {i:>2} ||m||={norm:9.4f} head={head}")

    if args.layers:
        print("\n=== per-layer last-pos output (first divergence localizes the bug) ===")
        for i, linear, head, norm, mn, mx_, mean in records:
            kind = "linear" if linear else "FULL  "
            print(f"layer {i:>2} [{kind}] ||x||={norm:9.4f} head={head}")

    last_logits = logits[0, -1, :]
    mx.eval(last_logits)
    vals = last_logits.tolist()
    order = sorted(range(len(vals)), key=lambda j: vals[j], reverse=True)[:10]
    print("\n=== last-position top-10 ===")
    for rank, j in enumerate(order):
        print(f"  {rank}: id={j} logit={vals[j]:.4f} tok={tokenizer.decode([j])!r}")


if __name__ == "__main__":
    main()
