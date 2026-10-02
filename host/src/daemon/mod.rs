//! The background daemon (`displayswarm-hostd`): the server, the USB watcher and
//! every session, an IPC endpoint for the UI and a tray icon.
//!
//! [`start`] brings all of it up on the current tokio runtime and returns a
//! [`Daemon`]; the `displayswarm-hostd` binary and the UI's `--standalone` mode
//! both use it.

pub mod autostart;
pub mod core;
pub mod diagnostics;
pub mod ipc_server;
pub mod logging;
pub mod wifi;

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use self::core::DaemonCore;
use crate::ipc::transport::IpcTransport;
use crate::server::SessionManager;

pub struct DaemonOptions {
    pub transport: Arc<dyn IpcTransport>,
    /// Show the tray icon / notifications (Linux only).
    pub tray: bool,
    /// Start the server right away; `None` follows the saved setting.
    pub start_server: Option<bool>,
    /// Device manager; `None` opens the real store and backends.
    pub manager: Option<Arc<SessionManager>>,
    pub logs: logging::LogBuffer,
}

pub struct Daemon {
    pub core: Arc<DaemonCore>,
    transport: Arc<dyn IpcTransport>,
    ipc_task: JoinHandle<()>,
    tray_task: Option<JoinHandle<()>>,
}

/// Binds the IPC endpoint (failing if another daemon owns it), starts the
/// server per the settings and the tray.
pub async fn start(opts: DaemonOptions) -> Result<Daemon, String> {
    let manager = opts.manager.unwrap_or_else(SessionManager::with_defaults);
    let core = DaemonCore::new(manager, core::default_runner(), core::default_settings_path(), opts.logs);
    let ipc_task = ipc_server::serve(core.clone(), &*opts.transport)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", opts.transport.describe()))?;
    log::info!("IPC listening on {}", opts.transport.describe());
    core.start_background();
    if opts.start_server.unwrap_or_else(|| core.settings().autostart_server) {
        core.start_server();
    }
    #[cfg(target_os = "linux")]
    let tray_task = opts.tray.then(|| tokio::spawn(crate::tray::run(core.clone())));
    #[cfg(not(target_os = "linux"))]
    let tray_task = None;
    Ok(Daemon { core, transport: opts.transport, ipc_task, tray_task })
}

impl Daemon {
    /// Resolves when someone asked the daemon to quit (IPC `quit`, tray).
    pub async fn wait_quit(&self) {
        let mut rx = self.core.quit_signal();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Stops the server (waiting for the screencast to be released), then the
    /// IPC endpoint and the tray.
    pub async fn shutdown(self) {
        self.core.request_quit();
        self.core.stop_server();
        if !self.core.wait_stopped(Duration::from_secs(10)).await {
            log::warn!("The server did not stop within 10 s; exiting anyway");
        }
        self.ipc_task.abort();
        self.transport.cleanup();
        if let Some(t) = self.tray_task {
            if tokio::time::timeout(Duration::from_secs(3), t).await.is_err() {
                log::warn!("The tray did not shut down in time");
            }
        }
    }
}
