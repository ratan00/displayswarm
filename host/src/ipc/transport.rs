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

    /// `ERROR_PIPE_BUSY`: the pipe exists but no instance is free to connect to.
    const ERROR_PIPE_BUSY: i32 = 231;

    /// A named pipe for the user, local clients only.
    ///
    /// The pipe's DACL grants access to the current user's SID and SYSTEM only
    /// (the default would also let Administrators in and give everyone else
    /// read access), and a client talks to the pipe only after checking that
    /// its server process runs as the same user. Without that check another
    /// account on the machine could create the pipe name first and pose as the
    /// daemon. Together they are what the Unix socket's 0600 mode and the
    /// owner-only directory give on Linux.
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
                let fresh = security::create(ServerOptions::new().reject_remote_clients(true), &self.name)?;
                let connected = std::mem::replace(&mut self.next, fresh);
                Ok(Box::new(connected) as BoxedStream)
            })
        }
    }

    impl IpcTransport for NamedPipeTransport {
        fn bind(&self) -> BoxFuture<'_, io::Result<Box<dyn IpcListener>>> {
            Box::pin(async move {
                // A live daemon answers; `first_pipe_instance` also refuses a second server.
                // Busy: every instance is mid-accept, which also means a daemon is there.
                let probe = ClientOptions::new().open(&self.name);
                if probe.is_ok() || probe.err().and_then(|e| e.raw_os_error()) == Some(ERROR_PIPE_BUSY) {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another daemon is already listening on {}", self.name),
                    ));
                }
                if let Some(dir) = self.path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let next = security::create(
                    ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true),
                    &self.name,
                )?;
                Ok(Box::new(Listener { name: self.name.clone(), next }) as Box<dyn IpcListener>)
            })
        }

        fn connect(&self) -> BoxFuture<'_, io::Result<BoxedStream>> {
            Box::pin(async move {
                // Between accepting one client and creating the next instance the
                // pipe is busy; that window is short, so wait it out.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    match ClientOptions::new().open(&self.name) {
                        Ok(c) => {
                            security::check_server(&c)?;
                            return Ok(Box::new(c) as BoxedStream);
                        }
                        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && std::time::Instant::now() < deadline => {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
        }

        fn cleanup(&self) {}

        fn describe(&self) -> String {
            self.name.clone()
        }
    }

    /// The pipe's access control.
    mod security {
        use std::io;
        use std::os::windows::io::AsRawHandle;
        use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
        use ::windows::core::{PCWSTR, PWSTR};
        use ::windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
        use ::windows::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use ::windows::Win32::Security::{
            EqualSid, GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER,
        };
        use ::windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
        use ::windows::Win32::System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        fn os(e: ::windows::core::Error) -> io::Error {
            io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
        }

        /// A process's token user (its `TOKEN_USER` buffer; the SID points into it).
        fn token_user(process: HANDLE) -> io::Result<Vec<u8>> {
            unsafe {
                let mut token = HANDLE::default();
                OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(os)?;
                let mut len = 0u32;
                let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
                let mut buf = vec![0u8; len.max(64) as usize];
                let r = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), buf.len() as u32, &mut len);
                let _ = CloseHandle(token);
                r.map_err(os)?;
                Ok(buf)
            }
        }

        fn sid_of(buf: &[u8]) -> ::windows::Win32::Security::PSID {
            unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid }
        }

        /// SDDL granting full access to the current user and SYSTEM, nobody else.
        fn sddl() -> io::Result<String> {
            let me = token_user(unsafe { GetCurrentProcess() })?;
            unsafe {
                let mut text = PWSTR::null();
                ConvertSidToStringSidW(sid_of(&me), &mut text).map_err(os)?;
                let sid = text.to_string().unwrap_or_default();
                let _ = LocalFree(HLOCAL(text.0.cast()));
                Ok(format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"))
            }
        }

        /// Creates a pipe instance with the owner-only DACL.
        pub fn create(opts: &mut ServerOptions, name: &str) -> io::Result<NamedPipeServer> {
            let text: Vec<u16> = sddl()?.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                let mut sd = PSECURITY_DESCRIPTOR::default();
                ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(text.as_ptr()), SDDL_REVISION_1, &mut sd, None)
                    .map_err(os)?;
                let mut attrs = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: sd.0,
                    bInheritHandle: false.into(),
                };
                let r = opts.create_with_security_attributes_raw(name, &mut attrs as *mut _ as *mut std::ffi::c_void);
                let _ = LocalFree(HLOCAL(sd.0));
                r
            }
        }

        /// Refuses a pipe whose server process does not run as this user.
        pub fn check_server(pipe: &NamedPipeClient) -> io::Result<()> {
            let denied = |why: &str| io::Error::new(io::ErrorKind::PermissionDenied, format!("not our daemon: {why}"));
            unsafe {
                let mut pid = 0u32;
                GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid).map_err(os)?;
                let process =
                    OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(|_| denied("cannot open its process"))?;
                let theirs = token_user(process);
                let _ = CloseHandle(process);
                let theirs = theirs.map_err(|_| denied("cannot read its user"))?;
                let mine = token_user(GetCurrentProcess())?;
                if EqualSid(sid_of(&mine), sid_of(&theirs)).is_ok() {
                    Ok(())
                } else {
                    Err(denied("it runs as another user"))
                }
            }
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
