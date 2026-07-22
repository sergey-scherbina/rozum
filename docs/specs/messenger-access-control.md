# Messenger access control (per-user capabilities)

## Overview

The operator controls who may use the Telegram bot and what each user may do,
editing it **live from inside Telegram** — no config files, no restart. Access is
a per-user capability set persisted as JSON and shared by the two processes that
enforce it: the bridge (who may chat + command handling) and the model
participant (which tools it runs on a user's behalf).

Related: `messenger-bridges-daemon.md` (the bridge/trust boundary),
`assistant-sandbox-tools.md` (the tools the capabilities gate).

## Capabilities

Per user (numeric Telegram id):

| Capability | Meaning |
|---|---|
| `chat` | The user's messages are relayed to the room and the model answers. Without it the user is ignored. |
| `read` | The model may `list_files` / `read_file` in the sandbox on this user's behalf. |
| `write` | The model may `write_file` in the sandbox on this user's behalf. |
| `shell` | The model may `run_command` (confined to the sandbox), only if the participant also runs with `--shell`. |

The **owner** implicitly has all capabilities and is the only id that may run
management commands. The owner is `TELEGRAM_OWNER_ID` if set, otherwise the
private-chat peer id (so a personal bot needs no configuration). In a group,
set `TELEGRAM_OWNER_ID` to your id.

## Storage

`$XDG_STATE_HOME/rozum/messenger-acl/telegram.json` (fallback
`~/.local/state/rozum/...`), written atomically (temp + rename). Shape:

```json
{ "owner": 1711036782,
  "members": { "42": { "name": "Bob", "chat": true, "read": true, "write": false, "shell": false } } }
```

A missing or corrupt file is treated as empty (deny-by-default); the operator
re-grants. The bridge writes it (via commands); the participant reads it fresh on
each reply, so grants/revocations take effect on the next message.

## In-Telegram commands

Typed as normal chat messages; never relayed to the room. A trailing `@BotName`
(group form) is accepted. On startup the bridge registers these via `setMyCommands`,
so they appear behind the Telegram Menu button and the `/` autocomplete.

| Command | Who | Effect |
|---|---|---|
| `/help`, `/start` | anyone | Command list. |
| `/whoami`, `/id` | anyone | Reply the sender's numeric id + name (how a prospective user learns their id). |
| `/members`, `/who` | owner | List owner + members with their capabilities. |
| `/grant <id> [caps…]` | owner | Add/update a member. `caps` are `chat read write shell` (or `all` / `none`); default when omitted is `chat`. Unknown token → error, nothing changed. |
| `/revoke <id>` | owner | Remove a member. |

When an **unknown** sender messages, the owner is pinged **once** with that
sender's id + name so they can `/grant` it.

## Enforcement

- **Chat** is enforced by the bridge: a message is relayed only if the sender is
  the owner, an ACL member with `chat`, or matches the env allowlist
  (`TELEGRAM_ALLOWED_USER_IDS`, which remains an additional accept path).
- **read / write / shell** are enforced by the participant: it parses the
  triggering user's id from the bridge's `[<name> #<id>]:` prefix, loads the ACL,
  and advertises only that user's permitted tools for that reply. The owner gets
  all; a user with none gets plain chat.

## Decisions

- **One ACL file, two enforcers.** Chat gating must live in the bridge (before a
  message ever reaches the room); tool gating must live in the participant (which
  owns the tools). Both read one file rather than duplicating policy. Rejected:
  a second IPC channel to sync policy between the processes.
- **Owner bootstrapped, not configured.** A personal (private-chat) bot makes the
  peer the owner automatically, so the feature works with zero setup; a group
  bridge names the owner via `TELEGRAM_OWNER_ID`.
- **Deny-by-default, discovery via `/whoami`.** Unknown senders are ignored (the
  owner is pinged once). Ids are learned with `/whoami`, not by trusting the
  chat. The env allowlist stays as an explicit escape hatch.

## Results

`cargo test -p rozum-meeting --lib --no-default-features` covers the ACL
(parse/grant/revoke/caps/round-trip/corrupt-file) and the command handler
(owner-only gating, default/explicit caps, group-suffixed commands, bad tokens).
Live Telegram E2E is operator-driven (needs a bot token + a second user).
