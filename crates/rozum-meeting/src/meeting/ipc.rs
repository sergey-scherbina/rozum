//! The byte transport the meeting daemon speaks over — a unix socket, or a Windows named pipe.
//!
//! Everything above this is unchanged: one writer, direct reads, the `mcp-proxy` bridge, the
//! ownership lock beside the endpoint. Only the bytes' road differs, because `std::os::unix::net`
//! does not exist on Windows and `rozum-meeting` therefore did not compile there at all
//! (`docs/specs/windows-daemon-ipc.md`).
//!
//! **A named pipe, not loopback TCP.** TCP on `127.0.0.1` is reachable by every account on the
//! machine, while both a unix socket and a named pipe carry an ACL — and this endpoint speaks MCP
//! with the identity of whoever joined. Swapping a permissioned transport for an open port to save
//! an afternoon is how a local privilege boundary quietly disappears.
//!
//! **UNVERIFIED ON WINDOWS.** It compiles for the target and nothing more: there is no Windows
//! machine here. Every claim about its behaviour is a claim about code that has never run, and the
//! daemon says so on startup there rather than letting the first user assume otherwise.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub use unix_impl::{Listener, Stream};
#[cfg(windows)]
pub use windows_impl::{Listener, Stream};

/// The endpoint a client connects to, as a string this platform understands.
///
/// On unix that is the socket path itself. On Windows a path is not an endpoint: a pipe lives in
/// `\\.\pipe\`, so the socket path's FILE NAME is reused as the pipe name — one derivation, so the
/// client and the daemon cannot disagree about where the daemon is.
pub fn endpoint_name(socket_path: &Path) -> String {
    #[cfg(windows)]
    {
        let leaf = socket_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "meeting.sock".to_string());
        return format!(r"\\.\pipe\rozum-{leaf}");
    }
    #[cfg(unix)]
    {
        socket_path.to_string_lossy().into_owned()
    }
}

/// The two halves of a connection, split so a reader and a writer can be held separately.
///
/// `tokio::io::split` on BOTH platforms rather than unix's `into_split`: a named pipe has no
/// owned-halves API, and one uniform split keeps the callers above free of `cfg`. The cost is a
/// lock per connection, which on a control socket carrying JSON-RPC lines is not measurable.
pub type ReadHalf = tokio::io::ReadHalf<Stream>;
pub type WriteHalf = tokio::io::WriteHalf<Stream>;

pub fn split(stream: Stream) -> (ReadHalf, WriteHalf) {
    tokio::io::split(stream)
}

