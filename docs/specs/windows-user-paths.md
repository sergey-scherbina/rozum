# Where this user's files are, and the variable Windows does not set

Status: implemented 2026-08-14. `crates/rozum-paths/`.

Queued as `windows-fs-locks` — "route the `.rozum/room/` advisory lock through a cross-platform
lock (`fs2` / `fd-lock`) instead of a raw `flock`, and confirm all room/cache path handling is
`PathBuf`-based". **Two thirds of that entry describe work that no longer exists**, and the third
one, once measured, was not where the entry said. This is what the code actually said when asked.

## What the entry asked for, and what was there

**The locks are already cross-platform, and not by `fs2`.** Every advisory lock in the workspace is
`std::fs::File::try_lock` — stabilised in the std library, both platforms, no crate. The residency
ledger, the admission queue, the daemon's socket ownership, the room registry: all of them. The code
around them already carries Windows-aware comments ("Windows cannot unlink a file while this handle
owns the lock", "Windows locks deny reads through the …"), so this was ported deliberately by
somebody and the board entry simply never heard about it. Nothing to do.

**Path separators are already `PathBuf`.** Four `format!("{}/…")` hits in the two crates that hold
room and cache paths, and not one of them is a filesystem path: a thread id (`date/n`), a
directory-listing display suffix, and two URLs.

**What was actually wrong is one line neither half of the entry mentions:**

```rust
std::env::var_os("HOME")            // ← not a Windows variable
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("/tmp"))
```

On Windows `HOME` is unset (it is `USERPROFILE`), so **every** path in the process took the fallback
— and `PathBuf::from("/tmp")` there names `\tmp` on the current drive: a directory nobody created
and every account on the machine shares.

For logs that is untidy. For one file it is not: `share::gateway_dir()` holds the **residency
ledger**, whose whole job is to stop a second model load from exhausting host RAM — the failure that
took this Mac down through a kernel-watchdog panic (BUG-003). A ledger two users share is not a
ledger. That is the finding this item turned out to be about.

## The rule, in one place

`rozum-paths` — a leaf crate with **no dependencies**, the shape `rozum-stamp` already set here.

| | order |
|---|---|
| `home_dir()` | `HOME`, then `USERPROFILE` (Windows). `None` if neither. |
| `state_dir()` | `$XDG_STATE_HOME/rozum`, then `%LOCALAPPDATA%\rozum`, then `~/.local/state/rozum` |
| `config_dir()` | `$XDG_CONFIG_HOME/rozum`, then `%APPDATA%\rozum`, then `~/.config/rozum` |
| `temp_dir()` | `std::env::temp_dir()` |

**Why a crate and not a module.** Four crates need this and one of them is `rozum-meeting`, which
deliberately does not depend on `rozum-core` — the meeting daemon is engine-free. A rule that cannot
be shared gets copied, and the copies drift: `rozum-gateway` **alone** held three copies of the state
directory, already differing in shape, and a fourth in `rozum-meeting` resolved the same
`rooms.json` that the gateway console reads. They agreed by coincidence.

**`state_dir` and `config_dir` are separate on purpose.** Unix keeps them apart and so does Windows,
differently: `%APPDATA%` roams between machines and `%LOCALAPPDATA%` does not. Collapsing them would
put a residency ledger on a network share.

**`XDG_*` wins on both platforms.** Every isolated test in this workspace redirects `XDG_STATE_HOME`;
a Windows arm that consulted `%LOCALAPPDATA%` first would point those tests at the real state
directory.

**`None`, not a fallback, when there is no home.** The caller decides: a log file falls back to the
temp dir; a ledger should refuse. Returning a shared path from inside the rule would take that
decision away from both.

## Two paths that deliberately did NOT move

- **`/tmp/rozum-agentic-*`** (`matrix.rs`) stays a literal. `scripts/bench/agentic.sh` creates those
  workdirs with `mktemp -d /tmp/rozum-agentic-XXXXXX`, so the scanner and the maker must name the
  same directory. `std::env::temp_dir()` reads `$TMPDIR` — on macOS a per-user `/var/folders/…` —
  and the archive step would then find nothing, silently, on every run.
- **`~/Library/LaunchAgents`, the seatbelt profile, the launchd plists.** These paths describe
  *macOS*, not this user's filesystem conventions. They resolve home through the shared rule and
  keep spelling the rest out, because the whole function is launchd's — a Windows arm belongs in
  `windows-service-install`, not behind a name that hides which platform it means.

## What is proven

- `cargo check --workspace --no-default-features --target x86_64-pc-windows-gnu`: 0 errors,
  unchanged from `windows-spawn-seams`.
- Host: 13 suites green, 0 failures, and two tests that exist for this change specifically —
  `rozum-paths`' four (resolution order, and that an absent home yields `None` rather than a shared
  directory) and `share::tests::the_residency_ledger_did_not_move_on_the_machine_this_runs_on`,
  which asserts the exact old layout, because the risk of this change is on the platform that IS
  running: if the ledger moves, a live gateway's reservations become invisible to the next one.

## What is NOT proven

The Windows variables and their order are what the platform documents, not what was observed — there
is no Windows machine here, as with the rest of this port. On unix, wherever `HOME` is set (launchd,
every shell here), every path resolves exactly as before; the only paths that move are the ones that
previously had nowhere to go.
