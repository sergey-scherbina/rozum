# Messenger admin console — CLI + UCC screen for bots, groups and rosters

Status: in progress (2026-07-27)
Owner: operator request — "CLI для реестра … заведи. И в контрол центре тоже сделай отдельный
экран и инструмент для управления ботами и группами в телеграме"

## Why

Everything about the messenger assistant is currently administered from **inside Telegram**
(`/addgroup`, `/removegroup`, `/groups`, `/grant`, `/revoke`, `/members`). That is a good
interface right up until it isn't:

- **It fails exactly when you need it.** On 2026-07-27 the operator left the test supergroup
  `-1004378341901` and could not get back in (the group is orphaned — 0 admins, Bot API has no
  method to add a user to a chat). The registry entry pointing at that group could only be
  removed with `/removegroup` from a chat, or by hand-editing JSON in the state dir. There is no
  CLI. See BUGS.md and the SPRINT entry `stale-group-registry-cleanup`.
- **Hand-editing is a footgun.** The pool re-reads the registry every 5s and reconciles, but the
  **bridge reads it only at startup** — so a manual edit applies to half the system until someone
  remembers to restart the bridge. Nothing tells you that.
- **There is no answer to "is the bot even alive?"** BUG-013 (the :8089 gateway crash-looping for
  4 days) was invisible partly because nothing shows bot/bridge/pool health in one place.
- Adding a second bot (`f89bcfd`) meant hand-writing two plists, a launcher script, and a secret
  file. That is a recipe, not a feature.

## Scope (operator decisions, 2026-07-27)

Asked explicitly, answered explicitly:

- **Bots in UCC: view + start/stop AND add-a-bot from the UI.** The operator chose the deeper
  option knowing the trade-off that was spelled out: the token travels through the browser and an
  HTTP request. Mitigations are mandatory, see Security.
- **ACL rosters are in scope** — a "Права" tab on the same screen, not a separate task.

Out of scope for this pass: Discord (same shape, not wired), and any change to the in-chat
commands, which keep working exactly as they do today.

## Design

### One implementation, three front-ends

The operations live in **one place** — `crates/rozum-meeting/src/messenger_admin.rs` — and the CLI,
the REST layer and the in-chat commands all call it. No logic in the route handlers, no logic in
the `.ssc`. This is the whole reason the CLI and the screen can't drift apart.

```
messenger_admin.rs      ← operations + validation (pure where possible, unit-tested)
   ├── rozum-gateway messenger …        (CLI)
   ├── /control/messenger/*             (REST, admin-gated)  → UCC screen
   └── telegram/mod.rs handle_topology  (in-chat, unchanged behaviour)
```

### CLI surface

```
rozum-gateway messenger bots                       # roster: @username, id, service state, rooms, groups
rozum-gateway messenger groups list   [--registry telegram]
rozum-gateway messenger groups add    <chat_id> [--room R] [--title T] [--registry telegram]
rozum-gateway messenger groups remove <chat_id>  [--registry telegram]
rozum-gateway messenger acl show      <room>
rozum-gateway messenger acl grant     <room> <user_id> <caps…>   # chat|read|write|shell|all|none
rozum-gateway messenger acl revoke    <room> <user_id>
```

`messenger` is a NEW top-level command group. The existing `telegram --room … --name …` (the bridge
itself, invoked by the launchd wrappers) is untouched — breaking it would take the assistant down.

### The apply problem, fixed at the root

A registry mutation is only real once the **bridge** sees it. Rather than make every caller
remember to restart it, the bridge now **watches its own registry file**: it records the mtime at
startup and re-checks each poll iteration; on change it logs and returns, which is the existing,
proven "topology changed" path (exit 0 → launchd KeepAlive → fresh bridge, durable cursor = no
message loss). Consequences:

- `/addgroup` keeps working exactly as before (it already returns through that path).
- CLI edits, UCC edits and hand-edits now all apply by themselves.
- Gotcha #10 in the bridge memory ("edited but not applied") stops existing.

### REST surface (all admin-gated)

