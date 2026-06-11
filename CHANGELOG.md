# Changelog

## runtime-config — declare backends, policy & default model in `rozum.toml`
Completed: 2026-06-11
The gateway's backend selection and default model can now be declared once in a
`rozum.toml` instead of re-typed as `--model` / `--backend` every session. A new
`src/config.rs` (`RuntimeConfig`, serde + `toml`) is resolved from `$ROZUM_CONFIG`
→ `./rozum.toml` → `$XDG_CONFIG_HOME/rozum/rozum.toml`; a malformed file (or a
`$ROZUM_CONFIG` that points at a missing one) is a hard error rather than a silent
fall-back, because a config the user deliberately wrote must surface. The schema is
a `[runtime]` block (`model`, `n_ctx`, `policy`, `backend`) plus an ordered list of
`[[backend]]` tables (`id`, `engine`, optional `model`/`n_ctx`/`url`/`enabled`).
Policies: `single` / `fallback` / `fanout`. Engine names span everything rozum can
build — the gateway engines `gguf`/`mistralrs`/`lmstudio`/`mlx`/`url` and the sync
meeting-room engines `hello`/`candle`/`llama-gguf`/`native-rust`/`external-command`
(the latter map to a placeholder in the sync `BackendRegistry`; the gateway builds
the HTTP/native ones for real).

`RuntimeConfig::default()` **is** the old auto-detect chain in code — `Fallback`
over `[gguf, mistralrs, lmstudio, mlx, url]` — so a user who never writes a config
sees zero behaviour change. The daemon's initial model load and every `gateway
switch` now walk this chain (`main.rs::build_from_config` / `build_choice`,
returning the first backend that builds), with the config injected into the
`Switchboard`'s `BackendBuilder` from `gateway-switch`. `--backend B` still
force-bypasses the chain to a single engine. `[runtime].model` / `[runtime].n_ctx`
fill in when `--model` / `--n-ctx` are omitted on `rozum gateway`; per-backend
`url` pins an explicit endpoint for an `lmstudio`/`mlx`/`url` entry. The
library/binary split from `gateway-switch` is preserved: the plan
(`gateway_chain()`) lives in the library, the async build stays in the binary. 12
Metal-free unit tests; lib suite 101 passing. No new deps (`toml` was already in).

### Build fix bundled with this work
The `gateway-switch` commit had swept in stray, incomplete `channel-wakeup` WIP
(`exec_agent` / `exec_agent_anthropic` call sites passing a `&channels` argument
the signatures never accepted), so `master` did not build on default features. A
separate fix commit threads `ChannelWakeup` through and applies `flags_for()`,
which also completes the `channel-wakeup-launch-flag` mechanism: a capable
`claude` now gets `--dangerously-load-development-channels server:<name>` appended
at launch (`--no-channel-wakeup` suppresses; `--channel-mcp-name` sets the name).

## gateway-switch — transparent in-place model/backend switch, reload & unload
Completed: 2026-06-11
`rozum gateway switch --model Y [--backend B] [--n-ctx N]` swaps the resident
model of the running shared daemon **in place**: it drains in-flight work, drops
the old model (never two resident — the memory constraint), loads the new one,
bumps a new `generation`, and resumes. Clients' launch-local proxies hold their
queued requests across the gap (`/v1/admit` advertises a closed window while
draining, so it looks like backpressure, not a failure) and a request already
mid-flight is held in the daemon until the swap finishes — so the swap is
transparent, just slower. The daemon now holds its backend in a `Switchboard`
(swap cell + an injected `BackendBuilder` closure over `rozum`'s own
backend-selection chain), and every chat handler takes a `ChatLease` for the
whole stream so a switch waits for streaming to finish before swapping. Drain
tracks a dedicated `generating` counter (the idle-watchdog `in_flight` counter
can't be used — it's held for parked requests and would deadlock the drain),
bounded by `ROZUM_GATEWAY_DRAIN_SECS` (default 120). `--backend` forces an engine
(`gguf`/`mistralrs`/`lmstudio`/`mlx`/`url`); on a build failure the switch reverts
the spec so the next request lazily reloads the old model.

`rozum gateway reload` drains then re-execs the current binary (transparent
daemon/binary upgrade after a `rozum` upgrade); the brief port gap rides the
proxies' existing replay path. `rozum gateway unload` drops the model to free RAM
but keeps the daemon listening — the next chat lazily reloads it (serialized so
racing requests reload once). `generation` was added to the `active.json`
registry (`#[serde(default)]`, continued monotonically across respawns) so a
proxy can tell "the daemon I was talking to was replaced" from a transient blip;
`rozum gateway status` shows it as `gen:`. Control plane is auth-gated localhost
`POST /control/{switch,unload,reload}`. A `--dedicated` gateway has no builder, so
all three are cleanly refused. No new deps.

