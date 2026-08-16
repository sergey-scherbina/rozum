# Slice 2 — the session question, answered by measurement

Status: spec (2026-08-16)
Slice 2 of [`ucc-ssc-backend.md`](ucc-ssc-backend.md), whose slice 1 is in production
Owner: `ucc-ssc-backend`

Slice 1 asked whether a `.ssc` server can stand beside the Rust one and answer real traffic. It
can, and it does: `/control/public/matrix/cell` and `/view/{token}` on the live console are served
by ScalaScript. Slice 1 got there by picking the only two routes that need no session at all.

Every remaining route has one, and slice 1's own spec named the question that blocks all of them:

> **How does a `.ssc` server participate in a session it does not own?**

with three options — (a) re-implement the chain, (b) sit behind the Rust one, (c) receive an
already-resolved principal — and one instruction: **measure, do not assume.**

## What the session actually is (measured 2026-08-16)

| | |
|---|---|
| Cookie | `rozum_sess`, an **opaque** token — not a signed claim, nothing to verify offline |
| Resolution | `session_user` (`auth.rs:461`): linear scan of the sessions file, `token == … && expires_at > now` |
| Second identity | busi SSO — cookie `busi_device` checked for membership in `~/.busi/tokens.txt`, resolving to the fixed user `busi-sso` |
| Permissions | `user_has_perm` (`auth.rs:132`): user → `role_ids` → roles, all from files; `admin` satisfies everything; `busi-sso` is granted the operator set explicitly |
| What the gate hands downstream | `require_auth` injects **`Extension<String>`, the user id, and nothing else** (`auth.rs:502`) |

Everything above is **files on disk**, which is why (a) has always looked cheap. It is worth being
precise about what "cheap" means here before choosing.

## The measurement that decides it

**No route handler consumes the principal. Not one.**

    $ grep -c Extension crates/rozum-gateway/src/control.rs
    0

All 66 routes in `control.rs` take the request and the process's own state. The user id is used to
DECIDE whether the handler runs — by `require_auth` and the five `require_perm_*` layers — and then
it is dropped. Nothing downstream personalises anything by user.

That collapses the question. A `.ssc` server serving a gated route does not need the principal,
because the Rust handler it replaces does not use it either. **Option (c) is option (b) plus a
header nobody reads**, and option (a) re-implements a decision the ported code never sees the result
of.

So: **(b). The gate stays in Rust; the `.ssc` server serves what the gate has already admitted.**

## What (b) actually needs, which is not nothing

Slice 1 is safe under (b) for a reason that does not generalise: its two routes are public by
design, so it does not matter who reaches them. Extend the same arrangement to a gated route and
the arrangement is the hole:

1. `SSC_HTTP_BIND=127.0.0.1` binds the `.ssc` server to loopback, and **any local process can call
   it** — every agent this project launches runs on this host. The toolkit offers `serve(port)`
   only (`std/http.ssc:51`); there is no unix-socket bind to hide behind.
2. `ucc_ssc_proxy` (`control.rs:243`) forwards **nothing**: a bare
   `reqwest::Client::new().get(&url)` with no headers at all. Cookies, and anything else that might
   identify the caller, stop at the proxy today.

So a gated route served this way would be reachable, ungated, on `:8412`, by anything on the
machine — while `:8411` correctly answers 401. **Slice 3 must not port a single gated route before
the door exists.**

### The door

A shared secret between the proxy and the `.ssc` server, exactly the pattern
`ROZUM_WEB_SECRET` already uses for the meeting REST server (`rest_read.rs:118`): environment
first, file fallback, so it does not matter who started the process.

- `ucc_ssc_proxy` sends the secret on every proxied request.
- The `.ssc` server refuses any request that does not carry it, with 403 and a body naming the
  reason. It can: `Request` already carries `headers` (`std/http.ssc:186`), which is also how it
  reads the view token today.
- The secret is a **door, not an authorisation**. It says "this came through the gate", nothing
  about who. That is precisely the property the measurement above says is sufficient.

This is small — one header on each side — and it is the whole of slice 2's code.

## Why not (a), stated as a cost rather than a taste

Mechanically (a) is easy: three file reads, and the `.ssc` server already does a file-backed
authorisation decision for view tokens. The cost is not the code, it is that **the rule then exists
twice, in two languages, and the copies drift.**

