# The agentic matrix

## Sharing a resident model (`BENCH_GATEWAY_URL`)

Two agents CAN work against one resident model — `rozum launch` reuses a healthy gateway whose
model matches, and the gateway admits 2 concurrent requests. What cannot coexist is two GATEWAYS
each holding ~12 GB of the same weights, and this harness deliberately loads its own so that time
and RSS are measured in isolation. That is why a matrix waits in the admission queue whenever
somebody else has the model resident.

```bash
BENCH_GATEWAY_URL=http://127.0.0.1:8089 AGENTS=nadia TASKS=wordcount REPS=3 scripts/bench/agentic.sh
```

**Pass/fail only.** Timings are contended (measured: the same task at 67 s, 193 s and 163 s in one
shared run) and the footprint column is left EMPTY rather than filled from a process this run does
not own. Do not compare seconds across a shared run and a private one.

Two things this mode had to be taught, both found by running it:

- It must NOT `gateway stop --force` at startup — that killed the very gateway it meant to borrow,
  and every cell then failed with `rozum launch: no gateway running`. The operator's `:8089` went
  down for the seconds launchd needed to bring it back.
- It must not tear the gateway down at the end either. A gateway we did not start is somebody's
  resident model, and stopping it would take their work with it.

