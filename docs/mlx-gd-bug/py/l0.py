#!/usr/bin/env python
"""L0: minimal raw-MLX serial decode forward with the gated_delta custom kernel.

No mlx_lm, no framework. N "layers": derive q/k/v/g/beta from a running `hidden`
via plain matmuls, run the kernel, store state in a cache, add output back into
`hidden`. A SECOND..Nth token reads the cached states. A/B: per-call eval (force the
kernel materialized each call) vs NO per-call eval (only eval the token's hidden).
If the no-eval run diverges from the per-call-eval run -> the bug reproduces here.
"""
import mlx.core as mx

# gated_delta kernel source — verbatim from mlx_lm.models.gated_delta (scalar gate).
SOURCE = r"""
    auto n = thread_position_in_grid.z;
    auto b_idx = n / Hv;
    auto hv_idx = n % Hv;
    auto hk_idx = hv_idx / (Hv / Hk);
    constexpr int n_per_t = Dk / 32;
    auto q_ = q + b_idx * T * Hk * Dk + hk_idx * Dk;
    auto k_ = k + b_idx * T * Hk * Dk + hk_idx * Dk;
    auto v_ = v + b_idx * T * Hv * Dv + hv_idx * Dv;
    y += b_idx * T * Hv * Dv + hv_idx * Dv;
    auto dk_idx = thread_position_in_threadgroup.x;
    auto dv_idx = thread_position_in_grid.y;
    auto i_state = state_in + (n * Dv + dv_idx) * Dk;
    auto o_state = state_out + (n * Dv + dv_idx) * Dk;
    float state[n_per_t];
    for (int i = 0; i < n_per_t; ++i) {
      auto s_idx = n_per_t * dk_idx + i;
      state[i] = static_cast<float>(i_state[s_idx]);
    }
    auto g_ = g + b_idx * T * Hv;
    auto beta_ = beta + b_idx * T * Hv;
    for (int t = 0; t < T; ++t) {
      float kv_mem = 0.0f;
      for (int i = 0; i < n_per_t; ++i) {
        auto s_idx = n_per_t * dk_idx + i;
        state[i] = state[i] * static_cast<float>(g_[hv_idx]);
        kv_mem += state[i] * static_cast<float>(k_[s_idx]);
      }
      kv_mem = simd_sum(kv_mem);
      auto delta = (static_cast<float>(v_[dv_idx]) - kv_mem) * static_cast<float>(beta_[hv_idx]);
      float out = 0.0f;
      for (int i = 0; i < n_per_t; ++i) {
        auto s_idx = n_per_t * dk_idx + i;
        state[i] = state[i] + static_cast<float>(k_[s_idx]) * delta;
        out += state[i] * static_cast<float>(q_[s_idx]);
      }
      out = simd_sum(out);
      if (thread_index_in_simdgroup == 0) {
        y[dv_idx] = static_cast<InT>(out);
      }
      q_ += Hk * Dk; k_ += Hk * Dk; v_ += Hv * Dv; y += Hv * Dv; g_ += Hv; beta_ += Hv;
    }
    for (int i = 0; i < n_per_t; ++i) {
      auto s_idx = n_per_t * dk_idx + i;
      o_state[s_idx] = static_cast<StT>(state[i]);
    }
"""
_kernel = mx.fast.metal_kernel(
    name="gated_delta_step",
    input_names=["q", "k", "v", "g", "beta", "state_in", "T"],
    output_names=["y", "state_out"],
    source=SOURCE,
)


def gd_kernel(q, k, v, g, beta, state):
    B, T, Hk, Dk = q.shape
    Hv, Dv = v.shape[2:]
    return _kernel(
        inputs=[q, k, v, g, beta, state, T],
        template=[("InT", q.dtype), ("StT", state.dtype),
                  ("Dk", Dk), ("Dv", Dv), ("Hk", Hk), ("Hv", Hv)],
        grid=(32, Dv, B * Hv),
        threadgroup=(32, 4, 1),
        output_shapes=[(B, T, Hv, Dv), state.shape],
        output_dtypes=[q.dtype, state.dtype],
    )


B, Hk, Hv, Dk, Dv = 1, 16, 48, 128, 128
d = 2048
n_layers = 48
n_tokens = 8


def det(out, inn, seed):
    n = out * inn
    return (mx.sin(mx.arange(n, dtype=mx.float32) * 0.0007 + seed) * 0.02).reshape(out, inn).astype(mx.bfloat16)


wq = det(d, Hk * Dk, 0.1)
wk = det(d, Hk * Dk, 0.2)
wv = det(d, Hv * Dv, 0.3)
wg = det(d, Hv, 0.4)
wb = det(d, Hv, 0.5)
wo = det(Hv * Dv, d, 0.6)


def forward(hidden, cache, do_eval):
    for l in range(n_layers):
        hflat = hidden.reshape(B, d)
        q = (hflat @ wq).reshape(B, 1, Hk, Dk)
        k = (hflat @ wk).reshape(B, 1, Hk, Dk)
        v = (hflat @ wv).reshape(B, 1, Hv, Dv)
        g = mx.sigmoid(hflat @ wg).astype(mx.float32).reshape(B, 1, Hv)
        beta = mx.sigmoid(hflat @ wb).reshape(B, 1, Hv)
        state = cache[l] if cache[l] is not None else mx.zeros((B, Hv, Dv, Dk), dtype=mx.float32)
        y, ns = gd_kernel(q, k, v, g, beta, state)
        cache[l] = ns
        if do_eval:
            mx.eval(y, ns)          # per-call materialize (the Rust default)
        hidden = hidden + (y.reshape(B, Hv * Dv) @ wo).reshape(B, 1, d)
    return hidden


def run(do_eval):
    cache = [None] * n_layers
    hidden = det(B, d, 1.0).reshape(B, 1, d)
    outs = []
    for _ in range(n_tokens):
        hidden = forward(hidden, cache, do_eval)
        mx.eval(hidden)             # per-token sync (serial, like our Rust real path)
        outs.append(hidden.reshape(-1).astype(mx.float32))
    return outs


ref = run(True)    # per-call eval = reference
cand = run(False)  # NO per-call eval = candidate
mx.eval(ref, cand)
mxd = 0.0
for t in range(n_tokens):
    d_ = float(mx.max(mx.abs(ref[t] - cand[t])).item())
    mxd = max(mxd, d_)
    print(f"L0 token {t}: max|d| = {d_:.5f}")
print(f"L0 overall max|d| = {mxd:.5f}  ({'REPRODUCED (no-eval diverges)' if mxd > 1e-2 else 'no divergence'})")
