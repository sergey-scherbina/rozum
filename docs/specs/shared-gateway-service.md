# shared-gateway-service — install the gateway as an always-warm user service

By default the shared gateway is lazy: `rozum launch` spawns it on demand and it idle-exits to free
RAM. For a machine where you always want a local model ready, `rozum service install` registers the
gateway as a **user service** that starts at login and is kept alive — launchd on macOS, `systemd
--user` on Linux. (`shared-gateway-service`.)

## CLI

```
rozum service install --model qwen3-4b [--model claude-haiku-4-5] [--port 8089] [--n-ctx N] \
                      [--offline] [--strategy classify|learned|cheapest]
rozum service uninstall
rozum service status
```

`--model` is repeatable / comma-separated (a cascade), exactly like `rozum gateway`. The service runs
`rozum gateway --model … [flags]`; `--offline`/`--strategy` are threaded as gateway flags, and
`ROZUM_CASCADE` / `ROZUM_CONFIG` from the installing shell are captured into the service environment
so a named/JSON cascade keeps working under the service.

## Design

The plist / unit **generation** lives in the library (`src/service.rs`) and is pure + unit-tested:

- `launchd_plist(program, args, env)` — a `~/Library/LaunchAgents/com.rozum.gateway.plist` with
  `ProgramArguments`, `EnvironmentVariables`, `RunAtLoad` + `KeepAlive`, logging to the gateway state
  dir. XML-escaped values.
- `systemd_unit(program, args, env)` — a `~/.config/systemd/user/rozum-gateway.service` with
  `ExecStart`, `Environment=`, `Restart=on-failure`, `WantedBy=default.target`.
- `launchd_plist_path()` / `systemd_unit_path()`.

The binary (`main.rs::run_service`) builds the gateway args from the current executable path +
flags, writes the generated file, and invokes `launchctl load -w` / `systemctl --user enable --now`
(install) or `unload`/`disable` + remove (uninstall). Install is idempotent (it unloads first).

Logs go to `$XDG_STATE_HOME/rozum/gateway/service.log`; the daemon's own `gateway status` /
`/stats` still work against the running service.

## Notes

- It runs as a **user** agent (no root): `launchctl … gui/<uid>` semantics, `systemctl --user`.
  Survives logout only if lingering is enabled (`loginctl enable-linger`) on Linux; on macOS a
  LaunchAgent runs while the user is logged in.
- The generation is tested feature-free; the `launchctl`/`systemctl` invocation is validated by the
  operator (it touches the real service manager).
