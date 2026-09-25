"""Shared harness for release gates that exercise a real Record Store binary.

A gate here is a script that starts the binary it was given, drives it the way a
client or operator would, and states what it found as checks and metrics. It
never adopts a server it did not start, never touches a deployment it did not
create, and never uses a credential that outlives the run: every key below is
generated for the process and thrown away with its temporary directory.

Outcomes are exit codes so that the recorder (`record.py`) can tell a product
failure from a broken runner without parsing logs:

    0   every check passed
    1   a check failed: the product did something it must not
    65  the measurement is not valid (too noisy, too short, wrong environment)
    75  the gate could not run: the environment, not the product, is at fault

A gate that cannot decide is never reported as a pass.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import random
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

EXIT_PASS = 0
EXIT_FAIL = 1
EXIT_INVALID = 65
EXIT_INFRA = 75

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class GateFailure(Exception):
    """The product violated a guarantee. Stops the gate immediately."""


class InfrastructureError(Exception):
    """The gate could not run. Not evidence about the product either way."""


class InvalidMeasurement(Exception):
    """The run completed but its numbers cannot be trusted."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def workspace_version() -> str:
    import tomllib

    with open(REPOSITORY_ROOT / "Cargo.toml", "rb") as handle:
        return tomllib.load(handle)["workspace"]["package"]["version"]


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def environment_fingerprint() -> dict[str, Any]:
    """What a measurement depends on, recorded next to it."""
    info: dict[str, Any] = {
        "os": platform.system(),
        "os_release": platform.release(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "runner": os.environ.get("RUNNER_NAME") or os.environ.get("HOSTNAME") or platform.node(),
        "ci": bool(os.environ.get("CI")),
    }
    try:
        import psutil

        info["memory_bytes"] = psutil.virtual_memory().total
    except Exception:  # noqa: BLE001 - fingerprint is best-effort
        pass
    if platform.system() == "Darwin":
        info["cpu_model"] = _command_output(["sysctl", "-n", "machdep.cpu.brand_string"])
    elif Path("/proc/cpuinfo").exists():
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                info["cpu_model"] = line.split(":", 1)[1].strip()
                break
    return info


def filesystem_of(path: Path) -> str:
    try:
        import psutil

        best = None
        resolved = str(path.resolve())
        for partition in psutil.disk_partitions(all=True):
            if resolved.startswith(partition.mountpoint) and (
                best is None or len(partition.mountpoint) > len(best.mountpoint)
            ):
                best = partition
        return f"{best.fstype} at {best.mountpoint}" if best else "unknown"
    except Exception:  # noqa: BLE001
        return "unknown"


def _command_output(command: list[str]) -> str:
    try:
        return subprocess.run(command, capture_output=True, text=True, timeout=10).stdout.strip()
    except Exception:  # noqa: BLE001
        return ""


@dataclass
class Artifact:
    """The binaries under test, identified by digest rather than by path."""

    directory: Path

    @property
    def server(self) -> Path:
        return self.directory / "record-store-server"

    @property
    def cli(self) -> Path:
        return self.directory / "record-store"

    def describe(self) -> dict[str, Any]:
        for binary in (self.server, self.cli):
            if not binary.is_file():
                raise InfrastructureError(f"missing binary {binary}")
        reported = _command_output([str(self.cli), "--version"])
        return {
            "directory": str(self.directory),
            "server_sha256": sha256_file(self.server),
            "cli_sha256": sha256_file(self.cli),
            "reported_version": reported,
        }


@dataclass
class Secrets:
    """Credentials generated for one run. Never read from the environment."""

    root_access_key: str = field(default_factory=lambda: "gate-" + secrets.token_hex(8))
    root_secret_key: str = field(default_factory=lambda: secrets.token_hex(24))
    master_key: str = field(default_factory=lambda: secrets.token_hex(32))
    system_token: str = field(default_factory=lambda: secrets.token_hex(32))

    def values(self) -> list[str]:
        return [self.root_secret_key, self.master_key, self.system_token]


# Every server a gate starts, so that however the gate ends -- a failed check
# raising mid-scenario included -- none is left running afterwards.
_STARTED: list["Server"] = []


def reap_servers() -> None:
    for server in _STARTED:
        server.kill()
    _STARTED.clear()


class Server:
    """One Record Store process that this gate started and owns."""

    def __init__(
        self,
        artifact: Artifact,
        data_directory: Path,
        log_directory: Path,
        *,
        credentials: Secrets,
        encrypted: bool,
        name: str = "server",
        extra_environment: dict[str, str] | None = None,
        expected_version: str | None = None,
    ) -> None:
        self.artifact = artifact
        self.data_directory = data_directory
        self.log_directory = log_directory
        self.credentials = credentials
        self.encrypted = encrypted
        self.name = name
        self.extra_environment = extra_environment or {}
        self.expected_version = expected_version
        self.process: subprocess.Popen[bytes] | None = None
        self.s3_port = free_port()
        self.api_port = free_port()
        self.rpc_port = free_port()
        self.starts = 0
        self._log_handle = None

    @property
    def s3_endpoint(self) -> str:
        return f"http://127.0.0.1:{self.s3_port}"

    @property
    def api_endpoint(self) -> str:
        return f"http://127.0.0.1:{self.api_port}"

    @property
    def log_path(self) -> Path:
        return self.log_directory / f"{self.name}.log"

    def environment(self) -> dict[str, str]:
        # A clean environment, so nothing from the developer's shell or the CI
        # runner (a real key, a production token) can leak into the process.
        environment = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.log_directory),
            "RECORD_STORE_MODE": "standalone",
            "RECORD_STORE_ROOT_ACCESS_KEY": self.credentials.root_access_key,
            "RECORD_STORE_ROOT_SECRET_KEY": self.credentials.root_secret_key,
            "RECORD_STORE_CREDENTIAL_MASTER_KEY": self.credentials.master_key,
            "RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN": self.credentials.system_token,
            "RECORD_STORE_STORAGE_DATA_DIRECTORY": str(self.data_directory),
            "RECORD_STORE_STORAGE_ENCRYPTION_ENABLED": "true" if self.encrypted else "false",
            "RECORD_STORE_S3_BIND": f"127.0.0.1:{self.s3_port}",
            "RECORD_STORE_API_BIND": f"127.0.0.1:{self.api_port}",
            "RECORD_STORE_RPC_BIND": f"127.0.0.1:{self.rpc_port}",
            "RECORD_STORE_LOG_JSON": "true",
        }
        environment.update(self.extra_environment)
        return environment

    def start(self, *, ready_timeout: float = 60.0) -> float:
        """Starts the process and returns seconds from spawn to ready."""
        if self.process is not None and self.process.poll() is None:
            raise InfrastructureError(f"{self.name} is already running")
        self.log_directory.mkdir(parents=True, exist_ok=True)
        # The server creates its data directory but, deliberately, not missing
        # parents above it.
        self.data_directory.parent.mkdir(parents=True, exist_ok=True)
        self._log_handle = open(self.log_path, "ab")
        self._log_handle.write(f"\n--- start {self.starts + 1} ---\n".encode())
        self._log_handle.flush()
        started = time.monotonic()
        self.process = subprocess.Popen(
            [str(self.artifact.server)],
            env=self.environment(),
            stdout=self._log_handle,
            stderr=subprocess.STDOUT,
            cwd=self.log_directory,
        )
        self.starts += 1
        if self not in _STARTED:
            _STARTED.append(self)
        self.wait_ready(timeout=ready_timeout)
        elapsed = time.monotonic() - started
        self.confirm_identity()
        return elapsed

    def start_expecting_refusal(self, timeout: float = 30.0) -> tuple[int | None, str]:
        """Starts a process that must refuse to serve; returns its exit code and log."""
        self.log_directory.mkdir(parents=True, exist_ok=True)
        marker = self.log_path.stat().st_size if self.log_path.exists() else 0
        with open(self.log_path, "ab") as handle:
            process = subprocess.Popen(
                [str(self.artifact.server)],
                env=self.environment(),
                stdout=handle,
                stderr=subprocess.STDOUT,
                cwd=self.log_directory,
            )
            try:
                code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                # It started serving instead of refusing: stop it before judging.
                process.kill()
                process.wait()
                code = None
        text = self.log_path.read_bytes()[marker:].decode(errors="replace")
        return code, text

    def wait_ready(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.process is None or self.process.poll() is not None:
                raise GateFailure(
                    f"{self.name} exited with {self.process.returncode if self.process else '?'} "
                    f"before becoming ready; see {self.log_path}"
                )
            try:
                with urllib.request.urlopen(f"{self.api_endpoint}/ready", timeout=2) as response:
                    if response.status == 200:
                        return
            except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
                pass
            time.sleep(0.05)
        raise GateFailure(f"{self.name} did not become ready within {timeout}s")

    def confirm_identity(self) -> None:
        """The answering server must be the process just started, in standalone mode."""
        info = self.api("GET", "/api/v1/system/info")
        if info.get("name") != "record-store" or info.get("mode") != "standalone":
            raise InfrastructureError(f"unexpected backend on {self.api_endpoint}: {info}")
        if self.expected_version and info.get("version") != self.expected_version:
            raise GateFailure(
                f"{self.name} reports version {info.get('version')}, expected {self.expected_version}"
            )

    def stop(self, timeout: float = 60.0) -> float:
        """SIGTERM and wait. Returns the shutdown duration in seconds."""
        if self.process is None or self.process.poll() is not None:
            return 0.0
        started = time.monotonic()
        self.process.send_signal(signal.SIGTERM)
        try:
            code = self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            self.process.kill()
            self.process.wait()
            raise GateFailure(f"{self.name} ignored SIGTERM for {timeout}s") from error
        elapsed = time.monotonic() - started
        self._close_log()
        if code != 0:
            raise GateFailure(f"{self.name} exited with {code} after SIGTERM; see {self.log_path}")
        return elapsed

    def kill(self) -> None:
        """SIGKILL: no drain, no destructors, no flush. The crash under test."""
        if self.process is not None and self.process.poll() is None:
            self.process.send_signal(signal.SIGKILL)
            self.process.wait()
        self._close_log()

    def _close_log(self) -> None:
        if self._log_handle is not None:
            self._log_handle.close()
            self._log_handle = None

    def sample(self) -> dict[str, float]:
        import psutil

        if self.process is None or self.process.poll() is not None:
            raise GateFailure(f"{self.name} is not running")
        process = psutil.Process(self.process.pid)
        with process.oneshot():
            times = process.cpu_times()
            sample = {
                "rss_bytes": float(process.memory_info().rss),
                "cpu_seconds": float(times.user + times.system),
                "threads": float(process.num_threads()),
            }
            try:
                sample["open_files"] = float(process.num_fds())
            except (AttributeError, psutil.AccessDenied):
                sample["open_files"] = float("nan")
            try:
                sample["connections"] = float(len(process.net_connections(kind="tcp")))
            except (psutil.AccessDenied, AttributeError):
                sample["connections"] = float("nan")
        return sample

    def api(self, method: str, path: str, body: Any = None, *, expect: tuple[int, ...] = (200, 201, 204)) -> Any:
        status, payload = self.api_raw(method, path, body)
        if status not in expect:
            raise GateFailure(f"{method} {path} answered {status}: {payload!r}")
        return payload

    def api_raw(self, method: str, path: str, body: Any = None, token: str | None = None) -> tuple[int, Any]:
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(f"{self.api_endpoint}{path}", data=data, method=method)
        request.add_header("authorization", f"Bearer {token or self.credentials.system_token}")
        if data is not None:
            request.add_header("content-type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                raw = response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            raw = error.read()
            status = error.code
        try:
            return status, json.loads(raw) if raw else None
        except json.JSONDecodeError:
            return status, raw.decode(errors="replace")

    def s3(self, access_key: str | None = None, secret_key: str | None = None, **config: Any):
        import boto3
        from botocore.config import Config

        return boto3.client(
            "s3",
            endpoint_url=self.s3_endpoint,
            region_name="us-east-1",
            aws_access_key_id=access_key or self.credentials.root_access_key,
            aws_secret_access_key=secret_key or self.credentials.root_secret_key,
            config=Config(
                signature_version="s3v4",
                s3={"addressing_style": "path"},
                request_checksum_calculation="when_required",
                response_checksum_validation="when_required",
                retries={"max_attempts": 1, "mode": "standard"},
                connect_timeout=10,
                read_timeout=120,
                **config,
            ),
        )

    def cli(self, *arguments: str, extra_environment: dict[str, str] | None = None, timeout: float = 600) -> subprocess.CompletedProcess[str]:
        """Runs the `record-store` CLI with this deployment's configuration."""
        environment = self.environment()
        environment["RECORD_STORE_MANAGEMENT_TOKEN"] = self.credentials.system_token
        environment.update(extra_environment or {})
        return subprocess.run(
            [str(self.artifact.cli), *arguments],
            env=environment,
            capture_output=True,
            text=True,
            timeout=timeout,
        )


class Gate:
    """Collects checks and metrics and turns them into an exit code and a record."""

    def __init__(self, gate_id: str, description: str) -> None:
        self.gate_id = gate_id
        self.description = description
        self.checks: list[dict[str, Any]] = []
        self.metrics: dict[str, dict[str, Any]] = {}
        self.notes: list[str] = []
        self.context: dict[str, Any] = {}
        evidence = os.environ.get("GATE_EVIDENCE_DIR")
        self.evidence_directory = Path(evidence) if evidence else Path(tempfile.mkdtemp(prefix=f"gate-{gate_id}-"))
        self.evidence_directory.mkdir(parents=True, exist_ok=True)
        self.work_directory = Path(tempfile.mkdtemp(prefix=f"rs-gate-{gate_id}-"))
        self.keep_work = os.environ.get("GATE_KEEP_WORK") == "1"

    def check(self, name: str, passed: bool, detail: Any = None) -> bool:
        self.checks.append({"name": name, "passed": bool(passed), "detail": detail})
        marker = "ok  " if passed else "FAIL"
        print(f"  [{marker}] {name}" + (f" -- {detail}" if detail not in (None, "") and not passed else ""), flush=True)
        return bool(passed)

    def require(self, name: str, passed: bool, detail: Any = None) -> None:
        """A check that makes the rest of the gate meaningless when it fails."""
        if not self.check(name, passed, detail):
            raise GateFailure(f"{name}: {detail}")

    def metric(
        self,
        name: str,
        value: float,
        unit: str,
        *,
        limit: float | None = None,
        comparison: str = "<=",
        enforcement: str = "blocking",
        provisional: bool = False,
        basis: str = "",
    ) -> None:
        """Records a measurement and, when a limit is given, checks it."""
        entry: dict[str, Any] = {"value": value, "unit": unit}
        if limit is not None:
            within = value <= limit if comparison == "<=" else value >= limit
            entry.update(
                limit=limit,
                comparison=comparison,
                enforcement=enforcement,
                provisional=provisional,
                basis=basis,
                within=within,
            )
            label = f"{name} = {value:.4g} {unit} {comparison} {limit:.4g}"
            if enforcement == "blocking":
                self.check(label, within, basis)
            else:
                print(f"  [{'ok  ' if within else 'WARN'}] {label} (advisory)", flush=True)
                if not within:
                    self.notes.append(f"advisory limit exceeded: {label}")
        self.metrics[name] = entry

    def note(self, text: str) -> None:
        self.notes.append(text)
        print(f"  note: {text}", flush=True)

    def run(self, body: Callable[["Gate"], None]) -> None:
        print(f"== {self.gate_id}: {self.description}", flush=True)
        started = time.time()
        outcome, reason = "pass", ""
        try:
            body(self)
            if any(not check["passed"] for check in self.checks):
                outcome = "fail"
                reason = "; ".join(c["name"] for c in self.checks if not c["passed"])[:2000]
            elif not self.checks:
                outcome, reason = "invalid_measurement", "the gate made no checks"
        except GateFailure as error:
            outcome, reason = "fail", str(error)
        except InvalidMeasurement as error:
            outcome, reason = "invalid_measurement", str(error)
        except InfrastructureError as error:
            outcome, reason = "infrastructure_error", str(error)
        except Exception as error:  # noqa: BLE001
            # An unexpected exception inside a gate is a defect in the gate or in
            # the product; either way it is not a pass. Classified as a failure
            # so that it cannot be retried away as infrastructure.
            outcome, reason = "fail", f"unhandled {type(error).__name__}: {error}"
            traceback.print_exc()
        reap_servers()
        detail = {
            "gate": self.gate_id,
            "outcome": outcome,
            "reason": reason,
            "checks": self.checks,
            "metrics": self.metrics,
            "notes": self.notes,
            "context": self.context,
            "environment": environment_fingerprint(),
            "duration_seconds": round(time.time() - started, 3),
        }
        (self.evidence_directory / "detail.json").write_text(json.dumps(detail, indent=2, default=str))
        destination = os.environ.get("GATE_DETAIL")
        if destination:
            Path(destination).write_text(json.dumps(detail, indent=2, default=str))
        if not self.keep_work:
            shutil.rmtree(self.work_directory, ignore_errors=True)
        print(f"== {self.gate_id}: {outcome.upper()}" + (f" ({reason})" if reason else ""), flush=True)
        sys.exit(
            {
                "pass": EXIT_PASS,
                "fail": EXIT_FAIL,
                "invalid_measurement": EXIT_INVALID,
                "infrastructure_error": EXIT_INFRA,
            }[outcome]
        )


def artifact_from_arguments(argv: list[str] | None = None) -> tuple[Artifact, dict[str, str]]:
    """`--bin-dir DIR` names the binaries under test. There is no default path.

    Defaulting to `target/release` is how a gate ends up testing whatever an
    earlier build left there. The caller says which artifact it means.
    """
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--previous-bin-dir", type=Path)
    parser.add_argument("--seed", type=int, default=int(os.environ.get("GATE_SEED", "20260923")))
    parser.add_argument("--profile", default=os.environ.get("GATE_PROFILE", "pr"))
    arguments, _ = parser.parse_known_args(argv)
    extra = {
        "seed": str(arguments.seed),
        "profile": arguments.profile,
        "previous_bin_dir": str(arguments.previous_bin_dir) if arguments.previous_bin_dir else "",
    }
    return Artifact(arguments.bin_dir.resolve()), extra


def identify_artifact(gate: Gate, artifact: Artifact) -> dict[str, Any]:
    """Records which binary ran, and refuses one that is not this checkout's version."""
    try:
        description = artifact.describe()
    except InfrastructureError:
        raise
    gate.context["artifact"] = description
    expected = f"record-store {workspace_version()}"
    gate.require(
        "artifact reports this checkout's version",
        description["reported_version"] == expected,
        f"binary says {description['reported_version']!r}, Cargo.toml says {expected!r}",
    )
    return description


def payload(rng: random.Random, size: int) -> bytes:
    return rng.randbytes(size)


class ReadFailed(Exception):
    """A GET that started but did not deliver a complete body."""


def read_all(client, bucket: str, key: str, **arguments: Any) -> bytes:
    """Reads a whole body. A stream that breaks is a failed read, not a crash of the gate."""
    from botocore.exceptions import ResponseStreamingError

    response = client.get_object(Bucket=bucket, Key=key, **arguments)
    try:
        return response["Body"].read()
    except (ResponseStreamingError, ConnectionError, OSError) as error:
        raise ReadFailed(f"{bucket}/{key}: {type(error).__name__}") from error


def list_all_keys(client, bucket: str, page_size: int = 7) -> list[str]:
    """Walks ListObjectsV2 to the end with a small page, so pagination is exercised."""
    keys: list[str] = []
    token = None
    pages = 0
    while True:
        arguments = {"Bucket": bucket, "MaxKeys": page_size}
        if token:
            arguments["ContinuationToken"] = token
        page = client.list_objects_v2(**arguments)
        keys.extend(item["Key"] for item in page.get("Contents", []))
        pages += 1
        if not page.get("IsTruncated"):
            return keys
        token = page.get("NextContinuationToken")
        if not token:
            raise GateFailure("a truncated listing page carried no continuation token")
        if pages > 100_000:
            raise GateFailure("listing did not terminate")


def all_events(server: Server, bucket: str) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    cursor: tuple[str, str] | None = None
    while True:
        query = {"bucket": bucket, "limit": "500"}
        if cursor:
            query.update(after_time=cursor[0], after_id=cursor[1])
        page = server.api("GET", "/api/v1/events?" + urllib.parse.urlencode(query))
        events.extend(page.get("events", []))
        if not page.get("next_time") or not page.get("next_id") or not page.get("events"):
            return events
        cursor = (page["next_time"], page["next_id"])


def audit_chain_intact(server: Server) -> tuple[bool, dict[str, Any]]:
    cursor = 0
    last: dict[str, Any] = {}
    for _ in range(10_000):
        last = server.api("GET", f"/api/v1/audit/chain?from={cursor}&limit=10000")
        if not last.get("intact", False):
            return False, last
        following = last.get("next_from")
        if following is None or following <= cursor or last.get("checked", 0) == 0:
            return True, last
        cursor = following
    return False, {"problem": "chain walk did not terminate"}


class SlowBody:
    """A request body that trickles out, so a crash can land mid-upload.

    Over plain HTTP botocore signs the payload, which means it reads the whole
    body once to hash it, rewinds, and only then sends. Slowing that first pass
    would delay the request rather than stretch the transfer, so the body is
    instant until it has been rewound once and slow from then on.
    """

    def __init__(self, data: bytes, chunk: int = 256 * 1024, delay: float = 0.02) -> None:
        self.data = data
        self.offset = 0
        self.chunk = chunk
        self.delay = delay
        self.sending = False
        self.reached_end = False

    def read(self, size: int = -1) -> bytes:
        if self.offset >= len(self.data):
            self.reached_end = True
            return b""
        if not self.sending:
            end = len(self.data) if size is None or size < 0 else self.offset + size
            piece = self.data[self.offset : end]
            self.offset += len(piece)
            return piece
        time.sleep(self.delay)
        size = self.chunk if size is None or size < 0 else min(size, self.chunk)
        piece = self.data[self.offset : self.offset + size]
        self.offset += len(piece)
        return piece

    def __len__(self) -> int:
        return len(self.data)

    def seek(self, offset: int, whence: int = 0) -> int:
        position = offset if whence == 0 else (len(self.data) + offset if whence == 2 else self.offset + offset)
        if position == 0 and (self.reached_end or self.offset >= len(self.data)):
            self.sending = True
        self.offset = position
        return self.offset

    def tell(self) -> int:
        return self.offset
