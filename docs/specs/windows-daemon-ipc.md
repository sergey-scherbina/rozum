# The meeting daemon's transport, and what it takes to leave unix

Status: implemented 2026-08-09 — **compiles for Windows, has never run there.**
`crates/rozum-meeting/src/meeting/ipc.rs`.

## What was in the way

`rozum-meeting` did not compile for Windows at all: 11 errors, every one of them a unix-domain
socket (`std::os::unix`, `tokio::net::UnixListener`/`UnixStream`, `tokio::signal::unix`, and an
inode read). The daemon's design — one writer, direct reads, an ownership lock beside the endpoint,
the `mcp-proxy` bridge — was never the problem; the bytes' road was.

## The seam

`meeting::ipc` is the whole platform difference:

| | unix | Windows |
|---|---|---|
| endpoint | the socket path | `\\.\pipe\rozum-<file name>`, derived from the same path |
| listener | `UnixListener` | a named-pipe server instance, with the successor created BEFORE serving the current one |
| stream | `UnixStream` | `NamedPipeServer` / `NamedPipeClient` behind one enum |
| split | `tokio::io::split` | the same |
| shutdown | SIGTERM + Ctrl-C | Ctrl-C |
| endpoint identity | the inode | `None` — "cannot tell", never "someone took it" |

**A named pipe, not loopback TCP.** TCP on `127.0.0.1` is reachable by every account on the machine,
while a unix socket and a named pipe both carry an ACL, and this endpoint speaks MCP as whoever
joined. Trading a permissioned transport for an open port to save an afternoon is how a local
privilege boundary quietly disappears.

**The successor-first ordering** is the one real subtlety: a named pipe has no listener object, so
the server must create the next instance before serving the current one, or a client connecting in
between finds no instance and fails.

## What is proven

- `cargo check -p rozum-meeting --target x86_64-pc-windows-gnu`: **0 errors**, from 11.
- `rozum-agent`, `rozum-meet`, `rozum-cli`: 0 errors for the same target.
- The unix path is unchanged and exercised: the workspace suite is green, and an isolated daemon
  opened and listed an incident over the socket through the new seam.

## What is NOT proven, and what still blocks Windows

**Nothing here has run on Windows.** The daemon says so on startup there, with a request to report
what happens, rather than letting the first user read a defect as their own fault.

And the whole binary still does not build there, for a reason that is no longer this: `webauthn-rs`
(the UCC's Face ID / passkey support) pulls `openssl-sys`, whose build script needs a Windows
OpenSSL. Recorded as `windows-openssl-webauthn`. Three ways out when someone wants it — vendored
OpenSSL, a rustls-based webauthn, or putting the control plane behind a feature so a Windows GGUF
user need not build it at all — and choosing between them wants a Windows machine to test on.
