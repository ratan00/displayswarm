#!/usr/bin/env python3
"""
Test utility to verify /dev/uinput access and virtual tablet creation on Linux.
"""

import os
import sys

def check_uinput():
    uinput_path = "/dev/uinput"
    print(f"[*] Checking {uinput_path} permissions...")

    if not os.path.exists(uinput_path):
        print(f"[!] {uinput_path} does not exist. Try: sudo modprobe uinput")
        return False

    readable = os.access(uinput_path, os.R_OK)
    writable = os.access(uinput_path, os.W_OK)

    if readable and writable:
        print("[+] SUCCESS: /dev/uinput is readable and writable by current user!")
        print("[+] DisplaySwarm can create virtual tablet & touchscreen devices natively without root.")
        return True
    else:
        print(f"[!] PERMISSION DENIED: Read={readable}, Write={writable}")
        print("\nTo grant permanent permissions for DisplaySwarm without running as root:")
        print("  1. Add a udev rule:")
        print('     echo \'KERNEL=="uinput", SUBSYSTEM=="misc", MODE="0660", GROUP="input"\' | sudo tee /etc/udev/rules.d/99-uinput.rules')
        print(f"  2. Add your user to the 'input' group:")
        print(f"     sudo usermod -aG input {os.getenv('USER', 'your_username')}")
        print("  3. Or for quick testing:")
        print("     sudo chmod 666 /dev/uinput\n")
        return False

if __name__ == "__main__":
    check_uinput()
