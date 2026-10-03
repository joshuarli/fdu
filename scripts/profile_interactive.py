#!/usr/bin/env python3

import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time


def main() -> int:
    binary = os.environ.get("FDU_BIN", "target/release/fdu")
    root = sys.argv[1] if len(sys.argv) > 1 else "."
    if len(sys.argv) > 2:
        raise SystemExit("usage: profile_interactive.py [PATH]")

    pid, master = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        os.environ["FDU_PROFILE"] = "1"
        os.environ["TERM"] = "xterm-256color"
        os.execv(binary, [binary, "--interactive", "--read-only", root])

    output = bytearray()
    keys = [b"j", b"a", b"n", b"j"]
    keys_sent = 0
    next_key_at = 0.0
    quit_sent = False
    deadline = time.monotonic() + 120
    status = None
    try:
        os.set_blocking(master, False)
        while time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.05)
            if readable:
                try:
                    data = os.read(master, 8192)
                    if data:
                        output.extend(data)
                except OSError:
                    pass

            now = time.monotonic()
            ready = b"Ready" in output
            scanning = b"Scanning" in output
            if ready and not quit_sent:
                os.write(master, b"q")
                quit_sent = True
            elif scanning and keys_sent < len(keys) and now >= next_key_at:
                os.write(master, keys[keys_sent])
                keys_sent += 1
                next_key_at = now + 0.08

            waited, wait_status = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                status = wait_status
                break
        if status is None:
            os.kill(pid, signal.SIGTERM)
            _, status = os.waitpid(pid, 0)
            raise TimeoutError("interactive profiler did not exit after its input sequence")
    finally:
        os.close(master)

    profile_lines = re.findall(rb"(?:fdu-profile|fdu-allocations) [^\r\n]*", output)
    for line in profile_lines:
        print(line.decode("utf-8", errors="replace"))
    print(f"fdu-pty-profile keys_sent={keys_sent} quit_sent={int(quit_sent)}")
    if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
        return os.waitstatus_to_exitcode(status)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
