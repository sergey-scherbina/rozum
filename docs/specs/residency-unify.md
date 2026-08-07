# Design: unify residency — one in-process multi-model manager (DRAFT, for joint impl)

Status: 2026-07-16 — **U1 + U2 + the reservation-update API LANDED** (initial U1/U2:
`ee30c92` + `fb58d7f`); U3 remains. The cross-process ledger now publishes readable metadata
separately from its lifetime lock, including Windows-tested grow/shrink updates. U3 is the remaining
routing layer; the host-budget composition described below has landed.
- ✅ **U1 (host-aware + footprint-accurate warm admission)** — `WarmConfig::new(n_ctx)`: weight =
  `runtime_footprint_bytes`, budget = `host_ram_budget_bytes − committed_by_others`. New pub
  `share::committed_by_others_bytes`. Closed a latent overcommit (multislot on by default + raw-weight sizing).
- ✅ **U2 (precise pressure eviction)** — the watchdog sheds idle WARM secondaries first under OS pressure
  (`sweep_idle_warm(0)`), keeping the primary serving; primary-unload last resort.
- ✅ **Reservation updates:** a process republishes primary + warm footprint on every grow/shrink.
- ⏳ **Remaining:** **U3 routing** and the decision to make the in-process path primary.

## Original problem: two parallel residency systems that did not compose

Before U1, co-residency happened two ways, with **two independent memory-accounting systems**:

1. **In-process warm cache (Switchboard, `rozum-gateway/gateway.rs:114+`).** One gateway
   process holds a **primary** model + a `warm` map of secondary residents, admitted/evicted by
   `plan_residency` (utility = frequency×recency) under a budget (`total*0.8`, `gateway.rs:~236`).
   Precise (in-process), instant swap (drop/build a backend, no process spawn), already exists
   (multislot Phase 2). **But its budget is scoped to its OWN residents** — it doesn't see other
   gateway processes.
2. **Cross-process lock ledger (v2, `rozum-core/share.rs`).** Each gateway PROCESS reserves its
   footprint in `residents/<pid>` and admits against the host budget. Coarse (process
   granularity), heavy (N HTTP servers), eviction = process death or `shed` self-unload.

**They did not compose:** a gateway's in-process warm admission (system 1) ignored other processes'
reservations (system 2), so two warm-capable gateways could each fill `total*0.8` → **overcommit**.
And the governor (`shed`) could only self-unload its whole model, not evict a specific warm secondary.

## The unified design

**One in-process Switchboard is the primary multi-model host; the lock ledger becomes the
cross-process backstop; the governor evicts precisely.** Three coherent pieces:

