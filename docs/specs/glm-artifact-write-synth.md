# GLM artifact → Write synthesis

**Goal.** Make GLM-4-32B-0414 a full agentic driver — including **create-from-scratch** — by
synthesizing a `Write` tool call when GLM emits a labeled file **artifact** instead of *naming* the
tool. Today GLM is reliable for edit/debug/chat (it names `Read`/`Edit`/`Bash` cleanly + the shipped
logit-constraint `99c6081` makes args schema-valid), but on create-from-scratch it shows file content
in fenced blocks (`Cargo.toml` / `src/main.rs`) and names nothing → no tool call → `turns=1 tools=0
pass=0`. This is a **GLM-4-0414 model decision property**, proven NOT prompt-induced (claude's captured
system prompt has zero narration framing and pushes toward tools — see `glm4-bringup.md` § ROOT CAUSE),
so it can't be fixed prompt-side without regressing edits. The synth meets the model where it is.

Precedent: codex's `synthesize_write_from_obj` (gateway.rs ~1982) already does this for a structured
`{path, content}`. The GLM case is harder because the artifact is **unstructured free text** — the
synth must recover the file PATH from how GLM labels each block.

## REAL artifact captured (2026-06-23) — the format is now known
Captured via `agentic.sh KEEP=1` (claude×GLM-4-32B×build, the real condition: `turns=1 tools=0`,
35s). Raw at `/tmp/glm_artifact_agentlog.json`. GLM's actual output:
```
First, I'll create the Cargo.toml file:
` ` `toml
[package]
name = "reverse-cli"
version = "0.1.0"
edition = "2021"
` ` `
Now, I'll create the src/main.rs file with the code ...:
` ` `rust
use std::env;
fn main() { ... }
` ` `
Now I'll run the program with the command "cargo run -- hello":
` ` `bash
cargo run -- hello
` ` `
```
**The real format (overrides the speculative matchers below):**
1. **Filename lives in the PRECEDING PROSE sentence** — "I'll create the **Cargo.toml** file:", "the
   **src/main.rs** file" — NOT a fence info-string, NOT a `path:` label, NOT a first-line comment.
2. **Fence info-string = the language** (`toml`/`rust`/`bash`), not the path.
3. **Command fences exist and must be excluded** — the ` ```bash ` block is `cargo run -- hello` (a
   command to RUN, not a file). Its preceding prose has NO filename → the "recoverable filename in
   preceding prose" guard naturally excludes it. (Optionally map such a block to a `Bash` tool call.)
4. GLM's content is CORRECT (valid Cargo.toml + main.rs) — it just narrates + shows instead of calling
   `Write`. So a synth that recovers (filename, fence-body) genuinely fixes the cell.

**⇒ Extractor = for each fenced block, scan the immediately-preceding prose (last ~1-2 lines/sentence)
for a filename token** (`Cargo.toml`, `src/main.rs`, or `[\w./-]+\.(rs|toml|md|txt|json|py|toml|cfg|lock|sh)`;
backtick-quoted or bare). Found + path-safe ⇒ `Write{file_path, content=fence body}`. No filename ⇒ skip
(handles the command fence). This is the matcher to build + unit-test against the captured fixture.

## Status: SPEC + plan only — DO NOT build the parser blind
The phenomenon (GLM prints labeled file artifacts on create) is real (documented from prior live
transcripts), but the exact label FORMAT is unverified — there is no real GLM create-from-scratch
output on disk (gateway logs are startup-only; agent work dirs weren't kept). **Building a format
parser against assumptions is exactly the strawman mistake that sank the narration-framing sanitizer**
(`glm4-bringup.md` § Real A/B). So: capture real GLM output FIRST (slot-gated), then build the parser
against it.

## Design

### Trigger guards (ALL must hold — false-positive prevention is the whole game)
A chat answer that merely *includes* an example code block with a filename mention must NEVER be
written to disk. Synthesize ONLY when:
1. **GLM family** — `dialect.uses_glm_envelope()` (no effect on Qwen/gpt-oss/other models).
2. **A file-write tool is offered** — the request's tools include `Write` (or the active toolset's
   create-file tool); gives the exact tool name + confirms the agent *can* accept the call.
3. **No tool call was parsed** — `serving::parse_tool_calls(text)` returned empty (GLM named nothing).
   On edit tasks GLM names `Edit` → a call exists → synth never fires → zero edit regression.
4. **Artifact-dominant + recoverable path** — ≥1 fenced block whose file path is recoverable AND the
   fenced content is the bulk of the response (not prose with one incidental snippet). Reject if the
   only fence has no path, or the response is mostly prose.
5. **Path safety** — recovered path is a *relative* file path, no `..`, no absolute paths; else skip.

### Extraction (format matchers — CONFIRM against real samples before trusting)
Parse labeled fenced blocks → `(path, content)` pairs; emit one `Write{file_path, content}` per block
(multi-file create = multiple Writes). Candidate label formats to support (ranked, confirm which GLM
actually uses):
- (a) **Preceding label line**: `` `Cargo.toml`: `` / `Cargo.toml:` / `**src/main.rs**` immediately
  before the fence.
- (b) **Fence info-string**: ```` ```rust:src/main.rs ```` or ```` ```Cargo.toml ````.
- (c) **First-line path comment** inside the fence: `// src/main.rs`, `# Cargo.toml`.
- (d) Inline imperative: "create `src/main.rs`:" on the lead-in line.
The extractor is a list of independent matchers; adding the real-sample format = adding one matcher +
its fixture. Content = the fence body verbatim (minus the path-comment line for (c)).

