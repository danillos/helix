#!/usr/bin/env python3
"""Exercise the Rex bridge in a temporary Helix terminal without changing user files."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", required=True, type=Path)
binary = str(parser.parse_args().binary.resolve())
session = "session:rex-prototype"
block = "block:rex-prototype"
workspace = Path(tempfile.mkdtemp(prefix="helix-rex-prototype-"))
source = workspace / "same file.txt"
source.write_text("original text\n")
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
environment = os.environ.copy()
environment.update(TERM="xterm-256color", REX_SESSION=session, REX_BLOCK=block)
process = subprocess.Popen(
    [binary, str(source), "--log", str(workspace / "editor.log")],
    cwd=workspace,
    env=environment,
    stdin=slave,
    stdout=slave,
    stderr=slave,
    start_new_session=True,
)
os.close(slave)
cache = Path(environment.get("XDG_CACHE_HOME", str(Path.home() / ".cache")))
endpoint = cache / "helix/rex" / f"{process.pid}.sock"
transcript = bytearray()


def read_terminal():
    while process.poll() is None:
        if not select.select([master], [], [], 0.02)[0]:
            continue
        try:
            chunk = os.read(master, 65536)
        except OSError:
            return
        if not chunk:
            return
        transcript.extend(chunk)


reader = threading.Thread(target=read_terminal, daemon=True)
reader.start()


def keys(text):
    os.write(master, text.encode())
    time.sleep(0.16)


def command(text):
    keys(":" + text + "\r")


def navigate(direction):
    start = time.monotonic()
    response = json.loads(
        subprocess.check_output(
            [binary, "--rex-navigate", str(process.pid), direction, session, block],
            text=True,
            timeout=3,
        )
    )
    response["elapsed_ms"] = round((time.monotonic() - start) * 1000, 2)
    print(direction, response, flush=True)
    return response


def connect():
    connection = socket.socket(socket.AF_UNIX)
    connection.settimeout(2)
    connection.connect(str(endpoint))
    stream = connection.makefile("rwb", buffering=0)
    hello = json.loads(stream.readline())
    return connection, stream, hello


def send(stream, value):
    stream.write(json.dumps(value).encode() + b"\n")


try:
    for _ in range(50):
        if endpoint.exists():
            break
        time.sleep(0.02)
    assert endpoint.exists(), transcript[-1000:]
    command("vsplit")
    left = navigate("left")
    assert left["outcome"] == "moved" and left["before"] != left["after"]
    right = navigate("right")
    assert right["outcome"] == "moved" and right["after"] == left["before"]
    assert navigate("right")["outcome"] == "edge"
    command("hsplit")
    assert navigate("up")["outcome"] == "moved"
    assert navigate("down")["outcome"] == "moved"
    assert navigate("down")["outcome"] == "edge"
    for pending in [":echo pending", " ", "f"]:
        keys(pending)
        assert navigate("left")["outcome"] == "blocked"
        keys("\x1b")

    # A lost reply must preserve the committed result for the same request ID.
    connection, stream, hello = connect()
    request_id = "lost-reply-fixture"
    send(stream, {
        "operation": "navigate", "instance": hello["instance"], "id": request_id,
        "direction": "left", "deadline_ms": int(time.time() * 1000) + 150,
    })
    time.sleep(0.07)
    stream.close()
    connection.close()
    connection, stream, hello = connect()
    send(stream, {"operation": "cancel", "instance": hello["instance"], "id": request_id})
    recovered = json.loads(stream.readline())
    stream.close()
    connection.close()
    assert recovered["outcome"] == "moved", recovered
    assert navigate("right")["outcome"] == "moved"
    assert navigate("right")["outcome"] == "edge"

    # Delay a request past its deadline after the capability handshake.
    connection, stream, hello = connect()
    os.kill(process.pid, signal.SIGSTOP)
    send(stream, {
        "operation": "navigate", "instance": hello["instance"], "id": "expired-fixture",
        "direction": "left", "deadline_ms": int(time.time() * 1000) + 150,
    })
    time.sleep(0.25)
    os.kill(process.pid, signal.SIGCONT)
    expired = json.loads(stream.readline())
    stream.close()
    connection.close()
    assert expired["outcome"] == "not_applied", expired
    assert navigate("right")["outcome"] == "edge"
    keys("i")
    assert navigate("left")["outcome"] == "moved"
    keys("X")
    keys("\x1b")
    keys("\x13")
    assert "X" in source.read_text(), source.read_text()
    log = (workspace / "editor.log").read_text()
    assert "ERROR" not in log, log
    command("quit-all!")
    process.wait(timeout=3)
    assert process.returncode == 0
    assert not endpoint.exists(), "Normal editor exit must remove its endpoint"
    print("PASS: directions, same-file/nested views, prompts/prefixes, lost reply, expiry, Insert mode, cleanup.")
finally:
    if process.poll() is None:
        os.kill(process.pid, signal.SIGCONT)
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    reader.join(timeout=1)
    os.close(master)
    (workspace / "transcript.vt").write_bytes(transcript)
    print("Evidence:", workspace, flush=True)
