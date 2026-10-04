"""Exercise terminal restoration on a real Unix PTY without third-party packages."""

import fcntl
import os
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time


def check_exit(binary, delivered_signal=None, active=False):
    master, slave = os.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", NO_COLOR="1")
    command = [binary, "tui"] + (["--demo"] if active else [])
    process = None
    output = bytearray()

    def confirmation_visible(start=0):
        # Ratatui skips existing spaces with cursor moves between words.
        text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(output[start:]))
        return b"y/Enter:quit" in text.replace(b" ", b"")

    def read_until(predicate):
        deadline = time.monotonic() + 10
        while not predicate():
            assert time.monotonic() < deadline, "timed out waiting for terminal output"
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                output.extend(os.read(master, 65536))
            assert process.poll() is None or predicate(), "TUI exited unexpectedly"

    try:
        process = subprocess.Popen(
            command,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            start_new_session=True,
        )
        notice = b"Scripted run in progress." if active else b"Offline prototype."
        read_until(lambda: notice in output)
        raw = termios.tcgetattr(slave)
        assert not raw[3] & (termios.ICANON | termios.ECHO), "TUI did not enter raw mode"
        assert b"\x1b[?1049h" in output, "TUI did not enter alternate screen"
        assert output.count(b"\x1b[>1u") == 1, "expected one keyboard enhancement push"

        if delivered_signal is not None:
            os.kill(process.pid, delivered_signal)
        else:
            os.write(master, b"\x03")
            if active:
                read_until(confirmation_visible)
                assert process.poll() is None, "Ctrl-C skipped quit confirmation"
                os.write(master, b"n")
                # A second confirmation proves that declining kept the TUI alive.
                confirmation_end = len(output)
                os.write(master, b"\x03")
                read_until(lambda: confirmation_visible(confirmation_end))
                os.write(master, b"y")

        deadline = time.monotonic() + 10
        while process.poll() is None:
            assert time.monotonic() < deadline, "TUI did not exit"
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                output.extend(os.read(master, 65536))
        while select.select([master], [], [], 0.05)[0]:
            output.extend(os.read(master, 65536))
        assert process.returncode == 0, f"TUI exit status: {process.returncode}"
        assert termios.tcgetattr(slave) == before, "terminal attributes were not restored"
        assert output.count(b"\x1b[<1u") == 1, "expected exactly one keyboard enhancement pop"
        assert output.count(b"\x1b[?1049l") == 1, "expected exactly one alternate screen leave"
        assert b"\x1b[?2004l" in output, "bracketed paste was not disabled"
        assert b"\x1b[?25h" in output, "cursor was not shown"
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)


for exit_signal in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
    for demo_active in (False, True):
        check_exit(sys.argv[1], delivered_signal=exit_signal, active=demo_active)
check_exit(sys.argv[1])
check_exit(sys.argv[1], active=True)
