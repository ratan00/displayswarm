//! Async client of the daemon's IPC.
//!
//! [`IpcClient::request`] can be called from any task; replies are matched to
//! requests by id, events arrive on the receiver that [`IpcClient::connect`]
//! returns.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::proto::{decode_line, encode_line, Event, Request, RequestEnvelope, Response, ServerMessage, MAX_LINE};
use super::transport::IpcTransport;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Response, String>>>>>;

/// How long a request may take (the diagnostics probe hardware).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct IpcClient {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
}

impl IpcClient {
    /// Connects and starts the reader/writer tasks (on the current runtime).
    pub async fn connect(transport: &dyn IpcTransport) -> io::Result<(IpcClient, mpsc::UnboundedReceiver<Event>)> {
        let stream = transport.connect().await?;
        let (read, mut write) = tokio::io::split(stream);
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(async move {
            while let Some(mut line) = out_rx.recv().await {
                line.push('\n');
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let pending_reader = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read);
            let mut buf = String::new();
            loop {
                buf.clear();
                match lines.read_line(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) if n > MAX_LINE => break,
                    Ok(_) => {}
                }
                match decode_line::<ServerMessage>(&buf) {
                    Ok(ServerMessage::Reply { id, ok, error, response }) => {
                        let result = if ok {
                            Ok(response.unwrap_or(Response::Ok))
                        } else {
                            Err(error.unwrap_or_else(|| "request failed".into()))
                        };
                        let tx = pending_reader.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                        if let Some(tx) = tx {
                            let _ = tx.send(result);
                        }
                    }
                    Ok(ServerMessage::Event(ev)) => {
                        let _ = event_tx.send(ev);
                    }
                    Err(e) => log::warn!("Ignoring an undecodable line from the daemon: {e}"),
                }
            }
            // Connection over: fail every waiting request (their senders drop)
            // and close the event stream (event_tx drops).
            pending_reader.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });

        Ok((IpcClient { out, pending, next_id: AtomicU64::new(1) }, event_rx))
    }

    /// Sends a request and waits for its reply.
    pub async fn request(&self, request: Request) -> Result<Response, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        let line = encode_line(&RequestEnvelope { id, request });
        if self.out.send(line).is_err() {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err("connection to the daemon lost".into());
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("connection to the daemon lost".into()),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                Err("the daemon did not answer in time".into())
            }
        }
    }

    /// Whether the connection is gone.
    pub fn is_closed(&self) -> bool {
        self.out.is_closed()
    }
}
