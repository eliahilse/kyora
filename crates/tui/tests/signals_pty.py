"""Verify the surviving TUI caller regains default Unix signal behavior."""

import fcntl
import os
import select
import signal
import struct
import subprocess
import sys
import termios
import time


def check_signal(test_binary, exit_signal):
    master, slave = os.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
    output = bytearray()
    process = None

    def read_until(predicate):
        deadline = time.monotonic() + 10
        while not predicate():
            assert time.monotonic() < deadline, "timed out waiting for terminal output"
            if select.select([master], [], [], 0.05)[0]:
                output.extend(os.read(master, 65536))
            assert process.poll() is None, "caller exited before run() returned"

    try:
        process = subprocess.Popen(
            [test_binary, "--exact", "signals_after_tui_returns", "--nocapture"],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=dict(os.environ, TERM="xterm-256color", NO_COLOR="1"),
            start_new_session=True,
        )
        for session in (1, 2):
            read_until(lambda: output.count(b"Offline prototype.") >= session)
            # The signal must exit each TUI session cleanly, including re-entry.
            os.kill(process.pid, exit_signal)
            marker = f"TUI session {session} returned".encode()
            read_until(lambda: marker in output)

        assert termios.tcgetattr(slave) == before, "terminal attributes were not restored"
        assert output.count(b"\x1b[?1049l") == 2, "expected both alternate screen leaves"
        # run() has returned but the same Rust process is still alive.
        os.kill(process.pid, exit_signal)
        try:
            status = process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            raise AssertionError(f"caller ignored {signal.Signals(exit_signal).name}")
        assert status == -exit_signal, f"expected default signal termination, got {status}"
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)


for exit_signal in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
    check_signal(sys.argv[1], exit_signal)
