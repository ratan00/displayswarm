#!/usr/bin/env python3
"""
Test script to verify DisplaySwarm's protocol framing, packet serialization,
and network streaming without needing full build chains.
"""

import socket
import struct
import time
import sys

MAGIC_VIDEO = 0x564D5649 # "VMVI"
MAGIC_INPUT = 0x564D494E # "VMIN"
MAGIC_CONTROL = 0x564D4354 # "VMCT"

def run_test_server(port=9999):
    print(f"[*] Starting test DisplaySwarm Host on 0.0.0.0:{port}...")
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("0.0.0.0", port))
    server.listen(1)
    print(f"[+] Waiting for Android client connection on port {port}...")

    conn, addr = server.accept()
    print(f"[+] Android client connected from {addr}!")

    # 1. Read ClientHello
    hello_data = conn.recv(12)
    if len(hello_data) == 12:
        magic, ptype, ver, width, height, refresh, codecs = struct.unpack(">IBBHHBB", hello_data)
        if magic == MAGIC_CONTROL and ptype == 1:
            print(f"[+] Handshake successful: Display={width}x{height} @ {refresh}Hz (codecs=0x{codecs:02x})")
            # Send ServerHello acknowledgment
            server_hello = struct.pack(">IBBHHBB", MAGIC_CONTROL, 2, 1, width, height, 60, 0)
            conn.sendall(server_hello)
            print(f"[+] ServerHello sent: {width}x{height} @ 60fps")

    # 2. Stream test frames
    frame_index = 0
    start_time = time.time()
    try:
        while True:
            now_us = int(time.time() * 1_000_000)
            
            # Dummy NAL payload
            dummy_nal = b"\x00\x00\x00\x01\x65" + (b"\xAA" * 128)
            frame_type = 2 if frame_index % 60 == 0 else 3
            
            # Header: magic(4), type(1), index(4), timestamp_us(8), length(4)
            header = struct.pack(">IBIQI", MAGIC_VIDEO, frame_type, frame_index, now_us, len(dummy_nal))
            conn.sendall(header + dummy_nal)
            
            frame_index += 1
            time.sleep(1.0 / 60.0) # 60 FPS
            
            if frame_index % 120 == 0:
                elapsed = time.time() - start_time
                fps = frame_index / elapsed
                print(f"[Stream] Sent {frame_index} frames (~{fps:.1f} FPS)")
    except (KeyboardInterrupt, BrokenPipeError):
        print("\n[*] Streaming terminated.")
    finally:
        conn.close()
        server.close()

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9999
    run_test_server(port)