## launch-no-model — `rozum launch --no-model` (upstream Anthropic, no gateway)
Completed: 2026-06-11
`rozum launch` can now run an agent with no local model at all: `--no-model`
(and a new first **"Anthropic (cloud — no local model)"** entry in the interactive
picker) bypass the gateway entirely — no daemon spawn, no lease, no launch-local
proxy, and none of the `ANTHROPIC_*`/`OPENAI_*` gateway/model env overrides. The
child inherits the operator's own Anthropic auth (`ANTHROPIC_API_KEY` / claude.ai
OAuth), exactly like a bare `claude`; only rozum's agent-context defaults
(`CLAUDE_CODE_DISABLE_*`, each applied only if unset) still apply. Resolution is
modeled as `LaunchTarget::{Local(spec), Anthropic}`; `--no-model` `conflicts_with`
`--model`/`--dedicated`/`--n-ctx`/`--port` (clap-enforced) and is hoisted by
`reorder_launch_args` like the value flags (also fixing `--dedicated` placement
after the program name). This is the mode that makes Claude Code features
requiring real Anthropic auth — notably **channels** — available to a
rozum-launched agent (empirically a local-gateway base URL does *not* block
channels, but no-model is the clean path). Spec: `docs/specs/launch-wrapper.md`.
No new deps.

## shared-gateway-poison — soft, graduated poison-prompt protection
Completed: 2026-06-11
A request that repeatedly crashes the shared daemon is now handled gently instead
of either retrying forever or hard-banning a possibly-good prompt. The proxy
fingerprints each request (`share::fingerprint`, a hash of the raw body bytes it
forwards verbatim — so the proxy and daemon agree without dialect normalization).
Crash-attribution is precise: an upstream send error is blamed on the prompt only
when the connection was established and then died (`!is_connect()`); a pure connect
failure is a failover gap and stays on the wait-for-health replay path. On a
crash-attributed failure the proxy degrades (the retry takes an exclusive `lane`
write-lock, serializing the risky prefill so no neighbour competes for memory —
clearing most big-prompt OOMs), counts per fingerprint, and after `ROZUM_POISON_MAX`
(default 3) attempts returns a soft, retryable 422 (`poison_refused`). When those
graduated retries are exhausted *and* the crash was the sole in-flight request
(`admit.stats().in_use <= 1`), the fingerprint is confirmed machine-wide to a TTL'd
`poison.json` (`ROZUM_POISON_TTL_SECS`, default 3600); ambiguous concurrent crashes
stay local. A confirmed entry is fast-refused both by the proxy before forwarding
and by the daemon's new `poison_layer` before running the model (defense-in-depth
that survives the very crash it guards against), and decays on the next clean (2xx)
prefill, both locally and machine-wide. Tunables: `ROZUM_POISON_MAX`,
`ROZUM_POISON_TTL_SECS`. No new deps.