### U1 — Make warm admission host-aware (close the compose gap)
`Switchboard::ensure_warm` / `plan_residency` budget must be `host_budget − committed_by_other_pids`
(read the v2 ledger's `committed_by_others`, not just `total*0.8`). So the in-process warm set + any
external gateways sum to ≤ the host budget. The Switchboard process publishes its **total** resident
footprint (primary + warm) as its single `residents/<pid>` reservation, updated on each warm
load/evict via `share::update_my_reservation`. Net: one budget,
honored both within a process and across processes.

### U2 — Governor evicts a specific model (precise `shed`)
Today `shed` self-unloads the (whole) idle model under OS pressure. With the Switchboard it should,
on pressure, **evict the lowest-utility idle WARM secondary first** (via `plan_residency`'s ranking),
keeping the primary + busy models — graceful, minimal degradation. Self-unload of the primary stays
the last resort. This is the "cross-process, utility-ranked eviction" remaining item, now in-process
and precise.

### U3 — Request→model routing
A request names a model; the gateway: primary → serve; warm → serve; else → admit (load + evict to
fit, via U1). The router/cascade (`rozum-agent`) already classifies; surface a policy so "run what's
needed" loads/keeps the right set. (Smaller; can follow U1+U2.)

## Migration — phased, behaviour-preserving, matrix-gated

The Switchboard + warm cache already exist, so this is **composition + wiring**, not a rewrite:

- **P1 — host-aware warm budget (U1).** Smallest safe step: warm admission consults the ledger.
  Behaviour-preserving when multislot is off (single model). Gate: existing gateway tests + the
  two-small-model co-residency proof (`live-coresidency-proof`) shows two warm models sum ≤ budget.
- **P2 — governor precise eviction (U2).** `shed` calls `Switchboard::evict_lowest_utility_idle()`
  instead of self-unload-all. Gate: a memory-pressure sim evicts the right victim, primary survives.
- **P3 — routing policy (U3).** Optional; surfaces the existing internal routing.

Keep the v2 lock ledger throughout (it's the cross-process safety backstop for stray/external
gateways like the matrix's dedicated ones). The Switchboard is the *primary* path; the ledger is the
*floor*.

## Risks / open questions (for sunny-civet)
- **Matrix-critical path:** the Switchboard is in the serving hot loop — every change behaviour-
  preserving + matrix-gated. Keep single-model behaviour byte-identical when multislot is off.
- **Reservation-update API (closed):** `share.rs` updates readable metadata while preserving the
  process's lifetime lock; Windows CI covers the live locked-reservation path.
- **Double-accounting:** ensure a process's warm footprint is counted once (its own `residents/<pid>`),
  not also as "committed_by_others" to itself. `scan_residents(skip_pid=mypid)` already skips self ✓.
- **Eviction vs in-flight:** never evict a warm model with in-flight work (the lease counters at
  `gateway.rs:206` already track per-warm in-flight) — U2 evicts idle only.
- **Cap interaction:** each resident still gets its `set_memory_limit` soft hint = its share; with
  multiple in-process residents, set it per-model or skip (process-global limit). Decide in P1.

## Done-when
Two+ models co-reside in ONE gateway process within one host budget that ALSO accounts for any
external gateway; the governor sheds the least-useful idle model first under real pressure; single-
model behaviour unchanged; matrix green. Folds in the program's "co-residency = N processes",
"cross-process eviction", and (via U1) tightens the footprint accounting.

## The remaining decision, measured (2026-08-07)

The entry says U3 leaves "the architecture call — make the in-process Switchboard the *primary*
multi-model path over the N-process flock (affects the matrix harness → team decision, not solo)".
Measured before asking anything:

**1. For SERVING, the in-process path is already primary.** Multislot is ON by default
(`switchboard.rs:136`; `ROZUM_MULTISLOT=0` opts out), warm admission is host-aware (U1), the
governor sheds warm secondaries first (U2), and `ROZUM_WARM_MODELS` preloads a set (U3). Nothing is
pending there.

**2. The only remaining N-process user is the BENCH HARNESS**, plus `rozum launch --dedicated`.
`scripts/bench/agentic.sh` starts one gateway per model spec (`PORT_BASE + idx`) and measures **that
model's RAM as the gateway PROCESS footprint**, under `/usr/bin/time -l`.

So the "architecture call" is really: **should the matrix stop spawning a process per model?** That
is a measurement and blast-radius decision, not a serving one. The five things that have to be
answered before it can be:

| # | Question | What measuring already says |
|---|---|---|
| Q1 | Is "primary" about serving or about the harness? | Serving is done. Only the harness is left, so the item is much smaller than it reads. |
| Q2 | If the harness goes in-process, how is a model's footprint attributed? | Today it is the process's peak RSS — one number per model, trivially trustworthy. In one process the only source is MLX's own `get_active_memory`/peak deltas, which are per-PROCESS too and would have to be sampled around each load. Weaker evidence for the number the whole matrix is judged on. |
| Q3 | Blast radius. | One process means one model's Metal fault takes every co-resident down. This repo has BUG-001 (matrix kernel panic on teardown) and BUG-003 (3×gateway → jetsam → reboot); per-model processes are what bounds that today. |
| Q4 | Per-model memory caps in one process? | **Not possible.** `set_memory_limit`/`set_cache_limit` are process-global (`mlx_native_backend.rs::cap_mlx_memory`, "Process-global, idempotent"), so with N residents a model's share can only be enforced by ADMISSION, never by the runtime. |
| Q5 | Does the ledger still mean anything? | With the harness in-process there is one `residents/<pid>` entry covering N models. `committed_by_others` stays correct for EXTERNAL gateways, which is the case it exists for — but the ledger stops being a per-model view, and anything reading it that way would need to change. |

### Recommendation

**Close the item by scoping it, rather than by doing it.** Make the in-process Switchboard primary
for SERVING — which it already is — and keep the N-process shape for the MATRIX, on purpose:
per-model footprint attribution and blast-radius isolation are the two things the harness exists to
provide, and both are lost by merging the processes. The ledger stays the cross-process floor, which
is what the spec's own migration section already says.

What would change my mind: a matrix run whose wall-clock is dominated by process startup rather
than by generation (measure before believing it), or a per-model footprint source in-process that is
as trustworthy as peak RSS.

**This is a recommendation, not a decision** — the operator asked for the questions to be written
down and the choice made deliberately.
