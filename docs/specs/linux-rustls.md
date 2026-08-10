# TLS without a system library: what "compiles on Linux" was hiding

Status: implemented 2026-08-10. `reqwest` is declared with `rustls-tls` and
`default-features = false` in every crate of this workspace that makes HTTP calls.

## The finding

Asked whether the workspace compiles on Linux, I checked instead of remembering, and `rozum-core` —
which knows nothing about the browser console — died in `openssl-sys`' build script before a line of
our code was compiled. The chain is `reqwest → native-tls → openssl-sys`: `native-tls` is
`reqwest`'s default, and it means Security.framework on macOS (free) and **OpenSSL** on Linux and
Windows — a SYSTEM library that has to be found.

That is a different blocker from the webauthn one fixed the day before, and it sat one layer lower:
`ucc` gating removed OpenSSL from the console, and this removes it from everything else.

**CI did not catch it because ubuntu-latest ships OpenSSL.** The Linux job passes today and would
have passed with this defect forever; a hosted runner with the library preinstalled is not the same
machine as a container, or as a user's box.

## The change

`reqwest = { default-features = false, features = ["json", "stream", "rustls-tls", "charset",
"http2"] }`. rustls is vendored Rust, so a Linux build needs nothing installed. `default-features =
false` is what actually drops `native-tls`; adding `rustls-tls` alongside the default would have
linked both and changed nothing about the requirement.

`charset` and `http2` are named explicitly because they are in reqwest's default set and this code
uses them — dropping defaults silently drops those too, and a build that loses HTTP/2 to a TLS
change is the kind of regression nobody connects to its cause.

## Verified

- **Live TLS**: `rozum-gateway models info mlx-community:Qwen3-0.6B-4bit` fetched HuggingFace
  metadata over HTTPS with the new backend — the behaviour, not just the dependency graph.
- `cargo tree -i openssl-sys --target x86_64-unknown-linux-gnu` finds **no such package** in the
  workspace's Linux graph (with `ucc` off; the console still carries webauthn's own OpenSSL by
  design, and it is optional now).
- The full suite on macOS is green, and the Mac binary is unchanged in behaviour.

## The limit of what can be checked here

A Linux `cargo check` from this Mac now stops in `ring`'s build script — rustls' crypto compiles C
for the target and there is no `x86_64-linux-gnu-gcc` on this machine. That is this laptop, not the
code: on Linux `ring` builds with the system compiler, which is what CI's `ubuntu-latest` job does on
every push. Cross-compiling the workspace end to end would need a cross toolchain installed here, and
installing one to answer a question CI already answers is not worth the operator's disk.
