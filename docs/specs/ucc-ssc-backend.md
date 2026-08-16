# The UCC server half in ScalaScript — what is actually on the critical path

Status: spec (2026-08-08) — **slice 1 of a weeks-long, cross-repo item**
Owner: `ucc-ssc-backend`
Backlog entry: "express the UCC server half in ScalaScript, like the meeting web … Effort: large
(weeks, cross-repo)."

## Why this spec exists before any code

The entry lists what the toolkit is "MISSING for this today": WebAuthn/passkeys, PTY↔WebSocket
bridging, process spawn/kill + registry primitives, a launchd story, and access to rozum's
residency API. That list is a plan-shaped object, and this subsystem has produced five entries in
one week whose "remaining" list was already done. So: measure first, and say which gap is on the
critical path rather than which gaps exist.

## The census (measured 2026-08-08, `crates/rozum-gateway/src/control.rs`)

**63 routes.** By what they do:

| Kind | Count | Note |
|---|---|---|
| read | 19 | status, config, listings, matrix views |
| action | 23 | launch/stop, messenger admin, matrix control, project add |
| terminal | 5 | session attach / output / send / ws bridge |
| auth | 16 | register, login, invites, roles, users, view tokens |

By what gates them: **4 are public** — `/view/{token}` and the three `/control/public/matrix*` —
carrying a view token and needing no session. Everything else sits behind one of seven
`route_layer` groups: `require_auth`, `require_admin`, and `require_perm_{read,chat,agents,matrix,
projects}`.

## What the entry gets wrong, measured

- **"Can a .ssc program serve HTTP" is not a gap.** `rozum-meeting-ssc` is a pure .ssc→Rust server
  and has been live on `:8405` for weeks. The pattern is proven; what is unproven is everything
  around auth.
- **WebAuthn is HALF present, not absent.** `std/ui/webauthn.ssc` exists — 41 lines of *browser*
  passkey actions. The server ceremony (registration/authentication challenge, credential storage,
  origin/rp checks) is what is missing. "Add WebAuthn to the toolkit" and "add the server half of a
  ceremony whose browser half exists" are different sizes of job.

## The critical path, which is none of the listed gaps

The first thing that blocks a .ssc UCC server is **not** passkeys, PTY or spawn. It is:

> **How does a .ssc server participate in a session it does not own?**

59 of the 63 routes are gated by middleware that reads a cookie, resolves a user, and checks a
permission. A .ssc server serving any of them must either (a) re-implement that chain, (b) be
reverse-proxied behind the Rust one, or (c) receive an already-resolved principal. Until that is
decided, every "port a read route" task is blocked on the same unanswered question, and porting
routes one at a time will discover it 19 times.

## Slice 1 — the four public routes, and nothing else

`/view/{token}` and `/control/public/matrix{,/live,/cell}` need **no session at all**: they carry a
view token, which is a string the server checks against a file. They are read-only, they are the
matrix views an operator actually opens on a phone, and they answer the question the whole item
exists to answer — *can a .ssc server stand beside the Rust one and serve real traffic?* — without
touching auth, spawn, PTY or launchd.

**CORRECTED 2026-08-08, before writing any code — the slice is TWO routes, not four.** Measured
each one's data source:

| Route | Reads | Movable? |
|---|---|---|
| `/control/public/matrix/cell` | CSV files under the bench results dir | **yes** — pure file read |
| `/view/{token}` | an HTML file under the UCC site dir | **yes** — pure file read |
| `/control/public/matrix/live` | `matrix_live()`, a Mutex that IS file-backed | no — a second process reads a copy that is stale between writes |
| `/control/public/matrix` | `matrix_queue()`, a process-global `OnceLock<Mutex<Vec<MatrixJob>>>` **never written to disk** | **no** — the state exists only inside the gateway process |

So "the four public routes need no session" was true and not sufficient: two of them need the
gateway's *memory*, which no separate server can have. The queue in particular is not persisted at
all — see `matrix-queue-persist` in BACKLOG, which would unblock them and is worth doing on its own
merits (a queue that survives a gateway restart), but is not this slice.

