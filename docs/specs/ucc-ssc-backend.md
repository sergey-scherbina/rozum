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

**Done when:** the four routes are served from a .ssc program behind the same origin, the Rust
handlers for them are deleted rather than left as a second implementation, and the existing
view-token gate still refuses a revoked token.

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