/// Wait for a shutdown signal: Ctrl-C everywhere, plus SIGTERM where SIGTERM exists.
///
/// launchd stops a job with SIGTERM and Windows has no such signal, so this is a difference in the
/// PLATFORM rather than in the daemon, and it belongs here with the rest of them.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    #[cfg(windows)]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The identity of the endpoint we bound, so a successor binding its own at the same name is
/// noticed. On unix that is the socket's inode; a named pipe has no such handle-independent
/// identity, so Windows returns `None` and the caller must treat that as "cannot tell" rather than
/// as "someone took it" — see `socket_is_still_ours`.
pub fn endpoint_identity(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::hash::{Hash, Hasher};
        use std::os::unix::fs::MetadataExt;
        // Device + inode + creation time, folded into one number — NOT the inode alone.
        //
        // Linux reuses inode numbers immediately: remove a socket and bind a successor in the same
        // directory and the new one very often lands on the same `ino`, so a wedged owner asking
        // "is the socket still mine?" would be told yes about someone else's. macOS does not reuse
        // that eagerly, which is why the inode-only version passed here for months and failed the
        // first time CI ran it on Linux. `ctime` moves on every (re)creation, so the pair
        // distinguishes a successor from ourselves even when the number comes round again.
        //
        // The trade, stated: `ctime` also moves if someone chmods or links the socket while we own
        // it, which would read as "not ours" — a false negative. Nobody chmods a bound socket, and
        // the failure it would cause (an owner concluding it lost the path) is the safe direction:
        // the dangerous one is a wedged owner believing it still serves clients who left.
        std::fs::metadata(path).ok().map(|m| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            m.dev().hash(&mut h);
            m.ino().hash(&mut h);
            m.ctime().hash(&mut h);
            m.ctime_nsec().hash(&mut h);
            h.finish()
        })
    }
    #[cfg(windows)]
    {
        let _ = path;
        None
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::*;

    pub type Stream = tokio::net::UnixStream;

    /// Exactly the previous `UnixListener`, wrapped so both platforms present one shape.
    pub struct Listener(tokio::net::UnixListener);

    impl Listener {
        pub fn bind(socket_path: &Path) -> io::Result<Self> {
            tokio::net::UnixListener::bind(socket_path).map(Listener)
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            self.0.accept().await.map(|(s, _)| s)
        }
    }

    pub async fn connect(socket_path: &Path) -> io::Result<Stream> {
        tokio::net::UnixStream::connect(socket_path).await
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

    /// One connection, from either end. A named pipe gives the server and the client different
    /// types — unlike a unix socket, where both sides are a `UnixStream` — so the callers above,
    /// which only ever read and write, get one type that delegates.
    pub enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    macro_rules! delegate {
        ($self:ident, $inner:ident => $body:expr) => {
            match $self.get_mut() {
                Stream::Server($inner) => { let $inner = Pin::new($inner); $body }
                Stream::Client($inner) => { let $inner = Pin::new($inner); $body }
            }
        };
    }

    impl AsyncRead for Stream {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            delegate!(self, s => s.poll_read(cx, buf))
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            delegate!(self, s => s.poll_write(cx, buf))
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            delegate!(self, s => s.poll_flush(cx))
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            delegate!(self, s => s.poll_shutdown(cx))
        }
    }

    /// A named pipe has no listener object: the server creates an instance, waits for a client on
    /// it, and creates the NEXT instance before serving this one — otherwise there is a window in
    /// which a connecting client finds no instance and fails. That ordering is the whole subtlety
    /// of this backend, and it is why `accept` builds the successor first.
    pub struct Listener {
        name: String,
        next: Option<NamedPipeServer>,
    }

    impl Listener {
        pub fn bind(socket_path: &Path) -> io::Result<Self> {
            let name = endpoint_name(socket_path);
            let first = ServerOptions::new().first_pipe_instance(true).create(&name)?;
            Ok(Listener { name, next: Some(first) })
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            let server = match self.next.take() {
                Some(s) => s,
                None => ServerOptions::new().create(&self.name)?,
            };
            server.connect().await?;
            self.next = Some(ServerOptions::new().create(&self.name)?);
            Ok(Stream::Server(server))
        }
    }

    pub async fn connect(socket_path: &Path) -> io::Result<Stream> {
        ClientOptions::new().open(endpoint_name(socket_path)).map(Stream::Client)
    }
}

#[cfg(unix)]
pub use unix_impl::connect;
#[cfg(windows)]
pub use windows_impl::connect;

#[cfg(test)]
mod tests {
    use super::*;

    /// The endpoint derivation is shared by the daemon and every client, so it has to be ONE
    /// function: two derivations of "where the daemon is" is two answers, and the second one is
    /// found by a user who cannot connect.
    #[test]
    fn the_endpoint_name_is_derived_once_for_both_ends() {
        let p = Path::new("/tmp/rozum/meeting.sock");
        let a = endpoint_name(p);
        assert_eq!(a, endpoint_name(p), "must be pure");
        #[cfg(unix)]
        assert_eq!(a, "/tmp/rozum/meeting.sock");
        #[cfg(windows)]
        assert_eq!(a, r"\\.\pipe\rozum-meeting.sock");
    }

    /// On unix the identity is the inode and a missing file has none. The Windows arm returns
    /// `None` too, which callers must read as "cannot tell" — never as "it was taken".
    #[test]
    fn a_missing_endpoint_has_no_identity() {
        assert_eq!(endpoint_identity(Path::new("/nonexistent/rozum/meeting.sock")), None);
    }
}
