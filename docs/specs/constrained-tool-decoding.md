# Constrained Tool-Argument Decoding (native MLX)

## Goal

Make small local models emit **valid, schema-conformant** tool-call arguments by
constraining the sampler during decode: once the model commits to a tool call,
the arguments JSON is forced to conform to that tool's `input_schema` so an
invalid argument object literally cannot be produced. This supersedes the older
`structured-output` item and is the rozum side of `structured-output-for-tools`.

Today tool-use is **post-hoc**: the native MLX backend renders `tools` into the
prompt (Qwen/Hermes format), generates freely, and `parse_tool_calls` extracts
`<tool_call>{json}</tool_call>` after the fact. A small model can emit malformed
JSON, hallucinated keys, wrong types, or miss required keys — and the parse then
fails or yields garbage args. Constrained decoding removes that failure class.

## Scope (v1)

- `src/constrain.rs` — the engine: a JSON-Schema → incremental **prefix
  acceptor**, plus a token-level mask built over the model's vocabulary. Pure
  Rust, no MLX types, fully unit-tested without a model.
- `src/mlx_native_backend.rs` — a dense-arch constrained B=1 decode loop that
  applies the engine's per-step token mask to the logits before `sample_with`.
  Activated only when (a) the request carries `tools`, (b) constrained decoding
  is enabled (`ROZUM_MLX_CONSTRAIN=1`), and (c) the model opens a `<tool_call>`.
- Behind a flag and OFF by default → the existing free-decode + post-hoc parse
  path is byte-identical when disabled.

### Supported schema subset (v1)

The common shape of real tool schemas:

- `object` with `properties` (name → subschema) and `required`; no additional
  properties (keys are restricted to the declared properties).
- scalars: `string` (optional `enum` of string literals), `integer`, `number`,
  `boolean`, bare `enum` (string literals), `const`.
- `array` with scalar `items`.
- nested `object` (recursively constrained).

Anything outside the subset (e.g. `oneOf`, `$ref`, tuple `items`, pattern
constraints) degrades gracefully to **generic well-formed JSON** for that value —
still guarantees valid JSON, just not the finer schema checks. Never rejects a
schema it cannot fully model; it only relaxes.

## Engine interface

```rust
/// A compiled JSON-Schema prefix acceptor. Stateless w.r.t. the input: it is
/// re-run on the whole partial-JSON suffix each step (args are short), which is
/// far easier to get right than an incremental state machine.
pub struct JsonSchema { /* parsed subset */ }

pub enum Prefix { /// `s` is a complete value (only whitespace may remain)
                  Complete,
                  /// `s` is a valid incomplete prefix — more chars needed
                  Partial,
                  /// `s` violates the schema and cannot be extended to match
                  Invalid }

impl JsonSchema {
    pub fn parse(schema: &serde_json::Value) -> Self;
    pub fn prefix(&self, s: &str) -> Prefix;          // is `s` a valid (in)complete prefix?
    pub fn is_complete(&self, s: &str) -> bool;       // Prefix::Complete
}
```

The Hermes envelope is itself expressed as a schema:
`{"name": {enum: <tool names>}, "arguments": <selected tool schema>}`. Because
`name` is decoded first, the matcher knows which tool's `arguments` schema to
enforce once the name literal completes.

### Token mask

The decode loop holds the JSON-in-progress as decoded **text** (driven off the
streaming detokenizer, not raw token ids — sidesteps BPE boundary quirks). Each
step:

1. take the top-K logits (K≈256) — a trained tool-caller keeps the right token
   near the top, so K bounds the cost vs a full 150k-vocab scan;
2. keep a candidate token iff `schema.prefix(json_so_far + token_text)` ≠
   `Invalid`;
3. if none of the top-K qualify (rare), fall back to a full-vocab scan for the
   single best valid token;
4. set every other logit to −∞ and hand off to the existing `sample_with`
   (temp/top-k/top-p/penalty still apply among the *allowed* tokens).

The constraint engages when `full_text` contains `<tool_call>` and the object
`{` has started; it releases when the object completes at depth 0 (the model is
then free to emit `</tool_call>` and EOS).

## Non-goals (follow-ups)

- Hybrid (Qwen3.6 GatedDeltaNet) constrained decode — v1 is dense arches; hybrid
  falls back to the free path. (The user's Qwen3.6 is hybrid; wiring the same
  mask into `run_*_hybrid`'s single-stream path is the immediate follow-up.)
- Full JSON-Schema (`oneOf`/`$ref`/patterns) — v1 relaxes these to generic JSON.
- Batched constrained decode (per-row masks). v1 is B=1, which is what a single
  tool call needs.
- A general `response_format: json_schema` request field (structured output not
  tied to tools). The engine is the shared core; exposing it is a small add-on.

## Validation

- Unit tests (model-free) for `JsonSchema::prefix`: valid prefixes accepted,
  invalid bytes rejected, enums/types/required-keys enforced, completion
  detected, the relax-on-unknown path. This is where correctness lives.
- One e2e (small dense model, ignored/network): a tool whose schema has an enum
  + a typed field; assert the emitted arguments parse and conform even when the
  unconstrained model would drift.
