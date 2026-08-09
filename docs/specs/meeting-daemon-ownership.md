# Who owns the meeting daemon

Status: implemented 2026-08-09. `crates/rozum-meeting/src/meeting/daemon_proxy.rs`,
`rozum meetings handoff`. Sits on top of
[`meeting-socket-ownership.md`](meeting-socket-ownership.md), which decided how ownership is TAKEN;
this decides who should be taking it.

## The state this replaces

Every client — the Telegram bridge, the MCP proxy, a bare CLI call — started its own detached daemon
when it found none. Whoever then won the `flock` beside the socket served `:8401` and the MCP socket
for everyone. It worked, and that is why it lasted: the service ran while the guarantee behind it did
not. `com.rozum.meeting-daemon`, the copy with `KeepAlive` — the one thing that brings the service
back at 4am — sat loaded and owning nothing, and `doctor --services` had been reporting exactly that
as a `warn` since the check existed.

## The decision

**Where launchd's job exists, launchd owns the daemon. Where it does not, whoever needs it starts
it.**

`spawn_daemon` now asks first: if the job is installed, `launchctl kickstart` (without `-k` — a job
already running must not be restarted because a client wanted to talk to it) and wait for the socket.
Only where the job does not exist — another checkout, a CI box, a second machine — does a client
start its own, exactly as before.

The rejected alternatives, and why:

- **launchd only, clients never spawn.** One owner, always restartable, but a client on a machine
  without the job gets nothing. "Works anywhere with no install" is the property that made this
  usable; trading it away swaps one failure for another.
- **Leave it as it was.** Works until the unmanaged daemon dies, which is precisely when nobody is
  watching.

## The handoff is a command, not a repair

When an unmanaged daemon already holds the socket, `rozum meetings handoff` stops it gracefully — the
same signal `meetings stop` sends, never a kill — and kickstarts the job. `--dry-run` says what it
would do.

**Deliberately not automatic.** A working service is stopped for a second or two, and choosing when
that happens is an operator's call, not a watcher's. `doctor --services` names the command instead of
performing it; a check that fixes things is a check nobody can trust to only look.

## The environment question, which turned out to be already answered

A client-spawned daemon inherits the CALLER's environment and so carries no `ROZUM_WEB_SECRET` —
observed serving `:8401` in production. That was fixed under BUG-024 by reading the secret from
`~/.rozum/secrets/web-secret` when the variable is absent, which makes `:8401` a property of the
INSTALLATION rather than of who happened to win the socket. Re-checked here rather than assumed:
`rest_read.rs` resolves env first, file second.
