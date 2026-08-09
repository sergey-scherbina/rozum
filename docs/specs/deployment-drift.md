# Deployment drift: what is MERGED versus what is RUNNING

Status: implemented 2026-08-08. `crates/rozum-core/src/build_stamp.rs` + `src/doctor.rs`.
Companion to [`service-liveness.md`](service-liveness.md), which answers the other half.

## The gap

`doctor --services` could say every service was healthy and be right, while the binary serving it
was days behind `master`. That happened three times in two days (2026-08-07..08); once a feature was
"shipped" for a day while the daemon serving it had never heard of it. Every other kind of drift in
this repo has a check. This one had none, and it was caught by hand each time.

Health and freshness are different questions. `answers 200` was only ever the first.

## How a binary says what it is

`build.rs` in `rozum-core` bakes the commit into an exported marker string, so every binary linking
that crate carries it. The check reads the FILE — it never runs the service to ask. A binary that
cannot start is exactly the case worth reporting, and asking the resident-model gateway its version
would cost a model load.

The stamp lives in its own crate, `rozum-stamp`, with NO dependencies: `rozum` (the 627 KB
dispatcher) and `rozum-meet` exist precisely because they are thin, and making them depend on
rozum-core to carry a 57-byte marker would trade the thing they are for the ability to describe it.
Both must also REFERENCE it — a crate nothing references is a crate the linker never pulls in.

Three things this had to survive. The first two were found by its own test; the third was found on
the operator's machine, after being deployed:

- **`#[used]` is not enough.** The compiler keeps the static; the linker dropped it anyway, and the
  scan found nothing in the very binary that declared it.
- **A `&str` static is a POINTER.** `#[used]` and `no_mangle` keep the pointer while the string
  bytes sit in another section that `-dead_strip` removes once nothing references them. That version
  passed in DEBUG and vanished in RELEASE — the suite was green while every deployed binary went out
  unstamped, which is the "unknown reported as silence" this module exists to remove, reintroduced
  by its own implementation. The text itself is the static now, as a byte array.
  **A property that only holds in the profile nobody ships is not a property**, so the real gate is
  in `scripts/install-bins.sh`: it refuses to publish a workspace binary that carries no stamp.
- **The scanner contains the string it scans for.** Taking the FIRST match found the `MARK_PREFIX`
  constant, read zero hex digits after it, and reported "unstamped" for a stamped binary. It walks
  every occurrence and takes the first followed by a real sha.

## What is reported, and as what

| State | Verdict |
|---|---|
| stamped, `origin/master` has nothing newer | silent — the row stays `ok` |
| stamped, N commits behind | **`warn`**: "deployed binary is N commits behind origin/master (abc1234)" |
| stamped with a commit this checkout does not know | `warn`, said as exactly that — a binary built elsewhere or from a pruned branch |
| **no stamp, and it is one of our cargo binaries** | **`warn`**: its age is unknown |
| no stamp, foreign binary (`rozum-meeting-ssc`, a shell script) | silent — it links none of our crates and never can carry one |

**`warn`, never `fail`.** Being behind between a merge and a deploy is normal, and a red that is
usually red gets ignored — which is how this check would become the noise it exists to replace.

**Unstamped is reported, not skipped.** An unstamped binary predates stamping or was built outside a
checkout; either way its age is unknown, and reporting unknown as silence is the substitution this
whole check exists to remove. It is also self-clearing: one deploy and the fleet is stamped.

## Measured against `origin/master`, not the checkout

A stale clone comparing against its own `HEAD` pronounces itself perfectly up to date — the bug
wearing a green hat. The count is `<stamped>..origin/master`, and a commit git does not recognise
FAILS the count rather than returning zero. Both are tested against a throwaway repo with a real
remote, because the whole claim is about which ref is used.

The checkout to measure in is this binary's own build-time repo, falling back to the cwd's. A build
inside a worktree bakes the worktree path, and worktrees are deleted when their branch lands — so a
missing path means "cannot compare", never "up to date".
