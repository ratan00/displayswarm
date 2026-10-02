use super::*;
use crate::protocol::{FRAME_TYPE_KEY, KEY_FLAG_SHIFT};

/// Shared with `client-android/app/src/test/.../WireTest.kt`.
const VECTORS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../protocol/v2-vectors.txt"));

/// The golden messages. `WireTest.kt` builds the same list under the same
/// names; `protocol/v2-vectors.txt` holds the bytes both must produce.
fn samples() -> Vec<(&'static str, Vec<u8>, Message)> {
    let one = |name, m: Message| (name, m.encode(), m);
    let video = VideoFrame {
        codec: CODEC_H264,
        frame_type: FRAME_TYPE_KEY,
        frame_index: 9,
        capture_us: 123_456,
        data: (0u8..20).collect(),
    };
    vec![
        one("hello", Message::Hello(Hello {
            width: 1600,
            height: 720,
            refresh_mhz: 60_000,
            density_dpi: 320,
            codecs: CODEC_H264 | CODEC_HEVC,
            features: FEATURE_TOUCH | FEATURE_STYLUS | FEATURE_KEYBOARD,
            max_touch_points: 10,
            device_id: "a1b2".into(),
            device_name: "Galaxy M06".into(),
            app_version: "2.0.0".into(),
        })),
        one("hello_ack", Message::HelloAck(HelloAck {
            status: HELLO_OK,
            width: 1600,
            height: 720,
            fps: 60,
            codec: CODEC_H264,
            features: FEATURE_TOUCH | FEATURE_STYLUS | FEATURE_KEYBOARD,
            role: ROLE_MIRROR,
            host_name: "laptop".into(),
            message: String::new(),
        })),
        one("hello_ack_mismatch", Message::HelloAck(HelloAck {
            status: HELLO_VERSION_MISMATCH,
            host_name: "laptop".into(),
            message: "Update the DisplaySwarm app".into(),
            ..Default::default()
        })),
        one("ping", Message::Ping { id: 0x0102_0304, t_send_us: 0x0A0B_0C0D_0E0F_1011 }),
        one("pong", Message::Pong { id: 7, t_ping_us: 1, t_recv_us: 2, t_reply_us: 3 }),
        one("heartbeat", Message::Heartbeat),
        one("bye", Message::Bye { reason: BYE_SERVER_STOPPING, text: "stopped".into() }),
        one("stats", Message::Stats(Stats {
            frames_decoded: 1,
            frames_dropped: 2,
            decode_latency_us: 3,
            present_latency_us: 4,
            rx_kbps: 5,
        })),
        one("host_state", Message::HostState { state: HOST_STATE_STREAMING, detail: "ok".into() }),
        one("keyframe_request", Message::KeyframeRequest),
        one("set_bitrate", Message::SetBitrate { kbps: 8000 }),
        one("resize", Message::Resize { width: 1600, height: 720, density_dpi: 320, rotation: 1 }),
        one("set_role", Message::SetRole { role: ROLE_EXTEND }),
        one("audio_latency", Message::AudioLatency { latency_us: 45_000 }),
        one("audio_sync", Message::AudioSync { delay_us: 120_000 }),
        one("service_state", Message::ServiceState { enabled: SERVICE_AUDIO_OUT | SERVICE_MIC }),
        one("video_frame", Message::VideoFrame(VideoFrame {
            data: vec![0, 0, 0, 1, 0x65],
            ..video.clone()
        })),
        (
            "video_fragmented_16",
            Message::VideoFrame(video.clone()).encode_fragmented(16),
            Message::VideoFrame(video),
        ),
        one("audio_frame", Message::AudioFrame(AudioPacket { seq: 3, pts_us: 1000, channels: 2, data: vec![1, 2, 3] })),
        one("mic_frame", Message::MicFrame(AudioPacket { seq: 4, pts_us: 2000, channels: 1, data: vec![9] })),
        one("touch", Message::Touch(Touch {
            action: ACTION_DOWN,
            action_id: 1,
            points: vec![
                TouchPoint { id: 0, x: 0.25, y: 0.5, pressure: 1.0 },
                TouchPoint { id: 1, x: 0.75, y: 0.5, pressure: 0.5 },
            ],
        })),
        one("pen", Message::Pen(Pen {
            action: ACTION_MOVE,
            tool: PEN_TOOL_PEN,
            buttons: PEN_BUTTON_PRIMARY,
            samples: vec![
                PenSample { age_us: 4000, x: 0.5, y: 0.5, pressure: 0.25, tilt_x: 0.0, tilt_y: 0.0 },
                PenSample { age_us: 0, x: 0.5, y: 0.75, pressure: 0.5, tilt_x: 0.25, tilt_y: -0.25 },
            ],
        })),
        one("mouse", Message::Mouse(Mouse {
            action: ACTION_DOWN,
            buttons: MOUSE_BUTTON_PRIMARY,
            relative: false,
            x: 0.5,
            y: 0.25,
        })),
        one("scroll", Message::Scroll(Scroll { phase: PHASE_NONE, x: 0.5, y: 0.5, dx: 0.0, dy: -1.0 })),
        one("pinch", Message::Pinch(Pinch { phase: PHASE_BEGIN, cx: 0.5, cy: 0.5, scale: 1.0, rotation: 0.0 })),
        one("key", Message::Key(Key { action: ACTION_DOWN, key_code: 29, scan_code: 30, meta: KEY_FLAG_SHIFT, text: "A".into() })),
        one("text", Message::Text("héllo".into())),
        one("clipboard", Message::Clipboard { mime: "text/plain".into(), data: b"hi".to_vec() }),
        one("file_offer", Message::FileOffer(FileOffer { id: 1, size: 1024, name: "a.txt".into(), mime: "text/plain".into() })),
        one("file_chunk", Message::FileChunk { id: 1, offset: 0, data: vec![1, 2] }),
        one("file_control", Message::FileControl { id: 1, op: FILE_ACCEPT }),
        one("battery_status", Message::BatteryStatus(BatteryStatus {
            percent: 17,
            charging: false,
            thermal: THERMAL_SEVERE,
            temp_decidegrees: 412,
        })),
        one("pair_request", Message::PairRequest {
            mode: PAIR_MODE_PIN,
            device_id: "a1b2".into(),
            device_name: "Galaxy M06".into(),
            credential: vec![1, 2, 3, 4],
        }),
        one("pair_response", Message::PairResponse { status: PAIR_OK, message: "ok".into(), token: vec![9, 8, 7] }),
    ]
}

