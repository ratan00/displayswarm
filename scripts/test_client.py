#!/usr/bin/env python3
"""
Simulated Android client to test the DisplaySwarm Host streaming pipeline,
framerate, and stylus/touch event transmission.
"""

import socket
import struct
import time
import sys
import threading

MAGIC_VIDEO = 0x564D5649 # "VMVI"
MAGIC_INPUT = 0x564D494E # "VMIN"
MAGIC_CONTROL = 0x564D4354 # "VMCT"

def run_test_client(host="127.0.0.1", port=9999):
    print(f"[*] Connecting to DisplaySwarm Host at {host}:{port}...")
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    print("[+] Connected to Host!")

    # 1. Send ClientHello: width=2560, height=1600, refresh=60
    hello = struct.pack(">IBBHHBB", MAGIC_CONTROL, 1, 1, 2560, 1600, 60, 1)
    sock.sendall(hello)
    print("[+] Sent ClientHello (2560x1600 @ 60Hz)")

    # 1b. Read ServerHello acknowledgment (12 bytes)
    server_hello = sock.recv(12)
    if len(server_hello) == 12:
        magic, ptype, status, width, height, fps, reserved = struct.unpack(">IBBHHBB", server_hello)
        if magic == MAGIC_CONTROL and ptype == 2:
            print(f"[+] ServerHello received: {width}x{height} @ {fps}fps, status={status}")
        else:
            print(f"[!] Unexpected ServerHello magic: {hex(magic)}, type: {ptype}")
    else:
        print(f"[!] Short ServerHello response: {len(server_hello)} bytes")

    # 2. Input sender thread (simulating touch and stylus)
    stop_event = threading.Event()
    def input_loop():
        pointer_id = 0
        while not stop_event.is_set():
            # Send simulated stylus move with pressure
            now = time.time()
            x = (now % 2.0) / 2.0
            y = 0.5
            pressure = 0.75
            tilt_x = 0.1
            tilt_y = 0.2
            
            # Input packet: magic(4), type(1), ptr(1), action(1), flags(1), x(4), y(4), pressure(4), tilt_x(4), tilt_y(4)
            pkt = struct.pack(">IBBBBfffff", MAGIC_INPUT, 2, pointer_id, 2, 0, x, y, pressure, tilt_x, tilt_y)
            try:
                sock.sendall(pkt)
            except Exception:
                break
            time.sleep(0.016) # ~60Hz input rate

    sender_th = threading.Thread(target=input_loop)
    sender_th.daemon = True
    sender_th.start()

    # 3. Receive video stream
    frames = 0
    start_time = time.time()
    try:
        while True:
            # Read 21-byte video header
            hdr = sock.recv(21)
            if not hdr or len(hdr) < 21:
                break
            magic, ftype, findex, timestamp_us, length = struct.unpack(">IBIQI", hdr)
            if magic != MAGIC_VIDEO:
                print(f"[!] Invalid magic: {hex(magic)}")
                break

            # Read payload
            payload = sock.recv(length)
            while len(payload) < length:
                chunk = sock.recv(length - len(payload))
                if not chunk:
                    break
                payload += chunk

            frames += 1
            now_us = int(time.time() * 1_000_000)
            latency_ms = (now_us - timestamp_us) / 1000.0

            if frames % 60 == 0:
                fps = frames / (time.time() - start_time)
                print(f"[Client] Received frame #{findex} | Type: {ftype} | Size: {length} bytes | Latency: {latency_ms:.2f}ms | FPS: {fps:.1f}")

            if frames >= 180:
                print(f"[+] Successfully verified 180 frames! Pipeline working as expected.")
                break
    except KeyboardInterrupt:
        pass
    finally:
        stop_event.set()
        sock.close()

if __name__ == "__main__":
    host = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 9999
    run_test_client(host, port)