That is not hypothetical here — it is the current state of slice 1. The view-token check lives in
`public_matrix_cell_route` (Rust) *and* in `public-matrix.ssc`, and the only thing keeping them
honest is that the deploy compared 1892 answers byte-for-byte on one day. The project has already
paid for this shape once: BUG-026 landed in one of three implementations of the same contract, and
`nadia:SPEC.md §3.1` exists because "a contract kept in one of three is not a contract".

Duplicating the *session and permission* chain is that risk applied to the security boundary, to
buy a principal that no handler reads. If a future route ever does personalise by user, (c) becomes
a one-line addition to the door — a second header — and the decision can be revisited then, with a
route that actually needs it in hand.

## Verification

- Unit, Rust: the proxy attaches the secret header when one is configured, and sends the request
  WITHOUT it when none is. **Corrected while implementing, against what this line said first:** the
  two halves must fail in opposite directions. The `.ssc` half fails CLOSED — it is the half that
  decides, and once it requires the door an unauthenticated caller gets 403. The proxy failing
  closed too would buy nothing and would take a working console down the moment the binary is
  upgraded before the secret file exists, which is the kind of upgrade that teaches people to skip
  upgrades. A host with no secret keeps serving exactly what it serves today, and the two routes
  proxied today are public by design.
- Unit, `.ssc`: a request without the header is refused; with the wrong value refused; with the
  right value served. Pure, no socket.
- On the host: with the console running, `curl :8412/control/public/matrix/cell` **directly** must
  answer 403-no-door, while the same URL through `:8411` still answers exactly what it answers
  today. **Done as far as the toolchain allows, 2026-08-16, and the gap is stated rather than
  glossed:** on an isolated port, with `ROZUM_UCC_SSC_SECRET` set, the cell route answered
  `{"error":"this port is reachable only through the console"}` without the header and fell through
  to its own token rule with it. The `/view/` route could not be exercised at all — the shared
  scalascript toolchain is STALE (built from `4b717804c`, tree at `c7259999f`) and its prefix route
  does not match `/view/abc` there, with or without the door, while the deployed binary serves that
  route in production. That is a toolchain vintage, not this change, and rebuilding the operator's
  shared toolchain to find out is not this slice's business. The direct-`:8412` check is the one
  that proves a door is a door: that server was reachable in production while this was written,
  which is what made slice 2 worth doing before slice 3 rather than after.
- `rozum doctor --services` must still report `svc:ucc-ssc` as healthy. Its probe hits the `.ssc`
  server directly and accepts a 403, precisely because a service refusing a request it should
  refuse is a service that is alive — the door does not change that reading.
  **But it does weaken it, and that is worth saying out loud rather than discovering later.** Today
  the 403 proves the process parsed the request and applied its own view-token rule; behind a closed
  door it will prove only that the door refused an outsider, which a process holding a socket and
  nothing else could also do. The fix, when the door goes live, is to give the probe the secret so
  it keeps testing the service rather than the doorman — a one-line change to `services.rs`, listed
  here so it lands with the door instead of being noticed a month later.

## What is NOT live yet

The `.ssc` half of the door is source, not a running binary. `rozum-ucc-ssc` on the host is the
binary built on 2026-08-15 and it does not know about the header; the Rust proxy now sends one, and
sending a header nobody reads changes nothing. So **today the door is built and not yet closed**,
which is exactly the state slice 3 must not start from.

Closing it needs three things, in order, and none is a code change here:

1. the scalascript toolchain restaged (`(cd ../scalascript && ./install.sh --dev)`) — the shared
   tree, which is why this is the operator's call and not a step taken quietly;
2. `clients/control/build-public-matrix.sh` rebuilding `rozum-ucc-ssc`, and the secret placed at
   `~/.rozum/secrets/ucc-ssc-door` (600, beside the messenger tokens) for both jobs;
3. the acceptance slice 1 used and earned — compare the console's answers against the current ones
   before trusting the switch, and check `:8412` direct now refuses while `:8411` is unchanged.

## Out of scope

- The three remaining `/control/public/matrix*` routes stay Rust: they read process-global state
  inside the gateway (`matrix_queue()`, `matrix_live()`), which no separate server can see. Measured
  in slice 1 and unchanged.
- Terminal (5 routes) and auth (16) stay Rust, as the item has said from the start.
- Porting any read route is slice 3. This spec's job is to make that a port rather than a security
  decision.
