# Finding the shared gateway, and starting it when it is not there

Status: implemented 2026-10-08 (`crates/rozum-core/src/gateway_ensure.rs`, `rozum gateway ensure`,
`crates/nadia`). Sits on [`shared-gateway.md`](shared-gateway.md) (the registry, the spawn lock,
leases) and follows [`meeting-daemon-ownership.md`](meeting-daemon-ownership.md) (who starts a daemon
when launchd has a job for it).

## What happened

The operator started `nadia` and it said it could not reach a gateway. One was running — on `:8089`,
where `com.rozum.gateway` puts it and where `DEFAULT_GATEWAY_PORT` says it is. The Rust nadia, with
no `--gateway` and none of the two environment variables, fell back to a literal
`http://127.0.0.1:8080/v1`. Nothing listens there.

The gateway already says where it is: every daemon writes `{model, port, pid, …}` to
`$XDG_STATE_HOME/rozum/gateway/active.json` the moment it binds, and `rozum launch` has read it since
the shared gateway existed. nadia never did. The Scala nadia was fixed for the same mismatch on
2026-10-06 by asking `rozum gateway status --json`; the Rust one, in this repository, never was — a
fix made in one implementation of three, which is how BUG-026 happened too.

## The decision

**One function finds the gateway or starts it, and every client calls it.** In-process for Rust
(`rozum_core::gateway_ensure::ensure`), through `rozum gateway ensure --json` for everything else (the
Scala and ScalaScript nadias). No client keeps its own default port.

### Order

1. **The registry.** `active.json` exists and its port answers `GET /v1/models` → that gateway.
2. **The default port.** No usable registry, but `:DEFAULT_GATEWAY_PORT` answers (a registry deleted
   by hand, a daemon from an older build) → that gateway; the model is read from `/v1/models`.
3. **Nothing answers, and starting is allowed:**
   - **launchd has `com.rozum.gateway`** → `launchctl kickstart gui/<uid>/com.rozum.gateway`, WITHOUT
     `-k` (a job already running is not restarted because a client wanted it), then wait for the
     registry to answer. **A client never starts its own gateway where the job exists, not even when
     the kickstart is slow** — the reason is in `meeting-daemon-ownership.md`: the fallback re-creates
     two daemons racing for one port, the unmanaged one winning. A job that cannot serve is reported,
     naming the job and its log.
   - **No job** (another checkout, a CI box, a machine without `rozum service install`) → take
     `share::try_spawn_lock`; if someone else holds it, they are starting one — wait for it. Holding
     it, spawn `rozum-gateway gateway --port DEFAULT_GATEWAY_PORT [--model M]` detached (own process
     group, stdio to `gateway/gateway.log`) with `ROZUM_GATEWAY_LAUNCH_MANAGED=1`, so it idle-exits
     once no client holds a lease — the same lifetime `rozum launch` gives the daemon it spawns.
     Without `--model` the daemon takes `[runtime].model` from `rozum.toml`; with neither it exits 2,
     and the error says to pass `--model` or set it.
4. **Nothing answers, and starting is not allowed** (`--no-start`) → an error naming what was looked
   at: the registry path and the default port.

The binary spawned is `rozum-gateway`: the current executable when it is one (`rozum`/`rozum-gateway`),
else the one next to it, else the one on `PATH` — the resolution `meetings_binary` already uses, since
from nadia `current_exe` is nadia.

### The model

- `ensure` reports the model the gateway holds and does NOT switch it. The gateway is shared: switching
  it under another client because one client named a different model is `rozum launch`'s
  takeover policy, made with lease information; a coding agent asking for a model is not entitled to
  it. nadia warns when the model it was asked for is not the one held (`same_model`, so `org:repo`,
  `org/repo` and `hf:org/repo` compare equal — `nadia:SPEC.md` §8 rule 5), and runs against what is
  there, which is what a rozum gateway answers with anyway (rule 6).
- A model that is not resident (idle-unloaded after 300 s) is not an error: the gateway reloads it on
  the next request. `ensure` reports `resident` so a client can say the first reply will be slow.

### Leases

A client that uses the gateway holds a lease (`share::touch_lease`, heartbeated) for as long as it
runs. That keeps a launch-managed daemon from idle-exiting under a quiet chat, and tells `gateway stop`
and `rozum launch`'s takeover that someone is attached. `gateway_ensure::LeaseGuard` heartbeats every
20 s and removes the lease on drop.

## Surfaces

```
rozum gateway ensure [--model M] [--no-start] [--json]
```

Human output: one line, `gateway http://127.0.0.1:8089  model <m>  (<how>)`, `how` one of `running`,
`started by launchd`, `spawned`. `--json`:

```json
{"url":"http://127.0.0.1:8089","port":8089,"model":"mlx-community:Qwen3.5-4B-MLX-4bit",
 "pid":917,"how":"running","resident":false}
```

Exit 0 with a gateway, 1 without; the reason on stderr.

nadia (Rust): `--gateway`, then `OPENAI_BASE_URL`, then `ROZUM_GATEWAY_URL` — unchanged, explicit
always wins — and then `ensure`, only in the modes that talk to a model (`run`, `chat`, `serve`, a
`--replay` with live tools). `mcp list`, `runs`, a strict replay touch no gateway and start none. When
`ensure` started something, nadia says so on stderr in one line; when it fails, nadia exits 2 with
`ensure`'s reason.

## Not done here

- `ROZUM_GATEWAY_PORT`, promised by `shared-gateway.md`, is still read by nothing; the port stays
  `DEFAULT_GATEWAY_PORT`. Out of scope: the registry already carries the real port, which is what
  clients need.
- The Scala and ScalaScript nadias switch from `gateway status --json` to `gateway ensure --json` in
  the nadia repository, under this spec.
