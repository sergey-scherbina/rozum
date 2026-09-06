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

## Per-room rosters and group management

Each chat/room has its **own** roster (`messenger-acl/<room>.json`) — a grant in one
group does not apply in another or in the private chat. The owner is global
(bootstrapped into every room's roster). The set of connected groups is a registry
(`messenger-groups/telegram.json`) the operator edits LIVE from the bot (owner-only):

| Command | Effect |
|---|---|
| `/groups` | List connected groups (chat id → room). |
| `/addgroup` | Sent IN a group → connect it to room `group-<id>` with a fresh roster. |
| `/removegroup [id]` | Disconnect a group (default = the current chat). |

The bridge routes primary + registry chats over one `getUpdates`; on a topology change
it re-execs to apply (the durable cursor means no message loss). A
`meetings participant-pool` supervisor runs one model per registered room and reaps
those removed. Extra/group chats validate leniently — a group where the bot is neither
admin nor privacy-off is skipped with a warning, never taking the private chat down.

## Storage

`$XDG_STATE_HOME/rozum/messenger-acl/<room>.json` per room (fallback
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

## Granting access in advance, by forward (2026-09-06)

`/grant` takes a numeric id, and Telegram shows one nowhere in its UI. Until now the only way to
learn someone's was to have them write to the bot first — which is exactly what you cannot do for
a person you want to admit BEFORE they arrive.

A forwarded message carries its original author's id, and that is the one identity a bot can
learn about someone who has never contacted it. Forward any message from them to the bot and it
answers with the person, their id, and the command:

```
↪️ Переслано от Bob (id 987654321). Скопируй команду ниже, чтобы дать доступ
   (можно дописать read write shell):

/grant 987654321 chat
```

The command arrives as its OWN message. Telegram copies a whole message at a tap, so a hint that
carries its explanation and its command together cannot be copied without the prose — which
defeats the point of printing a command. The not-admitted notice was split the same way for the
same reason.

**It offers; it does not grant.** Forwarding is also how you hand the model something to read,
and in a room running `--reply-policy always` that is an ordinary daily act — granting on any
forward would turn sharing content into handing out access, silently. The act stays explicit.

Owner-only and once per author, on the same terms as the not-admitted notice: nobody else can act
on it, and a repeat is noise.

Both `forward_from` (pre-7.0) and `forward_origin` (Bot API 7.0) are read, since both are in the
wild. Nothing is disclosed when the original author restricts forwarding, when the origin is a
channel, or when the author is a bot — none of those is an error, there is simply no id to use.

## Where the refusal notice goes (2026-09-06)

To the OWNER, privately — not into the chat the message came from.

It is addressed to the owner: it names a person and offers a `/grant` only the owner can run.
Posting it where it arrived meant a GROUP read it — everyone learning that someone had been
refused, and the refused person being shown a command they cannot use. Noise for the group and a
small indignity for them.

A Telegram private chat's id is the user's id, so the owner's own id is the address; the notice
names which chat the attempt came from. It falls back to the originating chat only when no owner
is recorded, which is the one case with nowhere better to send it.

In the group itself the bot now says nothing: a non-member's message is simply not relayed, which
is what a bot that does not serve them should look like.

**Note what a stranger in their OWN private chat receives: nothing at all.** The bridge routes
only chats it serves; a message from any other chat is logged once to stderr and dropped, before
command handling. So `/whoami` from someone the bot has never been introduced to is not answered
either — forwarding one of their messages is the only way to learn their id.
