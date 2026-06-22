# Design: unify residency — one in-process multi-model manager (DRAFT, for joint impl)

Status: 2026-06-22 — **U1 + U2 LANDED** (nimble-raven, `ee30c92` + `fb58d7f`); U3 + the ledger
reservation-update API remain (joint with `sunny-civet`, who owns the v2 flock ledger + `shed`
+ the Switchboard wiring). The biggest remaining lever of `docs/specs/safe-multi-model-program.md`
(step 3) and `safe-multi-model-residency.md` (the "Co-residency = N processes" obstacle).
- ✅ **U1 (host-aware + footprint-accurate warm admission)** — `WarmConfig::new(n_ctx)`: weight =
  `runtime_footprint_bytes`, budget = `host_ram_budget_bytes − committed_by_others`. New pub
  `share::committed_by_others_bytes`. Closed a latent overcommit (multislot on by default + raw-weight sizing).
- ✅ **U2 (precise pressure eviction)** — the watchdog sheds idle WARM secondaries first under OS pressure
  (`sweep_idle_warm(0)`), keeping the primary serving; primary-unload last resort.
- ⏳ **Remaining:** the ledger **reservation-update API** (so a process publishes primary+warm, not just
  primary — see U1 note below), **U3 routing**, and the decision to make the in-process path primary.

## The problem: two parallel residency systems that don't compose

Co-residency today can happen two ways, with **two independent memory-accounting systems**:

1. **In-process warm cache (Switchboard, `rozum-gateway/gateway.rs:114+`).** One gateway
   process holds a **primary** model + a `warm` map of secondary residents, admitted/evicted by
   `plan_residency` (utility = frequency×recency) under a budget (`total*0.8`, `gateway.rs:~236`).
   Precise (in-process), instant swap (drop/build a backend, no process spawn), already exists
   (multislot Phase 2). **But its budget is scoped to its OWN residents** — it doesn't see other
   gateway processes.
2. **Cross-process flock ledger (v2, `rozum-core/share.rs`).** Each gateway PROCESS reserves its
   footprint in `residents/<pid>` and admits against the host budget. Coarse (process
   granularity), heavy (N HTTP servers), eviction = process death or `shed` self-unload.

**They don't compose:** a gateway's in-process warm admission (system 1) ignores other processes'
reservations (system 2), so two warm-capable gateways could each fill `total*0.8` → **overcommit**.
And the governor (`shed`) can only self-unload its whole model, not evict a specific warm secondary.

## The unified design

**One in-process Switchboard is the primary multi-model host; the flock ledger becomes the
cross-process backstop; the governor evicts precisely.** Three coherent pieces:

### U1 — Make warm admission host-aware (close the compose gap)
`Switchboard::ensure_warm` / `plan_residency` budget must be `host_budget − committed_by_other_pids`
(read the v2 ledger's `committed_by_others`, not just `total*0.8`). So the in-process warm set + any
external gateways sum to ≤ the host budget. The Switchboard process publishes its **total** resident
footprint (primary + warm) as its single `residents/<pid>` reservation, updated on each warm
load/evict (needs a `share.rs` "update my reservation" API — sunny-civet's ledger). Net: one budget,
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

Keep the v2 flock ledger throughout (it's the cross-process safety backstop for stray/external
gateways like the matrix's dedicated ones). The Switchboard is the *primary* path; the ledger is the
*floor*.

## Risks / open questions (for sunny-civet)
- **Matrix-critical path:** the Switchboard is in the serving hot loop — every change behaviour-
  preserving + matrix-gated. Keep single-model behaviour byte-identical when multislot is off.
- **Reservation-update API:** U1 needs `share.rs` to support updating a live reservation (the
  process's footprint changes as warm models come/go). Your ledger — how do you want it shaped?
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