/// `name: hex` lines; `#` starts a comment.
fn parse_vectors() -> Vec<(String, Vec<u8>)> {
    VECTORS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (name, hex) = l.split_once(':').expect("vector line is `name: hex`");
            let digits: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
            let bytes = (0..digits.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).expect("hex byte"))
                .collect();
            (name.trim().to_string(), bytes)
        })
        .collect()
}

/// Decodes a byte string holding exactly one (possibly fragmented) message.
fn decode_all(mut bytes: &[u8]) -> Message {
    let mut re = Reassembler::default();
    loop {
        let header = Header::parse(bytes[..HEADER_SIZE].try_into().unwrap()).unwrap();
        header.check().unwrap();
        let end = HEADER_SIZE + header.len as usize;
        let payload = bytes[HEADER_SIZE..end].to_vec();
        bytes = &bytes[end..];
        if let Some(full) = re.push(&header, payload).unwrap() {
            assert!(bytes.is_empty(), "trailing bytes after the message");
            return Message::decode(header.channel, header.msg_type, &full).unwrap();
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

#[test]
fn encoders_match_golden_vectors() {
    let vectors = parse_vectors();
    let samples = samples();
    assert_eq!(
        vectors.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        samples.iter().map(|(n, _, _)| *n).collect::<Vec<_>>(),
        "vector file and sample list must name the same messages in the same order"
    );
    for ((name, expected), (_, encoded, message)) in vectors.iter().zip(&samples) {
        assert_eq!(hex(encoded), hex(expected), "{name}: encoding differs from the golden bytes");
        assert_eq!(&decode_all(expected), message, "{name}: golden bytes decode differently");
    }
}

/// Regenerates the vector file body: `cargo test print_golden_vectors -- --ignored --nocapture`.
#[test]
#[ignore]
fn print_golden_vectors() {
    for (name, bytes, _) in samples() {
        println!("{name}: {}", hex(&bytes));
    }
}

#[test]
fn every_known_type_has_one_channel_and_roundtrips_through_decode() {
    for (name, _, m) in samples() {
        assert_eq!(channel_of(m.msg_type()), Some(m.channel()), "{name}");
    }
}

#[test]
fn header_layout_is_frozen() {
    let h = Header { version: VERSION, channel: CH_VIDEO, msg_type: MSG_VIDEO_FRAME, flags: FLAG_MORE, len: 0x0102_0304 };
    assert_eq!(h.encode(), [0x56, 0x4D, 2, 3, 0x20, 1, 1, 2, 3, 4]);
    assert_eq!(Header::parse(&h.encode()).unwrap(), h);
}

#[test]
fn header_check_rejects_bad_frames() {
    let ok = Header { version: VERSION, channel: CH_CONTROL, msg_type: MSG_PING, flags: 0, len: 12 };
    assert!(ok.check().is_ok());
    assert!(Header { version: 3, ..ok }.check().is_err());
    assert!(Header { channel: 6, ..ok }.check().is_err());
    assert!(Header { len: MAX_FRAGMENT as u32 + 1, ..ok }.check().is_err());
    assert!(Header { flags: 0x02, ..ok }.check().is_err());
    assert!(Header::parse(b"XM\x02\x00\x03\x00\x00\x00\x00\x00").is_err());
}

/// A v1 packet parses as a "VM" frame whose version byte is a letter, which is
/// how the host tells a v1 app apart without a separate scan.
#[test]
fn v1_packets_parse_with_a_non_numeric_version() {
    for v1 in [b"VMCT\x01\x01\x06\x40", b"VMVI\x02\x00\x00\x00", b"VMMS\x04\x00\x00\x00"] {
        let mut raw = [0u8; HEADER_SIZE];
        raw[..8].copy_from_slice(v1);
        let h = Header::parse(&raw).unwrap();
        assert_ne!(h.version, VERSION);
        assert!(h.version >= b'A');
    }
}

#[test]
fn fragments_of_other_channels_interleave() {
    let video = Message::VideoFrame(VideoFrame {
        codec: CODEC_H264,
        frame_type: FRAME_TYPE_KEY,
        frame_index: 1,
        capture_us: 5,
        data: vec![0xAB; 100],
    });
    let frames = video.encode_fragmented(32);
    // Split the video frames apart and splice a Ping between each.
    let mut stream = Vec::new();
    let mut rest = &frames[..];
    let ping = Message::Ping { id: 1, t_send_us: 2 }.encode();
    while !rest.is_empty() {
        let len = u32::from_be_bytes(rest[6..10].try_into().unwrap()) as usize;
        stream.extend_from_slice(&rest[..HEADER_SIZE + len]);
        stream.extend_from_slice(&ping);
        rest = &rest[HEADER_SIZE + len..];
    }

    let mut re = Reassembler::default();
    let mut got = Vec::new();
    let mut s = &stream[..];
    while !s.is_empty() {
        let h = Header::parse(s[..HEADER_SIZE].try_into().unwrap()).unwrap();
        let end = HEADER_SIZE + h.len as usize;
        if let Some(full) = re.push(&h, s[HEADER_SIZE..end].to_vec()).unwrap() {
            got.push(Message::decode(h.channel, h.msg_type, &full).unwrap());
        }
        s = &s[end..];
    }
    let pings = got.iter().filter(|m| matches!(m, Message::Ping { .. })).count();
    assert_eq!(pings, 4, "14 + 100 payload bytes in 32-byte fragments is 4 frames, one ping after each");
    assert_eq!(got.iter().filter(|m| **m == video).count(), 1);
}

#[test]
fn reassembler_rejects_a_type_change_mid_message() {
    let mut re = Reassembler::default();
    let first = Header { version: VERSION, channel: CH_VIDEO, msg_type: MSG_VIDEO_FRAME, flags: FLAG_MORE, len: 1 };
    assert_eq!(re.push(&first, vec![1]).unwrap(), None);
    let bogus = Header { msg_type: 0x7F, flags: 0, ..first };
    assert!(re.push(&bogus, vec![2]).is_err());
}

#[test]
fn reassembler_bounds_each_channel() {
    let mut re = Reassembler::default();
    let h = Header { version: VERSION, channel: CH_INPUT, msg_type: MSG_TEXT, flags: FLAG_MORE, len: 0 };
    assert_eq!(re.push(&h, vec![0; MAX_FRAGMENT]).unwrap(), None);
    assert!(re.push(&h, vec![0; 1]).is_err(), "input messages are capped at one fragment's worth");
}

#[test]
fn unknown_types_decode_as_unknown() {
    let m = Message::decode(CH_CONTROL, 0x1F, &[1, 2, 3]).unwrap();
    assert_eq!(m, Message::Unknown { channel: CH_CONTROL, msg_type: 0x1F, payload: vec![1, 2, 3] });
}

#[test]
fn known_type_on_the_wrong_channel_is_an_error() {
    assert!(Message::decode(CH_VIDEO, MSG_PING, &[0; 12]).is_err());
}

#[test]
fn short_payload_is_an_error_and_trailing_bytes_are_ignored() {
    assert!(Message::decode(CH_CONTROL, MSG_PING, &[0; 11]).is_err());
    let mut p = vec![0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 9];
    p.extend_from_slice(b"appended by a later revision");
    assert_eq!(Message::decode(CH_CONTROL, MSG_PING, &p).unwrap(), Message::Ping { id: 7, t_send_us: 9 });
}

#[test]
fn long_text_is_truncated_on_a_char_boundary() {
    let m = Message::Text("é".repeat(40_000));
    let bytes = m.encode_fragmented(MAX_FRAGMENT);
    match decode_all(&bytes) {
        Message::Text(t) => {
            assert!(t.len() <= u16::MAX as usize);
            assert!(t.chars().all(|c| c == 'é'));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn empty_payload_is_one_empty_frame() {
    assert_eq!(Message::Heartbeat.encode(), [0x56, 0x4D, 2, 0, MSG_HEARTBEAT, 0, 0, 0, 0, 0]);
}

#[test]
fn legacy_rejection_is_a_v1_server_hello_then_a_v1_bye() {
    let bytes = legacy_v1_rejection("update");
    assert_eq!(&bytes[..12], b"VMCT\x02\x01\x00\x10\x00\x10\x00\x00");
    assert_eq!(&bytes[12..20], b"VMMS\x05\x00\x00\x07");
    assert_eq!(&bytes[20..], b"\x02update");
}

#[tokio::test]
async fn message_reader_reads_a_fragmented_stream_and_stops_on_garbage() {
    use tokio::io::AsyncWriteExt;
    let (mut tx, rx) = tokio::io::duplex(1 << 16);
    let key = Message::Key(Key { action: ACTION_DOWN, key_code: 29, scan_code: 0, meta: 0, text: "a".into() });
    let clip = Message::Clipboard { mime: "text/plain".into(), data: vec![b'x'; 5000] };
    let mut stream = key.encode();
    stream.extend(clip.encode_fragmented(1000));
    stream.extend_from_slice(b"garbage!!!");
    tx.write_all(&stream).await.unwrap();
    drop(tx);

    let mut r = MessageReader::new(rx);
    assert_eq!(r.next().await.unwrap(), key);
    assert_eq!(r.next().await.unwrap(), clip);
    assert_eq!(r.next().await.unwrap_err().kind(), io::ErrorKind::InvalidData);
}
