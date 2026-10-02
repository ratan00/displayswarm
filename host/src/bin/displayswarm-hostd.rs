//! `displayswarm-hostd`: the DisplaySwarm background daemon.
//!
//! Runs the server, the USB watcher and every session, serves the UI over a
//! Unix socket (`$XDG_RUNTIME_DIR/displayswarm/hostd.sock`) and owns the tray icon.
//! Normally started by the systemd user unit or on demand by the UI.
//!
//! Options: `--socket PATH`, `--no-tray`, `--no-server` (do not start the
//! server until asked), `--help`.

use displayswarm_host::daemon::{self, logging, DaemonOptions};
use displayswarm_host::ipc::{default_socket_path, transport_at};

fn main() {
    let mut socket = default_socket_path();
    let mut tray = true;
    let mut start_server = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => match args.next() {
                Some(p) => socket = p.into(),
                None => return eprintln!("--socket needs a path"),
            },
            "--no-tray" => tray = false,
            "--no-server" => start_server = Some(false),
            "-h" | "--help" => {
                return println!("usage: displayswarm-hostd [--socket PATH] [--no-tray] [--no-server]");
            }
            other => return eprintln!("unknown argument {other:?} (try --help)"),
        }
    }

    let logs = logging::LogBuffer::new();
    logging::init(&logs);

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => return eprintln!("cannot start the async runtime: {e}"),
    };
    let code = runtime.block_on(async move {
        let transport = transport_at(socket);
        let daemon = match daemon::start(DaemonOptions { transport, tray, start_server, manager: None, logs }).await {
            Ok(d) => d,
            Err(e) => {
                log::error!("{e}");
                eprintln!("displayswarm-hostd: {e}");
                return 1;
            }
        };
        #[cfg(unix)]
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        // Windows has no SIGTERM; Ctrl-C (and the console close event) arrive as `ctrl_c`.
        #[cfg(not(unix))]
        let mut term: Option<tokio::signal::windows::CtrlClose> = tokio::signal::windows::ctrl_close().ok();
        tokio::select! {
            _ = daemon.wait_quit() => log::info!("Quit requested"),
            _ = tokio::signal::ctrl_c() => log::info!("Interrupted"),
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {
                log::info!("Terminated")
            }
        }
        daemon.shutdown().await;
        0
    });
    // Background blocking tasks (the encoder probe) must not delay the exit.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    std::process::exit(code);
}
