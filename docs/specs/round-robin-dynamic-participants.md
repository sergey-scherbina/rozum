# Round Robin Dynamic Participants

## Overview

Round-robin moderation must remain intuitive when participants join or leave
mid-meeting. The next speaker should be selected relative to the last
transcript speaker in the current participant list, not from a stale numeric
index captured before the participant set changed.

## Interface

The public moderator mode name remains `round-robin`. The `Moderator`
interface already passes `last_speaker: Option<&ParticipantId>` to
`next_speaker`; round-robin uses that value when it is present.

## Behavior

- [x] With no last speaker, round-robin starts at the first current
  participant.
- [x] With a last speaker that is still present, round-robin chooses the next
  participant after that speaker in the current participant order.
- [x] If the last speaker is the only participant and a new participant joins,
  the next turn goes to the newly joined participant.
- [x] If the last speaker is no longer present, round-robin falls back to a
  stable current-list index.
- [x] Empty participant lists still end the meeting.

## Out of scope

- Weighted or smart speaker selection.
- Per-participant priority.
- Changing manual moderation behavior.

## Design

Round-robin keeps a small fallback index for cases where there is no usable
last speaker. When a usable last speaker exists, it computes the next index
from the current participant list and updates the fallback index to the
following position.

## Results

Implemented in `src/meeting/moderator/round_robin.rs`. Added unit coverage for
dynamic joins after a single-participant turn and fallback behavior when the
last speaker is missing. Verified with `cargo test` (28 tests passing).