| Route | Method | Body |
|---|---|---|
| `/control/messenger/status` | GET | — |
| `/control/messenger/group/add` | POST | `registry, chat_id, room?, title?` |
| `/control/messenger/group/remove` | POST | `registry, chat_id` |
| `/control/messenger/acl` | GET | `?room=` |
| `/control/messenger/acl/grant` | POST | `room, user_id, name?, caps` |
| `/control/messenger/acl/revoke` | POST | `room, user_id` |
| `/control/messenger/bot/service` | POST | `bot, action=start\|stop\|restart` |
| `/control/messenger/bot/add` | POST | `name, token, room?` |
| `/control/messenger/bot/remove` | POST | `bot` |

`status` is the single signal the screen renders from (one `fetchUrlSignal`, the established UCC
pattern): bots with their live `getMe` identity + launchd state + room + group count, the groups of
every registry with their room and whether the pool has a participant for it, and the list of rooms
that have an ACL roster.

### UCC screen

New hash route `#/messenger`, nav entry from the dashboard, three sections on one page (the SPA has
no tab primitive; sections separated by `divider()` is the existing idiom):

1. **Боты** — table + start/stop/restart per row, and an "add bot" form (name, token, primary room).
2. **Группы** — table per registry + remove per row, and an add form (registry, chat_id, room, title).
3. **Права** — room picker → members table with caps + grant/revoke.

## Security

The operator accepted the token-through-browser trade-off; these mitigations are not optional.

- **Admin-gated.** Every route sits behind `require_auth` + `require_admin`, i.e. Face ID/passkey or
  busi SSO with the admin role, on the tailnet-only origin — the same gate as user/role management.
- **Write-only token.** The token is accepted on `bot/add`, written to `~/.rozum/secrets/<name>-token`
  mode `600`, and never read back out. `status` NEVER returns a token; the UI never renders one.
- **Never logged.** No token in an error message, a log line, or a `Debug` derive. The validation
  error for a bad token is the Telegram API's own description, with the token removed from any URL
  before it can be printed.
- **Validated before install.** `bot/add` calls `getMe` first — an invalid token is rejected without
  writing a file or a plist, so a typo can't leave a crash-looping service behind.
- **The UI clears the field** on success, so the token does not sit in the page.

## Verification — what was actually done (2026-07-27)

DONE:

- **Unit tests**: 9 on `messenger_admin` (bot-name path/label safety, seeding only claiming bots
  whose secret exists, upsert/round-trip, `launchctl print` parsing incl. the BUG-013 crash-loop
  shape, service-action parsing, and — the important one — *generated plists and wrapper never
  contain the token*, plus XML and shell escaping of interpolated values), 1 on the bridge's
  registry-change predicate covering absent→present, modified, and present→absent.
- **`cargo test --workspace`: 736 passed, 0 failed** — the whole suite, not `cargo check`, per
  `gw-test-suite-not-compiling` (`cargo check` does not build `#[cfg(test)]`).
- **CLI against the real deployment**: both bots resolve through `getMe`, launchd states read
  correctly, group add/remove round-trips and is idempotent in both directions.
- **SPA**: `ssc-tools emit-spa` compiles the page (540 KB, 0 JS syntax errors, all wiring present).
- **REST**: every new route answers `401` unauthenticated while an unknown path under the same
  prefix answers `405` — real registration behind real auth, not a blanket.

NOT YET DONE — needs a deploy of this branch, so it is deliberately not claimed:

- **The live watch proof**: a CLI group-add picked up by the RUNNING bridge with no manual restart.
  The bridge on the machine is the Jul-23 binary, which predates the watch. The predicate is
  unit-tested; the integration is not, and a unit test can't stand in for it.
- **`ucc-e2e.mjs`** against the new screen (it drives the live authenticated origin).

A bug that only running the code could find, worth recording: **Telegram group ids are always
negative, and clap parsed `-1004378341901` as a flag** — so the single most common invocation of
`groups add` failed outright. Fixed with `allow_negative_numbers`.

## Notes

- The registries are namespaced per bot (`telegram`, `telegram-groups`); every operation takes the
  registry explicitly and defaults to `telegram`. Cross-registry leakage is the failure this whole
  feature has to avoid — it's what `f89bcfd` was for.
- Room names for groups come from `messenger_groups::default_room` (`group-<|chat_id|>`) so re-adding
  a group lands on the same room and its ACL roster survives.
