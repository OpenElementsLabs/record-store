#!/usr/bin/env python3
"""SEC-SLOW-CLIENTS: a client that never finishes its request cannot hold a connection.

docs/reference/configuration.md, `server.header_read_timeout_seconds`: a
connection whose request headers have not arrived within the timeout, or that
sits idle that long between requests, is closed -- on the S3 listener and on
the management listener. Before this, a client that trickled one header line a
second was kept for as long as it liked (release/findings/RSG-004, closed).

Against the real binary, with a short configured timeout:

  * 32 connections per listener that trickle a header line every 250 ms, and
    16 per listener that send nothing at all, are every one closed by the
    server within the timeout plus a fixed slack, while still trickling;
  * a kept-alive connection that goes idle after a request is closed the same way;
  * a well-behaved client is served throughout, and afterwards the process
    holds no more descriptors than before the attack.

Then once with no setting, so the documented default (30 s) is what ships.
"""

from __future__ import annotations

import socket
import threading
import time
import urllib.request

from gatelib import Gate, Secrets, Server, artifact_from_arguments, identify_artifact

TIMEOUT_SECONDS = 3
DEFAULT_TIMEOUT_SECONDS = 30
# Scheduling, the timer wheel and a loaded runner: generous next to the timeout,
# tiny next to "forever".
SLACK_SECONDS = 2.0
TRICKLERS = 32
SILENT = 16


def held_until_closed(port: int, opening: bytes, trickle: bool, give_up: float) -> float | None:
    """Seconds the server kept the connection, or None if it was still open at `give_up`."""
    connection = socket.create_connection(("127.0.0.1", port), timeout=5)
    started = time.monotonic()
    try:
        connection.sendall(opening)
        connection.settimeout(0.25)
        line = 0
        while time.monotonic() - started < give_up:
            try:
                if connection.recv(4096) == b"":
                    return time.monotonic() - started
            except socket.timeout:
                pass
            except OSError:
                return time.monotonic() - started
            if trickle:
                try:
                    connection.sendall(f"X-Slow-{line}: 1\r\n".encode())
                    line += 1
                except OSError:
                    return time.monotonic() - started
        return None
    finally:
        connection.close()


def idle_after_request(port: int, give_up: float) -> float | None:
    """Seconds an idle kept-alive connection was kept after its first response."""
    connection = socket.create_connection(("127.0.0.1", port), timeout=5)
    try:
        connection.sendall(b"GET /health HTTP/1.1\r\nHost: gate\r\n\r\n")
        response = b""
        while b"\r\n\r\n" not in response:
            response += connection.recv(4096)
        started = time.monotonic()
        connection.settimeout(0.25)
        while time.monotonic() - started < give_up:
            try:
                if connection.recv(4096) == b"":
                    return time.monotonic() - started
            except socket.timeout:
                pass
            except OSError:
                return time.monotonic() - started
        return None
    finally:
        connection.close()


def attack(port: int, limit: float) -> list[float | None]:
    give_up = limit * 4 + 10
    results: list[float | None] = []
    lock = threading.Lock()

    def one(opening: bytes, trickle: bool) -> None:
        held = held_until_closed(port, opening, trickle, give_up)
        with lock:
            results.append(held)

    threads = [threading.Thread(target=one, args=(b"GET / HTTP/1.1\r\nHost: gate\r\n", True)) for _ in range(TRICKLERS)]
    threads += [threading.Thread(target=one, args=(b"", False)) for _ in range(SILENT)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return results


def main(gate: Gate) -> None:
    artifact, _ = artifact_from_arguments()
    identify_artifact(gate, artifact)
    server = Server(artifact, gate.work_directory / "short", gate.evidence_directory / "logs",
                    credentials=Secrets(), encrypted=False, name="short",
                    extra_environment={"RECORD_STORE_HEADER_READ_TIMEOUT_SECONDS": str(TIMEOUT_SECONDS)})
    server.start()
    limit = TIMEOUT_SECONDS + SLACK_SECONDS
    before = server.sample()["open_files"]

    served = {"ok": 0, "failed": 0}
    stop = threading.Event()

    def well_behaved() -> None:
        while not stop.is_set():
            try:
                with urllib.request.urlopen(f"{server.api_endpoint}/health", timeout=5) as response:
                    served["ok" if response.status == 200 else "failed"] += 1
            except OSError:
                served["failed"] += 1
            time.sleep(0.2)

    watcher = threading.Thread(target=well_behaved)
    watcher.start()
    for listener, port in (("S3", server.s3_port), ("management", server.api_port)):
        held = attack(port, limit)
        closed = [value for value in held if value is not None]
        worst = max(closed) if closed else float("inf")
        gate.check(f"{listener}: every trickling or silent connection is closed by the server",
                   len(closed) == len(held), {"still_open": len(held) - len(closed), "of": len(held)})
        gate.metric(f"{listener.lower()}_slow_header_connection_hold_seconds_max", worst, "s", limit=limit,
                    basis=f"header_read_timeout_seconds={TIMEOUT_SECONDS} plus {SLACK_SECONDS} s slack")
        idle = idle_after_request(port, limit * 4 + 10)
        gate.metric(f"{listener.lower()}_idle_keep_alive_hold_seconds", idle if idle is not None else float("inf"), "s",
                    limit=limit, basis="an idle kept-alive connection waits for its next request head under the same timeout")
    stop.set()
    watcher.join()
    gate.check("a well-behaved client was served throughout", served["ok"] > 0 and served["failed"] == 0, served)
    time.sleep(1)
    after = server.sample()["open_files"]
    gate.check("no descriptor outlives the attack", after <= before + 2, {"before": before, "after": after})
    server.stop()

    # The default, as shipped: a silent connection is closed at 30 s, not held.
    default = Server(artifact, gate.work_directory / "default", gate.evidence_directory / "logs",
                     credentials=Secrets(), encrypted=False, name="default")
    default.start()
    held = held_until_closed(default.s3_port, b"GET / HTTP/1.1\r\nHost: gate\r\n", True,
                             DEFAULT_TIMEOUT_SECONDS + 20)
    gate.metric("default_slow_header_connection_hold_seconds", held if held is not None else float("inf"), "s",
                limit=DEFAULT_TIMEOUT_SECONDS + SLACK_SECONDS,
                basis="documented default header_read_timeout_seconds = 30")
    gate.check("with the default, the hold is the configured timeout rather than a much earlier cut",
               held is not None and held >= DEFAULT_TIMEOUT_SECONDS - SLACK_SECONDS, held)
    default.stop()


if __name__ == "__main__":
    Gate("SEC-SLOW-CLIENTS", "a client that never finishes its request cannot hold a connection").run(main)
