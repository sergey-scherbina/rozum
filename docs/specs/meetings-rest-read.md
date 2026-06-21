# Meetings REST Read

## Overview

Expose a small read-only HTTP surface from the meeting daemon so stateless local
clients can inspect room history without speaking MCP or opening transcript files
directly. The API reads only the daemon's room registry, `index.json`, and daily
JSONL transcript files; it never submits messages, opens model state, or mutates
rooms.

## Interface

The daemon starts the REST read listener only when `ROZUM_WEB_SECRET` is set to a
non-empty value. The listener binds to `ROZUM_MEETINGS_REST_BIND` when set, else
`127.0.0.1:8401`.

Authentication matches `rozum meetings web`: HTTP Basic auth, any username,
password equal to `ROZUM_WEB_SECRET`. Unauthenticated or wrong-secret requests
return `401` with a Basic challenge.

- `GET /rooms/{name}/days`
  - Response: `{ "room": "...", "days": [{ "date": "YYYY-MM-DD", "count": N, "bytes": N }] }`
  - `days` is sorted ascending and is sourced from `index.json`; if the index is
    missing or unreadable, the handler rebuilds the listing from day files.
- `GET /rooms/{name}/messages/{date}?from=N&count=M`
  - `from` defaults to `0`.
  - `count` defaults to `100` and is capped at `500`.
  - Response:
    `{ "room": "...", "date": "YYYY-MM-DD", "from": N, "count": M, "next_from": N, "has_more": bool, "messages": [...] }`
  - `messages` contains stored turns with their canonical `(date, n)` fields.

Unknown rooms return `404`. Missing day files return an empty `messages` list,
not an error.

## Behavior

- [ ] The daemon can expose a local read-only HTTP listener alongside
      `meeting.sock` without changing the MCP tool contract.
- [ ] REST reads resolve room names through the daemon registry and read from
      disk; they do not open rooms as writers or mutate registry, meta, roster,
      index, or transcript files.
- [ ] `/rooms/{name}/days` returns sorted day metadata from `index.json`, with a
      disk-scan fallback when the index is absent.
- [ ] `/rooms/{name}/messages/{date}` returns a bounded page from a single daily
      transcript and reports `next_from` / `has_more`.
- [ ] Requests without the shared secret, or with the wrong secret, are rejected
      with `401`.

## Out of scope

- Writes, room creation, participant controls, long-polling, SSE, or submit
  endpoints.
- Replacing the TUI, MCP proxy, or existing `rozum meetings web` client.
- Cross-host exposure by default; the default bind address is loopback.
