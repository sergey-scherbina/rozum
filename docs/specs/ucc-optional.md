# The console is a feature, not a fact of the binary

Status: implemented 2026-08-09. `ucc` feature on `rozum` and `rozum-gateway`; the snapshot split out
into `crates/rozum-gateway/src/status.rs`.

## Why

`rozum-gateway` depended on `webauthn-rs` unconditionally — the Face ID / passkey ceremonies of the
browser console. `webauthn-rs-core` depends on `openssl` and `openssl-sys` with no feature gate, so
every build of this binary needed a native OpenSSL. On Windows that is the difference between
`cargo build` and an afternoon of vcpkg: a machine that wants a MODEL SERVER had to build a passkey
stack to get one.

## The split

- **`status.rs` is never gated.** The machine snapshot — active gateway, residency ledger, installed
  catalog, live agents/coders/sessions — is pure reading, and it is what `gateway status --json` and
  `doctor` consume. It had lived in `control.rs` since the console was written, which is the only
  reason it looked like console code.
- **`control` + `auth` are behind `ucc`** (default ON): the SPA routes, RBAC, invites, view tokens,
  and the passkey ceremonies. `webauthn-rs` is optional and only this feature pulls it.
- `gateway control-serve` without the feature exits with a sentence naming the feature, rather than
  a missing subcommand or a silent no-op.

## What it buys, measured

| Build | webauthn/OpenSSL in the Windows graph |
|---|---|
| default (`ucc` on) | yes — `webauthn-rs → openssl-sys` |
| `--no-default-features` | **no** — `cargo tree -i openssl-sys` finds nothing |

`cargo check -p rozum --no-default-features --target x86_64-pc-windows-gnu` gets past OpenSSL
entirely and now stops on the NEXT layer: 26 errors, all `std::os::unix` in the process-spawn
helpers (`coders.rs`, `spawn_support.rs`, `matter.rs`'s runner). Recorded as `windows-spawn-seams`;
this spec does not pretend that removing one blocker removed the platform's.

## What this is not

**It is not "webauthn on rustls".** That was the other candidate and it is not a switch: measured on
`webauthn-rs-core 0.5.5`, `openssl` and `openssl-sys` are unconditional dependencies with no feature
behind them, and the crate's attestation and COSE verification are built on them throughout.
Replacing that is a fork or an upstream project in someone else's crate — weeks, and not ours. The
cheap escape hatch for someone who wants the console on Windows is `openssl/vendored` (builds OpenSSL
from source; needs perl and nasm on the builder), which stays available and is now a choice rather
than a requirement.