**Done when:** `/control/public/matrix/cell` and `/view/{token}` are served from a .ssc program
behind the same origin, their Rust handlers are deleted rather than left as a second
implementation, and the view-token gate still refuses a revoked token. `cell` is the one that
proves anything — it parses a CSV and answers JSON, which is a real read route; `/view/{token}`
only proves a file can be served.

## Slices after that, in dependency order

2. **Decide the session question** (the critical path above). Write it as its own spec with the
   three options measured, not assumed — a reverse proxy is a deployment decision, not a code one,
   and it may make (a) unnecessary forever.
3. **Read routes** (19) once slice 2 lands.
4. **Action routes** (23) — these need the spawn/registry primitives the entry names. That gap is
   real; it is just not first.
5. **Terminal (5) and auth (16) stay Rust**, exactly as the entry says. A PTY bridge and a passkey
   ceremony are the two places where "one language end to end" buys least and risks most.

## What this spec refuses to do

- **Port routes one at a time to discover the session problem 19 times.** It is one question; answer
  it once.
- **Build the toolkit's server-side WebAuthn on spec.** Nothing needs it until slice 5, which the
  entry itself says should stay Rust longest — so it may never be needed at all.
- **Treat "one language end to end" as the goal.** The goal is that the async-job pattern exists
  ONCE instead of twice (`std/ui/patterns.ssc jobPanel` on the client, `spawn_launch_task` on the
  server). If a slice does not reduce that duplication, it is motion.

## Slice 1 implementation notes (measured 2026-08-08, before writing the port)

**The primitives exist.** `clients/meeting/meeting.ssc` — the live `:8405` server — already imports
`[route, serve, requestCookie](std/http.ssc)`, `[readFile, listDir, isDir](std/fs.ssc)` and
`[exec, ProcessOptions](std/process.ssc)`, and `std/json.ssc` provides `jsonParse` / `jsonRead` /
`jsonStringify`. Nothing new is needed from the toolkit for these two routes.

**`/control/public/matrix/cell` is not "read a CSV".** Faithfully it must:

1. Normalise `model` (`/`, `:`, space → `_`) and REJECT any of `stamp|agent|task|model` that is not a
   safe single path segment. This is a path-traversal guard on segments joined onto the results
   dir — a crafted `../..` must not walk out. **Port this first and test it first**; it is the only
   security-relevant line in the slice.
2. Parse `<results>/<stamp>/per-run.csv` and find the row matching agent+model+task.
3. Tail `cells/<agent>/<safe_model>/<task>/agent.log` (bounded, default 120, cap 3000) and read
   `verify.out` when present.
4. Check the `t=` view token against the tokens file before any of the above.

**`/view/{token}`** serves an HTML file from the UCC site dir after the same token check.

**The deployment question is small here** and should not be confused with slice 2's session
question: these routes must answer on the SAME ORIGIN as the console, so either the Rust gateway
proxies the two paths to the .ssc server's port, or the .ssc server fronts both. A proxy of two
paths is a deployment decision, reversible, and does not commit the session design.

**Order that de-risks it:** write the .ssc program serving both routes on its OWN port and prove it
against real results data (the traversal guard first), THEN decide the origin, THEN delete the Rust
handlers. Deleting them earlier leaves the console broken between steps for no gain.

## Slice 1, first implementation pass — what works, and the finding that stops it (2026-08-08)

`clients/control/public-matrix.ssc` exists and RUNS: a .ssc server on `:8412` answering real
requests against real files. Proven live, in this order, deliberately:

| | |
|---|---|
| the path-traversal guard | ported first and tested ALONE before a route existed — 9 cases, including `..`, `../etc/passwd`, bare and back slashes |
| an invalid view token | refused, `{"error":"invalid or revoked token"}` |
| a traversal attempt with a VALID token | refused, `{"error":"invalid stamp/agent/model/task segment"}` |
| a real matrix cell | **found** in the operator's own `per-run.csv` |

**What stops it, and it is a toolkit finding rather than a porting problem.**
`List.join` behaves in two incompatible ways depending on the frontend:

- **`--v1`**: `No method 'join' on ListV(...)` — a hard, loud error.
- **default front**: silently yields a `Stub`, which SERIALISES INTO THE RESPONSE as
  `{"cell":{Stub}}`. No error, no log line: **wrong data served to a user.**

