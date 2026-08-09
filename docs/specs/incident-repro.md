# Repro: what may leave a working tree, and on whose authority

Status: implemented 2026-08-09. `crates/rozum-meeting/src/meeting/repro.rs`, driven by
`rozum meetings incident repro`. Third of the incident specs, after
[`incident-resolving.md`](incident-resolving.md) and [`incident-evidence.md`](incident-evidence.md).

## Why this needed a spec and the other two did not

The evidence already attached to an incident — the transcript, the gateway log slice, the machine
snapshot — is rozum's data about itself. This is the first step that copies **someone else's files**
into a store with a different and wider readership: a room is read by the support console on `:8401`,
through busi SSO, and by whoever is in the Telegram bridge. "Capture the workdir" is one sentence
that hides a data-export decision, and inventing one inside an incident feature is how a support
tool grows a data-export problem.

So the questions are answered here, before any of them are answered by accident.

## 1. Inputs, not a copy of the tree

What lands in the incident is what makes the failure reproducible, not the tree it happened in:

- the commit, the branch, and whether the tree was dirty;
- the **diff of TRACKED files** — staged and unstaged;
- the command that failed, when the reporter names one (`--cmd`);
- an **allow-list** of environment variables, by exact name, never the environment.

**No untracked files, and no `.gitignore`d files. Ever.** That is where `.env`, tokens, key
material, dumps and customer data live. A diff of tracked files is bounded, reviewable by eye, and
already something the repository is willing to show. A tree is neither.

The cost is honest: a bug that only reproduces with an untracked fixture is not fully captured by
this. That is the trade, and the alternative — copying whatever happens to be lying in the directory
— is the one that ends badly exactly once.

## 2. A secret in the diff REFUSES the capture

The scan is heuristic — private-key headers, `Authorization: Bearer`, assignments to names like
`TOKEN`/`SECRET`/`PASSWORD`/`API_KEY`, Telegram bot-token shapes — and heuristics miss things, which
the refusal message says. What matters is what happens on a HIT: the capture is refused, not
scrubbed.

Redaction is not a safety net here. `meeting redact` hides content **at read time**; the bytes stay
in the room's `.jsonl` on disk. A "redacted" secret is a leaked secret with a curtain in front of
it, so the only safe moment to stop is before the write.

**Absence of a finding is not a clean bill of health**, and the message says that too. The reporter
is the one who knows what is in their diff.

## 3. Manual, not automatic

The capture happens when a human or an agent asks for it by name — never as a side effect of opening
an incident. Automatic capture means a program decides to copy someone's files into a shared room
whenever something goes wrong, and the machine snapshot precedent does not apply: that snapshot is
rozum describing itself, this is rozum describing its user's work.

(Operator's call, taken 2026-08-09: manual.)

## 4. Size: bounded, and truncated LOUDLY

A room is a permanent append-only log, so a capture is permanent too. The diff is capped at 256 KB;
past that the capture keeps the metadata, drops the diff body, and says which files were in it and
how big they were. A bundle that quietly held half a diff would be worse than one that holds none —
the reader would not know which half.

## 5. It is a message in the thread

Like the machine snapshot: one `event` message, so it belongs to the thread by construction,
`repair-threads` rebuilds it with everything else, every surface that shows a timeline shows it, and
it cannot rot out of sync with the incident it belongs to. No side files, no attachment store, no
second lifetime to manage.

## What this deliberately does not do

- **No binary artifacts, no build outputs, no core dumps.** They are large, unreviewable, and the
  reason to want them is nearly always answered by the command plus the diff.
- **No automatic secret scrubbing.** Refusing is a decision the reporter can act on; scrubbing is a
  guess about which bytes mattered.
- **No cross-machine fetch.** The capture runs where the failure was, with the reporter's own
  permissions, and nothing reaches into another host.
