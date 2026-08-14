# Starting and stopping a child process, where signals do not exist

Status: implemented 2026-08-14 — **compiles for Windows, has never run there.**
`crates/rozum-gateway/src/procctl.rs`.

The third of the Windows sub-tasks, and found the same way as the second: by removing the one
before it. With `webauthn-rs`/OpenSSL gone behind the `ucc` feature (`docs/specs/ucc-optional.md`),
`cargo check -p rozum --no-default-features --target x86_64-pc-windows-gnu` got as far as our own
code and stopped at **26 errors**.

## What was in the way

The gateway spawns three kinds of child — a meeting participant, a coder run, a matrix bench — and
then asks the same four questions about each: is it alive, stop it, freeze it, and keep it from
dying when the service that started it restarts. Every answer was written directly against `libc`
and `std::os::unix`, at the call site, in **five** files:

| | what it did | where |
|---|---|---|
| own process group | `Command::process_group(0)` | `spawn_support.rs`, `coders.rs`, `matrix.rs` |
| is this pid alive | `kill(pid, 0)` | `spawn_support.rs` |
| stop this child | `kill(pid, SIGTERM)` | `agents.rs` ×2, `coders.rs` ×2 |
| is this run alive | `kill(-pgid, 0)` | `matrix.rs` |
| pause / resume / stop a run | `killpg(pgid, SIGSTOP/SIGCONT/SIGTERM)` | `matrix.rs` |
| can this file be run | `PermissionsExt::mode() & 0o111` | `spawn_support.rs` |
| who started me | `std::os::unix::process::parent_id()` | `gateway.rs` |
| replace myself | `CommandExt::exec()` | `gateway.rs` |

**The board named three of those files; there were five.** `gateway.rs` and `agents.rs` were not in
the entry, which is what a file list written from memory costs — the count (26) was exact because it
came from running the compiler, and the file list was not because it did not.

## The seam

`procctl` is the whole platform difference. The callers no longer know which platform they are on.

| | unix | Windows |
|---|---|---|
| own process group | `process_group(0)` | `CREATE_NEW_PROCESS_GROUP` |
| pid alive | `kill(pid, 0)` | `OpenProcess` + `GetExitCodeProcess` |
| stop a child | `SIGTERM` to that pid | Ctrl+Break to its group — the only graceful stop the platform has |
| group alive | `kill(-pgid, 0)` — any member | the leader only |
| suspend / resume | `SIGSTOP` / `SIGCONT` | **unsupported, and it says so** |
| runnable file | the executable bit | the extension, against `PATHEXT` |
| parent pid | `getppid` | `None` — not known here |
| replace self | `exec` | spawn the successor, then exit |

### Three decisions worth the words

**`Unsupported` is a third outcome, not a dressed-up failure.** `SIGSTOP` has no supported Win32
equivalent — `NtSuspendProcess` is undocumented and a thread-walk is neither atomic nor safe against
a process that spawns while being walked. A seam that reported "pause failed" would send an operator
hunting a broken bench run that was never asked to freeze. So `Outcome` has three arms, and the
pause/resume routes carry a `why` when the platform has none. On unix the body is byte-for-byte the
`{"ok": …}` the console has always parsed; `why` appears only where the operation does not exist.

**Ctrl+Break, never `TerminateProcess`.** A hard kill of a process that may be mid-GPU-eval is what
rebooted this Mac once (BUG-001). The Windows failure mode is unproven, but every caller here means
"stop when you can" — a caller that wanted a kill would have to ask for one. Where the console
control event cannot be delivered (a service has no console) that is a `Failed`, not a silent no-op
and not an escalation to force.

**One `pid_alive` for the workspace.** `rozum-core::share` already had both arms, written for
reaping dead residency leases; the gateway had a second, unix-only copy, which is half of why it did
not compile. The core's is now `pub` and the gateway's is a one-line delegate. Two copies of a
platform fact is the same defect the boards keep turning up in themselves — this week
`matrix_task_info` was two copies of a prompt table, five of six already drifted.

### One behaviour change on unix, deliberate

`group_alive(0)` is now false. It used to be `kill(-0, 0)`, which addresses **the caller's own
process group** and therefore answered "alive" for a matrix record that was never spawned — keeping
its stale live-state forever. The same guard keeps a stop from reaching this gateway.

## What is proven

- `cargo check -p rozum --no-default-features --target x86_64-pc-windows-gnu`: **0 errors in
  `rozum-gateway`**, from 26.
- The host is unchanged and exercised: `cargo test -p rozum-gateway --lib` green, workspace suite
  green, and the seam's own 7 tests pass.
- The Windows arm that is genuinely **tested**, not merely compiled, is the `PATHEXT` logic:
  `program_candidates` / `has_runnable_extension` are pure functions over a `PATHEXT` string, so
  they run on the machine that exists. They are also the arm most likely to be wrong in a way no
  compiler catches — without them `agent_on_path("claude")` answers no for every agent on a Windows
  host, and the UCC refuses to launch anything while naming PATH as the reason. A wrong answer
  delivered confidently is worse than a compile error.

## What is NOT proven

**Nothing here has run on Windows.** Specifically unverified, in the order I would test them:

1. Ctrl+Break reaching a `CREATE_NEW_PROCESS_GROUP` child from a service with no console.
2. The self-reload: `exec` keeps the pid, the open files and the supervisor's grip on the process;
   the Windows path starts a NEW process and exits, so the pid changes, the listening socket is
   handed over rather than inherited, and a service manager sees its process exit.
3. `group_alive` narrowing to the leader — a bench whose leader exited but whose children are still
   running reads as finished there.

And the gateway still would not be a Windows *host* even if all three worked: it shells out to
`tmux` for terminal sessions and to `bash` for the bench, neither of which is on a Windows box.
Those are runtime gaps, not compile errors, and they are the next item's problem
(`windows-service-install`, `windows-fs-locks`) — recorded here so the compile-clean result is not
read as more than it is.
