# Knowing when a service we ship has died

Status: implemented 2026-08-05 (`rozum doctor --services`). Contract for `src/doctor.rs`.

## Why

Every green surface this project looks at stayed green through a four-day outage of its flagship
feature. `cargo test` passes, CI passes, `launchctl list` prints an exit code nobody reads, and the
failing job wrote nothing to its own log because it died before it could write anything (BUG-013).
It was found by accident.

2026-08-05 added three more of the same shape in one day, all invisible:

1. Both Telegram bridges sat dead at exit `-9` after a deploy. I found it reading `launchctl list`
   for an unrelated reason; the operator would have found it by writing to a bot that never replied.
2. Both bridges had been exiting on every meeting-daemon blip — three times each, in their own logs
   — and `KeepAlive` restarted them. Self-healing, invisible, and until that morning it took
   `nadia serve` and every running agent with it (BUG-023).
3. `nadia serve` died on every bridge restart and lost every agent's record. The only symptom was
   that the next agent came back as `#1`.

The shape, stated once: **a `KeepAlive` job that can never exec is indistinguishable from a healthy
one unless you probe what it is supposed to serve.** `launchctl` will happily report a job that has
restarted 36,000 times as present.

## Contract

### 1. It probes the service, not the process table

For each `com.rozum.*` job: is it loaded, is it running now, and what was its last exit. That much
comes from `launchctl`. Then, for every service that serves something, **an actual request to the
thing it exists to serve** — the gateway answers `/v1/models`, the control plane answers
`/control/auth/status`, the meeting daemon answers on its socket, and so on. A job whose endpoint
does not answer is `fail` however healthy the process table looks.

Where a service has no endpoint (the bridges talk outward to Telegram; the participant pools talk to
the daemon), say so rather than invent a probe: the check reports what it could establish, and
`skip` is an honest verdict. **A probe that cannot fail is not evidence.**

### 1b. It names WHO serves, not who is loaded

A launchd job can be alive and merely waiting while another process holds the socket and serves —
that is the state `BUG-025`'s ownership fix makes normal rather than exceptional. `running (pid X)`
plus `the endpoint answers` are then two true halves that together say something false. Where an
owner can be established independently (the lock beside the meeting socket), the line names the
serving pid and says when it is not the job's — because "it works, but the job cannot restart what
it does not own" is exactly the kind of thing this check exists to surface.

### 2. It reports; it does not restart

Nothing here stops, starts or kickstarts a job. Today's other lesson cost the operator two agents'
work: a process that restarts services on its own schedule will do it while somebody is mid-task,
and the loss is silent. The one place that may act is the deploy, which already knows what it just
stopped and is watched by whoever ran it.

### 3. Exit code carries the verdict

`--services` alone is a report (exit 0). With `--strict`, a failing service fails the command, so a
`StartInterval` job or a deploy step can gate on it without parsing the text.

### 4. News, not noise

The periodic job posts to the rozum room **on transition** — when a service that was answering stops
answering, or comes back. A line every five minutes is a line nobody reads, which is the same
failure as no line at all, arrived at from the other side.

## What this does not do

It does not watch anything the machine does not run (`launchctl` is the roster), it does not
retain history beyond the previous verdict, and it draws no conclusion about *why* a service is
down — that is what the logs and `BUGS.md` are for.
