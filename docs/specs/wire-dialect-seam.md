# The wire seam: what three dialects share, and what they must not

Status: implemented 2026-08-14. `crates/rozum-gateway/src/gateway.rs` (`trait WireDialect`,
`serve_wire`, `OaiWire` / `RespWire` / `AnthropicWire`).

This item was **parked**, and parked correctly: `architecture-spi.md` Stage 3 investigated a
`WireProtocol` trait, rejected it, and wrote down why. The operator overrode that on 2026-08-14
("try it, carefully, in a worktree"). This spec records what the re-measurement found, what was
built, and — the part that matters most — what was deliberately **not** built, because two of the
three original objections were right and are still honoured.

## What the earlier decision said, and what it did not weigh

> A unifying trait would fight genuinely different typed extractors (`OaiChatReq` vs `Value` vs
> `AnthropicMsg`) and different SSE event sequences, forcing either looser request validation (a
> behaviour change) or a fat trait that adds indirection without removing complexity.

Both halves are true, and neither is contradicted here: **the extractors and the serializers were
not touched.** Each dialect still declares its own request type, so axum's validation is exactly
what it was, and each still emits its own SSE sequence byte for byte.

What that investigation looked at was parse and serialize — where the layer really was already
factored. What it did not weigh is what sits *between* them. Measured on the three handlers as they
stood:

| Step, in order | OpenAI Chat | Responses | Anthropic |
|---|---|---|---|
| acquire the model lease | same | same | same |
| fit prompt to context window | same | same | same |
| attach the elision note | same | same | same |
| estimate prompt tokens | same | same | same |
| build `ChatRequest` + `apply_determinism_env` | same shape | same shape | same shape |
| loop-breaker (`chat_or_loopbreak`) | same | same | same |
| error → `log_event` + `chat_error_response` | same, `backend_error` | same, `backend_error` | same, **`api_error`** |
| `ReqMeta` → `meter` → `with_gen_timeout` | same | same | same |
| branch on `stream` | same | same | same |

That is roughly 45 lines written three times, on the path every agent and the whole matrix runs
through. Every cross-cutting change to it — auto-context, metering, the generation timeout — had to
be made three times or be wrong in one place.

**And it had already drifted.** `/v1/messages` builds its `SamplingParams` with `temperature` and
`max_tokens` only: `AnthropicReq` declares no `top_p` and no `top_k`, both of which Anthropic's own
Messages API defines. A client that sends them has them silently dropped. That is not a property of
the dialect; it is a line nobody copied. It is left **unfixed** here on purpose — this change is
behaviour-preserving by construction — and recorded as data in the golden file.

## The seam

```rust
trait WireDialect: Sized {
    const ENDPOINT: &'static str;      // route, for events + metrics
    const ERROR_KIND: &'static str;    // "backend_error" / "api_error" — a client switches on it
    fn model_hint(&self) -> Option<&str>;
    fn stream_mode(&self) -> bool;
    fn into_internal(&mut self, lease: &ChatLease) -> WireRequest;
    fn respond(self, chat, cancel, model, lease) -> impl Future<Output = Response> + Send;
}
```

Three deliberate shapes:

- **`into_internal` takes the lease**, because two dialects need the *resolved* model id before they
  can say what to send: codex-lean trims instructions and floors reasoning effort by model.
- **It takes `&mut self`**, because a dialect may derive state during parse that its serializer needs
  later — `RespWire` records whether codex offered `apply_patch` as a tool, which decides whether a
  model's `apply_patch` call is re-routed to `exec_command`.
- **`respond` returns `impl Future + Send`, not `async fn`**, because axum requires a `Send` handler
  future and an `async fn` in a trait does not promise one.

Adding a dialect is now: one extractor, one impl, one route.

## What this cost, measured

**+249 / −205 lines of code** (comments excluded): the seam is **44 lines larger** than what it
replaced. It does not shrink the file, and anyone selling this as "less code" is selling the wrong
thing. What it buys is that the spine exists once and cannot drift again, and that the next
cross-cutting change is one edit instead of three. That is the trade; the earlier decision priced
the indirection correctly and only missed what was being triplicated.

## The gate

`crates/rozum-gateway/src/testdata/wire-golden.txt`, frozen in its own commit **before** any handler
was touched, and byte-identical after. Six cases — three dialects × streaming/not — each recording
two things:

- the **response**, exact bytes, SSE frames included;
- the **request that reached the backend** — roles and text, tool names, every sampling knob —
  captured by a stub backend, because a mis-moved field (a dropped `response_schema`, a `max_tokens`
  read from the wrong key) is invisible in the response and would otherwise ship.

Ids and clocks are normalised; nothing else is. Regenerate deliberately with
`ROZUM_WIRE_GOLDEN_UPDATE=1` — a diff in that file during a behaviour-preserving change is the
change failing, not the file going stale.

## What is NOT proven

The agentic matrix has not been re-run against this. The gate above is hermetic and covers the
mapping and the bytes, which is what a refactor can break; it does not cover a real model's
behaviour under a real agent, and this path is the matrix's. The matrix needs the model slot and an
hour, and it is the operator's call when to spend them (`BENCH_DEDICATED=1` for a dedicated
gateway). Until then this is: byte-identical on six frozen cases and 124 crate tests, not
"validated end-to-end".
