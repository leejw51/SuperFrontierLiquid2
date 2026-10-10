#!/usr/bin/env python3
"""PTY-driven regression test for the `cb-client login` interactive flow.

Why Python and not cargo:
    Cargo integration tests run against an injected `Prompts` trait object
    (`ScriptedPrompts`), which sidesteps the real-tty code path in
    `prompts::StdPrompts::read_password`. The bug we're guarding against
    lives entirely in that real-tty path: a custom Unix `read_password_masked_unix`
    that opens `/dev/tty`, sets raw mode, and emits its prompt to a fd that —
    under some configurations — never appears on the master side of the PTY.
    To catch this we have to drive the actual binary through a real PTY, and
    Python's stdlib `pty.fork()` is the simplest portable way to do so.

How to run:
    python3 crates/cb-client/tests/login_pty.py

Requirements:
    A freshly-built `target/release/cb-client` at the repo root. Build with
    `cargo build --release -p cb-client` (or `make build-client`) before
    invoking this test. The test does NOT spawn cargo; it execs the binary
    directly so a stale build will produce stale results.

Behaviour:
    Forks a PTY, execs `cb-client` with a temp HOME (so the user's
    `~/.cb_client/` is untouched), drives the REPL through `login` with a
    fixed label, mnemonic, and default index. Asserts that each of
    `Label:`, `Mnemonic (hidden):`, and `Wallet index [0]:` appears on the
    master fd before the corresponding input is sent, and that the success
    line `logged in as <label>` follows. Exits 0 on pass, 1 on failure.
"""
import os
import pty
import select
import shutil
import signal
import sys
import tempfile
import time

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
BIN = os.path.join(REPO_ROOT, "target", "release", "cb-client")

# Per-assertion timeout (seconds). The whole test is bounded at ~3x this when
# every prompt is healthy; if a prompt is missing we'll fail at the first
# missing one rather than waste 3 budgets.
DEFAULT_TIMEOUT = 5.0
LABEL = "ptylabel"
MNEMONIC = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"


def log(msg: str) -> None:
    sys.stdout.write("[login-pty] " + msg + "\n")
    sys.stdout.flush()


def wait_for(master_fd: int, needle: bytes, accum: bytearray, timeout: float) -> None:
    """Read from master_fd until `needle` appears in `accum`, or `timeout`
    seconds elapse. On timeout, raise AssertionError with a snapshot of the
    bytes accumulated so far (helpful in CI logs)."""
    deadline = time.time() + timeout
    while needle not in accum:
        remaining = deadline - time.time()
        if remaining <= 0:
            shown = bytes(accum).decode("utf-8", errors="replace")
            raise AssertionError(
                "timed out after {:.1f}s waiting for {!r}; got {} bytes:\n{}".format(
                    timeout, needle.decode("utf-8", errors="replace"), len(accum), shown
                )
            )
        r, _, _ = select.select([master_fd], [], [], min(0.2, remaining))
        if not r:
            continue
        try:
            chunk = os.read(master_fd, 4096)
        except OSError as e:
            raise AssertionError("read error on master fd: {}".format(e))
        if not chunk:
            # EOF — child died unexpectedly
            shown = bytes(accum).decode("utf-8", errors="replace")
            raise AssertionError(
                "child closed PTY before {!r} appeared; got:\n{}".format(
                    needle.decode("utf-8", errors="replace"), shown
                )
            )
        accum.extend(chunk)


def write_line(master_fd: int, s: str) -> None:
    os.write(master_fd, (s + "\n").encode("utf-8"))


def run() -> int:
    if not os.path.isfile(BIN) or not os.access(BIN, os.X_OK):
        log("FAIL: binary not found or not executable at {}".format(BIN))
        log("      build it first: cargo build --release -p cb-client")
        return 1

    home = tempfile.mkdtemp(prefix="cb-pty-home-")
    pid, master_fd = pty.fork()
    if pid == 0:
        # Child: scrub HOME so we don't touch the user's wallet store.
        os.environ["HOME"] = home
        os.environ["TERM"] = "xterm"
        # Belt-and-suspenders: ensure the binary doesn't pick up an external
        # session.json by clearing common config-related vars.
        for k in ("XDG_CONFIG_HOME", "XDG_DATA_HOME"):
            os.environ.pop(k, None)
        try:
            os.execvp(BIN, [BIN])
        except OSError as e:
            sys.stderr.write("execvp failed: {}\n".format(e))
        os._exit(127)

    accum = bytearray()
    failed = False
    try:
        # 1. Banner.
        wait_for(master_fd, b"ul> ", accum, DEFAULT_TIMEOUT)
        log("ok: saw `ul> ` banner")

        # 2. Trigger login flow.
        write_line(master_fd, "login")

        # 3. Label prompt MUST appear before we type the label.
        wait_for(master_fd, b"Label:", accum, DEFAULT_TIMEOUT)
        log("ok: saw `Label:` prompt")
        write_line(master_fd, LABEL)

        # 4. Mnemonic prompt MUST appear before we type the mnemonic.
        #    THIS is the assertion that catches the broken-mnemonic-prompt bug:
        #    in the broken path, `read_password_masked_unix` writes the prompt
        #    to a /dev/tty fd that never surfaces on the master side, so the
        #    user (and this test) never see `Mnemonic (hidden):`.
        wait_for(master_fd, b"Mnemonic (hidden):", accum, DEFAULT_TIMEOUT)
        log("ok: saw `Mnemonic (hidden):` prompt (the regression check)")
        write_line(master_fd, MNEMONIC)

        # 5. Index prompt with default 0.
        wait_for(master_fd, b"Wallet index [0]:", accum, DEFAULT_TIMEOUT)
        log("ok: saw `Wallet index [0]:` prompt")
        write_line(master_fd, "")  # accept default

        # 6. Success line.
        wait_for(
            master_fd,
            "logged in as {}".format(LABEL).encode("utf-8"),
            accum,
            DEFAULT_TIMEOUT,
        )
        log("ok: saw `logged in as {}` confirmation".format(LABEL))

        # Best-effort clean exit.
        try:
            write_line(master_fd, "quit")
        except OSError:
            pass

    except AssertionError as e:
        failed = True
        log("FAIL: {}".format(e))
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            os.close(master_fd)
        except OSError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        shutil.rmtree(home, ignore_errors=True)

    if failed:
        return 1
    log("PASS: all login prompts surfaced on the master PTY")
    return 0


if __name__ == "__main__":
    sys.exit(run())