## shared-gateway-replay-retry (part 2) — two-tier admission
Completed: 2026-06-11
The daemon now advertises its admission state and each launch's proxy holds its
client's requests at the edge instead of bouncing them off a full daemon.
Tier-1 (global): `GET /v1/admit` reports `{limit,in_use,waiting,free}` from the
daemon's `AdmittingBackend` via a new defaulted `ChatBackend::admission_stats()`
(ungated backends report an always-free window). Tier-2 (per client): each proxy
runs its own `concurrency::AdmissionScheduler` (SJF + reserved fast lane, cost
estimated from body size, unbounded queue — a proxy never sheds its own client)
over the single agent's parallel requests, and `wait_for_window` polls `/v1/admit`
to hold a queued request until the daemon signals room (bounded; fail-open on a
probe failure, so the `429`/`Retry-After` backstop still applies). The local
admission guard is held for the whole stream. Env: `ROZUM_PROXY_ADMIT` (4),
`ROZUM_PROXY_FASTLANE_TOKENS` (1024). Reuses the one `concurrency` module at both
tiers. Completes `shared-gateway-replay-retry`. No new deps.

## shared-gateway-replay-retry (part 1) — replay before first token + smart retry
Completed: 2026-06-11
The launch-local proxy now makes a daemon crash transparent to the agent. The
`forward` path buffers the request body once and re-sends it on a replay loop:
a connection failure *before any response byte reaches the agent* is safe to
replay, so the proxy waits for re-election to bring the daemon back on the same
stable port (`wait_for_health`) and retries — the agent sees a slower response,
not an error. Once a `Response` is returned (status+headers committed), a
mid-stream death surfaces the error instead (we can't un-send tokens). Retries
use capped exponential backoff + ±50% jitter (no `rand` dep — wall-clock nanos),
a per-request attempt cap, wait-for-health between tries, and honor the daemon's
`429`/`Retry-After` by holding and retrying rather than bouncing it back. Tunable
via `ROZUM_PROXY_MAX_ATTEMPTS` (6), `ROZUM_PROXY_BACKOFF_MS` (150),
`ROZUM_PROXY_HEALTH_WAIT_SECS` (60). 3 new tests (backoff math + an end-to-end
replay-after-daemon-returns test). No new deps. (Two-tier admission follows in
part 2.)

## shared-gateway-proxy — launch-local reverse proxy in the request path
Completed: 2026-06-11
New `src/proxy.rs`: a model-free launch-local reverse HTTP proxy (gateway analog
of the mcp-proxy). `proxy::serve` forwards every request to the shared daemon's
stable port and streams the response back verbatim (SSE token streams included),
buffering the request body (the seed for future replay), stripping hop-by-hop and
framing headers both ways, with a no-timeout client. An unreachable daemon yields
a clean 502; `daemon_port` lives in an AtomicU16 so a later phase can re-point it
at a respawned daemon. `rozum launch` (`start_launch_proxy`) binds an ephemeral
loopback port, spawns the proxy, and points the agent at it (failover watchdog +
lease heartbeat still target the daemon); falls back to the daemon directly if the
proxy can't bind. Foundation for replay / poison / two-tier backpressure /
transparent swap. 5 new tests incl. two real end-to-end tokio tests. No new deps.

## models-rm — delete a cached model from disk
Completed: 2026-06-11
`rozum models rm <spec> [-y]` frees disk by deleting a cached model. It
exact-matches the spec against `scan_all_installed()`, refuses if it is the
active gateway model (reads `active.json` + health-probes), prints what will be
freed, and confirms (`--yes`/`-y` skips; a non-TTY without `--yes` is refused).
HuggingFace (`models--owner--name`) and LMStudio (the repo dir holding the
`.gguf`) directories are removed directly; Ollama is delegated to `ollama rm`
(its blobs are content-addressed and shared) and refused if the binary is absent.
Dependency-free `which` helper added. No new deps.

## launch-model-picker — optional --model, interactive picker, takeover-if-idle
Completed: 2026-06-11
`rozum launch --model` is now optional. `resolve_launch_model`: given → use it;
omitted + a healthy gateway running → reuse its model (`using running model: …`);
omitted + nothing running on a TTY → interactive `pick_model_interactive` (cached
models first, `(cached, size)`; then not-cached `RECOMMENDED`, `(not cached, ~GB)`;
a not-cached pick re-confirms the download); omitted + non-TTY → error. Model
mismatch now does **takeover-if-idle** in `ensure_shared_gateway`: a different
running model with no live client leases is SIGTERM'd and replaced on the same
port; with live leases it is reused-with-warning (don't steal a live session).
`--dedicated` still bypasses sharing. No new deps.

## shared-gateway-leases — client leases drive daemon lifetime + status/stop
Completed: 2026-06-11
Third phase of `shared-gateway`. Each launch holds a `leases/<pid>` file
heartbeated every 15s (mtime = liveness); `share::live_lease_count` counts fresh
leases and reaps dead ones. The daemon's idle watchdog now stays up while any
lease is fresh OR a request is in flight OR there was recent HTTP, and idle-exits
(ROZUM_GATEWAY_IDLE_SECS, default 900) only when all are quiet — so leases, not
raw HTTP traffic, are the primary keep-alive for launch clients, while a manually
run `rozum gateway` is still kept alive by traffic. Added `rozum gateway status`
(model/port/pid/n_ctx/uptime/clients) and `rozum gateway stop [--force]` (SIGTERM,
refused while clients attached); `gateway --model` is now optional (required only
to run the daemon). No new deps.

## shared-gateway-failover — respawn the shared daemon on death
Completed: 2026-06-11
Second phase of `shared-gateway`. `share::try_spawn_lock` adds an O_EXCL
`spawn.lock` with stale-steal + drop-release (best-effort anti-stampede; the TCP
bind remains the hard single-owner guarantee). `spawn_failover_watchdog` runs in
each launch alongside the agent: it polls the daemon every 5s and, after two
consecutive misses, respawns it on the same port under the spawn lock (rechecking
health first), waiting up to 120s. Simultaneous watchdogs are damped by the lock
and deduped by the port bind, so a crashed/killed daemon comes back without the
user relaunching; the agent reconnects over the brief gap via its own retry (same
stable URL). No new deps.

## shared-gateway-mvp — share one model daemon across launches
Completed: 2026-06-11
First phase of `shared-gateway`. `rozum launch` no longer always loads its own
in-process model (two launches → two models → OOM). New `src/share.rs` registry
(`active.json` under `$XDG_STATE_HOME/rozum/gateway/`, atomic write +
remove-if-mine, `health_ok` probe, `is_reusable`, stable `DEFAULT_GATEWAY_PORT`
8089). `rozum gateway` publishes the registry and idle-exits after
`ROZUM_GATEWAY_IDLE_SECS` (default 900) when nothing is in flight (in-flight-aware
via an Activity counter in the auth layer, so long generations don't trip it).
`rozum launch` reuses a healthy running gateway (or a different-model one with a
warning), else spawns a detached `rozum gateway` (own process group, stdio →
gateway.log) and waits for health; the TCP-port bind is the single-owner
guarantee. `--dedicated` keeps the old private in-process gateway. Deferred to
later phases: flock anti-stampede + crash re-election, client-pid leases, the
launch-local proxy / replay / poison / two-tier backpressure, switch/reload/
unload, gateway status/stop, the model picker, and `models rm`. 3 share unit
tests (no Xcode); fmt + feature build clean.

## concurrency-backend-abstraction — generic admission for any backend
Completed: 2026-06-11
Lifted the concurrency machinery (scheduler, memory budget, fast lane,
backpressure, circuit breaker) out of the mistralrs modules into a generic
`src/concurrency` module (renamed from `mistralrs_admission`), and re-applied it
as a decorator. `ChatBackend` gained an optional `concurrency_capacity() ->
Option<usize>` hook (default `None`); `concurrency::admit_wrap` wraps a backend in
`AdmittingBackend` iff it advertises a capacity, and passes remote / self-
serializing backends through untouched (the safe default). `MistralrsBackend`
now reports `Some(max_num_seqs)` and its `chat()` is plain inference again — the
decorator owns admission. The budget math (`budgeted_max_num_seqs`,
`ConcurrencyBudget`, `per_seq_prefill_peak`) moved to `concurrency` and is reusable
by any in-process backend. Admission env renamed to generic `ROZUM_ADMIT` /
`ROZUM_ADMIT_FASTLANE_TOKENS` / `ROZUM_ADMIT_QUEUE_MAX`. `build_gateway_backend`
routes every selected backend through `admit_wrap`. 13 concurrency unit tests on
the default build (no Xcode); feature build + fmt clean. The new mlx-rs backend is
the first intended consumer: implement inference + return a capacity, get
admission/fast-lane/backpressure/breaker for free.

## concurrency-load-shedding — backpressure + OOM circuit breaker (Phase D)
Completed: 2026-06-11
Final phase of `mistralrs-concurrency-scheduling`. `AdmissionScheduler.admit`
now returns `Result<AdmitGuard, AdmitError>`: a full wait queue
(`ROZUM_MISTRALRS_QUEUE_MAX`, default 32, 0=unbounded) sheds with `Overloaded`.
`MistralrsBackend::chat()` acquires the slot before returning the stream, so an
overloaded backend surfaces as a genuine HTTP 429 + `Retry-After` (new
`ModelError::Overloaded`, mapped in the gateway for both the OpenAI and Anthropic
dialects). Circuit breaker: `trip()` lowers the live admission limit (floor 1) on
a detected Metal allocation failure and a 30 s cooldown `recover_step()` raises
it back toward capacity; the OOM'd request is surfaced (not auto-retried, to
avoid re-OOM) and detection is best-effort substring matching. Per-class
`max_tokens` was dropped as redundant (cost already weights `max_tokens`). 7
scheduler unit tests (no Xcode); feature build + fmt clean. This completes the
concurrency feature (A+B+C+D); follow-ups — chiefly `concurrency-engine-yield`
for true mid-prefill interleaving — are in BACKLOG.

## concurrency-admission — admission scheduler + fast lane (Phase B+C)
Completed: 2026-06-11
Second phase of `mistralrs-concurrency-scheduling`. New engine-agnostic
`src/mistralrs_admission.rs`: an `AdmissionScheduler` that gates actual
concurrency in front of the static engine `max_num_seqs`, with a runtime
`set_limit` (for Phase D), shortest-job-first queue ordering, and one reserved
fast-lane slot so short interactive requests jump ahead of queued big ones.
`admit(RequestCost) -> AdmitGuard`; the guard is held for the whole `chat()`
stream and releases the slot on completion/disconnect, waking the next waiter
(dead/cancelled waiters are skipped and their slot reclaimed). Config from
`ROZUM_MISTRALRS_ADMIT` (limit ≤ capacity) and `ROZUM_MISTRALRS_FASTLANE_TOKENS`
(default 1024, 0 off). 5 async unit tests, no Xcode needed; feature build clean.

Finding recorded: the fork does **not** yield between prefill chunks (chunking
is internal to `pipeline::step`), so the fast lane gives admission-order
responsiveness but not mid-big-prefill preemption — engine-yield filed as
`concurrency-engine-yield` in BACKLOG. Phase D (backpressure + circuit breaker)
remains.

## concurrency-budget — load-time budgeted engine max_num_seqs (Phase A)
Completed: 2026-06-11
First phase of `mistralrs-concurrency-scheduling`. Replaces the total-`hw.memsize`
1/2 ladder with a footprint budget: `budgeted_max_num_seqs(ConcurrencyBudget)`
(pure, in the lib) returns `clamp((0.8·available − weights − kv_pool) /
per_seq_peak, 1, ceiling)`, where `per_seq_peak = prefill_chunk × ~465 KB/token`
(constant under chunked prefill) and `ceiling` defaults to 8 (Metal is one GPU —
past a handful of concurrent prefills you gain tail latency, not throughput).
`resolve_max_num_seqs` in `main.rs` gathers the footprint from the existing
preflight helpers and applies env overrides (`ROZUM_MISTRALRS_MAX_SEQS` forces,
`ROZUM_MISTRALRS_SEQS_CEILING` caps, `MISTRALRS_PREFILL_CHUNK` sizes the per-slot
cost), logging a `concurrency_budget` obs event. `MistralrsOptions::default()`
now carries a plain serialised floor of 1. 6 lib unit tests (no Xcode), feature
build clean. Phases B+C (admission scheduler + fast lane) and D (backpressure +
circuit breaker) remain in SPRINT.md.

## mistralrs-adaptive-concurrency — memory-adaptive default for max_num_seqs
Completed: 2026-06-11
The mistralrs backend's concurrent-prefill cap (`max_num_seqs`) default is no
longer a fixed `1`. A new pure `default_max_num_seqs(total_ram)` policy keeps
the serialised `1` floor on the 24–36 GB Apple Silicon target band (where two
concurrent large-prompt prefills can OOM the Metal command buffer) and lifts it
to `2` on machines with ≥ 48 GB total unified memory, where PagedAttention +
chunked prefill + the disconnected-seq reaping fix make real concurrency safe.
The gate is on total `hw.memsize` rather than instantaneous free memory (which
over-predicts runtime headroom at load time). `ROZUM_MISTRALRS_MAX_SEQS`
overrides. Rationale + trade-offs documented in
`docs/specs/mistralrs-backend.md`.

## web-basic-auth — HTTP Basic Auth on the web bridge
Completed: 2026-06-06
The web bridge now requires HTTP Basic Auth for `/`, `/ws`, and `/transcript`.
The password must equal the room name; the username is unconstrained and is
used as the participant's alias in the chat. The server stamps every outgoing
`meeting.submit` with the authenticated alias regardless of any client-supplied
`name` field, so a tampered client cannot post under a different name. The
auth username is sent to the client via a new `{kind:"hello",name:...}` WS
envelope right after connect; the page-side name input is removed.

## tui-soft-wrap — soft-wrap long input lines in the TUI
Completed: 2026-06-06
Custom render of the input area: `tui-textarea 0.7` still holds the data and
processes input events, but its renderer is bypassed. `draw_input` builds
visual rows by wrapping each logical line at `inner_width` and places the
cursor manually via `f.set_cursor_position`. Autosize now counts wrapped
visual rows, so a single long line grows the input chunk upward instead of
scrolling horizontally.

## mcp-proxy-auto-mark — auto-emit mark_responding from mcp-proxy
Completed: 2026-06-06
`ProxyState` gained a `heartbeat_task` handle. When `meeting.wait_my_turn`
returns `your_turn:true`, the proxy fires an immediate `meeting.mark_responding`
and spawns a background task that refreshes it every 15 s. The task is aborted
on the agent's next `submit`/`leave` and on a fresh `your_turn:true` (which
restarts the heartbeat). Manual `meeting.mark_responding` calls from the agent
still work and refresh the timer identically.

## mcp-proxy-reconnect — transparent reconnect of mcp-proxy after rozum restart
Completed: 2026-06-06
`ProxyState` remembers the joined room name; `call_room_tool` now
catches transport failures and calls a new `try_reconnect_current_room`
that sleeps a capped backoff (`200ms…5s`, ~18 s total) waiting for the
Unix socket to reappear, reconnects, re-issues `_join_internal` with
the same display name, and retries the original tool call. The agent's
MCP session no longer sees `Transport closed` during a `rozum --room R`
restart.

## room-transcript-persist — room transcript persisted across rozum restarts
Completed: 2026-06-06
`Meeting` gained `persist_path: Option<PathBuf>` and an
`enable_persistence` method that loads
`$XDG_STATE_HOME/rozum/rooms/<name>/room-transcript.jsonl` on
construction and re-numbers seq. `post_submission` appends every Turn
as one JSON line. A new top-level `--no-persist` flag disables both
(independent of the existing `rozum web --no-persist`). Web bridges
pick up the loaded history through their normal
`wait_my_turn(since_seq:0)` path. With `rozum --room R` the same room
name reopened after a restart resumes with full transcript intact.

## web-transcript-persist — bridge transcript persisted to disk
Completed: 2026-06-06
The web bridge now appends every `msg` envelope to
`$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl` (one JSON line per
turn). On startup the bridge loads the last `TRANSCRIPT_CAP=2000` lines back
into the in-memory ring so a page reload after a rozum restart still shows
recent history. A new `--no-persist` flag on `rozum web` disables both the
write and the load. Client-side deduplication now keys on `(seq, ts)` so
persisted entries from earlier sessions — where seq numbering restarts — do
not collide with current-session entries.

## web-transcript-history — transcript replay on connect + lazy older-history paging
Completed: 2026-06-06
The web bridge keeps a bounded in-memory transcript ring (cap 2000). A new
`GET /transcript?from_seq=&limit=` REST endpoint returns slices for paging.
On WebSocket connect the bridge sends a `kind:"history"` envelope with the
last 200 entries; the client replays them through the normal append path with
seq-based deduplication. Scrolling within 60 px of the log top triggers a
fetch of the next older 200 entries and prepends them while preserving the
viewport. `web-transcript-persist` (separate slug) will lift the in-memory
2000 cap by reading from `transcript.jsonl`.

## tui-arrow-scroll — Arrow Up/Down always scrolls the transcript
Completed: 2026-06-06
Dropped the `textarea.lines().len() <= 1` guard so the Up/Down arrows scroll
transcript history even when the input area is multi-line. Textarea cursor
navigation moves to `Ctrl+Arrow` / `Home` / `End`. Per operator request.

## tui-autosize-input — TUI input area grows with multi-line composition
Completed: 2026-06-06
Replaced fixed `Constraint::Length(3)` with a dynamic
`(textarea.lines().len() + 2).clamp(3, max(3, area.height/3))` so the input
area grows upward when the user enters multi-line content via `Alt+Enter`.
Up/Down arrows now scroll the transcript history (in addition to PgUp/PgDn).
Soft-wrap of a single overflowing line is **not** in this slug — split into
`tui-soft-wrap` because `tui-textarea 0.7` has no native wrap.

## web-scrollback-sticky — sticky-bottom scroll, "↓ N new" pill, long-message collapse
Completed: 2026-06-06
`#log` now tracks `data-stick` on scroll; new messages auto-scroll only when
the user is within 40 px of the bottom, otherwise a sticky `↓ N new` pill
appears and clicking it snaps to bottom. Messages whose body exceeds 6 wrapped
lines or 600 characters render collapsed with an `[expand ▾]` / `[collapse ▴]`
toggle. Pure client-side change in `src/web/index.html`.

## web-presence-row — presence row, joined/left, tagged envelopes for the web bridge
Completed: 2026-06-06
`src/web/mod.rs` `room_loop` now emits tagged JSON envelopes
(`kind:"msg"|"presence"|"joined"|"left"`) instead of raw transcript JSON.
`src/web/index.html` dispatches on `env.kind`: presence line above the input
with `✏️` / `⏳` glyphs, header chips for participants, dim system lines for
join/leave. Display names are rendered with `textContent` (no innerHTML) so
they cannot inject HTML.

## web-autosize-input — Claude-style autosizing textarea in the web client
Completed: 2026-06-06
Replaced the single-line `<input id="msg">` with a `<textarea rows="1">` that
grows upward on input up to `30vh` (`20vh` on mobile). `Enter` sends,
`Shift+Enter` inserts a newline, `Esc` clears, no horizontal scroll, collapses
back to one row after send. Verified live by the operator.
