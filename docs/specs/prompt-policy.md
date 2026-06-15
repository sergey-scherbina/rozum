# prompt-policy — who owns the system prompt

A short, deliberate policy (closes the `prompt-policy` backlog item). The question was "define
system prompts and safety/style constraints per model." The answer is a **decision**, not a feature:
where a model's instructions come from depends on the layer, and the gateway stays out of it.

## The gateway is a transparent provider — raw by default

`rozum gateway` / `rozum launch` serve an OpenAI/Anthropic-compatible API. A client (Claude Code,
Codex, any SDK) sends its **own** system prompt + messages; the gateway passes them through to the
backend **unchanged**. It does **not** inject a per-model system prompt — doing so would corrupt the
client's carefully-built prompt and break tool-use. "Raw mode" isn't a flag; it's the only mode, and
it's the default.

The one deliberate exception is **behavior shaping that the client can't express in the wire format**
and that produces *cleaner* output for it:

- `--enable-thinking` / `ROZUM_ENABLE_THINKING` — reasoning models emit `<think>…</think>` by
  default in their chat template; the gateway disables that unless asked, so CC/Codex get clean
  output. This is a render-time toggle the backend honors, not an injected message.

That's the whole gateway-level policy: **pass the prompt through; the only shaping is the
thinking toggle.** Per-model *safety* belongs to the model + the client; rozum-the-provider does not
add or remove safety instructions.

## Per-model style / persona lives in the caller (agent / room)

Where a *default* system prompt or a per-model persona genuinely belongs:

- **Reference agent runtime** (`src/agent.rs`): `run_agent(backend, system, user, …)` — the caller
  supplies `system`. A per-model default prompt, if ever wanted, is a convenience the caller adds
  there, above the SPI; the backend never owns it.
- **Meeting-room agents**: an agent's voice/format is room-level etiquette (see the `rozum` skill),
  not a gateway concern.
- **Piggyback note** (`docs/specs/rozum-native-channels.md`): the launch-local proxy may fold
  pending room activity into a request as an out-of-band system note — but that's the *proxy/agent*
  layer for a room-joined agent, explicitly opt-in/fallback, and never touches the bare gateway.

## Why not a per-model prompt registry in the gateway

It would be actively harmful for the headline use case (a local drop-in for CC/Codex): two competing
system prompts confuse the model and degrade tool-use. The transparent boundary is the feature — it's
what makes rozum a *provider* rather than an *opinionated agent*. If a future need arises, it lands in
the agent/room layer where the caller owns the conversation, not in the gateway passthrough.