`map`, `filter` and `drop` all showed as `Stub` for the same reason — each was followed by `join`.
`length`, `indexOf`, `zip`, `zipWithIndex` and `find` are real on both.

**Why this matters beyond one route.** The whole `ucc-ssc-backend` item is "port real logic to
.ssc". If a missing collection operation can reach production as plausible-looking JSON instead of
a crash, then every future slice carries that risk, and the failure mode is the worst kind — silent
and user-visible. **Before slice 2, this needs either a frontend that fails loudly by default, or a
list of which operations are safe.** `SSC_FRONT_STRICT=1` exists and turns the fallback into an
error; whether it also makes stubs fatal is the first thing to check.

**State of the branch (as of the section this paragraph sits in, 2026-08-08 — superseded, see the
closing section):** `feature/ucc-ssc-public` holds the working server with the guard, the token gate
and the CSV lookup. The cell body is not yet assembled, because assembling it needs string joining.
It is NOT merged — a route that can answer `{Stub}` should not be on master.

## Slice 1 finished on the `run` path, blocked on `build-rust` — three divergences, measured (2026-08-08)

**Working, proven against the operator's own data:** a real matrix cell returns its full row
(`pass=1 seconds=3.1 verdict=pass`), an invalid token is refused, a traversal attempt with a VALID
token is refused, a miss returns `"cell":null`. The upstream `Stub` fix landed and turned the silent
corruption into a loud error naming `Cons.join`, which is what made the rest measurable.

**The seam this slice actually found: `ssc run` and `ssc build-rust` are not the same language.**
Three concrete divergences, one in each direction and one fatal:

| construct | `run` | `build-rust` |
|---|---|---|
| `xs.join(",")` | dies — no such method | **works** (lowers to Rust's `[String]::join`) |
| `!f(x)` (unary not on a call) | works | **refuses to lower** — "unsupported expression: Term.ApplyUnary" |
| the whole file, after both are avoided | serves correctly | lowers, then the GENERATED RUST does not compile — 22 errors: 9 × E0308 mismatched types, 5 × E0282 annotations needed, and `no field zipWithIndex on Vec<String>` |

The last row is the blocker: the emitted Rust is not type-correct for constructs the interpreter
runs fine. That is not something this repo can work around by writing different .ssc — the same
source has to lower correctly, or the shipping path is closed.

**Consequence for the item.** `ucc-ssc-backend` is written against `run` and deployed via
`build-rust`, so every future slice sits on this seam. Until the two agree — or until the gap is
documented well enough to code against — porting more routes buys nothing but more instances of it.
Reported upstream as `join-works-under-build-rust-not-run`, with the other two to follow.

**State (2026-08-08 — superseded below):** `feature/ucc-ssc-public` holds the server, unmerged. On
the `run` path it is correct and complete; on the shipping path it does not build.

## Slice 1 finished on BOTH paths, against two upstream fixes that are still unmerged (2026-08-13/14)

The three divergences above were fixed upstream or worked around, and the slice was then finished
and measured. What it serves today:

- **`GET /control/public/matrix/cell?t=&stamp=&agent=&model=&task=&tail=`** — view-token gated.
  Answers the matched `per-run.csv` row, `agent_log` (tail, default 200, capped at 3000),
  `verify_out`, `triage_out`, `task_info` read from `scripts/bench/tasks.json`, and `has_logs`.
  Absent is `null` and not `""`, because the console distinguishes "kept no logs" from "the log is
  empty". 403 for a bad token, 400 for a path segment that could escape the results directory, 200
  otherwise, `application/json` on all three.
- **`GET /view/{token}`** — serves `view.html` with the token injected before the first `</head>`,
  404 when the file is absent, and 410 with the styled expired page for a token that is not valid.
  The token is checked BEFORE the file is read: an invalid token must not reveal whether the page
  exists.

**Verified against the Rust handlers it replaces:** all **1884 cells present on disk**, every field,
in both switch positions; `/view` byte-for-byte on a live and on a revoked token, headers included;
six edge cases. The switch itself is already on master — `ROZUM_UCC_SSC_ORIGIN` unset is today's
behaviour exactly, and an unreachable `.ssc` origin answers 502 rather than falling back to Rust, so
the switch cannot pass a test while quietly serving the old code.

**One deviation, kept:** the `.ssc` runtime adds `Cache-Control: no-store` to every answer; the Rust
routes send none on these two. Harmless on an authorized read, and it is the only header that
differs.

## Slice 1 is finished and unblocked (2026-08-15)

It builds and runs on scalascript's **unmodified main**, and nothing upstream is outstanding.

**Five defects had to be fixed upstream to get here, and none of them was known when the slice was
written.** Each was found by porting a route, minimised, filed with both lanes measured on a
toolchain whose `.build-digest` matched its tree, and re-measured here before the workaround came
out — never closed off a status field:

| upstream defect | what it cost here |
|---|---|
| `route-handler-lowered-to-string` | the file did not compile: `route` is declared `Request => Response`, lowered to `-> String` |
| `query-not-percent-decoded-on-build-rust` | a model spec arrived as `mlx-community%3A…` and matched no CSV row — wrong data, no error |
| `build-rust-indexof-on-string` | `indexOf` lowered to `.iter().position(…)` on a String |
| `build-rust-concat-list-element-with-toplevel-val` | a three-operand concatenation lowered to `String + String` |
| `string-length-counts-bytes-not-characters` | `length` answered BYTES, `substring` characters — a PANIC mid-request that poisoned the server |
| `serve-binds-all-interfaces` | no way to keep the server off the LAN; the console it replaces binds loopback |

One of their fixes broke this file rather than helping it, which is worth recording: closing
`toint-on-a-non-integer-diverges` by making BOTH lanes abort turned `isInt`'s `s.toInt.toString == s`
into a landmine, because `isNumeric` runs on every CSV field and `seconds` is `3.1`. It BUILT and
would have died on the first request. `isInt` is a digit walk now, and that is permanent — `toInt`
is a partial function and this predicate must be total.

**Verified against the running Rust console, after every workaround came out:** 1892/1892 cells
identical in status and every field; `/view` byte-identical on a live, a bogus and an empty token;
0 panics. The empty-token case was a divergence nobody had measured before — `/view/` bare answered
410 here and 404 in Rust; both refuse, so it was never a hole, but an unmeasured difference is its
own kind of debt.

**Deployment is the operator's, and it is now only a decision, not a blocker.** `ROZUM_UCC_SSC_ORIGIN`
unset is today's behaviour exactly; an unreachable origin answers 502 rather than falling back.
`SSC_HTTP_BIND=127.0.0.1` keeps the .ssc server off the network — demonstrated: unset binds `*:8412`,
set binds `127.0.0.1:8412` and the host's own LAN address refuses, the same answer `:8411` gives.
The one remaining deviation is a header: the .ssc runtime adds `Cache-Control: no-store`.

## Slice 1 is IN PRODUCTION (2026-08-15, operator: "Да")

`/control/public/matrix/cell` and `/view/{token}` on the live console are served by ScalaScript.

    com.rozum.ucc-ssc      ~/.local/bin/rozum-ucc-ssc, SSC_HTTP_BIND=127.0.0.1, cwd = the repo
    com.rozum.ucc-control  + ROZUM_UCC_SSC_ORIGIN=http://127.0.0.1:8412

The working directory is load-bearing on both: the cell route resolves `scripts/bench/results`
relative to the process, so a different cwd answers "no such cell" for every cell.

**What was verified live, in this order, and one step of it failed the first time:**

1. 371 cells captured from the Rust console BEFORE anything changed, so the comparison had a
   reference that could not move under it.
2. The .ssc service bound `127.0.0.1:8412` — checked with `lsof`, not assumed from the variable.
3. `ROZUM_UCC_SSC_ORIGIN` set, console restarted with `kickstart -k`, all 371 answers identical.
   **And the switch was not on.** `kickstart -k` restarts the process from the job definition
   launchd already loaded; a plist edit needs `bootout` + `bootstrap`. Every answer looked right
   because the Rust handler was still serving them — the identical result was evidence of nothing.
4. Caught by stopping the .ssc and watching `:8411` answer **200** instead of 502. That probe is the
   only reason this was not deployed believing itself switched. It is exactly what the 502 was for:
   a switch that fell back silently would have passed every check in this list.
5. Reloaded properly. The log now says
   `control server: /control/public/matrix/cell and /view/{token} → http://127.0.0.1:8412 (.ssc)`,
   the process carries the variable, and with the .ssc stopped `:8411` answers **502** while every
   other route still answers 200.
6. The deployed binary is byte-identical (md5) to the one measured at 1892/1892 on cells and
   byte-identical on `/view`, so that result carries over rather than being re-asserted. Live spot
   checks: `/view/<token>` 200 with the token injected, `/view/<bad>` 410, 120 sampled cell bodies
   parse with exactly the six expected fields.

**Rebuild path:** `clients/control/build-public-matrix.sh`, wired into `install-bins.sh` as
`rozum-ucc-ssc`. When scalascript's shared toolchain is stale — it was, on first use — the script
says which of "restage / our source / their compiler" it is and prints the three commands, instead
of leaving cargo output to be read as a defect here.


## Slice 3 — the gated read routes (2026-08-16)

**The census said 19 read routes; the `require_perm_read` group is 8, and only 3 of those are
portable.** Classified by DATA SOURCE, which is the only thing that decides it — the same question
slice 1 answered for the public four:

| route | source | portable |
|---|---|---|
| `/chat/messages` | room `.jsonl` files on disk | **yes** |
| `/chat/incidents` | room `threads.json` | **yes** |
| `/control/matrix/cell` | `scripts/bench/results` | **yes** — the `.ssc` already serves its public twin |
| `/control/status` | the gateway's own process state | no |
| `/control/matrix/status` | `matrix_queue()`, process-global | no |
| `/control/matrix/log` | `matrix_queue()` + files | no |
| `/control/matrix/live` | `matrix_live()`, process-global | no |
| `/control/model/info` | filesystem, but through `scan_all_installed` + `same_model` | **not now** — porting it copies the model-catalog matching rule into a second language, which is the duplication this whole item exists to reduce |

The authenticated cell route is served by the SAME `.ssc` body as the public one, with `authed`
picking the two differences the two Rust handlers actually have: the `tail` default (120 vs 200)
and the KEY ORDER (`task_info` second vs fifth). That is the shape this item wants — one
implementation behind two doors, not a third copy.

### Three compiler divergences, all of them SILENT (found by this slice)

Slice 1 filed five; these are the next three, and what makes them expensive is that none is a
compile error. Each was found by building a probe that ran the same code on both lanes.

1. **A type pattern on a local `val` matches ANYTHING** — sharpened 2026-08-16 while reducing it
   for the upstream report, and the sharpening changes the rule. It is not about arm order: the
   same match is correct when the scrutinee is a PARAMETER and wrong when it is a local `val`, on
   which `case m: Map[String, Any]` takes a JSON array. Every bare-array `rooms.json` therefore
   took the object branch, found no `rooms` key, and answered "no such room" for every room.
   Reordering the arms happened to fix it because it removed the local-val match; passing the
   parsed value into a function is the rule that actually holds.
2. ~~**`take(n)` after `map` over a `List[Any]` returns EMPTY.**~~ **WITHDRAWN 2026-08-16 — not a
   defect, and the withdrawal is the more useful record.** Reducing it to a minimal case for the
   upstream report showed `take` behaving correctly on both lanes. The list I was taking from began
   with an empty string — the first room in `rooms.json` is not the one being looked up — so
   `take(1)` returned exactly what it should have, and the empty answer came from (1). Filing it
   would have cost someone else the hour it cost me.
3. **`jsonParse` PANICS on unparseable input, and the http runtime then poisons its mutex.** One
   blank line at the end of one `.jsonl` file killed every subsequent request on that server with
   `PoisonError` — the process stays up and answers nothing. Guarded by only handing `jsonParse`
   text that starts with `{` or `[`.

(3) is the one worth carrying upstream first: a server that dies permanently on one malformed byte
is a different class of problem from a wrong answer.


## Slice 4 — nine routes moved (2026-08-16)

Classified before any porting, the same way slices 1 and 3 were, because the answer again decides
the size of the slice rather than the order of the work:

| routes | what they touch | portable |
|---|---|---|
| `matrix/{run,pause,resume,stop}` | `matrix_queue()` inside the gateway process | no |
| `gateway/{load,stop}`, `task`, `chat/stream` | the gateway's own model + state | no |
| `agent/{launch,stop}`, `coder/*`, `session/*` | DETACHED child processes + registry files | not yet — see below |
| `messenger/*` (7) | shell out to the `messenger` CLI (`messenger_json(&args)`) | only if `exec` works on the rust lane |
| `project/add` | `create_dir_all` + a file | **yes** |
| `chat/post` | proxies to the meeting daemon on `:8405` | **yes**, it is an HTTP call |

**The blocking gap is real and it is not "spawn/kill" in general — it is DETACHED spawn.**
`std/process.ssc` offers exactly one primitive, `exec(cmd, args, opts) -> ProcessResult`, which
WAITS for the child and returns its output. Every launch route here starts something that must
outlive the request. Whether even `exec` is lowered on the rust lane is unmeasured — its own
header names JVM, Node and Browser backends and not Rust.

So slice 4 is two routes' worth of porting behind one toolkit question, and the honest order is:
measure `exec` on the rust lane first; if it works, the messenger seven come with it; the launch
routes need a `spawn` that does not exist yet in any backend.


### Slice 4, done: what moved and what did not

**Moved (9):** `/control/project/add` and eight of the nine `messenger/*`. The proxy had to learn
the METHOD and the BODY first — it issued a GET whatever it was given, which was invisible while
only GET routes used it and would have turned every ported action route into a silent read.

**`messenger/bot/add` must not move**, and this is a rule rather than a scheduling note: it hands
the bot token to the child on STDIN so it never reaches an argv, and `std/process.ssc` has no
stdin. Porting it would put a live token on a command line every local process can read.

**Still Rust, unchanged from the table above:** the four matrix-queue routes, `gateway/{load,stop}`,
`task`, `chat/stream` — all process-global — and every launch route, blocked on a DETACHED spawn
that no backend offers.

**`chat/post` was left for last on purpose.** It is portable (an HTTP POST to the meeting daemon,
and the toolkit has `httpPost`), but its acceptance would post real messages into the operator's
rooms; doing it properly needs a scratch room, which is a fixture this slice did not have.

### Two things the acceptance had to invent, both worth keeping

**A mutating route cannot be accepted by comparing a server against its own previous deploy.** Both
implementations ran in isolated `HOME`s — the Rust half as a second console on :8421 with its own
admin session fixture — and the comparison was over what each ANSWERED and what each WROTE. The
operator's registries, bots and launchd jobs were never in the experiment.

**Formatting is part of the bytes.** The CLI prints `--json` pretty and the Rust wrapper
re-serializes it compact. Passing the child's text through was wrong by whitespace; re-serializing
in `.ssc` was wrong by key order (`jsonStringify` of a parsed value sorts). Compacting — a quote-
and escape-aware strip of whitespace outside strings — is the only spelling that matches on both.

### One gap this slice did not create but did surface

`com.rozum.ucc-ssc`'s plist exists on the machine and NOWHERE in the repo — unlike the seven jobs
under `clients/control/launchd/`. It carries `SSC_HTTP_BIND`, the working directory the cell route
depends on, and now `ROZUM_BIN`. A service whose definition lives only on one disk is one
reinstall from being gone; filed in BACKLOG as `ucc-ssc-plist-not-in-repo`.


## The two primitives, filed upstream (2026-08-16)

Both gaps that stop the port are now reports in scalascript's INBOX, each with a runnable example
under `examples/reported/` built and measured with `build-rust`:

- `process-needs-a-detached-spawn` — `exec` waits for the child (a `/bin/sleep 2` costs the handler
  2.14 s, measured), so no launch route can move. `spawn(...) -> Child(pid)` would be enough;
  killing already works through `exec("kill", …)`.
- `process-needs-a-stdin-pipe` — `ProcessOptions` has no stdin, so a secret can only travel in argv
  where `ps` shows it. `messenger/bot/add` stays Rust for that reason and no other.

Filed as FEATURES and registered rather than routed (their P-3.3: `lane`/`area` are the triager's
judgement). If either lands, the corresponding routes become a port rather than a design question —
which is the whole reason to write them down as primitives instead of as "the rest of slice 4".
