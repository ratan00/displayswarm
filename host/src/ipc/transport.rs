//! The byte-stream transport behind the IPC, kept behind a small trait: a Unix
//! socket on Unix, a named pipe on Windows, with no change to the protocol code.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};

/// Any duplex byte stream.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> AsyncStream for T {}

pub type BoxedStream = Box<dyn AsyncStream>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Accepts client connections.
pub trait IpcListener: Send {
    fn accept(&mut self) -> BoxFuture<'_, io::Result<BoxedStream>>;
}

/// Where the daemon listens and how a client reaches it.
pub trait IpcTransport: Send + Sync {
    /// Starts listening. Fails when another daemon already listens, and
    /// replaces the leftovers of a crashed one.
    fn bind(&self) -> BoxFuture<'_, io::Result<Box<dyn IpcListener>>>;
    /// Opens a connection to the daemon.
    fn connect(&self) -> BoxFuture<'_, io::Result<BoxedStream>>;
    /// Removes what `bind` left behind (the socket file).
    fn cleanup(&self);
    /// For logs and error messages.
    fn describe(&self) -> String;
}

/// The transport for this platform at `path`: a Unix socket file, or (Windows)
/// the named pipe derived from it.
pub fn transport_at(path: impl Into<PathBuf>) -> Arc<dyn IpcTransport> {
    #[cfg(unix)]
    return Arc::new(UnixTransport::new(path));
    #[cfg(windows)]
    return Arc::new(NamedPipeTransport::new(path));
}

/// [`transport_at`] the default path.
pub fn default_transport() -> Arc<dyn IpcTransport> {
    transport_at(default_socket_path())
}

/// The named pipe that stands for `path`: `\\.\pipe\displayswarm-<name>-<hash>`.
/// The hash of the whole path keeps users (and the standalone UI's per-process
/// sockets) apart; the path itself is never created on Windows, it only anchors
/// the pipe name and the files next to it.
pub fn pipe_name_for(path: &Path) -> String {
    // FNV-1a: stable across runs and Rust versions, unlike `DefaultHasher`.
    let full = path.to_string_lossy();
    let hash = full.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    // Split on both separators so the name does not depend on the host OS.
    let file = full.rsplit(['/', '\\']).next().unwrap_or_default();
    let stem: String = file
        .rsplit_once('.')
        .map_or(file, |(s, _)| s)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    format!(r"\\.\pipe\displayswarm-{stem}-{hash:016x}")
}

/// `$XDG_RUNTIME_DIR/displayswarm/hostd.sock` (fallback `/tmp/displayswarm-<uid>/hostd.sock`);
/// on Windows `%LOCALAPPDATA%\displayswarm\hostd.sock`.
pub fn default_socket_path() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        return base.join("displayswarm").join("hostd.sock");
    }
    #[cfg(not(windows))]
    default_unix_socket_path()
}

#[cfg(not(windows))]
fn default_unix_socket_path() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("displayswarm"),
        None => {
            #[cfg(unix)]
            let uid = unsafe { libc::getuid() };
            #[cfg(not(unix))]
            let uid = 0;
            PathBuf::from(format!("/tmp/displayswarm-{uid}"))
        }
    };
    dir.join("hostd.sock")
}

