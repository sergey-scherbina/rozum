# Pre-change fixtures for the rich-rooms migration

`roster.pre-roles.json` and `meta.pre-roles.json` are the shapes the operator's live daemon was
writing on 2026-08-07, BEFORE `RosterEntry.roles` existed. Their job is one assertion: a binary
that knows about roles must still read a roster written by one that did not.

**They are redacted, and that is not laziness.** Every real `RosterEntry` carries a
`session_token` — the participant's reconnect key — and the live `rozum` room holds 418 of them.
Checking those bytes in would publish 418 credentials to git history, where they cannot be recalled.
So the FIELD NAMES, types and nesting are the real ones, read from that file; ids, tokens and the
home path are replaced with obvious placeholders.

One deviation is worth naming: the live room contains `human` and `mcp` participants but no
`bridge` one, so the third entry is synthesised from the same shape to cover that variant. It was
constructed, not observed — do not read it as evidence that a bridge entry looks exactly like this.