### Plumbing (integration point)
`serving::parse_tool_calls(&self.full_text)` is called at generation-finish
(`mlx_native_backend.rs` ~2115) and only takes `text` — it has neither the offered tool names nor the
GLM-family flag in scope. Thread both to the finish path:
- the request's tool names (already known where the job is built),
- the GLM-family flag (already computed: `dialect_for(template).uses_glm_envelope()`, see
  `mlx_native_backend.rs:997`).
Then: `if calls.is_empty() && glm && has_write_tool { calls = synth_glm_writes(text, &tool_names) }`,
and the existing loop (2120-2131) emits `ToolUseStart/Delta/End` for the synthesized calls unchanged —
so both the OpenAI and Anthropic streaming paths get real `tool_use` blocks with no extra wiring.

### Flag
`ROZUM_GLM_ARTIFACT_SYNTH` — **default OFF (opt-in)** until the live A/B confirms a lift with no
regression. Same discipline as every other GLM lever here.

## Validation plan (slot-gated — the gate on shipping)
1. **Capture real output** (model-only, no agent): load GLM-4-32B, `curl /v1/chat/completions` with a
   create-from-scratch task + a `Write` tool, capture the raw `content`. Confirms the actual label
   format(s) → becomes the unit-test fixtures. (Claim the slot per the 🛑 REBOOT-SAFETY PROTOCOL; one
   model; graceful teardown.)
2. **Build + unit-test** the matchers against the real fixtures (offline, deterministic).
3. **Live A/B**: `agentic.sh` claude×GLM-4-32B, synth OFF vs ON, on `build` (create) AND `fix` (edit):
   - create cells: pass-rate must LIFT (the win), tools>0.
   - edit cells: must NOT regress (guard #3 ⇒ they shouldn't even trigger the synth).
4. **False-write fuzz**: feed GLM chat prompts whose answers contain example code blocks; assert the
   synth does NOT fire (guard #4). Any false write = block shipping.
5. **Decision gate**: default-ON only if (3) lifts create + zero edit/chat regression + (4) clean.
   Otherwise stays opt-in / backlog (the clean workaround — Qwen3.6-35B for create — already covers
   the need).

## Risks
- **False synthesis** (chat code → file write) — the dominant risk; mitigated by the 5 guards +
  default-OFF + the fuzz gate.
- **Inventing a call the model didn't make** — unlike codex's structured case; acceptable only because
  the guards make it fire just on clear, path-labeled, tool-available create turns.
- **Path ambiguity** — if GLM doesn't label a recoverable path, the synth can't fire (guard #4) and
  the cell stays a miss (no worse than today). That's the floor, and it's safe.

Pairs with `glm4-bringup.md` (decision gap, ROOT CAUSE), `project-codex-patch-barrier`
(synthesize_write_from_obj precedent), BACKLOG `glm-artifact-write-synth`.

### RESOLVED — synth works end-to-end, GLM create-from-scratch PASSES (2026-06-23)
Two bugs found + fixed; final live cell `claude×GLM-4-32B×build` = **pass=1, turns=6, tools=3** (was
turns=1 tools=0 pass=0).

1. **Firing bug — wrong finalize path.** The synth was wired into `BatchSeq::finalize` (the mlx
   batch/hybrid path), but GLM-4 is a **DENSE arch** → a single request finalizes in the
   native-engine-spi seam `engine::consume_tokens`, never reaching BatchSeq. Fix: thread the gate +
   tools through `EngineMeta { glm_synth, tools }` (set in the mlx dense dispatch) and run the synth
   in `consume_tokens`'s finalize. (Batched GLM still synthesizes in BatchSeq.) `finalize_dbg`
   instrumentation pinned this: the event never fired → finalize never ran on that path.

2. **GLM emits MALFORMED JSON.** Its tool-args objects are doubly broken: it closes with `]` instead
   of `}`, AND leaves UNESCAPED inner quotes in `content` (`println!("{}", x)`). `balanced_json_objects`
   couldn't extract them → mode-2 missed → mode-1 wrote the raw JSON wrapper INTO the file. Fix: a
   single fence-aware pass (json-fence→mode-2, content-fence→mode-1, so they never cross-contaminate)
   + `parse_tool_args_lenient` (strict → bracket-repair → key-by-key extraction reading `content` to
   the LAST quote, tolerating unescaped inner quotes). Unit-tested against the real captured fixtures
   (well-formed + `]`-malformed + unescaped-quote). rozum-core 113 green.

**Status: opt-in (`ROZUM_GLM_ARTIFACT_SYNTH=1`), PROVEN on one cell. Recommend a multi-rep A/B
(create lift + edit/chat no-regression — earlier fix=2/2) before flipping the global default.** GLM
is now usable for create-from-scratch with the synth enabled; the model's JSON is messy but the
tolerant parser absorbs its two common malformations.
