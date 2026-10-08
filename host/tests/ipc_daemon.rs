//! Drives a real daemon (IPC endpoint on a temp socket) with the IPC client.

use std::sync::Arc;
use std::time::Duration;

use displayswarm_host::daemon::{self, logging::LogBuffer, DaemonOptions};
use displayswarm_host::devices::{DeviceRecord, DeviceStore};
use displayswarm_host::ipc::*;
use displayswarm_host::server::{Backends, SessionManager};

fn temp_socket(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("displayswarm-ipc-{tag}-{}", std::process::id())).join("hostd.sock")
}

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>, want: impl Fn(&Event) -> bool) -> Event {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let e = rx.recv().await.expect("event stream closed");
            if want(&e) {
                return e;
            }
        }
    })
    .await
    .expect("timed out waiting for an event")
}

fn options(transport: Arc<dyn IpcTransport>, manager: Arc<SessionManager>, logs: LogBuffer) -> DaemonOptions {
    DaemonOptions { transport, tray: false, start_server: Some(false), manager: Some(manager), logs }
}

#[tokio::test]
async fn client_drives_the_daemon() {
    let path = temp_socket("drive");
    let mut store = DeviceStore::in_memory();
    store.put(DeviceRecord::new("dev1", "Test phone")).unwrap();
    let manager = SessionManager::new(store, Backends::default());
    let logs = LogBuffer::new();
    logs.push("a log line".into());
    let transport = transport_at(&path);
    let daemon = daemon::start(options(transport.clone(), manager, logs)).await.expect("daemon starts");

    // A second daemon on the same socket is refused.
    let empty = SessionManager::new(DeviceStore::in_memory(), Backends::default());
    let second = daemon::start(options(transport.clone(), empty, LogBuffer::new())).await;
    assert!(second.is_err(), "a second daemon must not take over the socket");

    let (client, mut events) = IpcClient::connect(&*transport).await.expect("connect");
    assert_eq!(client.request(Request::Ping).await.unwrap(), Response::Pong { protocol: PROTOCOL_VERSION });

    let Response::State { state } = client.request(Request::Subscribe).await.unwrap() else { panic!("state") };
    assert_eq!(state.devices.len(), 1);
    assert_eq!(state.devices[0].role, None);
    assert!(!state.server.running);

    client.request(Request::SetRole { device_id: "dev1".into(), role: "tablet".into() }).await.unwrap();
    let ev = next_event(&mut events, |e| matches!(e, Event::DevicesChanged { devices } if devices[0].role.is_some())).await;
    let Event::DevicesChanged { devices } = ev else { unreachable!() };
    assert_eq!(devices[0].role.as_deref(), Some("tablet"));

    // Errors come back as errors, and the connection survives them.
    assert!(client.request(Request::SetRole { device_id: "ghost".into(), role: "tablet".into() }).await.is_err());
    client.request(Request::Identify { device_id: "dev1".into() }).await.unwrap();
    next_event(&mut events, |e| matches!(e, Event::Identify { .. })).await;

    let Response::Logs { lines } = client.request(Request::GetLogs { max: 5 }).await.unwrap() else { panic!("logs") };
    assert!(lines.contains(&"a log line".to_string()));

    client.request(Request::ForgetDevice { device_id: "dev1".into() }).await.unwrap();
    let Response::State { state } = client.request(Request::GetState).await.unwrap() else { panic!("state") };
    assert!(state.devices.is_empty());

    // Quit reaches the daemon's owner.
    client.request(Request::Quit).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), daemon.wait_quit()).await.expect("quit signalled");
    daemon.shutdown().await;
    assert!(!path.exists(), "the socket file is removed on shutdown");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn garbage_lines_do_not_kill_the_connection() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let path = temp_socket("garbage");
    let transport = transport_at(&path);
    let manager = SessionManager::new(DeviceStore::in_memory(), Backends::default());
    let daemon = daemon::start(options(transport.clone(), manager, LogBuffer::new())).await.unwrap();

    let stream = transport.connect().await.unwrap();
    let (r, mut w) = tokio::io::split(stream);
    let mut r = BufReader::new(r);
    w.write_all(b"this is not json\n{\"id\":3,\"cmd\":\"ping\"}\n").await.unwrap();
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    let ServerMessage::Reply { ok, .. } = decode_line(&line).unwrap() else { panic!("reply") };
    assert!(!ok);
    line.clear();
    r.read_line(&mut line).await.unwrap();
    assert_eq!(
        decode_line::<ServerMessage>(&line).unwrap(),
        ServerMessage::reply(3, Ok(Response::Pong { protocol: PROTOCOL_VERSION }))
    );
    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
