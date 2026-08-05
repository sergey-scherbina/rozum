# The meeting daemon's launchd job must wait, not step aside

Status: spec (2026-08-05)
Fixes: `BUGS.md` BUG-025 (the respawn loop half; the socket-ownership half is left open on purpose)
Owner: `meeting-daemon-supervise`
Found by: two agents independently — `doctor --services` reporting the split ownership, and a
`runs = 78` counter climbing while nothing looked broken from outside.

## The defect, end to end

1. A client cannot reach the daemon, so `daemon_proxy::spawn_daemon` runs `meetings start`
   (`daemon_proxy.rs:798`).
2. That spawns `meetings start --foreground` **detached**, in its own process group
   (`main.rs:spawn_detached_meetings`). The parent returns, so the daemon is reparented to launchd
   and shows `ppid 1`.
3. Now a daemon exists that the launchd job does not own.
4. launchd runs its job — also `meetings start --foreground`. It sees a live socket and does this
   (`main.rs:3616`):

   ```rust
   if daemon_alive(&sock).await {
       eprintln!("meeting daemon already running ({})", sock.display());
       return;                       // exit 0
   }
   ```

5. The plist says `KeepAlive = true`, which means *this job must never be done*. So launchd starts
   it again. It steps aside again. **Forever, at roughly one process every nine seconds.**

Measured on the operator's machine before the manual repair: `runs` 78 → 90 in about four minutes,
and a 525-line, 44 KB log consisting of the same sentence.

**The one-line statement of the bug:** `exit(0)` is a process saying *my work is done*, and
`KeepAlive = true` is a supervisor saying *you are never done*. Politely stepping aside is the one
thing a supervised process must not do.

Nothing looks broken while this happens — rooms answer, `:8401` answers — which is why it ran for an
unknown length of time. It is BUG-013's family again: running, and not serving what you think.

## Required behaviour

When `meetings start --foreground` finds a daemon already alive:

- **Under a supervisor** — wait for the incumbent to disappear, then become the daemon. The job
  holds its slot, so there is no respawn loop, and the service self-heals the moment a
  client-spawned daemon dies, which is exactly what a supervised service is for.
- **Anywhere else** — keep today's behaviour: say so and exit 0.

That second half is not politeness, it is required. `spawn_detached_meetings` starts its child with
the very same `--foreground`, and two clients can both find no daemon and both spawn one. Today the
loser exits. If waiting were unconditional, every lost race would leave a permanent idle standby
process — trading a respawn loop for a process leak.

## How "under a supervisor" is decided

launchd sets `XPC_SERVICE_NAME` in every job it starts; it is absent from an ordinary shell and from
a client-spawned child. Verified on this host: the launchd-owned daemon carries
`XPC_SERVICE_NAME=com.rozum.meeting-daemon`, an interactive shell carries none.

The alternative was an explicit `--supervise` flag written into the plist. Rejected because the
plists are **already installed** on every host that runs this: a flag only takes effect after a
service reinstall, so the fix would ship and quietly not apply. The environment marker fixes every
existing installation the moment the binary is replaced.

`getppid() == 1` was also considered and rejected — a detached client-spawned daemon is reparented
to pid 1 too, so it cannot tell the two cases apart, which is the whole question.

## Verification

- Unit: the decision function returns *supervise* for `XPC_SERVICE_NAME` present and non-empty,
  *step aside* for absent, empty, and whitespace-only. No socket, no daemon, no processes — the
  earlier lesson from this repo is that a test which reaches a socket is not a unit test, and one
  that did created two live rooms in the operator's running daemon.
- On the host, after deploying: `launchctl print gui/501/com.rozum.meeting-daemon | grep 'runs ='`
  twice a minute apart with a client-spawned daemon alive. The counter must not move, and
  `launchctl list` must show a pid rather than `-`.

## Out of scope, deliberately

Two daemons can still share one socket path: a second binder unlinks the first one's socket file
rather than refusing. This change removes the loop and gives the job real ownership, but the
duplicate-daemon class needs a lock file so a second binder refuses — bigger, separate, and still
open under BUG-025. Left there rather than folded in here, because a fix that also rewrites socket
ownership is one nobody can review as a unit.
