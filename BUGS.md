# Bugs

## BUG-058 — a tool-call body generates in silence, and the inactivity watchdog kills it as a wedge

- **Status:** FIXED 2026-08-16 (`ChatEvent::Progress` + the tick in `BatchSeq::push`).
- **Severity:** P1 on slow models. It aborts CORRECT, in-progress answers, and the symptom reads as
  "the model is slow" or "the model wedged" — it was mis-read as both, twice, before being found.

This is the open half BUG-057 left. `BatchSeq::push` streams the growing text until the
`<tool_call>` marker appears, sets `tool_seen`, and from then on the whole `if !self.tool_seen`
block is skipped — nothing is emitted again until `finalize()`. That is correct for the WIRE (the
markup and its JSON are not user-visible text) and wrong for LIVENESS: `serving::with_gen_timeout`
bounds the gap between EVENTS, so a model writing a long tool call is indistinguishable from a model
that has stopped, and gets aborted mid-answer.

**The arithmetic is what makes it a P1.** Those same requests logged 2.2-3.1 tok/s. At 3 tok/s the
120 s inactivity window is **~360 tokens** — and a `Write` carrying a whole source file is routinely
more than that. So on a slow model, a class of correct answers could not be delivered at all.

**Observed (Qwen3.5-9B, `duration`, from `~/.rozum/gateway.jsonl`):** ttft 26.5 s, exactly 14 output
tokens of preamble, then silence to the 120 s timeout, `stop_reason: incomplete` — four times over
on a byte-identical prompt, 500 s of a 900 s run.

**Fix.** A `ChatEvent::Progress` variant: a liveness tick carrying nothing, emitted per token while
the tool body accumulates. The watchdog counts it, the meter ignores it (no ttft, no token count),
and every wire encoder drops it. The blast radius was measured rather than guessed before choosing
this over an empty `TextDelta`: eight non-exhaustive matches across four files, each given an
explicit arm saying why it renders nothing.

