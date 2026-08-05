# One daemon owns the meeting socket, and the loser refuses instead of stealing

Status: spec (2026-08-05)
Fixes: `BUGS.md` BUG-025, the socket-ownership half (the respawn-loop half shipped in `5ff78e7`)
Owner: `meeting-socket-ownership`

## The defect

`serve_daemon_until` binds like this (`crates/rozum-meeting/src/meeting/daemon.rs:1124`):

```rust
let _ = std::fs::remove_file(socket_path);
let listener = UnixListener::bind(socket_path)?;
```

The `remove_file` is there for a real reason — a unix socket left behind by a crashed process makes
`bind` fail with `EADDRINUSE` forever, so something must clear it. But it cannot tell a **dead**
socket from a **live** one. So a second daemon unlinks a running daemon's socket and binds its own
in its place, and both processes are then alive: the first still holds its listener on an inode
nobody can reach any more, the second answers everything new.

Observed on the operator's host, twice within an hour: `:8401` was served by one pid while the unix
socket's accepted connections were on another — nine connection fds against one orphaned listener.

**Why it does not show up as broken.** Both daemons read the same room files from disk, so answers
agree and every surface stays green. It is BUG-013's family: running, and not serving what you
think. The visible cost is process churn; the invisible one is that in-memory state (sessions,
per-room subscriptions, anything not re-read from disk) exists twice.

This is also what makes the supervised takeover from `5ff78e7` a race: the supervisor polls, a
client spawns the instant its connect fails, and whoever calls `remove_file` last wins. A poll
interval narrows that window and cannot close it, because *stealing is legal*.

## Required behaviour

1. A daemon takes an exclusive **lock beside the socket** before it touches the socket file, and
   holds it for its whole life.
2. A daemon that cannot take the lock **refuses to serve** — it does not unlink, does not bind, and
   returns an error naming the reason. The socket file is never removed by a process that does not
   own it.
3. Clearing a stale socket stays possible: the lock owner may unlink and rebind freely, which is
   exactly the crashed-predecessor case the `remove_file` was written for.
4. A crashed owner must not block its successor. `flock` is released by the kernel when the process
   dies, so there is no stale-lock state to reap — the property that makes this better than a pidfile.
5. The supervised start (BUG-025's first half) retries rather than dying when it loses the lock:
   it goes back to waiting. Losing a race must not cost the launchd job its process.

## Design

A sibling file, `<socket>.lock`, opened read/write and held with `File::try_lock`. Same mechanism
the residency ledger already uses (`crates/rozum-core/src/share.rs` — see `resident_is_live`, which
relies on the same kernel-releases-on-death property), so this introduces no new dependency and no
new failure mode to learn.

Rejected alternatives:

- **A pidfile.** Needs liveness probing and stale-file reaping — precisely the bookkeeping `flock`
  gets from the kernel for free. `share.rs` already carries a comment about migrating away from this.
- **Connect first, and refuse if something answers.** That is `daemon_alive`, which is what we do
  today, and it is a check-then-act race: the answer is stale the moment it returns.
- **Abstract-namespace sockets (Linux) / `SO_REUSEADDR` tricks.** Not portable to macOS unix sockets,
  which is the only platform this runs on today.

## Verification

- Unit: acquiring the lock twice on the same path fails the second time and succeeds again once the
  first handle is dropped. Pure filesystem, in a temp dir — no socket, no daemon, no processes. The
  standing lesson in this repo is that a test which reaches the meeting socket is not a unit test:
  one that did created two live rooms in the operator's running daemon.
- On the host: start a daemon, then start a second one by the client path. The second must exit with
  the ownership message, the socket must still belong to the first (unchanged inode), and
  `pgrep -f 'meetings start'` must show one daemon rather than two.

## Out of scope

The `spawn_detached_meetings` → `daemon_alive` → spawn dance stays as it is. With the lock in place
its worst outcome is a child that refuses and exits, which is a correct outcome rather than a bug;
rewriting the autostart path is a separate change and BUG-024 already covers its environment half.
