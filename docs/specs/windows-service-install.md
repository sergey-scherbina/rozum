# Installing rozum as a user service, where there is no launchd and no systemd

`rozum service {install,uninstall,start,stop,status}` and `rozum meetings {install,uninstall}` put
the gateway and the meeting daemon under the platform's supervisor so they start at login and stay
warm. Two arms existed: a launchd LaunchAgent on macOS, a `systemd --user` unit everywhere else.

## What was in the way

**"Everywhere else" included Windows**, and that is the actual defect this closes. The arms were
split on `#[cfg(target_os = "macos")]` / `#[cfg(not(target_os = "macos"))]`, so on Windows
`rozum service install` generated a systemd unit, wrote it to `%APPDATA%\rozum\systemd\user\`, and
then tried to spawn `systemctl` — reporting "failed to run the service manager" after having
already written a file that nothing will ever read.

It compiled. Windows CI was green. The wrongness was entirely in what the code DID, and a
compile-only gate cannot see that, which is worth stating plainly next to two sibling items
(`windows-daemon-ipc`, `windows-spawn-seams`) whose claim is exactly "compiles for Windows".

## Task Scheduler, not a Windows Service

Both existing arms install a **per-user** thing. A LaunchAgent runs as the logged-in user at login;
so does a `systemd --user` unit. Two facts decide the Windows equivalent:

- `sc.exe` installs a **machine** service, by default under `LocalSystem`. That is a different
  security posture for a process that serves this user's models, reads this user's `~/.rozum`, and
  holds the residency ledger that `windows-user-paths` just finished making per-user.
- The Service Control Manager **kills any binary that does not report `SERVICE_RUNNING`** over the
  service control protocol inside its start timeout. `rozum gateway` is a plain program. Making it a
  Windows Service is a change to the BINARY — an SCM entry point, a dispatcher thread, status
  reporting — not an arm in a module that generates files.

A logon-triggered scheduled task needs neither, keeps the per-user semantics, restarts on failure,
and can be started and stopped on demand. So that is the arm.

**The trade-off, written down rather than dismissed:** a task runs only while someone is logged on.
A headless Windows box that reboots to a login screen does not bring the gateway back. That is the
case that would justify the SCM work, and it is the one thing `sc.exe` buys that this does not.

## Two files, because of the environment

Task Scheduler XML has no element for environment variables — `<Exec>` carries a command and
arguments, and nothing else. Both other arms pass `env` through (`ROZUM_CASCADE`, `ROZUM_CONFIG`,
and the meeting daemon's `ROZUM_WEB_SECRET`). So the task runs a generated `.cmd` launcher which
sets them and then execs the program, appending both streams to the same `service.log` the plist
names via `StandardOutPath` — a bare task would have dropped that too.

    %APPDATA%\rozum\rozum-gateway.cmd        set the env, run the program, append to the log
    %APPDATA%\rozum\rozum-gateway.task.xml   logon trigger, restart on failure, no time limit

Config and not state: these are the "what to run" description an operator may open and read,
alongside `rozum.toml`. The log the service writes stays under the state dir.

### Three decisions worth the words

**A refusal instead of an escape, for a double quote.** `cmd.exe` quoting has no total escape for a
double quote inside a quoted string. The failure mode of guessing is a service that starts with
silently different arguments — a gateway serving a model nobody asked for, or a web secret that is
not the one the console just printed. Every value this actually carries (an exe path, a model spec,
a port, a hex secret) is quote-free, so `windows_launcher_cmd` returns an error naming the offending
value and the caller exits, instead of writing something plausible.

**`%` is doubled.** It is the one character a `.cmd` file eats: `%FOO%` expands. Every value that
lands in the launcher goes through `%` → `%%`.

**`<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>`, which is easy to omit and expensive to omit.**
The Task Scheduler default is 72 hours, after which it stops a task that is behaving perfectly. A
gateway that vanishes every three days is worse than one that was never installed.

The XML is written as **UTF-16LE with a BOM**. `schtasks /query /xml` emits UTF-16 and the importer
is documented against it, and the declaration this file carries says `UTF-16` — a UTF-8 file
claiming UTF-16 is the one combination that is wrong under every reading. The generator returns a
`String` so the tests can assert on text; the encoding happens once, at the write.

## What is proven

- `cargo check --workspace --no-default-features --target x86_64-pc-windows-gnu`: **0 errors**.
- That the cross-check actually COMPILES this arm rather than skipping it: a deliberate
  `THIS_DOES_NOT_EXIST` inside `status_service` made the Windows check fail with `E0425` and left
  the macOS check at 0. A guard that cannot be shown to fire is not a guard.
- 14 unit tests in `src/service.rs` over the generators: env set before the program runs, per-argument
  quoting (a path with a space is the normal case on Windows), `%` doubling, the quote refusal for
  both an argument and an environment value, the logon trigger, the execution-time-limit line, XML
  escaping, the UTF-16LE BOM bytes, and that the two tasks share neither a name nor a file.
- `cargo test --workspace --lib` green on macOS: the unix arms are unchanged, only their `cfg`
  narrowed from `not(macos)` to `all(unix, not(macos))`.

## What is NOT proven

**Nothing here has run on Windows.** `schtasks` is invoked as documented; whether the XML imports
cleanly, whether the logon trigger fires for a task registered this way, and whether the launcher's
quoting survives a real `cmd.exe` are all unverified. There is no Windows box in this project. The
generators are tested; the invocation is a claim.

Two runtime gaps remain and are NOT this item's: terminal sessions shell out to `tmux`
(`crates/rozum-gateway/src/sessions.rs`) and the matrix to `bash` (`matrix.rs`). Neither exists on a
Windows box. The decision handed over from `windows-spawn-seams` is recorded in `BACKLOG.md` as
`windows-tmux-bash-refusal`: **a refusal, not a seam** — reimplementing a terminal multiplexer and a
1000-line bash harness on ConPTY is a project, and a seam that pretended otherwise would fail later
and less clearly than a message saying so up front.