**Regression test:** `a_tool_body_being_written_is_not_mistaken_for_a_wedge` — a stream ticking
every 20 ms across a window of 60 ms reaches `Done` un-aborted and un-cancelled, and the same gap
with no ticks still aborts. The second half matters as much as the first: the watchdog exists for
real wedges (BUG-055's hangs) and must keep biting.

**Why it took three passes to find.** First reading: "the 9B is too slow to finish agentic work"
(a mean over a bimodal distribution — BUG-055). Second: "the model did the work and would not stop"
(the no-stop-after-success loop — it was one prompt re-sent, BUG-057). Only reading the per-request
events showed 14 tokens and then nothing, and only then did the token-emission loop explain why 14.
Each reading was of the same runs; the difference was how closely the evidence was looked at.

## BUG-057 — a request that stalls in decode is re-sent unchanged, and each retry costs a full inactivity timeout

- **Status:** FIXED 2026-08-16 (`serving.rs`: `request_signature` + `STALLED` tracker; the third
  identical stall is answered rather than run).
- **Severity:** P2 for agent runs. Nothing breaks; the clock is eaten and the cause looks like a
  slow model. It is also OPEN in its other half — see "what this does not fix" below.

The 9B `duration` cell that finished at the full 900 s harness limit, from `~/.rozum/gateway.jsonl`:

```
17:08:44  request_start  id 13  messages 27  est_prompt_tokens 8526  seed 1235
17:11:12  generation_timeout {inactivity_secs: 120}
17:11:12  request_done   id 13  ttft 26465ms  output_tokens 14  duration 147963ms  incomplete
17:11:12  request_start  id 14  messages 27  est_prompt_tokens 8526  seed 1235   ← identical
17:13:42  request_done   id 14  ttft 28362ms  output_tokens 14  duration 149827ms  incomplete
17:13:43  request_start  id 15  … identical …
17:16:13  request_done   id 15  ttft 28949ms  output_tokens 14  duration 150147ms  incomplete
17:16:14  request_start  id 16  … identical …
17:17:09  request_done   id 16  ttft 29566ms  output_tokens 14  duration  54356ms  incomplete
```

Requests 13-16 are the same body. Each one prefills fine (ttft ~27 s), emits **exactly 14 tokens**,
then produces nothing for 120 s; `with_gen_timeout` aborts it correctly, the client sees a stream
that ended early and re-sends the identical request. **500 of the 900 seconds went to re-running one
wedged decode**, on a cell whose work had been finished at request 12 — which is why it still
scored a PASS.

**This was mis-read first, and the mis-reading is the point.** From the outside it looked like "the
model did the work and then would not stop", i.e. the no-stop-after-success loop signature 4 exists
for. It is not that: there is no loop over tool calls, there is one prompt that wedges and a client
doing the only thing it can.

**Fix.** `chat_or_loopbreak` fingerprints the conversation and counts consecutive stalls for that
fingerprint (`with_gen_timeout` feeds the counter: a timeout increments it, a stream that ran to its
end clears it). Two stalls are tolerated — a wedge can be transient, and refusing the first retry
would turn a blip into a failure. The third is not run: the gateway answers with a synthetic stop
that says the request stalled twice, not to resend it unchanged, and that the work may already be
on disk. One slot rather than a map, because a stalled prompt is re-sent immediately and a different
conversation arriving is itself the reset.

**What this does not fix, and is now the open half:** why the decode wedges after 14 tokens on that
prompt, reproducibly, four times. The 9B was running with its MLX cache adaptively reduced to 1 GiB
on a host under memory pressure, which is the shape `with_gen_timeout`'s own doc comment describes
("a Metal eval wedged under memory pressure blocks inside one FFI call"). Not investigated further
here; the retry storm was the part costing whole runs.

**Regression test:** `an_identical_request_that_keeps_stalling_is_answered_not_re_run` — a backend
that emits a few tokens then never another, run three times; the first two time out, the third is
answered, and a different prompt still runs. Verified red without the guard.

## BUG-056 — signature 3 counts an edit that FAILED, so a refused no-op edit becomes the third strike

- **Status:** FIXED 2026-08-16 (`loopbreak.rs`: the signature-3 loop iterates `calls` and skips
  `is_error`, the rule signature 5 already used).
- **Severity:** P2. Same family as BUG-054 — a second way to count a non-edit as an edit — but it
  survived that fix because BUG-054 was about which INPUTS look like edits, and this is about
  whether the edit happened.

Found by looking at why the 9B loses, which the operator asked for after the three-rep run. Its
only `duration` loss, from the transcript:

```
[5] Edit  src/lib.rs   ← the implementation. CORRECT: days + the all-zero case      ok
[6] Edit  src/lib.rs   ← extends the test module                                     ok
[7] Bash  cargo test   ← red, because the test IT wrote expects "0d 1h 1m 1s"
[8] Edit  src/lib.rs   ← old_string == new_string
    → <tool_use_error>No changes to make: old_string and new_string are exactly the same.
    >> "…has been edited 3 times, re-doing and undoing the same change…"
```

Call 8 changed nothing — the tool refused it. Counting it made two forward edits into three, and
the run ended on a workdir whose implementation was already correct on all eight verifier values.

**Measured across every signature-3 stop of the day:** 6 stops, of which **2** were triggered by a
failed call, both with this exact shape (an identical-strings Edit). Excluding failed calls leaves
2 real edits in each — below the threshold, no stop. The other 4 are genuine content churn and
still fire.

`cargo test` being red there is the model's own doing: the test it added asserts a zero day
component that the stated rule forbids. That failure is real and stays. What was ours is ending
the run before it could act on it.

Signature 1 is the signature for repeated FAILING calls and wants three in a row; signature 3 is
about the file's content going in circles, and a call that changed nothing cannot be evidence of
that.

**Regression test:** `an_edit_that_errored_is_not_one_of_the_three`, transcribed from that
transcript, verified red against the old behaviour.

## BUG-055 — nothing bounds the wait for a backend to produce a stream, so a wedged prefill hangs the request until something else gives up

- **Status:** FIXED 2026-08-16 (`gateway.rs`: `start_chat_bounded`).
- **Severity:** P1 for any long-running agent. The request does not fail, it waits — so the
  symptom shows up as "the model is slow" and gets recorded against the model.

`with_gen_timeout` bounds the gap between two backend EVENTS. It cannot bound the wait for the
stream itself, because it only exists once `chat_or_loopbreak(...).await` has returned. A backend
that wedges before its first event — prefill stalled inside one FFI call, a worker handshake that
never completes — therefore had nothing bounding it at all.

**Found by re-reading two runs I had already drawn a conclusion from.** Both Qwen3.5-9B `duration`
cells, per-turn gaps from the transcript timestamps:

```
run 1 (RUN_TIMEOUT 1200s):  1.7 12.9 12.4 12.6 13.3 15.0 … then 1115.9
run 2 (RUN_TIMEOUT 1800s):  1.6 13.3 13.1 12.9 14.1 15.4 15.4 21.8 2.0 … then 1673.9
```

The model answers in 13–22 s, consistently. Then one turn never comes back, and the run ends on the
harness's RUN_TIMEOUT — never on `ROZUM_GEN_TIMEOUT_SECS=120`, which was set and working. That the
gap is unbounded rather than ~120 s is the proof it lies outside the stream guard.

**The conclusion this overturns is mine, and it was wrong twice over.** I recorded "eight tool calls
in thirty minutes is ~3.7 min a round trip" and "on a task needing eight to ten tool round trips the
9B does not finish on this host". Dividing a total by a call count instead of looking at the
distribution hid a bimodal shape completely — and the one number that mattered was not the model's
at all. See `BACKLOG.md`, corrected in the same commit.

**Fix.** `start_chat_bounded` wraps the creation await in the same ceiling the stream gets, cancels
the request on the way out so the worker can abandon the job the moment it unblocks, and returns
`ModelError::Timeout` (HTTP 504) instead of waiting. A zero ceiling disables it, exactly as it does
for `with_gen_timeout`, so an operator who switched the bound off does not get a surprise 504 here.

**Regression tests:** `a_backend_that_never_returns_a_stream_is_bounded_too` (a `chat()` that awaits
`pending::<()>()` forever — verified to hang the test harness itself without the fix) and
`a_zero_ceiling_leaves_stream_creation_unbounded`.

## BUG-054 — the loop-breaker counts a `Read` as an edit and a `}` as a ping-pong, and aborts working runs before they can verify

- **Status:** FIXED 2026-08-16 (`loopbreak.rs`: `edit_target_and_lines` requires an edit payload;
  `is_pingpong` / `is_substantive` replace the single-shared-line test).
- **Severity:** P1. It fires on the most ordinary sequence there is — read a file, then edit it
  twice — and what it does when it fires is stop the agent and tell it to report in one line. Every
  such run is then scored as a model failure. Ours, not the model's.

Found by asking why the 4B failed the new `duration` bench task. It had not. All three cells:

```
[1] Bash  find . -name "*.rs"
[2] Read  src/lib.rs
[3] Edit  src/lib.rs   ← the implementation fix. CORRECT: days + the all-zero case
[4] Edit  src/lib.rs   ← extends the test module
    >> "The file src/lib.rs has been edited 3 times, re-doing and undoing the same change
        without net progress ... Stopping to avoid corrupting the file in a churn loop;
        verify it builds and report in one line."
```

The model never ran `cargo test`. It never saw that its edit to the test module had broken an
assertion. It "reported in one line" because we told it to. The false success and the red suite in
those cells are both downstream of this message.

**Two independent defects, either alone enough to misfire.**

*A `Read` counted as an edit.* `edit_target_and_lines` returned `Some` for any input carrying a
`file_path`, and `Read` carries one and nothing else. Read + two edits = 3 against a threshold of
3. The message even said "edited 3 times" when the transcript shows two.

*The ping-pong proof was a closing brace.* The guard was `added.iter().any(|a| seen.contains(a))`
over whitespace-trimmed lines. Edit 3 removed the old function body, which ends in `}`; edit 4
added a test module, which also contains `}`. Replayed against the real transcript:

```
3. Edit removed= 17 added= 25  ping-pong=no
4. Edit removed= 13 added= 23  ping-pong=YES -> ['}', '}', '}']
```

**The fix keeps real churn firing.** An edit is now a call that carries an edit payload
(`old_string`/`new_string`/`content`/`edits`). A ping-pong now needs its re-introduced lines to be
**substantive** (containing at least one alphanumeric character — `}`, `{`, `});` are punctuation
that closes whatever came before and appear in every edit to a braced language) and to be at least
**half of what the edit adds**. The share is what separates the two cases: real churn re-applies a
version of the same thing, so nearly all of what it adds is what it just removed, while an edit
adding twenty new lines that repeats one is doing new work. A share rather than a line count,
because the genuine one-line toggle already in the suite (`collect::<String>()` ⟷ `collect()`) is
1 of 1 and must still fire — and does.

**Measured after the fix, same task, same model, same three reps:**

| | before | after |
|---|---|---|
| pass rate | 0/3 | **1/3** |
| turns / tool calls | 9-11 / 5-6, all aborted at call 4 | 13-18 / 7-11 |
| reached `cargo test` | never | every run |

One clean pass. One run with all eight durations correct, every seeded assertion intact, and a
red suite from a test the model wrote itself (`format(86399) == "1d 23h 59m 59s"` — 86399 is one
second SHORT of a day). One run where the implementation was genuinely wrong, shifting every unit
by one (`59` rendered `59m`). Those last two are the model's, and now they are measurable, which
they were not while the harness was stopping the run at call four.

The verifier line was corrected alongside: it used to call both defects "the seeded tests were
broken", and after the fix that was actively misleading — it said a seeded assertion had been
changed when none had. It now checks the three seeded assertions verbatim and says which case it
is. Both branches verified against the kept evidence of the runs that produced them.

**Regression test:** `gateway::tests::reading_a_file_then_editing_it_twice_is_not_churn`, the
sequence transcribed from the transcripts, plus one assertion per defect so neither can be quietly
restored. Verified to fail against each old behaviour separately. Both pre-existing churn tests
still pass.

## BUG-053 — an idle-unload frees the model's RAM but keeps its residency reservation, so no other gateway may use the memory it just released

- **Status:** FIXED 2026-08-16 (`switchboard.rs`: `residency_to_publish` / `republish_residency`;
  `share.rs`: `readmit_my_reservation`). Found by reading the ledger after BUG-052's fix finally
  made idle-unload reachable — the unload worked, and then nothing could use the result.
- **Severity:** P2. Not a crash: a correctness hole in the one mechanism that decides whether the
  host gets overcommitted. It makes idle-unload a no-op for its stated purpose.

The reservation was tied to the **process**, not to the model. `acquire_residency` hands back a
`ResidencyGuard` whose `Drop` removes `residents/<pid>`, and `main.rs` binds it to `_residency` for
the lifetime of the daemon. `Switchboard::unload` drops the backend — and touches nothing else. So
the daemon lives on, still publishing the footprint of a model it no longer has.

Every republish path had the same shape from the other direction: they derived the primary's
footprint from its **model id** via `warm_cfg.weight`, which answers whether or not the weights are
in memory. There was no path in the code that could ever publish "loaded: no".

**Observed on the live job (:8089, Qwen3.5-4B, 2026-08-16):**

```
/stats   loaded: False   mlx_memory_mb.active: 0
/stats   resident_models: ['mlx-community:Qwen3.5-4B-MLX-4bit']
~/.local/state/rozum/gateway/residents/82685
         {"model":"mlx-community:Qwen3.5-4B-MLX-4bit","footprint_bytes":7892971128,"prio":0}
```

7.35 GiB of real memory back in the host's hands, and 7.35 GiB still on the ledger telling every
other gateway it was taken. A sibling asking for that RAM waits out `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS`
and then refuses, against a machine that has the memory free.

**The fix is two halves and neither works alone.** Publishing the drop (`republish_residency`, the
primary counted only while `backend.is_some()`) is what makes the freed RAM allocatable. But
whatever we give back, a sibling may take — so the lazy reload must ask again before it allocates
(`readmit_my_reservation`, deciding and republishing under the admit lock, refusing with a 503 when
the host is now committed elsewhere). Lowering without re-admitting would be a straight overcommit
hole: exactly the jetsam→reboot cascade of BUG-003, arrived at from the other side.

`readmit_my_reservation` is not `acquire_residency`: that one *creates* a reservation and would
contend with our own live lock, and it joins the waiter queue — right for a new arrival, wrong for a
process resuming RAM it already had, where the queue would turn a 1.1 s warm reload into a stall.

**Regression tests, both verified to fail against the old behaviour** (`Some(8 GiB)` where `Some(0)`
is required; a refused re-admit admitted anyway):
`gateway::tests::unload_gives_the_ram_back_on_the_ledger_and_the_reload_takes_it_again`,
`share::tests::readmit_refuses_when_another_gateway_took_the_freed_ram`.

### Self-caught the same day: the re-admit refused on free RAM and took the chat down

The first cut ran the reload through the full `admits`, free-RAM lever included. Twenty minutes
later, live:

```
POST /v1/chat/completions -> 503
{"error":{"message":"model was unloaded to free RAM and the host is now committed elsewhere"}}

available = total - (wired 5221 + anon 21445 + compressor 3098) = 7101 MB
demand    = footprint 7527 + min_free 2048                      = 9575 MB
```

The host had the room. `footprint` is a padded startup estimate — weights + KV at full `n_ctx` +
cache reserve — and the same model's MLX **active** memory measures 2893 MB once loaded, 2.6× less.
Sizing a load to the host is `adapt_n_ctx_to_fit`'s job on the way in, and it was already doing it;
this gate had no business re-deciding it with a worse number, on a live request.

Free RAM was never a gate on the reload path before this function existed, so adding one fixed
nothing and broke the chat. `readmit_my_reservation` now checks the two levers that the release
actually opened: the **ledger** (did a sibling claim the RAM we published as free?) and **host
pressure** (the kernel's jetsam signal, the no-reboot invariant). Pinned by
`share::tests::readmit_does_not_gate_on_transient_free_ram`, which fails the old way round.

**Shipped alongside:** the idle-unload default is now **300 s**, not 900 (operator's call). A warm
reload measured **1.1 s** end-to-end — the weights are still in the OS page cache, MLX mmaps them —
so the threshold trades about a second of latency against ~7.4 GiB, and ten further minutes of
holding is not worth it. The durable plist no longer pins `ROZUM_GATEWAY_UNLOAD_IDLE_SECS` at all:
it said 1200 while the code said 900, which is the two-copies-one-goes-stale failure this repo keeps
paying for. One number, in `unload_idle_secs`.

## BUG-052 — idle-exit fires before idle-unload, so a supervised gateway reloads its model every 15 minutes instead of releasing it

- **Status:** FIXED 2026-08-16 (`gateway.rs`: `idle_exit_after` — a gateway that can reload does not
  idle-exit). Surfaced by BUG-051's fix, not caused by it: both timers were unreachable before.
- **Severity:** P2. The model's RAM is never actually released, and a full load is paid every
  quarter of an hour on an idle machine.

`idle_secs` defaults to **900** (`ROZUM_GATEWAY_IDLE_SECS`, set by the daemon path unconditionally);
`ROZUM_GATEWAY_UNLOAD_IDLE_SECS` on the durable job is **1200**. The smaller wins, so the unload
branch was dead code on this machine — the process exits instead, and `KeepAlive` returns it within
seconds, whereupon it eagerly reloads the weights because the job carries `--model`.

**Observed, on the live job, minutes after BUG-051 landed:**

```
gateway_exit {"reason": "idle", "idle_secs": 900}
uptime 870 → 26          (KeepAlive restarted it, model loaded again)
```

That log line is also BUG-051's proof: the idle clock reached 900 for the first time, which it could
not do while the doctor's probe reset it every 300 s.

**Why the fix is "do not exit" rather than "exit later".** For a gateway that CAN reload, exiting is
strictly worse than unloading in every case: unload frees the model and keeps the daemon for a lazy
reload, exit frees the same RAM and then hands it straight back under a supervisor. Making the exit
threshold larger would only lengthen the cycle, not remove it. Idle-exit stays exactly where it is
still the only lever: a gateway with no builder (nothing to reload with), and one whose unload is
switched off — `ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0`, which is what the bench harness sets so its
per-run gateways still go away. `launch_managed` is untouched; an agent-spawned gateway exits on
`clients_gone`, which is the right trigger for it.

The decision is a pure function so it is testable without a running gateway: three cases, including
the bench's.

## BUG-051 — the health check kept the model resident forever, so idle-unload could never fire

- **Status:** FIXED 2026-08-15 (`gateway.rs`: `last_active` is bumped only for requests that USE the
  model; `is_liveness_probe` is the deny-list). Not yet deployed — the running gateway is the
  installed binary, and restarting it is the operator's call.
- **Severity:** P2, and expensive in a way that shows up as something else. The host holds a
  model's whole reservation — **7.89 GiB** measured here — indefinitely, so every later "the slot is
  busy" and every RAM-blocked load is a downstream symptom of this.

Three facts, each measured, and they only make sense together:

| | |
|---|---|
| `src/services.rs:83` | the registry probes the gateway with `GET http://127.0.0.1:8089/v1/models` |
| `gateway.rs` auth/activity middleware | bumped `last_active` on EVERY request, never looking at the path |
| `com.rozum.doctor` | runs that probe **every 300 s**; the unload threshold is **1200 s** |

300 < 1200, so the idle timer was reset four times before it could expire. The idle-unload was
unreachable on this machine — not disabled, not misconfigured: **unreachable**.

**What it looked like from outside**, which is why it went unnoticed: `ROZUM_GATEWAY_UNLOAD_IDLE_SECS=1200`
sits in the launchd job, reaches the process (verified with `ps eww`), and is read correctly. The
gateway answers every probe. `rozum doctor` reports `7 ok, 0 warn, 0 fail`. Everything is fine
except the one thing nobody was looking at: 93 minutes after the last client request the model was
still resident, with an empty lease directory and no established connections.

**Two other explanations were ruled out before the real one, both by measurement.** Stale leases:
the lease directory has been empty since the last request completed. A missing reload builder
(`can_reload()` gates the unload and a `--dedicated` gateway has none): the daemon path passes
`builder: Some(gateway_backend_builder(...))`, so it is true here.

**A false signal worth recording.** `ps` shows this gateway at **RSS 0.03 GB** with a model loaded,
because MLX holds weights in unified memory outside the process heap. Read that way the model looks
unloaded already. The honest numbers are in `/stats` — `mlx_memory_mb {active: 3115, peak: 4560,
cache: 2049}` — and in the residency ledger's `footprint_bytes: 7892971128`.

**The fix is a DENY-LIST and the direction of the error is the argument.** `is_liveness_probe`
names `/v1/models`, `/models`, `/health`, `/healthz`, `/stats`, `/ready`, `/ping`; everything else
counts as use. Miss a probe and the model stays resident — today's behaviour, recoverable. An
allow-list of inference paths would fail the other way: an endpoint nobody added to it would let the
model unload UNDER A LIVE CLIENT, mid-conversation. `in_flight` still counts probes, because that
counter answers "is anything running right now" for exit and preemption, and a probe is running.

**What is proven and what is not.** Two tests cover both directions — the six probe paths, and that
all four inference endpoints plus anything unrecognised still count as use. What is NOT proven here
is the end-to-end behaviour, because observing an unload needs the model slot and the slot is what
this bug is holding. The deploy is its own proof: with the fixed binary the model should be released
~20 minutes after the last real request while the doctor keeps probing every 5.

One entry per bug, newest first. Status flow: `open → needs-info → fixed → done`.
See `vendor/agent-plugins/bugs/commands/bugs.md`.

---

## BUG-050 — constrained decode emits a TRAILING COMMA, so "structured output" does not parse

- **Status:** FIXED and VERIFIED LIVE 2026-08-15 on both dialects — `{"capital": "Paris, France."}`,
  parses. Found by the first live smoke of the day's work, and closed the same way it was found.
- **Severity:** P1 for the feature. `SamplingParams::response_schema` promises that a backend which
  supports it "guarantees the output parses and conforms". It does not parse.

```
{"capital": "Paris",
  }
```

Both OpenAI dialects, same output, temperature 0:

- `/v1/responses` with `text.format` — the path BUG-034 opened yesterday;
- `/v1/chat/completions` with `response_format` — which has supported structured output all along.

So this is the constraint machinery, not the new wiring: the grammar lets a `,` be followed by `}`
instead of requiring another member. The schema half works — the object has exactly the required
property, with the right type — which is what makes the failure quiet: it LOOKS like structured
output right up until `json.loads` refuses it, and the client that asked for a schema is the one
client guaranteed to call that.

**The fix, and the second hole it exposed.** `match_object` starts its loop after a comma, and the
loop's first branch accepts `}` — so a comma promising a member could be followed by the closer. A
`after_comma` flag refuses that. But refusing only the closer would have left the grammar with NO
legal continuation when every declared property is already named: the key list is empty, so the next
token would be masked whatever it is, and the decode would be stuck. So **the comma itself is
refused once nothing is left to name**, which keeps `}` the only way out. `match_any_object` — the
`Schema::Any` path that tool ARGUMENTS use before the tool is known — had the same hole, where it
produces an unparseable tool call rather than an unparseable answer.

**Found by running it, not by reading it.** The unit tests and the wire golden are green, CI is green
on three platforms, and the feature has been broken on the endpoint it shipped on. A live smoke of
three endpoints took two minutes and found what a day of green suites did not.

## BUG-049 — granting `TMPDIR` handed back the right to delete the workspace inside it

- **Status:** FIXED 2026-08-15. The root is a named argument now, and any extra grant that is an
  ANCESTOR of it is dropped.
- **Severity:** P1 on Linux — the one guarantee this sandbox exists for did not hold.

Landlock grants rights BENEATH a path, and removing a directory needs the right on its PARENT. That
is what made "the workspace root cannot be removed" fall out for free — **as long as no ancestor of
the root is also granted.** nadia grants `TMPDIR`, and every workspace `tempfile` creates lives
inside it, so `/tmp` was granted, `rm -rf <workspace>` succeeded, and the guarantee was worth
nothing exactly where it was being tested.

**I had this theory two rounds earlier and dropped it, correctly.** At that point the observed
failure had a different cause (an unbuilt ruleset, BUG-047/048) and the log said so; acting on the
theory then would have "fixed" something that was not broken yet while leaving the real cause. It
became true only once the ruleset actually built. The sequence is the point: three rounds, three
causes, each named by a log rather than by a hypothesis — and the hypothesis that was wrong twice
was right the third time, for reasons the earlier rounds could not have shown.

**The rule is a pure function on purpose.** `undermines_root(extra, root)` needs no kernel, so the
half I got wrong by reasoning is now provable on any machine — including the component-wise detail
that `/tmp/wor` is not an ancestor of `/tmp/work`. Only the enforcement still needs Linux.

## BUG-048 — the same EBADFD, from a second cause: under `cargo test`, `/dev/stdout` is a pipe

- **Status:** FIXED 2026-08-15. Paths are filtered by FILE TYPE before being added, because a failed
  `add_rule` consumes the ruleset and leaves nothing to continue from.
- **Severity:** P1 on Linux, and it is the same P1 as BUG-047 — I fixed half a cause and shipped.

BUG-047 fixed device rights (`MakeDir` on `/dev/null`). The next CI round returned the identical
error, and the second cause is a different shape entirely: **`/dev/stdout` resolves to
`/proc/self/fd/1`, which under `cargo test` is a PIPE** — not a filesystem object Landlock can rule
on. `add_rule` answers EBADFD, the whole ruleset fails to build, and both consumers fall over in
their own way exactly as before.

**Why "skip the bad path" had to become "do not offer it": `add_rule` takes `self` by value.** A
failed add consumes the ruleset, so there is nothing left to continue the loop with — recovery is
impossible after the fact and the decision must be made from the PATH. Filtered now to directories,
regular files and device nodes; pipes and sockets are skipped.

Skipping costs nothing real, and the reason is worth knowing: **Landlock governs OPENING a path, not
writing to an already-open descriptor.** A shell's inherited stdout keeps working whether or not
`/dev/stdout` could ever be granted.

**Both failing shapes are in the crate's own tests now** — a device and a pipe — because neither can
fail on macOS, where `confine_child` is a no-op. Three CI rounds were spent learning what only that
platform could tell me, and each round taught exactly one thing: handle writes only, then file
rights for files, then do not offer what cannot be granted.

## BUG-047 — one rule on `/dev/null` failed the whole ruleset, and took both sandboxes down with it

- **Status:** FIXED 2026-08-15. Directories get the whole write set; files and devices get only the
  rights that apply to a file.
- **Severity:** P1 on Linux while it lasted, and it is the third round of the same afternoon.

```
ruleset: failed to add a rule: File descriptor in bad state (os error 77)
```

`AccessFs::from_write` includes directory-only rights — `MakeDir`, `RemoveDir`, `MakeReg` and the
rest — and asking for those on `/dev/null`, a character device, is EBADFD. Landlock fails the whole
`add_rule`, so **the entire ruleset never built**, and one bad entry in a six-path list disabled
confinement completely.

**One cause, two different-looking failures**, which is why it took a round to see: with no ruleset,
nadia fell back to running UNCONFINED (so `rm -rf` over its own workspace succeeded, and the test
reported "deleting the workspace root must be refused") while the assistant's shell REFUSED every
command ("shell confinement unavailable"). Two symptoms, opposite in shape, from the same unbuilt
ruleset.

**The crate's own tests now carry a device in the writable list**, because they did not before —
they granted a directory and passed on macOS, where `confine_child` is a no-op. A test that cannot
fail on the machine that runs it has to at least fail on the machine that matters, and the only way
to arrange that is to put the failing SHAPE in it.

## BUG-046 — the Landlock ruleset handled EVERY access right, so the confined child could not exec at all

- **Status:** FIXED 2026-08-15, one CI round after BUG-044's implementation. Found by the test
  written to catch exactly this, on the platform I cannot run.
- **Severity:** P1 for anyone on Linux — nadia's `exec` and the assistant's shell were dead there,
  refusing every command with `EACCES` — and it shipped for the length of one CI round.

```
the confined child must still spawn: Os { code: 13, kind: PermissionDenied }
```

A Landlock ruleset denies every right it HANDLES except where explicitly granted. I handled
`AccessFs::from_all(abi)` and granted read+write beneath six paths, so reads and **execute** were
confined to those six — and `/bin/sh` is not one of them. The child died before running a byte.

**The docs I had just written say "writes only, reads free", and the code did the opposite one
function below them.** The seatbelt profile spells the same policy `(allow default) (deny
file-write*)`, and the reason is in a comment there too: confining reads is what aborts dyld's
shared-cache mapping on macOS. I quoted that and then handled every right anyway. Handling only
`AccessFs::from_write` leaves reads and exec untouched.

**What this vindicates is the un-gating.** The whole point of removing `#[cfg(target_os = "macos")]`
from the deletion test was that a guarantee nobody can check is not a guarantee — and the first
thing CI did with the new code was prove it wrong. Three tests failed on Linux and one of them was
`rozum-confine`'s own, which exists for no other purpose. From a mac all three had passed, because
on macOS `confine_child` is a no-op by design.

A second test now pins BOTH halves together — a read from outside the grant must still work, a write
outside it must not — because getting the first half wrong is invisible on macOS and fatal on Linux.

## BUG-045 — one Linux failure hid three more, because cargo stops at the first failing test binary

- **Status:** FIXED 2026-08-15 — all four causes repaired, and CI now runs its suites with
  `--no-fail-fast` so the next set arrives in ONE round instead of four.
- **Severity:** P2. Not the individual failures — the discovery loop. Each fix revealed the next
  only after a full CI round, which is how a job stays red long enough for people to stop reading it
  (BUG-043).

Fixing BUG-044 turned the Linux job from one failure to **three**, none of them new: `cargo test`
stops at the first failing test BINARY, so `nadia`'s failure had been hiding everything behind it
for days. What came out:

**1. `meeting::daemon::a_rebound_path_is_not_our_socket_any_more` — a real defect, not a test
artifact.** Socket identity was the inode number alone, and **Linux reuses inode numbers
immediately**: remove a socket, bind a successor in the same directory, and the new one very often
lands on the same `ino`. A wedged owner asking "is the socket still mine?" was answered YES about
someone else's socket. macOS does not reuse that eagerly, which is why an inode-only identity passed
here for months and failed the first time it met Linux. Identity is now device + inode + ctime,
folded into one number. The trade is stated in the code: `ctime` also moves if someone chmods the
socket, which would read as "not ours" — a false negative, and the safe direction, since the
dangerous one is a wedged owner believing it still serves clients who left.

**2–3. Two `sandbox_tools` tests** — the meeting assistant's in-chat shell REFUSED to run off macOS
(`error: shell confinement unavailable`), the mirror image of nadia's old silent unconfined run.
Both are fixed by `crates/rozum-confine`, and what that crate does NOT do is the part worth reading.

**The mechanism is shared; the policies are not, and that is deliberate.** I nearly folded the two
seatbelt profiles into one list before noticing they differ on purpose: nadia's is `(allow default)`
with writes denied and re-allowed under the workspace, `CARGO_HOME` and `TMPDIR`, because a coding
agent runs `cargo`; the assistant's is `(deny default)` with writes only under its root, because an
in-chat shell should reach less. Unifying them would have been a silent policy change for whichever
one lost — the same mistake as forcing one abstraction over two genuinely different things, in a
different costume. So `rozum-confine` applies a Landlock ruleset to a child and each caller passes
its own paths.

## BUG-044 — `nadia`'s sandbox test fails on Linux, and has since before 2026-08-14

- **Status:** FIXED 2026-08-15 — the TEST was wrong, and the gap it stumbled over is now named
  rather than implied. **Not caused by this week's work:** it was already failing at run
  `da4cc295` (07:45 on 2026-08-14), before that day's first push.
- **Severity:** P2 — it is one of only two things keeping the Linux job red, and a permanently red
  job is one nobody reads, which is how the macOS and Windows breakages above went unnoticed.

```
test sandbox::tests::the_agent_cannot_delete_its_own_workspace ... FAILED
crates/nadia/src/sandbox.rs:296: assertion `left != right` failed:
    deleting the workspace root must be refused: ""
```

**It is not path handling — it is that there is no sandbox on Linux.** `Sandbox::confine` defaults
to `cfg!(target_os = "macos")` and `exec` wraps the command in `sandbox-exec` only there, because
the mechanism IS seatbelt and seatbelt is macOS-only. So on Linux the command runs unconfined, the
`rm -rf` succeeds, and the test asserting refusal fails. The test was asserting a guarantee the
platform does not make; its neighbour two functions up was already `#[cfg(target_os = "macos")]` for
the same reason, and this one was not.

**Gated on 2026-08-15, then UN-gated the same day because the gap was closed** (Landlock, below).
Gating a test hides a gap unless the gap is stated, so while it was gated two things went with it. `exec` now says once, on stderr,
that confinement was wanted and this platform has none, instead of running unconfined in silence.
And the gap has a name: `nadia-linux-confinement` in `BACKLOG.md`. On Linux an agent can still
delete its own workspace — BUG-017, unfixed there.

**Closed properly on 2026-08-15:** Landlock now enforces the same policy on Linux that seatbelt
enforces on macOS — writes confined to the workspace root, `CARGO_HOME`, `TMPDIR` and the three
`/dev` sinks; reads free. The root itself cannot be removed and that falls out of the design rather
than needing its own rule: Landlock grants rights BENEATH a path, so `rm -rf <root>` deletes the
contents and fails on the final `rmdir(root)`, which needs a right on the parent that is never
granted. Same guarantee as seatbelt's explicit `(deny file-write-unlink (literal root))`, reached
from the opposite direction. The test is un-gated, so **CI proves it on the platform that has the
kernel** — the only machine here that can.

**Why this mattered beyond one red test:** the Linux job had been red since before 2026-08-14, and a
permanently red job is one nobody reads. That is exactly how two real breakages of mine went
unnoticed the same day (BUG-043). A red that is always red teaches people to stop looking.

## BUG-043 — I broke CI twice in one day and did not notice, because I never looked at it

- **Status:** FIXED 2026-08-15. macOS went green on the first repair; Windows needed a SECOND one,
  because the first fix was still wrong — see below.
- **Severity:** P1 as a process defect. Every board entry today quoted "13 workspace suites green"
  — true, and it was the wrong axis twice.

**macOS, red from 21:49.** Two compile errors of mine, both behind `mlx-native`: `E0507` from
BUG-041 (`StopReason` losing `Copy`, at a site in the MLX backend) and `E0433 cannot find obs` from
BUG-042's "this engine cannot honour it" notice, which called `crate::obs` in a crate that does not
re-export it. **The notice written to prevent silent failure did not compile.** Every check I ran
and quoted was `--no-default-features`; neither error is reachable that way. This is precisely the
lesson already in this repo as `cargo check hides test rot`, applied to features instead of tests.

**Windows, red from 08:55.** `the_residency_ledger_did_not_move_on_the_machine_this_runs_on` — the
test I wrote to prove the Windows path port moved nothing — asserted against a forward-slash
LITERAL, which is not what `PathBuf` yields on Windows. A unix-shaped test guarding a Windows port.

**And the first fix for it was ALSO wrong, which is the part worth keeping.** Replacing the literal
with joins made the separator right and left the assertion demanding the UNIX LAYOUT on Windows —
where `%LOCALAPPDATA%` outranks `HOME` by the rule I had just written in `rozum_paths`. CI said so
exactly:

```
left:  "C:\Users\runneradmin\AppData\Local\rozum\gateway"
right: "/Users/somebody\.local\state\rozum\gateway"
```

So the test was demanding that my own port be wrong. It now pins `LOCALAPPDATA` as well as `HOME`
and asserts each platform's own layout — deterministic on both rather than dependent on the runner.
Swept the rest of the workspace for the same shape: two more path literals in assertions, both
verified safe (no joins on either side, so no separator can differ).

**Its neighbour was collateral, and the mechanism is worth keeping.** A test that fails while
holding `POISON_ENV_LOCK` POISONS it, and every later test that `.unwrap()`s the lock dies of the
poison instead of its own result — so one wrong assertion produced two red tests, one of which had
nothing wrong with it. All 13 sites take the lock with `unwrap_or_else(|e| e.into_inner())` now, and
the rule is written at the declaration. Same shape as BUG-035, one crate over.

**What actually changes going forward:** a change is not verified by the suite I happen to run.
`--no-default-features` is one axis of four here (default/`mlx-native`, `mistralrs`, `gguf`,
feature-off), and three of them were never checked today. The fix is not "be careful" — it is to run
the axes the CI runs, and to LOOK at CI after pushing, which I had not done once.

## BUG-042 — `frequency_penalty` / `presence_penalty` were accepted and did nothing

- **Status:** FIXED for the engines that sample in our code (GGUF, x86) 2026-08-14, and **named
  rather than silent** on the one that does not (MLX). The MLX half needs a change to the vendored
  `mlx-lm` fork — filed as `mlx-openai-penalties`.
- **Severity:** P3 by traffic — only `/v1/chat/completions` defines them, and neither Claude Code
  (Anthropic) nor codex (Responses) sends them — but P2 by kind: accepted, documented, inert.

**Implemented honestly, which is the whole point of the entry.** OpenAI's pair is additive and
count-based: `logit -= frequency × count(token) + presence`. Our `repeat_penalty` is the llama.cpp
convention — multiplicative, flat, over a recent window. **Aliasing one onto the other would have
been the cheap fix and it would have presented an approximation as the parameter the client asked
for**, which is a worse failure than not supporting it: the client cannot tell.

Two decisions the specification leaves open, settled and written down:

- **Counting scope: generated tokens only, not the prompt.** The prompt is the user's own text;
  penalising it makes the model avoid the words the user just used, which is the opposite of what
  someone asking for less repetition wants. (This is also what vLLM does.)
- **Out-of-range values are clamped to [-2, 2].** Outside that range the effect is not "stronger",
  it is "the distribution collapses" — a client that typed 100 would get gibberish with a 200.

**The MLX gap is the honest part.** `rozum-core::sampler` is driven by GGUF and x86; the MLX
backend samples inside the vendored mlx-lm graph, where the knobs live in the fork's `SamplerOpts`.
So on the engine this machine actually runs, these two parameters are still not applied — and the
backend now says so once (`sampling_unsupported`) instead of ignoring them, which is the exact
silence this week's bugs were about. The fork keeps the full token history already, so the remaining
work is two fields, a count-based penalty function, and a rev bump.

**A second copy found on the way:** the GGUF path built `SamplerConfig` by hand instead of calling
`from_params`, so it went stale the moment the struct grew. It only failed loudly because a struct
literal must list every field. It now uses the shared mapping, with one deliberate override — GGUF's
historic default temperature is 0.7 where the shared mapping's is greedy.

## BUG-041 — Anthropic's `stop_sequence` field was missing entirely, so a client with several could not tell which fired

- **Status:** FIXED 2026-08-14 — `StopReason::StopSequence(String)`; both the streaming
  `message_delta` and the non-streaming body carry the string, and the upstream shim reads it.
- **Severity:** P3, and it completes BUG-040 rather than standing alone.

Two findings, and the first is about my own estimate.

**Losing `Copy` cost three compile errors, not the wide change I priced it at.** BUG-040 deferred
this on the grounds that `StopReason` is `Copy` and "83 call sites rely on it"; the alternative — a
field on `ChatEvent::Done` — was measured at 38 exhaustive destructuring sites. The compiler
answered in one run: **three** errors, each a single `.clone()`, in `consume_tokens`, the Anthropic
upstream shim, and the cascade. That is the second estimate I made by reading this week that the
compiler cut down (BUG-040 was the first), and the lesson is the same both times: price a refactor
by compiling it, not by grepping it.

**The field was not merely null — it was absent.** Anthropic's Message object always carries
`stop_sequence` (null when no stop string fired), and our non-streaming body omitted the key
entirely. The wire golden shows it as the one changed line: `"stop_reason":"end_turn"` gained
`,"stop_sequence":null` on every `/v1/messages` answer, not only on stop-sequence ones. A client
reading `body.stop_sequence` got `undefined` where the spec promises `null` — a distinction that
matters in exactly the languages most clients are written in.

The streaming half had the key with a hardcoded `null`, which was correct until a stop string could
actually fire and then quietly wrong.

## BUG-040 — a stop-sequence hit and a natural end were the same answer, in both directions

- **Status:** FIXED 2026-08-14 — `StopReason::StopSequence`; `/v1/messages` reports Anthropic's
  `stop_sequence`, and the upstream shim stops flattening it on the way in. The matched STRING is
  still not carried — measured cost below.
- **Severity:** P3. A client branching on `stop_reason` could not tell "the model finished" from
  "your stop sequence fired", which is the one distinction the parameter exists to make.

Split off from BUG-037 on the grounds that the enum had "83 references across twelve files" and a
new variant risked a wrong arm in a non-exhaustive match. **That estimate was wrong, and the
compiler settled it in one run: exactly ONE non-exhaustive match in the whole workspace**
(`obs.rs:345`). The 83 were mostly constructions. Recorded because the estimate is what deferred the
work, and a number produced by reading is worth less than the same number produced by the compiler.

**The wildcards were the real question, and reviewing them found a second defect.** Six matches on
`StopReason` end in `_`, so the new variant lands silently in whatever bucket they name:

| site | lands on | verdict |
|---|---|---|
| `oai_api.rs` finish_reason | `"stop"` | correct — OpenAI spells a stop-sequence hit `stop`, same as a natural end. Now an explicit arm, so it reads as a decision. |
| `responses_api.rs` status | `"completed"` | correct |
| `openai_http.rs` / `mistralrs_backend.rs` | `EndTurn` | correct — an upstream `stop` cannot say which it was |
| **`anthropic_http.rs`** | **`EndTurn`** | **wrong.** Upstream Anthropic DOES distinguish `stop_sequence`, and this shim was collapsing it on the way IN — a proxied answer lost exactly the information the serializer now preserves. Found by reviewing the wildcard, not by looking for it. |

**What is still not carried:** Anthropic's `stop_sequence` field (which string matched) stays `null`.
Carrying it means either giving `StopReason` a `String` payload — costing it `Copy`, which call
sites rely on — or a field on `ChatEvent::Done`, which has **38 exhaustive destructuring sites**.
Measured, not guessed, this time. Clients branch on `stop_reason`; the string is a detail, and it is
filed as `stop-sequence-which-one` rather than smuggled in.

## BUG-039 — `cargo test` wrote fabricated events into the operator's LIVE gateway log, which incident evidence reads

- **Status:** FIXED 2026-08-14 — a test binary with no explicit `ROZUM_GATEWAY_LOG` writes nothing.
  Measured: the gateway suite appended **5,303 bytes** before, **0** after; the workspace suite the
  same.
- **Severity:** P2, and higher than it looks. `~/.rozum/gateway.jsonl` is not a scratch file: it is
  what `meeting::store::gateway_log_slice` cuts an incident's evidence out of, and what
  `doctor`/`/stats` read. A test run manufacturing `request_done` and `generation_timeout` events
  into it means an incident report can quote something that never happened on the live gateway.

Found while verifying BUG-038, by asking whether the new logging call would touch the real file —
the answer for MY change was no (it only logs when a request carries unknown fields, and none of the
tests do), and the measurement then showed the suite was already writing 5.3 KB per run **on
untouched master**. The tail of the operator's log had
`{"event":"generation_timeout","inactivity_secs":0}` in it: that is the `gen_timeout_zero_disables`
unit test, sitting in production observability data.

**Why a test could not simply redirect it:** `obs::log_path()` resolves through a `OnceLock`, so
whichever code logs first in a process fixes the destination for every later call. A test that sets
`ROZUM_GATEWAY_LOG` after that point changes nothing, and one that sets it before depends on test
ordering — which is the same trap as BUG-035, one file over.

**The rule:** no explicit destination AND the executable lives in `target/*/deps/` ⇒ log nothing.
Cargo puts test binaries there and nothing this workspace ships does — the gateway runs from
`target/release/rozum-gateway`, `~/.cargo/bin`, or `~/.rozum/bin`. An explicit `ROZUM_GATEWAY_LOG`
still wins, so a test that wants to assert on the log can point it at a temp file.

**Both directions are tested, because the second is the dangerous one.** Recognising a test binary
too eagerly would silence the production log — the opposite failure, and just as quiet. The rule is
a pure function of the path with both cases asserted, plus one test that this very process is
detected, so the 5,303 → 0 measurement is not luck.

## BUG-038 — the gateway could not tell you it had ignored your parameter

- **Status:** FIXED 2026-08-14 — every request field this gateway does not act on is captured and
  named in `gateway.jsonl` as `wire_fields_ignored`, once per novel shape per endpoint.
- **Severity:** P2, and it is the reason the seven bugs above existed rather than an eighth of them.

BUG-031 … BUG-037 all had one mechanism: **`serde` drops a field no struct declares, in silence.**
The client gets a 200, the parameter does nothing, and nothing anywhere says so — no error, no log
line, no metric. Every one of them was found by eye, one at a time, by putting three dialects' parsed
requests side by side. That does not scale and it does not survive the next API revision.

Each dialect now carries `#[serde(flatten)] unknown: Map<String, Value>` and reports it. Three
properties, each deliberate:

- **Once per shape, not per request.** An agent sends the same shape thousands of times an hour; a
  line each would bury the signal. The key is the endpoint plus the sorted field list, so a NEW
  parameter — a client upgrading, a spec growing — is exactly one new line.
- **Bounded** (`SHAPE_MEMORY = 256`): the key comes from what a client sends, so a caller with
  random field names must not grow it forever.
- **It does not warn, refuse, or 400.** Ignoring an unknown field is correct HTTP behaviour, and
  some clients send bookkeeping fields on purpose (`store`, `metadata`, `user`). The defect was
  never the ignoring — it was the silence.

**It also answers the two questions the sweep could not.** Whether codex sends
`previous_response_id`, and whether Claude Code sends Anthropic's `thinking`, were unanswerable by
reading code — an undeclared field left no trace. The next real request answers both, in the log,
with no further guessing.

**What proves the capture is inert:** the wire golden did not move. `#[serde(flatten)]` changes how
serde deserializes a struct, so the risk was that the KNOWN fields would start parsing differently;
seven frozen cases say they do not.

## BUG-037 — stop sequences did not exist anywhere: a client asking to stop at a marker generated straight past it

- **Status:** FIXED 2026-08-14 — `SamplingParams::stop`, parsed from OpenAI's `stop` and
  Anthropic's `stop_sequences`, applied in the engine-agnostic `consume_tokens` so both engines
  honour it identically.
- **Severity:** P2. Unlike the rest of this week's set it was not a dropped field — there was no
  field. `stop` is how a client running a chat-format prompt keeps the model from writing the
  other side of the conversation, so "ignored" here means a plausible answer with someone else's
  turn glued to the end of it.

**Where the work actually was.** Not the parsing. Streaming text cannot be recalled, so the moment a
token arrives that *might* be the start of a stop string, those bytes must be HELD until the next
token decides. Models emit multi-byte tokens and a stop string lands mid-token routinely, so without
hold-back the client sees `"\nHum"` on screen and the stop that was supposed to remove it arrives
too late to matter.

`stop_boundary(text, stops) -> (cut, completed)` answers both questions in one pass — cut here and
end the turn, or cut here and wait — and is a pure function, so the hard part is tested without a
model, a GPU or a gateway. Its cases: present, half-present, half-present-then-ordinary-text (the
one that would silently truncate answers if the release were wrong), and a partial match that must
not split a UTF-8 character.

**Empty is a strict no-op**, and that is the property the whole change rests on: `stops.is_empty()`
returns `(text.len(), false)`, so every request that asked for nothing is byte-identical. The wire
golden shows it — the six pre-existing cases moved only by gaining a `stop=[]` field in the summary
line.

**Bounded on purpose:** `MAX_STOP_STRINGS = 16`. Every generated token is scanned against every stop
string, so an unbounded list is an unbounded per-token cost on a server anyone on the tailnet can
reach. OpenAI's own limit is 4, so no compliant client comes near it.

**What is deliberately NOT done.** Anthropic reports `stop_reason: "stop_sequence"` (plus which one)
where this returns `end_turn`. Distinguishing it means a new `StopReason` variant, and that enum has
**83 references across twelve files** — a mechanical change with real risk of a wrong arm in a
non-exhaustive match, on the decode path. The behaviour a client asked for (stop, and drop what
follows) is delivered; the label is a separate change, filed as `stop-reason-sequence` in
`BACKLOG.md`.

## BUG-036 — `repeat_penalty` was implemented all the way down to the Metal sampler, and no client could reach it

- **Status:** FIXED 2026-08-14 — both OpenAI dialects accept `repetition_penalty` (the vLLM/TGI
  spelling, which is what OpenAI-compatible local servers speak and the same reason `top_k` is
  already accepted). Not added to `/v1/messages`: Anthropic defines no such parameter.
- **Severity:** P3 as a feature, and it is filed for the shape rather than the impact — it is the
  third field in a week found implemented at one end and unreachable at the other.

`SamplingParams::repeat_penalty` is honoured by `sampler::sample` (HF convention: divide positive
logits, multiply negative ones, over a recent-token window) and threaded through the MLX backend to
`set_sampler`. **No wire dialect ever set it**, exactly as with `seed` before BUG-032.

**How deep the dead code went, and the part worth reading.** The MLX batching admission is

```rust
!constrained && !has_penalty && !has_seed
```

with a counter behind each reason. **Two of the three conditions could not occur**, because no
client could set a penalty or a seed — so `serial_penalty` and `serial_seed` were statistics about
paths the code could not take, sitting in the stats pane looking like measurements.

**A consequence of BUG-032 I did not name when I fixed it, corrected here:** now that a client CAN
send a seed, such a request leaves the batched decode path for the serial one. That is right —
reproducibility needs a per-sequence RNG and batching cannot give one — but it is a throughput cost
that the commit should have stated and did not. The same is true of a penalty, and it is now in the
field's doc comment where a client can find it.

## BUG-035 — the `nadia` gate tests race on a process-wide env var, so the workspace suite is flaky

- **Status:** FIXED 2026-08-14 — one `GATE_ENV_LOCK` across every test that touches those
  variables, and the reader test now ESTABLISHES the precondition it used to assume. After:
  **0 failures in 30 runs** of the same command that gave 2/25 before. Thirty clean runs do not
  prove a race gone — but the mechanism is closed, and the measurement is the same one, unchanged.
- **Severity:** P2, and the severity is about TRUST rather than function: a suite that fails ~8% of
  runs makes every "workspace green" claim unreliable, including the ones in this file.

`gate::tests` sets `ROZUM_GATE_OWNER` (and `NADIA_VERIFY`, `NADIA_VERIFY_ROUNDS`) with
`std::env::set_var` and removes it a few lines later. cargo runs the tests of one crate as THREADS
in one process, so any test scheduled inside that window sees the variable set.
`a_report_never_implies_a_pass_it_does_not_have` reads it through `Report::summary()` → `owner()` and
gets `⤳ проверку выполняет rozum-launch` where it asserts `не проверено`.

**Measured, not inferred:** `cargo test -p nadia --lib gate:: -- --test-threads=16`, 25 runs on
`origin/master` with no local changes → **2 failures**. A plain `-p nadia --lib` run passed 8/8,
which is exactly why this survived: it only shows under load or with more threads, and it is
invisible in the single-test form anyone reaches for when checking.

**The fix already existed in this repo, twice.** `rozum-core` serialises the same hazard with a
`POISON_ENV_LOCK` static, and `src/doctor.rs` carries a comment about the identical mistake — "I
wrote 'no other test in this crate reads XDG_STATE_HOME' instead of checking, and shipped a suite
that passed alone". The gate tests now have the same lock.

**What the earlier attempt missed, and why it matters more than the flake.** Someone had already hit
this and fixed it by MERGING the racing tests into one, with a comment saying so
(`who_owns_the_gate_for_this_run`). That caught the tests which obviously WRITE the variables and
missed the one that only READS them, indirectly, through `Report::summary()` → `owner()`. Merging
also does not scale: it ends with one enormous test and no names. A lock covers readers, and a new
test only has to take it.

**A second defect fell out, and it was never a race at all.** The reader test ASSUMED no outer gate
instead of establishing one, so it failed for anyone with `ROZUM_GATE_OWNER` exported in their
shell — deterministically, not occasionally. Proven both ways:
`ROZUM_GATE_OWNER=rozum-launch cargo test -p nadia --lib a_report_never` fails on master and passes
here.

## BUG-034 — structured output worked on one OpenAI dialect and silently did nothing on the other

- **Status:** FIXED 2026-08-14 (`RespReq.text` declared; `parse_text_format` reads `text.format`;
  the nesting difference handled once in `parse_format_object`).
- **Severity:** P2 — a constrained-decode request answered with unconstrained output and a 200. The
  client gets prose where it asked for schema-conforming JSON and finds out by failing to parse it.

`/v1/chat/completions` honours `response_format` → `SamplingParams.response_schema` → constrained
decode (`docs/specs/constrained-tool-decoding.md`). The Responses API spells the same capability
`text: { format: { type: "json_schema", schema: … } }`, and `RespReq` did not declare `text` at all,
so it was dropped by serde like every other undeclared field.

The two shapes differ by exactly one level of nesting — Chat wraps the schema in `json_schema`,
Responses puts it inline — and the type names (`json_schema` / `json_object` / `text`) are identical.
So the fix is not a second parser: `parse_format_object` now reads either nesting, and each dialect
says only where its format object lives.

**The dangerous direction is the default, and it is tested.** `{"type": "text"}` is what a Responses
client sends when it wants prose, and reading that as "constrain to JSON" would break every ordinary
request on the endpoint codex drives. `text`, an unknown type, and an absent field all parse to
`None` — unconstrained — with a test naming each case.

## BUG-033 — `/v1/chat/completions` streaming reports no token usage at all, and `stream_options` is not parsed

- **Status:** FIXED 2026-08-15, both halves. Server 2026-08-14 (`stream_options.include_usage`
  parsed; `usage: null` on every ordinary chunk; one extra final chunk with the real counts; a
  seventh golden case). Client 2026-08-15: `openai_http.rs` now SENDS the parameter it was already
  parsing the answer to, and falls back once on the single refusal that means "I do not know this
  field", remembering it per endpoint.
- **Severity:** P2 for anyone metering tokens over that endpoint; P3 for the agents here, which
  drive `/v1/messages` and `/v1/responses`.

Measured straight off the frozen wire bytes (`crates/rozum-gateway/src/testdata/wire-golden.txt`),
which is why this is a fact and not an impression:

| case | usage in the body |
|---|---|
| `/v1/chat/completions` stream=false | yes |
| `/v1/responses` stream=false | yes |
| `/v1/messages` stream=false | yes |
| **`/v1/chat/completions` stream=true** | **no** |
| `/v1/responses` stream=true | yes |
| `/v1/messages` stream=true | yes |

One cell out of six. OpenAI's contract is that usage in a stream is opt-in — the client sends
`stream_options: {"include_usage": true}` and gets a final chunk carrying `usage`. We parse no
`stream_options` (the whole tree contains no mention of it) and emit no such chunk, so a client that
asks is answered as if it had not: no error, no field, nothing to notice. Same family as BUG-031 and
BUG-032 — a documented request parameter that evaporates in `serde`.

**And the other half of the loop is ours too.** `openai_http.rs` — rozum acting as a CLIENT of an
upstream OpenAI — *parses* `usage` out of a streamed chunk and has a test pinning that expectation
(`parse_done_finish`, `openai_http.rs:440`), while never sending `stream_options` to ask for it. So
against a spec-compliant upstream our client would also get nothing. One missing parameter, both
directions.

**Why it was not four lines** (unlike BUG-032): honouring it means emitting an EXTRA final chunk
today's clients do not receive. That chunk must appear only when asked for, so the fix is parse +
conditional emit + a seventh golden case. **The proof that the default is untouched is the golden
diff: 17 lines added, none removed** — the six existing cases are byte-identical, so a client that
sends no `stream_options` receives exactly the frames it always did.

The shape follows OpenAI's, including the part that is easy to skip: with `include_usage`, every
ordinary chunk carries `"usage": null` and the extra one carries the counts with an empty `choices`.
The null is how a client tells "this build does not report usage" from "this chunk is not the one
that does".

**The client half, closed 2026-08-15 — and it needed neither of the two things this entry said it
needed.** The worry was real: sending an unknown parameter to a third party can 400 the whole
request, and the real providers cannot be probed from here. But "probe them" and "add a flag the
operator must know about" were not the only options. It ASKS, and on the one refusal that means "I
do not know this field" it retries once without it and remembers that per endpoint, so only the
first request against such a server pays for finding out.

The refusal test is name-based (`stream_options` / `include_usage` in a 4xx body) and that is the
load-bearing choice: treating any 4xx as "must be the new parameter" would retry a request the
server rejected for a real reason — a bad model, a context overflow — and then report the SECOND
failure, hiding the first. `ROZUM_OPENAI_STREAM_USAGE=0` remains for a refusal shape this code does
not recognise, because this is the one place rozum changes an outbound request.

Four tests cover it, and none of them touches a network. What is still NOT proven is the thing that
cannot be proven here: the behaviour of an actual third-party server. The fallback is what makes
that acceptable rather than a gamble.

## BUG-032 — the OpenAI `seed` was never parsed, so the code's own comment described a branch nothing could reach

- **Status:** FIXED 2026-08-14 (`OaiChatReq` declares `seed`; `OaiWire::into_internal` passes it;
  parse test + the wire golden, which now prints the field it was also blind to).
- **Severity:** P3 alone — reproducible sampling is a nice-to-have, and `/v1/chat/completions` is
  not the endpoint the agents drive. It is filed because of the second half.

`SamplingParams` has carried a `seed` field all along. **No wire dialect ever filled it.** The only
place in the entire crate that set one was `apply_determinism`'s own unit test, so this comment —

> `seed` only fills an unset seed so a caller that genuinely sent its own seed keeps it

— described behaviour no HTTP client could produce. Code and comment disagreed, and the comment was
the one that read like a promise.

**The decision, because there was one to make.** Honouring a client seed means it now overrides
`ROZUM_SAMPLING_SEED`, the bench's determinism control — which is why this was filed rather than
folded into BUG-031. Checked before doing it: the matrix runs `ROZUM_FORCE_GREEDY=1`,
`apply_determinism` then forces `temperature = 0`, and `sampler::sample` takes the argmax branch
without touching the RNG at all. **Greedy decode does not consume a seed**, so the bench's
determinism never depended on it. The operator chose to wire it (2026-08-14); the comment is now
true.

**Not extended to `/v1/responses`:** the Responses API does not define `seed`. Adding it there would
invent a parameter rather than honour one.

**Found the same way as BUG-031** — by the golden written for `plugin-wireprotocol`. With one
correction worth keeping: that gate printed every sampling knob EXCEPT `seed`, so it could not have
caught this on its own. The omission in the test mirrored the omission in the code, written the same
day by the same person. A golden only catches what it prints.

## BUG-031 — `/v1/messages` threw away `top_p` and `top_k`, and gave the client the opposite of what it asked for

- **Status:** FIXED 2026-08-14 (`AnthropicReq` declares both; `AnthropicWire::into_internal` passes
  them; parse test + the wire golden).
- **Severity:** P2 — wrong output, no error, on the endpoint Claude Code uses. Not a crash, which is
  why it survived: every layer behaved "correctly" on the way down.

Both are documented Messages API parameters. Neither existed on `AnthropicReq`, and serde drops an
undeclared field without a word, so the request parsed fine and the knobs were simply gone.

What made it worse than a no-op is the downstream default:

```rust
top_k: p.top_k.unwrap_or(0)     // 0 = disabled
top_p: p.top_p.unwrap_or(1.0)   // 1.0 = keep the whole distribution
```

A client asking to NARROW the tail (`top_p: 0.9`) therefore got the tail left wide open — not
"ignored", but the opposite of the request, with a 200 and a plausible-looking answer.

**How it was found, and why that matters more than the fix.** Not by reading the struct: by writing
the byte-level golden for `plugin-wireprotocol` (`docs/specs/wire-dialect-seam.md`), which records
the `ChatRequest` that reached the backend for each dialect side by side. Two dialects printed
`top_p=Some(0.9) top_k=Some(40)`, the third printed `None None` from the same input. A gap you can
only see next to its neighbours is invisible in the file where it lives.

**The matrix numbers are unaffected**, and that is checked rather than assumed: the bench runs with
`ROZUM_FORCE_GREEDY=1`, and `apply_determinism` clears `top_p`/`top_k` outright in that mode. This
changes behaviour only for a client that actually sends them.

**Its sibling, not fixed here:** `/v1/chat/completions` drops `seed` the same way — OpenAI defines
it, `SamplingParams` has the field, no dialect parses it. So `apply_determinism`'s comment ("only
fills an unset seed, so a caller that genuinely sent its own seed keeps it") describes a branch no
HTTP client can reach; the only place a client seed exists is that function's own unit test. Left
alone deliberately: honouring it would let a client's seed override the bench's
`ROZUM_SAMPLING_SEED`, which is a determinism-control decision, not a typo. Filed as
`oai-seed-never-parsed` in `BACKLOG.md`.

## matrix-task-info-is-a-stale-copy

<!-- status: fixed
     area: gateway
     gate: none -->

**Fixed** by `feature/matrix-task-info-single-source`: the compiled table is gone and both the
bench and the console read `scripts/bench/tasks.json`. The console had been showing prompts that
five of six tasks were never given, and answering `null` for `wordcount` and `multibug` — 61 real
cells. A test asserts every task the bench can run has a prompt and a difficulty, so the two
cannot drift apart again silently.

`matrix_task_info` (`crates/rozum-gateway/src/matrix.rs:546`) holds a prompt per task and the UCC
console shows it when an operator opens a cell. The bench holds its own prompts in
`scripts/bench/agentic.sh` `prompt_for()`. They are two copies, and five of the six have already
drifted apart — measured 2026-08-13 with an extractor that handles the escaped quotes both files
contain (a first pass without that read 59 chars where the file has 718, so the numbers below are
from the checked version):

    greet   identical      73  /   73
    build   differs       185  /  635
    fix     differs       183  /  678
    test    differs       177  / 1035
    debug   differs       157  /  593
    rpn     differs       239  / 1068

The gateway's copies are UNIFORMLY the shorter ones, so this is not two-way drift: the bench's
prompts grew (the "put files here directly, do NOT wrap them in a new project folder" guidance, the
tool hints added for codex/opencode) and the console's copy stayed at an older generation. An
operator reading a failed cell is shown a task that is not the one the model was given — which is
exactly the wrong moment to be misinformed, since the prompt is what they are trying to judge.

**Found while doing `ucc-ssc-backend`.** The .ssc port deliberately does NOT carry `task_info`,
because a second copy would drift; the measurement then showed the FIRST copy already had. So the
fix is not "move the table somewhere .ssc can read it" — it is to stop having a table: the bench
knows these prompts and should emit them, and both the gateway and the .ssc slice should read what
it emits. That also unblocks deleting the Rust handler, which is slice 1's remaining "done when".

## BUG-030 — the meeting PWA could not be rebuilt from source, on either lane

- **Status:** FIXED 2026-08-14, upstream, and the deployed binary is now built from source again.
  The fix is in scalascript main as `2315f2ecf` — they rebased my `feature/ssc-toint-on-a-char`
  (it was 21 commits behind; landing it as it stood would have been a verdict about a tree nobody
  runs), reproduced my measurements themselves and checked the type soundness before merging. Their
  shared toolchain was restaged at 09:18 the same morning, and `install-bins.sh rozum-meeting-ssc`
  builds and publishes with no override: `/Users/sergiy/.local/bin/rozum-meeting-ssc` 1,708,320
  bytes, `com.rozum.meeting-ssc` restarted and back on :8405.
- **Severity:** P2, and quiet. `clients/meeting/build.sh` had a `SKIP` branch, so every
  `install-bins.sh` run reported success while the PWA stayed at its 2026-08-08 build. A service
  whose only artifact is the copy already running is one bad restart from being gone.

One line of ours needs something neither lane provides:

```
hashStr(s) = s.trim.toList.map(c => c.toInt).sum      # clients/meeting/meeting.ssc:257
```

- `build-rust` — no `SscToInt` arm for a character: `error[E0277]: the trait bound &char: SscToInt
  is not satisfied`. `charAt` hits the same wall as `&SscChar`.
- `run` — `String.toList` answers a closure, so `"ab".toList.length` prints `<closure>` and the
  `.map` after it fails to dispatch.

Both filed with rozum's name on them; the first has a branch that makes both lanes answer 97 / 57
for `'a'` / `'9'` and builds the PWA again (1,708,272 bytes, runs, binds its port).

**The `run` half is still open upstream and is deliberately not blocking this.** Their release note
says so in as many words — the `"ab".toList` closure "is a separate defect and is not this branch's
business". The PWA is emitted by `build-rust`, so it is unaffected; what it means is that the same
source still cannot be exercised on the interpreter lane, and the slice in `ucc-ssc-public` carries
the same restriction for the same reason.

**How the colours were proved unchanged**, which is the property the note below protects. The old
deployed binary was serving :8405 while the new one was built from the same source with the port
moved to 8455, and the two were fetched side by side: `/`, `/manage`, `/login`,
`/manifest.webmanifest`, `/icon.svg`, `/mp/demo`, `/incidents/demo`, `/sw.js` — all eight
byte-identical, including all 45 palette occurrences on the index, so every handle hashes to the
colour it hashed to before. After publishing, the live page was byte-identical to the snapshot taken
from the old binary minutes earlier. Compiling is not the claim; serving the same bytes is.

**What this cost before it was looked at.** The installer's comment still described the 2026-08-08
failure, which had been fixed upstream — a stale reason is worse than none, because it sends the
reader after a bug that no longer exists. It was rewritten to say what failed *that* day, and then
that version went stale too, within a week. So the comment is gone and `build.sh` diagnoses itself
instead: on failure it compares the staged toolchain's digest against its own tree and says whether
this is a restage, our source, or theirs. A fact read off the machine cannot rot the way a sentence
about the machine does.

**Do not "fix" this by rewriting `hashStr`.** The line is correct ScalaScript and the colours it
produces are the ones every existing transcript was rendered with; changing the hash changes them.

**Do not restage the toolchain in scalascript's SHARED checkout to unblock a build here.** Siblings
measure against it, and swapping it under them attributes a verdict to the wrong tree. Stage a
detached worktree of their `origin/main` and point `SSC_ROOT` at it — with their content-addressed
cache that is a copy of seconds, not a build. `build.sh` prints exactly those three commands when
it detects staleness.

## BUG-029 — install-bins.sh published the CLI dispatcher over the engine six jobs exec

- **Status:** FIXED 2026-08-13 (`declared_pair` no longer treats `rozum` and `rozum-gateway` as
  interchangeable names; both exec-checks are bounded and the published path is checked too).
- **Severity:** P1. A plain `scripts/install-bins.sh` left `~/.cargo/bin/rozum-gateway` as a binary
  that execs itself. Six launchd jobs run that path — `gateway` (the resident model), `ucc-control`,
  `assistant`, `assistant-groups`, `doctor`, `meeting-daemon` — so the next restart of any of them
  would have hung. Nothing fell over at the time only because every one was holding the old inode.

The install did the right thing and then undid it, five minutes apart:

```
==> building rozum-gateway (rozum, release)
    /Users/sergiy/.cargo/bin/rozum-gateway  (2026-08-10 09:24  ->  2026-08-13 20:49)   # 56 MB engine
==> building rozum (rozum-cli, debug)
    /Users/sergiy/.cargo/bin/rozum          (2026-08-09 15:22  ->  2026-08-13 20:54)
==> also /Users/sergiy/.cargo/bin/rozum-gateway (a launchd job execs this copy)
    /Users/sergiy/.cargo/bin/rozum-gateway  (2026-08-13 20:53  ->  2026-08-13 20:54)   # 634 KB CLI
```

**Root cause.** `declared_pair` allowed `want=rozum` with `have=rozum-gateway`. That exception was
written for `~/.rozum/bin/rozum-ctrl`, which the line above rewrites to `rozum-gateway` — but it
also matched a path genuinely NAMED `rozum-gateway`. The binary split made the two names different
programs (`rozum` = the dispatcher from `rozum-cli`, `rozum-gateway` = the engine from `rozum`), and
after that the exception meant "publish the dispatcher over the engine". This is BUG-028's
neighbour with the sides swapped: the same file already carries a comment about publishing the
54 MB engine over the 627 KB dispatcher on 2026-08-08.

**Why the existing exec-check passed.** `publish` execs the staged copy before the `mv`, and the
staged copy was the dispatcher — whose `--help` execs `rozum-gateway`, which at that moment was
still the engine. The check could not fail: the loop does not exist until the replacement lands.
The published path is now exercised too, and both checks have a `timeout`, because "does not exec"
includes "never returns" — an unbounded check turns a bad binary into an install that hangs.
On failure the previous binary is restored and no service is restarted.

**Found by** running the installer, not by reading it. The tell was `ls`: `rozum` and
`rozum-gateway` the same 634936 bytes, and `rozum-gateway --help` never returning.

## BUG-028 — a binary built in a worktree dies when the worktree is removed

- **Status:** FIXED 2026-08-08 (`scripts/install-bins.sh` refuses to install from a worktree).
- **Severity:** P1 while it lasted — it took the operator's assistant off the air, and the symptom
  named nothing useful.

`mlx-sys` bakes the ABSOLUTE path of its build directory into the binary, because that is where
`mlx.metallib` lives. A gateway built in `.worktrees/feature/x` therefore works exactly until that
worktree is removed — and then fails at RUNTIME with `MLX error: Failed to load the default
metallib. library not found`, which says nothing about worktrees.

**It cannot be caught at install.** `install-bins.sh` execs the fresh binary before publishing it,
and `--help` never touches Metal, so the check passed. The binary was good; its build directory was
about to be deleted.

Sequence, all mine, within two hours: built and installed `rozum-gateway` from
`.worktrees/feature/install-all-copies` → removed that worktree when the branch landed → later
bounced `com.rozum.gateway` to point it at the canonical path → the model could not load at all.

Two ways I made it worse before making it better:

- **I read my own readiness loop wrong.** `for i in $(seq 1 120); do curl … && break; sleep 2; done`
  exits after 240 s whether or not anything answered, and I reported "поднялся" from the fact that
  the loop ended. The service was down. A loop that ends for two different reasons must SAY which.
- **I blamed RAM.** Free memory was genuinely low (a sibling agent's three JVMs), the admission gate
  was refusing, and that was true and irrelevant — the metallib line was two lines further down the
  same log.

Fixed by refusing at the source: `install-bins.sh` stops if it is running from a `.worktrees/`
checkout and says why. Verified both directions — refusal from a worktree, normal install from the
main checkout.

---

## BUG-027 — the repair round after a loop-break was spent echoing the loop-break

- **Status:** FIXED 2026-08-06 (`crates/nadia/src/{session,approval,main}.rs`).
- **Found by:** digging into the one matrix cell that stayed unstable, with the agent's own JSON
  trace — not by review, and not from the harness log, which does not carry tool calls.

Reproduced first, in the matrix's exact configuration (`rozum launch`, `ROZUM_VERIFY=0` so nadia's
own gate owns the run, seatbelt on): **1 of 6 runs left code that compiles**, and the loop-breaker
fired in every one. The same task run directly, without the launcher, passed 8 of 8 — which is why
this had looked like model variance for two days.

The trace says exactly what happened. The final repair turn:

```json
{"text":"The `bash` tool was called 4 times with identical arguments…","stop":"Done","steps":1,"operations":[]}
```

**One step, zero tool calls, and the whole answer is the breaker's own sentence quoted back.** The
break leaves its message as the last thing in the conversation and a 4B model reads that as the
answer. `rozum launch` never had the problem because its repair is a fresh PROCESS; nadia's was a
turn in the same poisoned session.

Fixed: when the breaker fired during a turn, the repair runs in a FRESH session (task + check
output, which is all the next attempt needs) and the breaker's window is cleared — a repair must
not begin three strikes into the window that ended the previous turn.

**And a second defect the fix exposed: the last repair was never checked.** The loop ran
`check → repair → check → repair` and stopped, so the final attempt's work was never judged —
three runs in six reported `✘ проверка НЕ прошла` on code that builds. Now it is `rounds` repairs
and `rounds + 1` checks: "the check decides" cannot be true while the last attempt goes unchecked.

Measured over six runs each, same config:

| | code compiles | verdict matched reality |
|---|---|---|
| before | **1/6** | 4/6 |
| fresh session only | 6/6 | 3/6 |
| **both fixes** | **5/6** | **6/6** |

The sixth is an honest failure: the model did not finish and the gate said so.

**The policy is now one tested function, not a shape inferred from a loop.** Both halves of this
bug lived in that inference — a `for` loop that stopped one check short, and a repair that reused
the session the breaker had just cut — so `gate::next_step` states the three rules and
`crates/nadia/src/main.rs` calls it. Re-measured after the refactor, same six-run protocol: **6/6
compile**, five verdicts printed.

**And the sixth names the cost:** a fresh restart is a whole new turn, and that run hit the 420 s
ceiling in the middle of it. Resuming a session is cheaper than restarting one; the restart is
worth it only where the session is poisoned, which is exactly when it is used.

---

## BUG-026 — the gate invented the answer, then failed correct work for not matching it

- **Status:** FIXED 2026-08-05 (`crates/rozum-agent/src/verify.rs`), test
  `an_expectation_the_task_never_states_is_not_checked`.
- **Found by:** asking why `wordcount` was 0/3 in the matrix when the docs record 4/4.

The `wordcount` task says what the program must DO — read a file, count words case-insensitively,
print the top 3 as `word count` — and never says what it prints, because that depends on a data
file. Asked to formalize it, the model answered with an expectation it made up: `a 3 / c 2 / d 2`,
three lines that appear nowhere in the task and match no input. The gate then demanded them.

**A correct program cannot pass that check**, and both repair rounds went into fighting it instead
of the compile errors that were actually in the way. Same false negative as BUG-018, arriving
through the schema's other field.

`checkable: false` is what the prompt asks for here and the model does not always give it, so the
guard is deterministic now, exactly like `task_argv_for`: an `expect` the task does not state is
dropped, and the check falls back to what can be established — `cargo build -q`.

Measured, same task, same model, same machine:

| | before | after |
|---|---|---|
| runs passing | **0 / 4** | **3 / 3** |
| output by hand | 4 different compile errors | `apple 3 / banana 3 / cherry 2` in all three |

The interesting part is what this corrects about YESTERDAY'S conclusion: the matrix had `wordcount`
down as the model's ceiling ("a 4B model cannot write this"), and it was our check making it
unwinnable. The evidence for the ceiling was four failures whose *causes* were four different
compile errors — that variety should have been the clue, and instead it read as capability.

---

## BUG-025 — the meeting daemon detaches, so its launchd job can never own it: a permanent respawn loop and duplicate daemons on one socket

- **Status:** FIXED 2026-08-05, both halves. The ownership half landed second
  (`docs/specs/meeting-socket-ownership.md`): a daemon takes an `flock` beside the socket before it
  touches the socket file, and a daemon that cannot take it REFUSES rather than unlinking a live
  listener's socket. Verified on the host — a challenger against a live owner printed the refusal
  and the owner was untouched, where before it would have bound over it.
  **Accepted again on the host 2026-08-16**, on an isolated `XDG_RUNTIME_DIR` so the operator's live
  daemon was never involved. The ordinary case never reaches the lock — `daemon_alive` refuses first
  ("already running", exit 0, socket inode unchanged, one process). Forcing the case the lock exists
  for (owner alive, socket file removed, so the pre-check sees nothing): the challenger was refused
  by the lock, named the wedged owner, and left the owner running. One defect found by doing it:
  that refusal exited **0**, so `$?` said the daemon had started — fixed in the commit before this.
  **Three things the host found that the unit tests could not, all now fixed:**
  1. `supervised_by_launchd` accepted any non-empty `XPC_SERVICE_NAME`. macOS sets that variable to
     the string `0` on ordinary processes, so an interactive start against a live daemon waited
     forever. It also INHERITS, so a client under `com.rozum.gateway` would have made the same call.
     It now tests for this job's exact label. **The unit test passed throughout, because it asserted
     the rule the code implemented rather than the behaviour the system needed.**
  2. The supervised retry had no sleep. With the owner's socket file missing, `daemon_alive` returns
     instantly and the bind is refused instantly, so the retry burned CPU — measured at 3–4% per
     process. Now 1 s between attempts (measured after: 0.03 s of CPU over 10 s).
  3. **Forbidding theft removed the system's only self-healing move.** An owner whose socket path no
     longer points at its socket is unreachable, holds the lock, and no successor may take over — so
     the service stayed dead until a human killed it, which is WORSE than the bug. The owner now
     watches its own reachability by inode every 5 s and exits when it is no longer the socket
     clients reach; the kernel drops its lock and the next start serves. Measured on the host:
     removing the socket file, service back in ~2 s with no human. The shutdown path had the same
     theft one step later — it removed the socket path unconditionally — and now removes it only
     while it is still ours.
- **A blind spot this opens in `doctor --services`, for whoever owns that check:** it reports the
  launchd job's pid and probes the endpoint, and both pass when the job's process is merely WAITING
  while a client-spawned daemon serves. Observed exactly that: `svc:meeting-daemon running (pid
  42206)` while 42132 held the lock and the socket. Healthy, but not what the line says.
- **Previously:** HALF FIXED — the respawn loop was fixed (`src/main.rs`, spec
  `docs/specs/meeting-daemon-supervise.md`): under launchd the foreground start now WAITS for the
  incumbent and takes over instead of exiting 0. **Verified on the host by rebuilding the exact
  condition** — job booted out, daemon started by the CLIENT path, job bootstrapped back: `runs`
  1 → 1 over 75 s where it used to climb every ~9 s, and the log stopped growing. Then the incumbent
  was killed and the job's process took over (`meeting daemon gone; taking over`), leaving ONE daemon
  that launchd owns and that holds both the socket and `:8401`. `doctor --services` now reports
  `svc:meeting-daemon` as `ok` where it reported the split as `warn`.
  **One measured limit, worth more than the fix:** the takeover is a RACE. At a 2 s poll the
  supervised process lost it every time — a client spawns the instant a connect fails, a poller
  wakes on its own schedule. 200 ms wins in practice (measured: takeover on the second check) but
  the race is real, and only the ownership lock removes it. **That half then LANDED — this sentence
  is what the record said before it did, and it stayed here reading as current until 2026-08-16,
  when it sent someone to re-implement a lock that was already in the tree with five unit tests
  (`acquire_socket_ownership`, taken by `serve_daemon_until` before `remove_file`).** The status
  line at the top of this entry was right the whole time; a narrative paragraph that keeps the old
  tense is a second copy of the status, and the copy is the one that goes stale.
- **Also filed independently, ninety minutes earlier, from the other end:** `doctor --services`
  reported the split ownership as `warn` (BACKLOG `meeting-daemon-ownership`, now a pointer here).
  Two agents, two symptoms, one problem — and that check is the way to see the fix land.
- **Found by:** noticing `com.rozum.meeting-daemon` had `runs = 78` and a log that was 525 lines of
  the same sentence. Nothing was broken from the outside — rooms answered, `:8401` answered.
- **Severity:** P2. Nothing is lost today, but it burns a process spawn every ~9 seconds forever,
  grows a log without bound, and can put the unix socket and `:8401` in DIFFERENT processes.

**The loop.** The plist is `KeepAlive = true` and runs `meetings start --foreground`. When any
daemon already holds the socket, that command prints `meeting daemon already running` and exits 0 —
so launchd immediately starts another, which sees the same thing. Measured: `runs` climbed 78 → 90
in about four minutes, roughly one spawn every nine seconds, indefinitely, with one log line each.

**Why the job cannot win.** The running daemon has `ppid 1` — it detaches. launchd can therefore
only own it when the job's OWN process becomes the daemon, which happens only if no other daemon
exists at spawn time. Once a client has autostarted one (BUG-024's `spawn_daemon`), the managed job
is locked out of ownership permanently, and `launchctl list` shows `-` where a pid should be.

**Killing does not fix it, and this is the part to keep.** Terminate the daemon and a replacement
appears within seconds, spawned by whichever client next touches the socket — observed three times
in a row on this host (pids 66346, 76781, and one during the bootstrap window). A daemon that exits
also takes the socket FILE with it, so the next client sees no socket, spawns its own, and binds a
fresh one — while the previous listener may still be alive on the unlinked inode. That is how two
daemons end up on one path.

**The split that results.** Observed directly: `:8401` was held by pid 70945 (the launchd-owned
instance) while the unix socket's accepted connections were on pid 76781 (a client-spawned one) —
nine connection fds on the second against one listener fd on the first. Both read the same room
files on disk, so this is not visible as wrong answers today; it is the same shape as the messenger
pool's orphan double-reply, and it is BUG-013's family: running, and not serving what you think.

**How it was left on this host.** launchd owns pid 70945, `runs` has been stable for minutes, the
socket answers `meetings status`, and `:8401` answers 200. One extra daemon may still appear; it is
harmless in the way described above.

**Fix directions, none implemented, smallest first.**
1. Make `meetings start --foreground` SUPERVISE rather than exit when a daemon exists — then the
   launchd job always owns a process and `KeepAlive` means what it says.
2. Make socket ownership authoritative with a lock file, so a second binder refuses instead of
   unlinking a live listener's socket. This is the one that removes the duplicate-daemon class.
3. Have `spawn_daemon` hand off to launchd where a job exists, rather than creating a peer it will
   then have to compete with. Related to BUG-024, which was closed on the narrower question.

**Repro.** `launchctl print gui/501/com.rozum.meeting-daemon | grep 'runs ='` twice a minute apart
while any client-spawned daemon is alive; the counter climbs. `ps -o ppid= -p $(pgrep -f 'meetings
start')` shows `1`.

---

## BUG-024 — a client-triggered auto-start brings the meeting daemon up WITHOUT its REST secret, and then holds the socket

- **Status:** DONE 2026-08-05. Filed first as a duplicate `BUG-017` — that number was already
  nadia's jail bug, so it moved here; anything you read quoting BUG-017 for the meeting daemon
  means this entry.
- **Symptom.** `:8401` stops answering while `launchctl list` shows the service, a daemon process
  exists, the socket is present and every room still works over MCP. Nothing looks wrong. The
  support console, the web console and the generated meeting client all go quiet.
- **Mechanism.** `daemon_proxy::spawn_daemon` runs `meetings start` when a client cannot reach the
  daemon (`crates/rozum-meeting/src/meeting/daemon_proxy.rs:798`). The child inherits the CALLER's
  environment — an agent's MCP proxy, a CLI invocation — which has no `ROZUM_WEB_SECRET`. The REST
  listener is spawned only when that variable is set (`rest_read::maybe_spawn_from_env`), so the
  resurrected daemon serves the unix socket and nothing on `:8401`. It then holds the socket, so the
  launchd job — which DOES carry the secret — cannot take over even when it restarts.
- **Repro.** Kill the daemon, then touch any room from an MCP client before launchd's restart wins:
  the daemon comes back, rooms work, `curl :8401/rooms` answers nothing.
- **I MISDIAGNOSED THIS FIRST**, and the wrong version reached the room: I said the "already
  running" guard tests a file rather than a process. It does not — `daemon_alive` opens a real
  connection. That guard is fine; the environment is the defect.
- **FIXED 2026-08-05** — the REST secret is now read from `~/.rozum/secrets/web-secret` when the
  environment has none, so `:8401` is a property of the INSTALLATION rather than of who won the
  socket; the env still wins, so a configured service keeps overriding the file. And the absence of
  any secret is now a loud `warn!` instead of a silent `return` — the silence is what made this cost
  hours. Covered by `the_web_secret_falls_back_to_disk_but_the_environment_still_wins`, which also
  pins that a blank value is NOT a secret. The secret was written to that path on this host at 600.
  **CONFIRMED IN PRODUCTION 2026-08-05, by accident.** While cleaning up BUG-025 a daemon spawned by
  a CLIENT — parent `launchd` after detaching, no `ROZUM_WEB_SECRET` anywhere in its environment
  (checked with `ps eww`, which does show other processes' environments on this host: `PATH` and
  `HOME` were visible and the secret genuinely was not) — served `:8401` correctly. Before this fix
  that daemon would have served the socket and nothing else, which is the whole bug. The unit test
  said the fallback works; this says it works where it matters.
  ⚠️ The code landed inside another agent's claim commit `beace56` by accident: I edited it in the
  SHARED main checkout instead of my worktree, and their `claim:` commit swept it. Told them in the
  room; history on a shared branch is not worth rewriting for this. `AGENTS.md` says worktree first,
  and that rule exists for exactly this.
- **The stronger fix was considered and DECIDED AGAINST 2026-08-05, by the operator, on my
  recommendation — and I had recommended the opposite before, so the reversal is the point.**
  Making `spawn_daemon` refuse the socket whenever REST cannot start reads well in the abstract, but
  the file fallback already removed the failure that actually happened. What would remain is the case
  where no secret exists anywhere — a host where the web console was never configured — and there
  starting without REST is CORRECT. The strong fix would trade a rare degraded mode for a common hard
  failure, on machines whose owners never asked for REST at all. A daemon that cannot serve its whole
  contract should not claim the socket *when that contract was asked for*; here it was not.
  If a future host does want it strict, the honest shape is to refuse only when REST was EXPECTED —
  a secret exists but binding failed — which is a different bug from this one.
- **Fix candidates considered:** have `spawn_daemon` refuse when the secret is absent and tell the
  caller to start the service instead; or have the daemon read the secret from the same place the
  service does rather than from its environment; or make an autostarted daemon step aside for the
  managed one. I argued for the first at the time; see the decision above for why that argument does
  not survive the second option having been implemented — reading the secret from where the service
  reads it makes the refusal moot in every case where anyone wanted REST.
- **Why it matters beyond this bug.** It is BUG-013's family: a service that is running and not
  serving, with every green surface still green.

---

## BUG-023 — one ended poll stream took the whole bridge down

- **Status:** FIXED 2026-08-05 (`crates/rozum-meeting/src/messenger.rs`), test
  `a_daemon_restart_is_survived_not_reported_as_failure`, checked against the old behaviour first.
- **Found by:** reading `launchctl list` while verifying something else — `com.rozum.telegram-groups`
  sat at exit code 1, and both bridge logs carry three `meeting daemon poll ended` exits each.

`DaemonBridge::next_outbound` turned an ended poll stream into a fatal error **on purpose**: "so a
process supervisor can restart the bridge after a daemon restart". launchd `KeepAlive` did restart
it, so the system looked self-healing — and the price was paid by everything else in that process.
One stream ending took down every chat the bridge serves, the group-registry watcher and nadia's
result watcher; until that same morning it also killed `nadia serve` and every running agent
(BUG-019's sibling, fixed in `nadia-serve-lifetime`).

Now the bridge reconnects with bounded backoff (~50 s of trying), says so once, and still gives up
if the daemon stays gone — a dependency that never comes back must not look healthy.

**Two things the test found that review had not:**

- **The reconnect replayed what had already been delivered.** Carrying the poll cursor across the
  reconnect is not enough: the delta is not exclusive at both ends, and the last message before the
  outage arrived a second time. The bridge now filters by the high-water it has actually handed to
  the messenger, so "a daemon blink does not repeat what I already read" is a property of the
  bridge rather than of cursor arithmetic.
- **Setting that cursor on a FIRST connection replayed the whole room.** `enter_named` leaves the
  cursor at the room's head, which is exactly what a new connection wants; overwriting it with
  `None` made the poll start at the beginning of the day file. Caught by the neighbouring test that
  exists for that rule — the one place review had already thought about.

Latency worth knowing: a daemon that dies without closing its sockets is noticed only when the
long poll times out (30 s). Nothing is lost — the gap is delivered late, because the resumed poll
starts from the bridge's own high-water — but it is not instant.

---

## BUG-022 — the message named the sandbox root while the work was elsewhere

- **Status:** FIXED 2026-08-05 (`crates/rozum-meeting/src/telegram/nadia.rs`), test
  `the_ack_names_the_directory_the_work_is_in`, proven to fail against the old behaviour.
- **Found by:** the operator, one deploy after BUG-021: "ответ всегда один и тот же".

The per-task directories from BUG-021 worked — four `/spawn`s produced four directories, each with
its own `touched: ["src/main.rs"]`. The **message** still said
`📁 /Users/sergiy/.nadia — это личная песочница nadia`.

The ACK read the workspace out of the POST response, and `POST /agents` answers `{"id": N}` and
nothing else. The field was always absent, the rendering fell through to the old shared-root hint,
and from the phone the fix looked like it had not shipped at all.

**Two sources for one fact.** The request knew the directory because it chose it; the message went
looking for it somewhere else. Fixed by choosing once in `spawn_workspace` and passing that value
to both the request and the rendering, so they cannot disagree.

**The test I wrote first would not have caught it** — it hand-built a response containing
`workspace` and asserted the rendering, which was never the broken half. The one that ships starts
where dispatch starts: it calls the chooser, renders with the real response shape (`{"id": 4}`),
and was confirmed to FAIL against the old code before being kept. A test written from the fix
tests the fix; a test written from the failure tests the failure.

---

## BUG-021 — a green check on work nobody did

- **Status:** FIXED 2026-08-05 (`crates/rozum-meeting/src/telegram/nadia.rs`), test
  `every_task_gets_its_own_directory`.
- **Found by:** the operator, sending the same `/spawn` twice from the phone.

Agent #2 reported *"Created a Rust program … Verified: cargo run -- 3 4 outputs 7"*, the gate
reported `✔ проверка прошла`, and `touched` was **empty** — it wrote nothing. The first task's
program was already in the sandbox root, `cargo run -- 3 4` printed 7 the moment it was asked, and
every signal we have said success. 36 s, no files.

**The check verified the directory, not the run.** That is the same false pass the gate was built
to prevent, entering through the one door nobody had closed: every task from the phone shared one
workspace. The quieter cost is that task N+1 overwrote task N's `src/main.rs` in place — the
hello-rpn next to it survived only because it lives in a subdirectory.

Fixed: a `/spawn` with no `/project` chosen gets `~/.nadia/tasks/<date-time>-<slug>/`, created by
the bridge before the agent starts (an agent that has to make its own workspace spends a tool call
on it and sometimes puts the project one level down, which the gate then reports as a failure). A
chosen project still means "work in this tree", because there reuse is the entire point.

Proven by running the same task in a fresh directory: 123 s, 7 tool calls, `touched:
["src/main.rs"]`, check passed — on work that run actually did.

**The general shape: a verifier is only as honest as the state it runs against.** Ours ran against
a directory that a previous run had already satisfied, so it reported on the directory. Any check
that can pass without the work happening needs to start from a state where it cannot.

---

## BUG-020 — the other bot answered

- **Status:** FIXED 2026-08-04 (`crates/rozum-meeting/src/telegram/{mod,nadia}.rs`), test
  `the_bot_that_took_the_command_is_the_bot_that_answers`.
- **Found by:** the operator, on the phone, in the one place no test looks.

They sent `/spawn` to **Rozum.Chat** (`@Rozum_chat_bot`), got "агент пошёл работать" from it — and
the RESULT arrived from **Rozum IA** (`@rozumia_bot`), a different bot in a different conversation.

Both bridges are the same binary and both run `nadia::watch_results` against ONE global
`~/.local/state/rozum/nadia-telegram.json`. `Watch` recorded `{chat, task}` — which chat, never
which bot — so delivery was a race between two five-second pollers: whichever won removed the entry
and sent it with its own token. In a private chat the chat id IS the operator's user id, and both
bots can post to it, so the wrong bot's message looks perfectly delivered.

The comment already on `Watch` reasons one level short of the bug: it explains that an agent id
alone could deliver one chat's result into another chat, and stops at the chat. `ChatState` had the
same shape one field over — `dialog` and `project` keyed by chat id alone, so `/nadia on` in one bot
turned the *other* bot's plain messages into agent tasks for the same person.

Fixed by giving both the owner they were missing: `Watch` carries the registry that took the
command (`TELEGRAM_REGISTRY`, `telegram` / `telegram-groups`) and a bridge delivers only its own —
and does not drop anyone else's, because the bridge that took the command is the one that owes the
answer. Chat state is keyed `<bot>:<chat>`. Entries written before the field belong to `telegram`,
the only bridge that could have written them; migrated on read so an upgrade does not lose a mode
the operator had set.

**The general shape, worth remembering: two processes, one state file, and a key that identifies
the conversation but not the participant.** Same class as the global `rooms.json`. Anything stored
per chat and read by two bots has to say which bot it belongs to.

---

## BUG-019 — the run with the most doubt got the least verification

- **Status:** FIXED 2026-08-04 (`crates/nadia/src/main.rs`, `crates/nadia/src/supervisor.rs`),
  regression test `a_budget_exhausted_run_is_still_checked` in `crates/nadia/src/gate.rs`.
- **Found by:** running the two ported gates end-to-end (`gate-e2e`), not by review.

An agent that stopped for any reason other than "I am finished" had its gate loop `break` before
the check ran — so a check that had already been derived, printed to the operator, and cost a model
call was discarded unrun. The report then read `⚠ не проверено — у задачи нет
машинно-проверяемого критерия`, which is false twice over: there WAS a criterion, and it was never
applied.

Measured: an RPN attempt exhausted its 24 steps; the program it left on disk printed nothing for
`cargo run -- "3 4 + 2 *"`, the exact invocation the task named. The operator was told the task had
no machine-checkable criterion.

The reasoning that produced it was sound for the JUDGE and wrong for the check: a model's opinion
about an interrupted attempt really is worth less than an explanation, but a deterministic check
costs one shell command and answers the question the operator actually has — *is what is on disk
right?* A budget-exhausted run is where that question matters most.

Fixed so the deterministic check runs whatever the stop reason was, while the judge still stands
down for a non-finished run and no repair round is spent on an agent with no budget to repair with.
The exit code follows the same rule in both directions: a run that exhausted its steps *after*
satisfying the criterion now exits 0 and says so, because the check decides — that is the whole
premise, and it cannot hold in one direction only.

**Interesting detail: the two ports were already right.** Scala 3 and ScalaScript ran the check
regardless of the stop reason; only the Rust reference had the early `break`. Porting a contract
into a second and third implementation is a way of reading it that review is not.

---

## BUG-018 — the gate failed correct work because it lexed the arguments wrong

- **Status:** FIXED 2026-08-04 (`crates/rozum-agent/src/verify.rs` + both ports), tests
  `arity_comes_from_the_task_not_from_the_model`, `the_lexer_groups_what_the_quotes_group`,
  and twins in `nadia:scala/sdk/Verify.test.scala` / `nadia:src/gate-check.ssc`.
- **Found by:** the first end-to-end run of the Scala 3 gate (`gate-e2e`).

For the task *"cargo run -- 3 4 must print 7"* the derived check was
`cargo run -q -- '3 4'` — both numbers in ONE argument. The program the model wrote was correct;
`cargo run -- 3 4` printed `7` by hand, in the same workspace, at the same moment. The gate spent
both repair rounds and reported `✘ проверка НЕ прошла`, exit 1.

**A false negative is the expensive kind of gate defect**: the operator is told correct work is
broken, and the model is sent to break it.

The cause was a schema that could not express arity — `run: [{"arg": "<value>", …}]`, one string,
quoted into one shell literal. `arg` had also just been taught (BUG-016 era, `vga-arg-quotes`) to
strip the task's delimiting quotes, which is right for `cargo run -- "3 4 + 2 *"` and destroys the
only signal that distinguishes it from `cargo run -- 3 4`.

The first fix — asking the model for a LIST — traded one false negative for its mirror image, and
the run that proved it was the RPN task: asked for a list, the model split the task's single quoted
argument into five, `'3' '4' '+' '2' '*'`. A program that accepts exactly what the task asked for
would now fail. Measured, not predicted; it passed only because the model then wrote a program that
accepts both shapes.

So the real fix moves the question away from the model: **arity is syntax, and the task already
wrote it.** `task_argv_for` lexes what follows `cargo run --` in the task with shell rules and
takes the shortest prefix whose words are the value the model reported — the model still says
*which* example and *what output*, which is what a model is good at. Both directions verified
end-to-end afterwards, in all three implementations, artifacts checked by hand.

---

## BUG-017 — the jail let the agent delete its own workspace

- **Status:** FIXED 2026-08-04 (`crates/nadia/src/sandbox.rs`: one `(deny file-write-unlink
  (literal "<root>"))` next to the existing subpath allow), with a test that runs the real
  `rm -rf` under the real profile.
- **Source:** found while demonstrating the new verify gate — not reported, and it would not have
  been: the symptom looks like the gate misbehaving.
- **Severity:** P1 for anyone who points `/project` at a real repository. The jail is the whole
  containment story for `bash`, and this was a hole in the middle of it.

**What happens.** The seatbelt profile allows `(subpath "<root>")`, and on macOS that covers the
directory NODE as well as its contents — so `rm -rf <root>` from inside the jail succeeds. The
agent then keeps running with no working directory:

```
nadia: check failed — repair round 1
nadia: check failed — repair round 2
nadia: ✘ проверка НЕ прошла: …
verify command failed to run          ← there is no directory to run it in
```

Reproduced deterministically outside nadia, with a hand-written profile of the same shape: `rm -rf`
of the root returns 0 and the directory is gone. With the deny added, the same command is refused
and the root survives, while writing, `mkdir`, and deleting files and subdirectories INSIDE keep
working — deleting what it created is ordinary work and must stay allowed.

**Why it matters more than one lost scratch directory.** With a project selected
(`/project rozum`), the workspace is a real repository. And the failure is silent in the worst way:
what the operator sees is `✘ проверка НЕ прошла`, which reads as "the agent could not do the task",
not as "the agent deleted the task".

**Not covered by this fix, deliberately.** An agent can still `rm -rf *` inside its workspace. That
is legitimate work and cannot be distinguished from vandalism by a profile — the answer there is
version control, not the sandbox.


## BUG-016 — nadia × Qwen3.5-4B: a path written without its leading slash lands a file in a mirror tree, and the run reports success

- **Status:** FIXED 2026-08-01 by candidate 1 below — the system prompt now names both wrong
  shapes verbatim (`{root}/src/main.rs` and `{root-without-leading-slash}/src/main.rs`) and says
  what happens if you use them. Measured on the same task: before, `touched` was
  `["scratchpad/proj-check/src/main.rs"]` and the file sat in a mirror tree; after, `["rpn.rs"]`
  in the workspace root. The jail was never changed — candidate 2 stays unspent, as ordered.
  Reachable again the moment a different model does it, which is why the entry stays.
- **Status (original):** open. NOT a nadia code defect — `Sandbox::resolve` handles an absolute path
  correctly (`is_absolute` → used as given, never re-rooted). This is the model emitting the
  workspace's absolute path with the leading `/` stripped, which is then a legal RELATIVE path.
- **Source:** found while verifying the UCC coder path, 2026-08-01. One run in four.
- **Severity:** P2. Nothing crashes; the answer is confidently wrong, which is worse.

**What happens.** Task: *create a file named `ucc.txt` whose only content is the word ok*, in
workspace `/private/tmp/…/uccbridge`. The model called `write_file` with
`private/tmp/…/uccbridge/ucc.txt` — the workspace path minus its leading slash. That is a
perfectly ordinary relative path, so the jail accepted it and created the whole mirror tree:

```
uccbridge/private/tmp/claude-501/…/scratchpad/uccbridge/ucc.txt
```

nadia then answered *"The file has been created successfully with the content ok. The task is
complete."* and exited 0. Both statements are true about the file it made; neither is true about
the file that was asked for.

**Why nothing caught it.** The workspace had no cargo project, so `rozum launch`'s verify-gate had
nothing deterministic to run (`verified = None`) — the documented behaviour, and precisely the
hole a false success falls through. The same task in the same shape succeeded in three other runs
(`hi.txt`, `live.txt`, `ok.txt` all landed at the root), so it is stochastic, not systematic.

**Candidate fixes, cheapest first.**
1. Tell the model, in the system prompt, that paths are relative to the workspace root and that it
   must not restate the workspace path — the same class of anchoring fix as R4's `tool_hint`
   (`project-matrix-hygiene-testcell`), which cured absolute-path anchoring for opencode.
2. Refuse (not clamp) a relative path whose leading components reproduce the workspace's own path —
   a jail that creates `<root>/private/tmp/…/<root>` is answering a question nobody asked.
3. A non-cargo verify floor: when the task names a file, check that file exists at the root.

Do 1 before 2: a refusal the model does not understand becomes a loop.

---

## BUG-015 — the warm cache loaded a second copy of the resident model, because one model has two spellings

- **Status:** fixed this commit (`gateway.rs`: `enter` compares with `same_model`, and the warm
  map is looked up by equivalence rather than by exact key), with two tests.
- **Source:** found while making nadia accept a Hugging Face repository id. Not reported by
  anyone — nothing fails visibly, it just costs twice the RAM.
- **Severity:** P2, but sharp on a small machine. One 4B model is ~5 GB; two of them on a 36 GB
  Mac already competes with the admission gate this project built to avoid jetsam
  (`docs/specs/`, residency work). On a bigger model it is the difference between fitting and
  swapping.

**What happens.** `Switchboard::enter` routed by `model != self.model_id()` — a string
comparison. rozum launches with `mlx-community:Qwen3.5-4B-MLX-4bit`; the Hub writes the same
repository as `mlx-community/Qwen3.5-4B-MLX-4bit`, and that is what anyone copying an id off a
model page sends. The two spellings compared unequal, so the request took the multislot path
and `ensure_warm` built a **second resident copy of the primary's own weights**.

Reproduced live against the running gateway — a chat request with the slash form, and the log
answers for itself:

```
{"event":"warm_built","model":"mlx-community/Qwen3.5-4B-MLX-4bit"}   ← primary is …:Qwen3.5-4B-MLX-4bit
```

**Why it survived.** `same_model` already existed, in `rozum-models::model_source`, written for
exactly this and used by `WarmConfig::new` two hundred lines away — the footprint lookup got it
right while the routing decision above it did not. The warm map had the same shape of bug one
level down: keyed by whatever string the first requester used, so an exact-key lookup misses its
own entry when the second caller spells it differently.

The test stub was complicit and is fixed too: `warm_cfg` matched model specs by exact key, so it
was *stricter* than production and could not have shown this. A stub that is stricter than the
real thing hides the whole class.

---

## BUG-014 — the loop breaker cut agents off mid-repair: signature 4 matched the call but not the result

- **Status:** fixed this commit (`loopbreak.rs`, signature 4 now keys on the result too), with
  tests pinning both directions.
- **Source:** found while reading a matrix run, NOT reported — the intervention is invisible from
  the score, because most of the cells it cut still passed.
- **Severity:** P2 — it did not change the 2026-07-31 result, but it fired on 11 of nadia's 16
  cells and 6 of codex's while leaving claude's untouched, so any comparison between agents was
  being made under an intervention that was not applied evenly.

**Symptom.** Cells ended with the agent stopped mid-task and this in `agent.log`:

```
The `bash` tool was called 4 times with identical arguments in the last 12 tool calls —
the agent is repeating the same action without making progress …
```

Measured over the kept workdirs of `scripts/bench/results/agentic-20260731-075158`
(Qwen3.5-4B, 8 tasks x 2 reps x 4 agents):

| agent | cells cut |
|---|---|
| nadia | 11 of 16 |
| codex | 6 of 16 |
| claude | 0 |
| opencode | 0 |

**Diagnosis.** Signature 4 counted a repeat by `(name, input)` alone. That is the signature of a
spin *and* of the verify half of fix → test → fix: an agent whose prompt tells it to check its
work re-runs `cargo test` on purpose, byte-identically, and the output differs every time because
the files changed underneath it. The skew across agents is the tell — it is not that nadia and
codex loop and claude does not, it is that their prompts ask them to verify in a way this
signature reads as churn.

**Fix.** A repeat counts only when the **result did not change either**. The true positive is
untouched (identical call *and* identical output is a spin — `stuck_loop_fires_on_repeated_bash_
verification` still fires, since its helper returns fixed content), and the false positive is
gone. `ContentBlock::ToolResult` already carried `content`; the collector was discarding it with
`..`.

**Prior art, same defect, same week.** nadia's own loop breaker
(`crates/nadia/src/session.rs`) shipped with exactly this bug and was fixed the same way on
2026-07-31 — there it cut `multibug` mid-repair, and adding the result comparison turned that
cell from FAIL to PASS. That is the evidence the fix does not cost the true positive.

**Not verified by a re-run yet.** The matrix that produced the measurement above ran against the
old binary; a clean comparison needs another pass. Left open deliberately rather than claimed.

---

## BUG-013 — `com.rozum.gateway` crash-looped for 4 days, silently taking the messenger assistant down

- **Status:** fixed live 2026-07-27 (service reloaded, verified end-to-end); deploy-side guard added
  this commit so a recurrence fails the deploy instead of going unnoticed.
- **Source:** found while answering "what is the state of the project" — NOT reported. Nothing
  surfaced it: no alert, no log line, no failing test. That is the actual severity here.
- **Severity:** P1 — the flagship feature (the Telegram assistant, the whole 20-23 July arc) was
  dead in the field for 4 days and the only symptom was "the bot doesn't answer".

**Symptom.** `launchctl print gui/501/com.rozum.gateway` → `state = spawn scheduled`,
`runs = 36301`, `last exit code = 78: EX_CONFIG`. Nothing listening on :8089. `~/.rozum-gateway.log`
untouched since 2026-07-23 05:22 — i.e. **36k respawns produced zero output**, the process died
before it could write a line. Meanwhile `~/.rozum/bin/rozum-gateway` was replaced at 06:17 that
same morning.

**Blast radius.** Every messenger participant is launched with
`--gateway-url http://127.0.0.1:8089/v1` (`com.rozum.assistant` pool, both the private room and
the group room). With :8089 dead they stayed alive, joined their rooms, and answered nothing.
`com.rozum.telegram` also stayed up and kept polling, so from the outside the bridge looked
healthy. Last real exchange in room `assistant`: 06:04 on 23 July, minutes before the binary swap.

**Diagnosis.** Running the job's EXACT command by hand works and serves normally (model loads,
`ready (context 32768)`, completions return) — so it is not the binary, the args, the model, RAM,
or the port. The job itself was the broken part: a stale launchd registration against a binary
that had been replaced underneath it (`properties = … needs LWCR update`), which launchd refuses
to exec and reports as EX_CONFIG with no output. `launchctl bootout` + `bootstrap` fixed it
instantly: `runs = 1`, `state = running`, `:8089` LISTEN in ~5s.

**Verified after the fix.** `curl /v1/chat/completions` on :8089 → `content: 'OK'`,
`finish_reason: stop`. Assistant pool restarted (`kickstart -k`) so both children are fresh; a
ping posted into room `assistant` was answered by `qwen` within seconds. Swept every other rozum
service — `assistant`, `telegram`, `meeting-daemon`, `mcp-http`, `ucc-control`, `meeting-ssc` all
`state = running`, `runs = 1..2`: the gateway was the only affected job.

**Why it hid for 4 days (the part worth fixing).** `deploy-ucc-web.sh` bootstraps the gateway and
then prints `warming in background` — it never checks that anything came up, and a KeepAlive job
that can never exec looks exactly like one that is still warming. FIX: step 5c polls :8089 for up
to 90s after the bootstrap; still not listening → read the job state back, WARN if launchd says
`running` (slow cold load), and hard-`exit 1` with the bootout/bootstrap recipe if it is not.

**Update 2026-08-05 — one candidate trigger RULED OUT, by experiment.** After taking both bridges
down with a hand-typed `cp` over a running binary (see `deploy-atomic-install`), the obvious story
was "in-place overwrite poisons the exec, launchd reports EX_CONFIG". It does not. A scratch
`KeepAlive` job was pointed at a copied binary, the binary was overwritten IN PLACE while its
long-lived process was running, and the process survived; killing it produced a clean respawn on
the new binary (`runs = 3`, exit 2 = "no such subcommand"). So that explanation is wrong, and the
record says so rather than carrying a plausible one.

What the same day DID establish: **78 is not ours.** Every binary here exits 0, 1 or 2 —
`grep process::exit` — so `EX_CONFIG` comes from launchd refusing to exec, which is also why 36,301
respawns wrote nothing. `doctor --services` now says that in words next to the code, instead of
printing a number the reader has to know.

And the cheap guard that makes the whole question matter less: `install_bin` in the deploy now
**execs a freshly built binary before publishing it**, and refuses to install one that will not
start — the running service keeps the old binary, which is the point. Whatever produced an
unexecutable file on 23 July, that is where it would have been caught, in one second instead of
four days.

**Note on the exact trigger.** The proximate cause (stale registration vs a replaced binary) is
established by the fix working; WHICH deploy path left it that way on 23 July is not provable from
the artifacts 4 days later — the script's own ordering (`rm`+`cp` the binary at line 86-87, bounce
the job at 505-511) is correct, and `UCC_SPA_ONLY=1` consistently skips both. Do not over-read it;
the guard above is what makes the question stop mattering.

---

## BUG-012 — UCC launch registries: concurrency races + terminal reconnect loop (audit sweep)

- **Status:** fixed on `a1c073c`, deployed 2026-07-08; live-verified (concurrent launch, stop-during-start).
- **Source:** adversarial audit of the day's async-launch + terminal work (two review agents), not a
  field report — caught before the operator hit them.
- **Severity:** P2 — real races, but they need concurrent actions / restarts / same-second launches
  to trigger; the happy path was already working.

**Findings + fixes (all in `control.rs` + `terminal.ssc`).**
1. Lost update: `live_sessions/agents/coders` rewrite the registry on the STATUS-POLL path, so a
   poll's save could clobber a concurrent launch → orphan process / row stuck `starting…`. Fixed:
   `registry_lock()` serializes every load-modify-save.
2. Orphan on stop-during-spawn: a stop that removed a `starting…` (pid 0) record while the bg task
   was mid-spawn left an untracked participant/coder process. Fixed: `update_*_record` returns
   whether it hit; the spawn kills the just-spawned pid if the record is gone.
3. Eternal `starting…`: a control-serve restart mid-launch orphaned the row forever (prune kept all
   `starting…`). Fixed: `STARTING_TTL_SECS` (900s) prune / show-failed.
4. ID collision: two launches of the same agent in one second shared a tmux name → the 2nd
   `new-session` 500'd (sessions) or updated the wrong record (agents/coders). Fixed:
   `next_launch_seq()` suffix.
5. Terminal infinite reconnect: `onopen` reset the retry budget every cycle, so an
   open-then-immediately-close (session already ended) looped forever. Fixed: reset only after a 5s
   stable connection.
6. Terminal duplicate sockets: a tap during the retry wait + the pending timer both called
   `connect()`, doubling output. Fixed: `clearTimeout` + already-opening guard; manual reconnect
   resets the budget.

**Verified.** `cargo test --workspace` 635/0 (incl. new next_launch_seq + starting-TTL tests);
live: two simultaneous `/session/launch` → distinct ids `…-0`/`…-1`, both running, 2 tmux, no 500;
coder launch + immediate stop → 0 rows, 0 stray processes. Also removed `footprint_report`/
`footprint_for` (orphaned when launches went async).

**Residuals — now FIXED too (`f686c61`, deployed 2026-07-08).** (a) `remain-on-exit` race: the tmux
launch is wrapped so the pane HOLDS after `rozum launch` exits (`…; printf exit-code; exec sleep`)
instead of relying on a post-create `set-option` — race-free; verified an instant-failing launch
(exit 127) keeps its output on screen. (b) mouse filter now parses the SGR button code and keeps
wheel (bit 6 — bare AND modified wheel scroll), strips press/drag/release/motion + legacy X10
(`ESC[M`+3) + focus reports; 15-case unit check confirms every keyboard sequence (CSI/SS3 arrows,
shift-tab, ^C, esc, typed text) passes untouched.

---

## BUG-011 — phone terminal: "open terminal failed: terminal does not support clear"

- **Status:** fixed, deployed 2026-07-07 ~20:1x.
- **Reporter:** operator — first REAL phone attach to a session terminal (screenshot: the error
  text + immediate «отключено — переподключиться?»). This was the last never-browser-validated
  UCC piece (P4 terminal byte-flow).
- **Severity:** P1 — the terminal view was unusable from the phone.

**Root cause.** `session_ws_bridge` spawns the PTY child `tmux attach -t rozum-<id>` with the
inherited environment — and control-serve runs under launchd, which has NO `TERM`. The tmux
client refuses a terminal without a usable terminfo entry ("terminal does not support clear"),
exits, and the WebSocket closes right after the error bytes reach xterm.js.

**Fix.** `cmd.env("TERM", "xterm-256color")` on the PTY child (xterm.js is xterm-compatible).

**Verified.** Headless-Chrome attach to a live tmux session via `terminal.html?id=…` over the
funnel: the xterm screen shows the actual claude REPL content (no error), input round-trips.

---

## BUG-010 — «запустить сессию» does nothing: formBody posted EMPTY fields (framework bug)

- **Status:** fixed (scalascript `3edbf883a` + rozum async launch), deployed 2026-07-07.
- **Reporter:** operator — "Я здесь нажимаю «запустить сессию» - ничего не происходит" (from the
  phone, sessions form fully filled).
- **Severity:** P1 — the launch POST fired instantly but carried
  `{"agent":"","model":"","workdir":"","prompt":""}` → 400, silently.

**Root cause (framework, std/ui SPA bridge).** `.ssc` forms reference field signals by NAME —
`formBody([("agent","seAgent"),…])` — but `_ssc_ui_signal(name, init)` DISCARDED the name, and the
submit-time store `_sv` is keyed by NUMERIC signal id, so `sv["seAgent"]` resolved to `''` for every
field. Every by-name formBody in every emitted SPA posted empties. Repro'd live in headless Chrome
with request capture (body was key-correct but value-empty while the page visibly showed all
values).

**Fix 1 (scalascript `3edbf883a`).** `_signalsByName` registry (+ registration in
`_ssc_ui_signal`/`_ssc_ui_seedSignal`) and `_ssc_ui_resolveFormFields`: the render walk resolves
field refs to bridge ids AND collects the signals so their `_sv` entries stay fresh; unresolved
refs pass through verbatim. Regression test `SpaFormBodyNamedSignalsTest` (real JsRuntimeSignals,
headless node).

**Fix 2 (rozum, same operator symptom).** Even with the body fixed, a cold-start launch blocks for
minutes with zero feedback and the Tailscale funnel can time the request out. `session_launch_route`
is now ASYNC: validates fast, records the session as `starting…` immediately (the row in Live
sessions IS the feedback), loads the gateway + creates tmux in a background task, flips status to
`running` / `failed: <reason>`. Failed rows stay visible until closed (✕) — launch errors finally
reach the phone. `live_sessions()` prunes only completed records whose tmux died. New `status`
column in the sessions table.

---

## BUG-009 — every UCC page click bounced to #/ — agent/model pickers "did nothing"

- **Status:** fixed on `f8cf165`, redeployed 2026-07-07 ~05:3x; verified in a real browser.
- **Reporter:** operator — "Теперь не работает выбор агента в сессии" (after BUG-008 restored
  navigation). Almost certainly ALSO the UI half of the original BUG-006 complaint ("не работает
  выбор агента и модели в сессии") — it predates today's deploys.
- **Severity:** P1 — every in-page button (agent picker, model select, …) appeared dead.

**Symptom.** On `#/sessions`, tapping claude/codex/opencode (or a model `select`) visually did
nothing. Browser repro showed why it LOOKED dead: the click actually fired AND the signal set, but
the page instantly navigated `#/sessions` → `#/`, hiding the form again.

**Root cause.** The deploy script's injected close-on-click-outside handler:
`if(document.querySelector("[role=dialog]") && !e.target.closest("[role=dialog]")) location.hash="/"`.
The Model-details modal lives in an always-present `data-ssc-cond` branch (`display:none` when
closed) and `querySelector` finds hidden nodes — so the condition was true on EVERY click anywhere,
and any click outside the (invisible) dialog warped to home. Menu links survived only because their
own `href="#/…"` default action re-set the hash afterward.

**Fix.** Guard on real visibility: `_dlg.getClientRects().length` (0 inside `display:none` subtrees,
and unlike `offsetParent` it works under `position:fixed`).

**Verified** (puppeteer-core + system Chrome, busi-SSO cookie): agent picker claude→codex→opencode→
claude all update the label with hash staying `#/sessions`; model `select` fills the form model;
dialog still opens via `#/detail/…` and still closes to `#/` on a genuine outside click. Repro/verify
scripts: scratchpad `ucc-repro3.js` / `ucc-verify.js` pattern.

---

## BUG-008 — UCC menu navigation dead after the 03:27 redeploy (compiler/std skew)

- **Status:** fixed on `9a39a60` + site re-emitted and redeployed 2026-07-07 ~05:0x.
- **Reporter:** operator — "Опять не работает навигация в контрол центре."
- **Severity:** P1 — menu taps change the URL hash but the page never re-renders.

**Symptom.** After the BUG-007 deploy (03:27), tapping UCC menu items did nothing (hash changed,
view didn't). The 02:21 page was fine.

**Root cause.** The 03:27 SPA was emitted with a SKEWED toolchain: the repaired `/tmp/ssc-tk/bin/ssc`
launcher pinned the **Jun-29** `ssc.jar` from `~/work/my/scalascript/bin/lib` while `ssc.lib.path`/
`ssc.std.path` pointed at the **Jul-7** live std/plugins tree. That jar predates the std/ui React
bridge fix that registers `window.addEventListener('hashchange', () => _syncBridgeSignals())` — so
the emitted SPA never resynced bridge signals on hash change and navigation went dead. (Earlier
deploys used the since-removed `coord-main` worktree build, which had the fix; scalascript refreshed
`bin/lib` to a fresh consistent build at 03:59, after our emit.) Diff proof: old-jar emit vs
fresh-jar emit differ by exactly that one hashchange hook.

**Fix.** `deploy-ucc-web.sh` now makes the `/tmp/ssc-tk/bin/ssc` launcher a one-line DELEGATE to the
operator's canonical `~/work/my/scalascript/bin/ssc` (kept in lockstep with `bin/lib` by the
scalascript repo), so compiler and std can never skew again; the jar heredoc stays only as a
fallback, and a caller-provided `$SSC` is never rewritten. Site re-emitted with the canonical ssc.

**Verified.** Deployed page contains the `_syncBridgeSignals` hashchange hook; Node sandbox check:
2 hashchange listeners registered, `#/sessions`/`#/coders`/`#/agents`/`#/matrix`/`#/` all run
clean; deploy JS syntax + runtime-init checks green; `cache-control: no-store` + non-caching SW →
a plain reload on the phone picks it up.

---

## BUG-007 — UCC web launch fails on a cold host: "no shared gateway running"

- **Status:** fixed on `452e192` (+ deploy-script fix `0094bee`), merged to master and DEPLOYED to
  control-serve 2026-07-07; verified live end-to-end (cold start, switch, stop, inference).
- **Open note (minor, pre-existing):** the `prompt` field seeding uses `tmux send-keys … Enter`;
  in a HEADLESS tmux (no client attached) the CC REPL received the text but Enter did not always
  submit during shell testing — from the phone terminal (real xterm.js attach) typing is
  interactive so this shouldn't bite. Watch it when the operator validates the terminal from the
  browser; if seeded prompts sit unsubmitted, delay + retry the Enter or submit after first attach.
- **Reporter:** operator — "Что у нас за проблемы с запуском моделей и агентов через веб
  интерфейс? Почему это не работает?" (2026-07-07, after the BUG-006 deploy).
- **Severity:** P1 — the next bug in the BUG-006 chain: with the body parsing fixed, launching
  models/agents from the web still only works if someone already started a shared gateway from a
  terminal.

**Symptom.** Authenticated `POST /control/session/launch` (same for agent/coder launch and chat)
returns 409: `could not load <model>: rozum gateway switch: no shared gateway running` — while the
attached admission report says `fits: true`. On a cold host (after reboot, or after the gateway
idle-exited) every model-needing UCC action fails.

**Root cause.** `control.rs::ensure_gateway` knew only two cases: reuse the registered gateway if
it already serves the model, else `rozum gateway switch`. But `switch` swaps the model on a
*running* daemon and refuses when none is running. The CLI path (`rozum launch` →
`ensure_shared_gateway`, src/main.rs) handles cold start by spawning a detached daemon; the UCC
duplicate never got that branch.

**Fix.** `ensure_gateway` (now async): health-check the registry record (a stale record from a
crashed gateway falls through instead of returning a dead port); `switch` only when a healthy
gateway serves a different model; otherwise cold-start a detached `rozum gateway --model … --port
8089` daemon (own process group, output → gateway.log — the same shape as `rozum launch`'s
`spawn_detached_gateway`) and wait ≤300s for it to register and answer health. The daemon runs the
residency admission gate itself and idle-exits per `ROZUM_GATEWAY_IDLE_SECS` (default 900s), so a
web-started gateway frees RAM when unused.

**Verified.** `cargo test -p rozum-gateway ucc_` + `control::tests::`; live authenticated smoke on
:8411 (busi SSO cookie, SPA-shaped JSON body without Content-Type): cold host → launch returns
`{"ok":true,"id":…}`, gateway self-starts and registers on :8089, tmux session appears with the
claude REPL up, session stop works. The switch branch verified too: second launch with
`mlx-community:Qwen3.6-35B-A3B-4bit-DWQ` (the operator's target) swapped the model in place
(generation 1→2, same pid) and the claude session came up against it.

**Deploy fallout fixed along the way (same branch).** `deploy-ucc-web.sh` died mid-run on this
deploy and TRUNCATED the live `~/.rozum/ucc/site/index.html` to 0 bytes (the incident class
`ucc-duplicate-const-fix` warned about): the SSC launcher `/tmp/ssc-tk/bin/ssc` pointed at the
since-removed `scalascript/.worktrees/coord-main/bin/lib` jar dir (java: ClassNotFoundException),
and line 165's `emit-spa > "$SITE/index.html"` truncates the target before java starts. Fixed:
emit to `index.html.new` + non-empty check + `mv` (a failed emit leaves the live page untouched);
launcher default jar dir → the main checkout `scalascript/bin/lib`; launcher auto-regens when it
references a stale jar dir. Page regenerated and redeployed (414478 bytes, JS checks green).

---

## BUG-006 — UCC session launch buttons silently do nothing

- **Status:** fixed on `e451e6a` + hardened on `0a537df`; deployed to control-serve 2026-07-07.
- **Reporter:** operator — "В контрол центре не работает выбор агента и модели в сессии; хочу через веб интерфейс запустить сессию клауди и квен3.6, но ничего не происходит".
- **Severity:** P1 — blocks the phone/web control-center path for interactive coding sessions.

**Symptom.** In the UCC `#/sessions` page, selecting an agent/model/workdir and pressing
`launch session` leaves the UI unchanged and no tmux-backed session appears.

**Root cause.** The ScalaScript SPA's `formBody(...)` sends a JSON string body without
`Content-Type: application/json`. Axum `Json<T>` extractors on `/control/session/launch`
and sibling action routes reject those requests before handler execution, while the SPA does
not surface the non-2xx response. Stop/project actions also need to accept the same JSON body
shape instead of treating raw body text as the id/name.

**Fix.** UCC write routes now parse the browser body directly: agent/coder/session launch accept
JSON objects regardless of content type; stop routes accept JSON `{ "id": ... }` plus legacy
form/plain ids; project creation accepts JSON `{ "name": ... }` plus legacy form/plain names.
Malformed JSON-like bodies and missing ids/names return structured 400s instead of falling through.

**Verified.** `cargo test -p rozum-gateway ucc_`; `cargo test -p rozum-gateway control::tests::`;
`clients/control/deploy-ucc-web.sh`; unauthenticated `POST /control/session/launch` now reaches auth
middleware and returns 401 rather than the old extractor failure.

---

## BUG-005 — uncached model under `--offline` refused with bogus "~4398046511103 MB overcommit"

- **Status:** fixed on `feature/footprint-uncached-sentinel` (cargo check `--no-default-features` +
  2 unit tests green). Found while queuing the `matrix-add-coders` Qwen3-Coder smoke.
- **Reporter:** operator (smoke run "gateway not ready").
- **Severity:** P2 — blocks loading any not-yet-downloaded model on the offline bench path with a
  baffling, non-actionable error; no data loss / no reboot.

**Symptom.** `rozum-gateway gateway --model mlx-community:Qwen3-Coder-30B-A3B-Instruct-4bit --offline`
on an **empty** host (0 resident, ~20 GB free) refused:
`loading this model (~4398046511103 MB) would overcommit host RAM … Waited 240s` → the bench saw
"gateway not ready" and ran zero tasks. The baseline models worked only because they were already
cached.

**Root cause (two parts, both in `src/main.rs`).**
1. `estimate_model_footprint_bytes` returns the unknown-size **sentinel `u64::MAX/4`** when a spec
   isn't found in the local cache (`scan_all_installed`). `u64::MAX/4` bytes = **4_398_046_511_103
   MB** — the exact number in the message. It was meant to mean "only admit on an empty host," but
   it exceeds *any* physical RAM, so the gate (`share::admits`) refuses it **even on a totally empty
   host** → the model can NEVER load via the gate when its size is unknown.
2. Under `--offline` (which `agentic.sh` sets) the model also can't be downloaded to make its size
   knowable → permanent dead-end, reported as a fake petabyte overcommit.

**Fix.**
- `acquire_residency_or_exit`: for a single (non-cascade) model that is **not locally cached** AND
  `is_offline()`, exit early with a clear, actionable message naming the real problem and a
  copy-pasteable pre-download command (`hf_download_hint`) — instead of feeding the sentinel to the
  gate. Online is unchanged (the load path can still download).
- The gate refusal message now detects a sentinel-sized footprint (`>= u64::MAX/8`,
  `UNKNOWN_FOOTPRINT_FLOOR`) and prints "size is UNKNOWN (a model isn't downloaded locally)" with the
  pre-download hint, instead of quoting the absurd sentinel-in-MB — covering online-uncached and the
  cascade-with-an-uncached-tier paths too.
- Operational: the bench needs models pre-cached (it runs `--offline`); the `matrix-add-coders`
  smoke queue now pre-downloads via `uv + huggingface_hub` before starting the gateway.

**Related:** distinct from `footprint-before-download` (a261fb0, which moved the *estimate* after
download on the launch path) — this is the gateway path's unknown-size **sentinel** semantics +
the offline dead-end.

## BUG-004 — `mcp-proxy` dies mid-session → `mcp__rozum__*` tools vanish (no rozum-side trace)

- **Status:** fixed on `feature/mcp-proxy-resilience` (cargo check clean). Pending: install
  to `~/.cargo/bin/rozum` + a live away-session soak. The fix is the inverse correction to
  BUG-002: that one made the watchdog reap orphans, this one stops it reaping *live* sessions.
- **Reporter:** operator — mid-session the `mcp__rozum__*` tools disappeared (harness emitted
  "MCP servers have disconnected: rozum") while the meeting daemon stayed up and the CLI kept
  working. Correctly diagnosed by the operator: the **stdio `mcp-proxy`** bridging this Claude
  Code session to the daemon died; the daemon + CLI are an independent path. "MCP off" ≠ "rozum
  down".
- **Severity:** P2 — no data loss, but the agent silently loses room coordination for the rest
  of the session (Claude Code does **not** re-spawn a dead stdio MCP server; recovery today is a
  manual `/mcp` reconnect or a CC restart — neither doable by the agent itself).

**Symptom.** `rozum mcp-proxy` (the per-session stdio child Claude Code spawns from
`~/.claude.json`: `{type:stdio, command:rozum, args:[mcp-proxy]}`) exits during a live session.
The only trace was `eprintln!("proxy error")` → Claude Code's per-server MCP log, which records
nothing on a clean `exit(0)` and only an opaque transport-close otherwise → root cause
**uninspectable after the fact**.

**Root cause (two parts).**
1. **No observability.** The proxy had no log of its own, so an exit reason could not be
   recovered.
2. **The idle watchdog reaped live sessions.** BUG-002's fix reaps a proxy idle past
   `ROZUM_MCP_PROXY_IDLE_SECS` (default 2 h) with an unconditional `exit(0)`. Its safety
   assumption — "an actively room-using agent calls `meeting.wait_my_turn` ~every 25 s, so only
   an abandoned proxy goes silent that long" — is **false for an interactive human-driven CC
   session**: the human is coding/chatting, not running a room poll loop. Step away >2 h and the
   still-wanted proxy is reaped → tools vanish. (A `serve()`/transport error → `exit(1)` is the
   other, now-logged, candidate.)

**Fix** (`crates/rozum-meeting/src/meeting/daemon_proxy.rs`):
- **Observability:** `proxy_log()` writes lifecycle lines (start, initialize, daemon-connect,
  every exit + reason) to `$RUNTIME/mcp-proxy.log` (rotates at 256 KiB; `ROZUM_MCP_PROXY_LOG=0`
  to disable). `install_panic_logger()` records panics (payload + location) before the process
  dies. `run_daemon_proxy` now logs `serve-error` / `stdin-eof` / `join-error` distinctly.
- **Watchdog (the real fix):** past the soft window it reaps **only if the client transport is
  actually gone** (`Peer::is_transport_closed()` — flips when the rmcp loop tears down, i.e. CC
  disconnected). A live-but-idle session keeps its transport open → **not reaped**. A stuck
  orphan whose pipe never closed (the BUG-002 case) is bounded by a new generous hard cap
  `ROZUM_MCP_PROXY_MAX_IDLE_SECS` (default 24 h, `0` disables). `ROZUM_MCP_PROXY_IDLE_SECS=0`
  still disables the watchdog entirely. This keeps BUG-002's orphan-cleanup while closing the
  live-session false-reap.

**Strategic follow-up (HTTP transport).** The deeper fragility is structural: a *per-session
stdio child* is a single point of failure that Claude Code won't restart in-session. rmcp 1.7
ships a `streamable_http_server` transport and Claude Code supports `type:"http"` MCP servers —
the long-lived daemon could expose an HTTP MCP endpoint that CC connects to and **reconnects**
to on drop, with no per-session child to crash. Bigger lift (session identity / per-client cwd /
project detection move off the child); deserves its own spec. See the resilience analysis.

---

## BUG-003 — concurrent model-loaded gateways exhaust host RAM → watchdog kernel panic → reboot

- **Status:** fixed on master (`3bcee03` v1 single-flight) + **v2 RAM-ledger**
  (`feature/gateway-residency-ram-ledger`): the gate now admits a genuinely-fitting
  small 2nd model and refuses only a true overcommit. Pending matrix re-validation
  under load (`validate-gate-live`).
- **Reporter:** operator ("система ребутнулась") — the Mac rebooted 2026-06-22 13:41.
- **Severity:** P0 — reboots the host, so any matrix run is untrustworthy (same
  class as BUG-001, **different mechanism**).

**Symptom.** The Mac (Mac16,6 / M4, 36 GiB, macOS 26.5.1) rebooted. The panic is a
**watchdog timeout**, NOT the BUG-001 IOGPU double-free:
- `panic-full-2026-06-22-134243.panic`: `watchdog timeout: no checkins from
  watchdogd in 92 seconds`. Userspace `watchdogd` was starved → kernel watchdog
  panic → reboot.
- 3× `JetsamEvent-2026-06-22-13{30,35:08,35:18}.ips`, every kill reason =
  `vm-compressor-space-shortage`; dozens of system daemons mass-killed
  (assistantd, secd, trustd, MTLCompilerService…). `largestProcess = rozum`.
- At 13:35:18 there were **3 concurrent big rozum processes** — pid 23694 ≈24.8 GB,
  pid 25158 ≈18.7 GB, pid 25274 ≈18.0 GB → **≈61.6 GB resident on a 36 GiB box**,
  two distinct binary UUIDs (= >1 build/gateway running at once).

**Root cause.** More than one **model-loaded gateway** resident at once. The trigger
was two matrix runs overlapping — a `nondet-*` matrix (35B + GLM-4-32B) in the
`feature/matrix-nondeterminism-flip` worktree **and** an `agentic-35b-leanprompt`
run in the main worktree. Each `scripts/bench/agentic.sh` starts a **dedicated**
`rozum gateway --model … --port 8300+` (`agentic.sh:214`), which **bypasses** the
shared-gateway port singleton (`DEFAULT_GATEWAY_PORT` 8089 / `active.json`) — so the
registry never sees the second resident model and nothing stops the overcommit.
A single big model is contained (Metal OOM is process-fatal but local, BUG-001/[[35B
prefill OOM]]); the system-killer is **N concurrent instances**. The BUG-001
`TEARDOWN_GRACE` fix addresses GPU teardown, not this RAM-overcommit path.

**Fix.** A **host-wide model-residency admission gate** (`crates/rozum-core/src/share.rs`,
`acquire_residency`): every model-loaded gateway takes an advisory `flock` on
`gateway_dir()/residency.lock` **before** bringing weights resident and holds it for
its process lifetime (wired into `run_gateway` + `run_launch_dedicated` in
`src/main.rs`). It is independent of port/run/worktree, so it catches exactly the
dedicated-bench path the port singleton misses. A second loader waits up to
`ROZUM_GATEWAY_RESIDENCY_WAIT_SECS` (default 240s, past the matrix teardown window)
then **refuses with a clear message naming the holder** — a reboot becomes a
recoverable error. `flock` is released by the OS on fd close / process death (incl.
SIGKILL), so there is no stale-lock failure mode. Escape hatch
`ROZUM_ALLOW_CONCURRENT_RESIDENT=1` for the rare two-small-models case. Unit tests:
`residency_gate_admits_one_and_releases_on_drop`, `residency_escape_hatch_skips_the_gate`.
Memory: `[[project-reboot-watchdog-oom]]`.

**v2 (RAM ledger).** The v1 hard mutex refuses even a tiny 2nd model. v2 replaces it
with a host RAM budget: each gateway *reserves* its estimated footprint (`residents/<pid>`
flock-held file) before loading; admit iff sole OR `in_use + footprint ≤ total_ram ×
ROZUM_GATEWAY_RAM_BUDGET_FRAC` (0.65) — a genuinely-small 2nd model co-resides, a true
overcommit (two big models) still refuses. Reservation up-front under a brief admit lock
⇒ no free-RAM-read TOCTOU; per-pid flock liveness ⇒ same death-safety as v1, no reaper.
Footprint estimated caller-side from the catalog (core stays model-free); unknown model ⇒
huge estimate ⇒ admitted only when host empty (conservative). 4 unit tests (sole / refuse-
overcommit + admit-fitting / reap-dead / hatch) + real-binary smoke. Spec § v2.

---

## BUG-002 — `mcp-proxy` processes pile up (orphaned when an agent re-spawns its MCP)

- **Status:** fixed + ON MASTER (cherry-picked `5be81a5`+`c742e2b` onto master `8eaf21a`) + INSTALLED to `~/.cargo/bin/rozum` (release, mlx-native+gguf). Verified: the installed binary self-reaps an idle proxy at the 60s tick (rc=0). Origin `91a03c7` on `feature/meeting-web-pwa-ssc`;
  cargo check/--tests clean + a functional idle test: a silent proxy now exits at the first 60 s
  tick, rc=0). The earlier `c0117bd` spawned the watchdog AFTER `serve()`, which blocks on the MCP
  handshake — so `91a03c7`/`c742e2b` moves it before.
- **Reporter:** operator ("почему у меня запущено три процесса розума?" → found ~6 stale
  `mcp-proxy`/`mpc-proxy`, some 4 days old).
- **Severity:** P3 — resource leak / clutter, not a correctness break.

**Symptom.** Several `rozum mcp-proxy` (and a typo'd-config `rozum mpc-proxy`) stdio bridges
linger for days, tiny RSS, doing no work. One live claude session held BOTH an old `mpc-proxy`
(4 d) and a working `mcp-proxy` (1.7 d) — a superseded duplicate that never exited.

**Root cause.** An MCP stdio proxy exits only on **stdin-EOF**. Parent *death* is fine (the
agent's pipe end closes → EOF → exit), which is why the lingering ones all had *live* parents.
The gap is parent **abandonment**: the agent stays alive but re-spawns a fresh MCP server on a
config reload / binary change and **never closes the old proxy's stdin**, so `service.waiting()`
blocks forever. (`mpc-proxy` is just a stale typo in an agent's MCP config — clap accepts the
abbreviation and runs `mcp-proxy` anyway; it marks the old entry, it is not the cause.)

**Fix.** Idle watchdog in `src/meeting/daemon_proxy.rs`: `forward_raw` stamps `last_active` on
every agent request; a 60 s-tick task exits the proxy once silent past
`ROZUM_MCP_PROXY_IDLE_SECS` (default 2 h, `0` disables). An actively room-using agent calls
`meeting.wait_my_turn` ~every 25 s, so only a genuinely-abandoned proxy goes silent that long.
Stale ones were also killed by hand at report time.

---

## BUG-001 — agentic matrix reboots the Mac (kernel panic on gateway teardown)

- **Status:** done (harness-side fix validated across inter-model teardowns on master, 2026-06-18)
- **Reporter:** found in-house (heavy-bench days); root-caused in
  `[[project-matrix-kernel-panic]]`.
- **Severity:** P0 — reboots the host, so any matrix run is untrustworthy.

**Symptom.** Running `scripts/bench/agentic.sh` over multiple models would reboot the Mac
(Mac16,6 / M4, macOS 26.5.1). Confirmed a **kernel panic**, not a RAM OOM / jetsam, from
`/Library/Logs/DiagnosticReports/*.panic`:
- `IOGPUGroupMemory::remove_memory_object() memory object not found @IOGPUGroupMemory.cpp:323`
  — the GPU driver double-frees / use-after-frees a Metal buffer the kernel already dropped.
- A later instance: `watchdog timeout: no checkins from watchdogd in 93s` with P-cores
  offline — the GPU/system fully wedged, then watchdog-killed.

**Repro (DESTRUCTIVE — do not run to reproduce).** The full matrix reboots the machine.
This was localized by **code-path analysis**, not by re-running, precisely because the repro
is a host reboot.

**Root cause.** The harness tore each model's shared gateway down with
`kill -INT` → wait 60 s → **unconditional `kill -KILL`** (`agentic.sh`). rozum has graceful
shutdown and `join()`s the MLX worker on Drop, but if the final Metal eval is **wedged under
memory pressure** the worker thread is stuck inside a GPU dispatch, Drop's `join()` blocks,
the 60 s grace expires, and `kill -KILL` lands **on top of live GPU command buffers** →
IOGPU accounting corruption → kernel panic. `ROZUM_MLX_RETAIN` (retained command buffers for
the hybrid-decode fast path) widens the window.

**Fix (harness-side, validated no-panic on 27B on the original
`feature/matrix-teardown-panic-fix` branch; that branch went 70 commits stale, so the
still-needed change was ported fresh onto current master rather than merged):**
`scripts/bench/agentic.sh` now tears the gateway down **gracefully**: `kill -INT` →
wait `TEARDOWN_GRACE` (180 s, env-overridable) for a clean exit → SIGKILL **only as a loudly
flagged last resort** → then a `GPU_SETTLE` (8 s) pause to let the kernel finish async IOGPU
reclamation before the next gateway allocates on the same Metal device. Also adds
`ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0` to the gateway launch so the shared gateway isn't
self-exited (`clients_gone`) between the claude/codex phases — see
`[[project-agentic-bench-clients-gone]]` (a different matrix bug, co-fixed here as it makes
the run reach a clean teardown at all).

- **Fix commit:** `326bb9d` (`scripts/bench/agentic.sh` graceful teardown + idle-secs).
- **Validation 2026-06-18 — DONE.** Two matrix runs on master with the fix, neither produced a
  new `.panic` file (baseline stayed at 1):
  1. Single-model (`Qwen3.6-35B-A3B-4bit` × claude+codex+opencode × 5 tasks):
     **15/15 PASS, rc=0, 0 timeouts** (`results/agentic-20260618-081632`) — validated the
     end-of-model graceful teardown + the `clients_gone` idle-secs fix.
  2. **Inter-model (the original panic point):** claude × `Qwen3.6-27B → Qwen3-30B-A3B →
     Qwen3.6-35B-A3B`, `ROZUM_MLX_CACHE_GB=1` — **15/15 PASS, rc=0, 0 timeouts, NO new panic
     across 2 inter-model teardown transitions**, and **no SIGKILL fired** (every gateway exited
     gracefully within `TEARDOWN_GRACE`; footprint flushed cleanly between models 17.8→19.6→21.1 GB)
     (`results/agentic-20260618-083911`). This is the transition where the kernel panic originally
     occurred — now clean.
- **Remaining (separate hardening item, NOT this bug):** the deeper rozum-side bounded/non-wedging
  teardown (a real Metal-eval timeout so Drop's `join()` can't block forever). Defense-in-depth so
  even a buggy harness can't SIGKILL into a live eval; can't be validated without risking a reboot,
  so left as a tracked follow-up.

**Open follow-up (defense-in-depth, NOT done — deliberately).** The deepest fix is rozum
*itself* guaranteeing a bounded, non-wedging teardown (a real Metal-eval timeout that returns
control; ensure Drop's `join()` can't block forever). That touches the GPU teardown hot path
(`mlx_native_backend.rs` Drop/join, `gateway.rs` `shutdown_signal`) and **cannot be validated
without risking a reboot**, so it is left as a tracked follow-up rather than shipped blind.
The harness fix removes the proven panic trigger (SIGKILL into a live eval); this hardens the
engine so even a buggy/aggressive harness can't panic the GPU.
</content>
