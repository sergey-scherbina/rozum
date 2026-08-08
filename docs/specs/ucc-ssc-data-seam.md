# The second axis: where a .ssc UCC server gets its DATA

Status: spec (2026-08-08), measurement slice of `ucc-ssc-backend`.
Companion to [`ucc-ssc-backend.md`](ucc-ssc-backend.md), written by the sibling agent on
`feature/ucc-ssc-public`. **That spec owns the session axis; this one does not restate it.**

## Why a second spec instead of an edit

The companion measured *what gates a route* — 59 of 63 behind `require_auth` plus a permission
layer — and named the session question as the critical path. Correct, and unchanged by anything
here.

It leaves a second question unanswered, and the answer decides which routes can move at all:
**where does a route's data live?** The companion hit one instance of this (the public matrix
routes read a `OnceLock<Mutex<…>>` that is never written to disk) and treated it as a property of
those two routes. It is not. It is an axis, and this is its measurement.

## Measured 2026-08-08 — 26 GET routes by data source

Method: read each handler and follow its calls (`crates/rozum-gateway/src/`). Hand-verified, not
grep-classified — **the first pass was wrong**: it counted `share::now_unix` as in-process state and
so reported `/control/auth/status` and `/control/invite/info` as unportable. `now_unix` is a clock.
A count that flatters the thesis is the one to re-check.

| Data source | Routes | Can a separate .ssc process serve it? |
|---|---:|---|
| **A. Plain files it can own** — SPA assets, bench CSV, chat transcripts, coder logs, matrix cell | 9 | **Yes, today.** No rozum-private format involved. |
| **B. rozum's own on-disk formats** — residency ledger, footprint caches, HF catalog scan, RBAC store | 8 | **Only by duplicating a private format** — see below. |
| **C. Process memory or an OS resource** — matrix queue/live (`OnceLock<Mutex<…>>`), tmux sessions | 7 | **No.** Not a toolkit gap; the state does not exist outside the process. |
| **D. Already behind a JSON-printing CLI** — the messenger console | 2 (+7 mutating) | **Yes, today** — and this is the interesting one. |

## The finding: almost nothing here is truly "in-process"

rozum was built for cross-process coordination, so its state is on disk by construction:
`share::read_active` reads a file, `share::list_residents` scans a flock'd ledger directory,
`footprint::cached_peak` reads a JSON cache, `scan_all_installed` walks the HF cache, the RBAC store
is three JSON files. A second process *can* read all of it.

**That is exactly the trap.** Reading it means re-implementing a private on-disk format and its
locking discipline in a second language. The ledger is not a JSON file — it is a directory of
reservation entries with lifetime-lock sidecars, reaped under an admit lock, where a reader that
skips the discipline sees dead reservations as live. The day the format changes, the console shows
a different truth than the gate enforces, and nothing fails loudly. **One format, one reader** is
the rule this spec asks for; a .ssc server that re-reads rozum's private files breaks it.

Category C needs no rule: `matrix_queue()` is a process-global `OnceLock<Mutex<Vec<MatrixJob>>>`
never written to disk (the companion's finding, confirmed at `matrix.rs:172`), and a tmux-backed
session is an OS resource. No toolkit primitive makes those portable — persisting the queue would
(`matrix-queue-persist` in BACKLOG), and that is a rozum change, not a ScalaScript one.

## The seam that already exists in rozum, and nobody planned it

`/control/messenger/status` does not read a registry file. It runs `messenger status --json` and
passes the JSON through — nine routes, the whole messenger console, already talk to their data
through a CLI that prints JSON. The toolkit has both halves of that shape: `exec` from
`std/process.ssc` (the live `:8405` meeting server already uses it) and `httpGet` from
`std/http.ssc`.

**So the portable boundary is not a new toolkit primitive. It is a rozum obligation:** a read route
moves to .ssc when its data is available as JSON from something Rust owns — a CLI subcommand or an
internal endpoint — and it does not move before that. `rozum gateway status` and `rozum models list`
print for humans and have no `--json`; that missing flag, not WebAuthn, is what stands between
`/control/status` and a .ssc implementation.

## Evidence: the shipping path carries this shape (probes run 2026-08-08)

Both probes emitted through `ssc-tools emit-rust` and were built and run as Rust binaries — the
shipping path, not the interpreter. The toolchain binary was built from an older commit, but
`v1/runtime/backend/rust` and `v1/runtime/std` are byte-identical between that commit and HEAD, so
the measurement is of the current backend.

1. **A read route serves.** `route`/`serve` + `mkString` → a live JSON answer on `:8499`, and a
   real `404` for an unknown path. (`mkString` lowers to Rust's `[String]::join`; the `Stub` the
   companion hit is on the *name* `join`, not on string joining.)
2. **The BFF shape works.** One .ssc route that reads a file from disk *and* calls the operator's
   live gateway over HTTP returned `{"disk_len":213,"upstream":202}` — `httpGet` lowers to a `ureq`
   client. This is the shape category B and D need, and it compiles and runs today.

That bounds — it does not contradict — the companion's blocker. Their file fails to lower with 22
rustc errors on richer constructs (`zipWithIndex` on `Vec<String>`, inference holes). The shipping
path is not closed; its *type coverage* is narrower than the interpreter's, and simply-typed
handlers cross it. Which is the second argument for the boundary above: a handler that fetches
ready-made JSON is simply-typed almost by definition, while a handler that recomputes rozum's state
is exactly the rich-typed code that does not lower.

## What this changes about the slice order

The companion's order stands. This adds a precondition to its step 3:

> **Read routes do not move as a block of 19.** They move in the order their data becomes available
> as JSON from Rust. Nine can move now (A). Two more the moment the messenger pattern is pointed at
> them (D). Eight wait on a `--json` surface for the ledger/catalog/footprint readers (B), which is
> a small rozum change with its own value — `rozum gateway status --json` is useful to every script
> on this machine. Seven cannot move until the state itself moves out of process memory (C), and
> two of those are the terminal, which the entry already says stays Rust.

## What this spec does not decide

- **The session question** — the companion's, unchanged and still first.
- **Origin/proxy layout** — a deployment decision, likewise theirs.
- **The `run` vs `build-rust` divergence** — reported upstream by the companion; it is a
  ScalaScript defect, and no rozum-side decision waits on it.
- **Whether any of this is worth doing.** This spec says which routes *can* move and at what cost.
  The reason to move them at all is the duplication named in the companion's last section, and a
  slice that does not reduce it is motion.