#[cfg(unix)]
pub use unix::UnixTransport;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use tokio::net::{UnixListener, UnixStream};

    /// A Unix domain socket at `path`, reachable only by its owner.
    pub struct UnixTransport {
        pub path: PathBuf,
    }

    impl UnixTransport {
        pub fn new(path: impl Into<PathBuf>) -> Self {
            Self { path: path.into() }
        }

        pub fn default_path() -> Self {
            Self::new(default_socket_path())
        }
    }

    struct Listener(UnixListener);

    impl IpcListener for Listener {
        fn accept(&mut self) -> BoxFuture<'_, io::Result<BoxedStream>> {
            Box::pin(async move {
                let (s, _) = self.0.accept().await?;
                Ok(Box::new(s) as BoxedStream)
            })
        }
    }

    impl IpcTransport for UnixTransport {
        fn bind(&self) -> BoxFuture<'_, io::Result<Box<dyn IpcListener>>> {
            Box::pin(async move {
                if let Some(dir) = self.path.parent() {
                    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
                }
                // A live daemon answers; a dead one leaves a file nobody serves.
                if UnixStream::connect(&self.path).await.is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another daemon is already listening on {}", self.path.display()),
                    ));
                }
                let _ = std::fs::remove_file(&self.path);
                let listener = UnixListener::bind(&self.path)?;
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
                Ok(Box::new(Listener(listener)) as Box<dyn IpcListener>)
            })
        }

        fn connect(&self) -> BoxFuture<'_, io::Result<BoxedStream>> {
            Box::pin(async move { Ok(Box::new(UnixStream::connect(&self.path).await?) as BoxedStream) })
        }

        fn cleanup(&self) {
            let _ = std::fs::remove_file(&self.path);
        }

        fn describe(&self) -> String {
            self.path.display().to_string()
        }
    }
}

#[cfg(windows)]
pub use windows_pipe::NamedPipeTransport;

#[cfg(windows)]
mod windows_pipe {
    use super::*;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    /// A named pipe for the user, local clients only.
    ///
    /// UNVERIFIED on Windows: the pipe gets the default security descriptor
    /// (full control for the creator, SYSTEM and Administrators, read for
    /// everyone else); a tighter DACL is left for the installer work.
    pub struct NamedPipeTransport {
        pub path: PathBuf,
        name: String,
    }

    impl NamedPipeTransport {
        pub fn new(path: impl Into<PathBuf>) -> Self {
            let path = path.into();
            let name = pipe_name_for(&path);
            Self { path, name }
        }
    }

    struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    impl IpcListener for Listener {
        fn accept(&mut self) -> BoxFuture<'_, io::Result<BoxedStream>> {
            Box::pin(async move {
                self.next.connect().await?;
                // A new instance must be waiting before the connected one is handed out.
                let fresh = ServerOptions::new().reject_remote_clients(true).create(&self.name)?;
                let connected = std::mem::replace(&mut self.next, fresh);
                Ok(Box::new(connected) as BoxedStream)
            })
        }
    }

    impl IpcTransport for NamedPipeTransport {
        fn bind(&self) -> BoxFuture<'_, io::Result<Box<dyn IpcListener>>> {
            Box::pin(async move {
                // A live daemon answers; `first_pipe_instance` also refuses a second server.
                if ClientOptions::new().open(&self.name).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another daemon is already listening on {}", self.name),
                    ));
                }
                if let Some(dir) = self.path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let next =
                    ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&self.name)?;
                Ok(Box::new(Listener { name: self.name.clone(), next }) as Box<dyn IpcListener>)
            })
        }

        fn connect(&self) -> BoxFuture<'_, io::Result<BoxedStream>> {
            Box::pin(async move { Ok(Box::new(ClientOptions::new().open(&self.name)?) as BoxedStream) })
        }

        fn cleanup(&self) {}

        fn describe(&self) -> String {
            self.name.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names_are_stable_distinct_and_well_formed() {
        let a = pipe_name_for(Path::new(r"C:\Users\amy\AppData\Local\displayswarm\hostd.sock"));
        assert_eq!(a, pipe_name_for(Path::new(r"C:\Users\amy\AppData\Local\displayswarm\hostd.sock")));
        assert_ne!(a, pipe_name_for(Path::new(r"C:\Users\bob\AppData\Local\displayswarm\hostd.sock")));
        assert!(a.starts_with(r"\\.\pipe\displayswarm-hostd-"), "{a}");
        // Pipe names may not contain backslashes after the prefix.
        assert!(!a[9..].contains('\\'), "{a}");
        // The standalone UI's per-process socket gets its own pipe.
        let s = pipe_name_for(Path::new("/tmp/x/standalone-42.sock"));
        assert!(s.starts_with(r"\\.\pipe\displayswarm-standalone-42-"), "{s}");
    }
}
