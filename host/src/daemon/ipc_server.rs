//! The daemon's IPC endpoint: accepts connections and serves the protocol of
//! `ipc::proto` on top of a [`DaemonCore`].

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::core::DaemonCore;
use crate::ipc::proto::*;
use crate::ipc::transport::{BoxedStream, IpcTransport};

/// Binds the transport and serves clients until the returned task is aborted.
/// Fails when another daemon already listens.
pub async fn serve(core: Arc<DaemonCore>, transport: &dyn IpcTransport) -> io::Result<JoinHandle<()>> {
    let mut listener = transport.bind().await?;
    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    tokio::spawn(serve_connection(core.clone(), stream));
                }
                Err(e) => {
                    log::error!("IPC accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }))
}

/// One client: reads request lines, answers each (concurrently, so a slow
/// diagnostics run does not hold up a role change), and, after `subscribe`,
/// forwards events.
pub async fn serve_connection(core: Arc<DaemonCore>, stream: BoxedStream) {
    let (read, mut write) = tokio::io::split(stream);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut forwarder: Option<JoinHandle<()>> = None;
    let mut reader = BufReader::new(read);
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) if n > MAX_LINE => {
                log::warn!("IPC client sent an oversized line; disconnecting it");
                break;
            }
            Ok(_) => {}
        }
        if buf.trim().is_empty() {
            continue;
        }
        let env = match decode_line::<RequestEnvelope>(&buf) {
            Ok(e) => e,
            Err(e) => {
                // Without a valid id there is nobody to answer; id 0 is reserved.
                let _ = out_tx.send(encode_line(&ServerMessage::reply(0, Err(format!("bad request: {e}")))));
                continue;
            }
        };
        let RequestEnvelope { id, request } = env;
        if matches!(request, Request::Subscribe) && forwarder.is_none() {
            // Subscribe before taking the snapshot that answers the request.
            let rx = core.subscribe();
            forwarder = Some(tokio::spawn(forward_events(core.clone(), rx, out_tx.clone())));
        }
        let core = core.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let result = core.handle(request).await;
            let _ = out_tx.send(encode_line(&ServerMessage::reply(id, result)));
        });
    }
    if let Some(f) = forwarder {
        f.abort();
    }
    // Let queued replies drain, then close.
    drop(out_tx);
    let _ = writer.await;
}

async fn forward_events(core: Arc<DaemonCore>, mut rx: broadcast::Receiver<Event>, out: mpsc::UnboundedSender<String>) {
    loop {
        let event = match rx.recv().await {
            Ok(e) => e,
            // Missed events: resynchronise with a fresh device list.
            Err(broadcast::error::RecvError::Lagged(_)) => Event::DevicesChanged { devices: core.devices() },
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if out.send(encode_line(&ServerMessage::Event(event))).is_err() {
            break;
        }
    }
}
