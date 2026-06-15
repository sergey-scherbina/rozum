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

The constraint engages when `full_text` contains `<tool_call>` and the body has
started; it releases when the body completes (the model is then free to emit
`</tool_call>` and EOS).

### Two tool-call formats

A model emits one of two envelopes after `<tool_call>` — the format is picked
from the first body character and re-validated each step:

- **JSON Hermes** (`{` → Qwen3 dense): `{"name": <enum>, "arguments": <schema>}`.
  `arguments` resolves to the chosen tool's schema once `name` is read.
- **XML** (`<` → Qwen3.6 / Qwen-Coder):
  `<function=NAME><parameter=KEY>\nVALUE\n</parameter>…</function>`. `NAME` ∈ tool
  names, `KEY` ∈ that tool's properties (no dupes, all required before
  `</function>`), an `enum` `VALUE` is restricted to its literals; other values
  are free text up to `</parameter>`. Same `Prefix` semantics, exposed as
  `Constraint::{Json, Xml}`.

### Arch coverage

The masked loop (`constrained_decode_loop`) is generic over the cache type AND over a
`ConstraintDriver`, so it runs on **both** the dense KV-cache path
(`run_constrained_dense`, every dense arch) and the Qwen3.6 **hybrid** `LayerCache` path
(`run_constrained_hybrid`), for **two** triggers:

- `ToolConstraint` — tool-call args (OPT-IN via `ROZUM_MLX_CONSTRAIN`; waits for the
  `<tool_call>` envelope).
- `ResponseConstraint` — **structured output** (`response_format`): the *whole* response
  is constrained to a fixed schema from the first generated token, released when the value
  completes. ALWAYS honored when the request carries a schema (an explicit correctness
  request, not an opt-in). The gateway maps OpenAI `response_format`
  (`{"type":"json_object"}` → any object; `{"type":"json_schema","json_schema":{"schema":…}}`
  → that schema) onto `SamplingParams.response_schema`.

## Non-goals (follow-ups)

- Full JSON-Schema (`oneOf`/`$ref`/patterns) — relaxed to generic JSON.
- Typed (number/integer/boolean) XML `VALUE`s — currently only `enum` values are
  strictly constrained in the XML form; other scalars are free text.
- Batched constrained decode (per-row masks). B=1 is what a single tool call needs.
- Forcing EOS exactly at completion (today the constraint releases on a complete value and
  the model naturally stops; a strict "no trailing tokens" mode is a later add).

## Validation

- Unit tests (model-free): `Schema::prefix` (JSON) + `xml_prefix` (XML) — valid
  prefixes accepted, invalid rejected, enums/types/required-keys enforced,
  completion detected, the relax-on-unknown path, and `Constraint` dispatch.
- e2e (ignored/network), enum `["kelvin","rankine"]` against a "celsius" prompt so
  a conforming `unit` proves the mask redirected the model off its preferred
  (invalid) token: `mlx_constrained_tool_call_conforms` (Qwen3-4B, JSON) and
  `mlx_constrained_tool_call_hybrid` (Qwen3.6 MoE, XML).
