from __future__ import annotations

import hashlib
import os
import shutil
import socket
import subprocess
import time
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]


@dataclass(frozen=True)
class RunningServer:
    endpoint: str
    process: subprocess.Popen[str]
    tls_dir: Path | None = None


def _free_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def _server_binary() -> Path:
    configured = os.environ.get("YESNODB_TEST_SERVER")
    if configured:
        return Path(configured)
    discovered = shutil.which("yesnod")
    if discovered:
        return Path(discovered)
    return ROOT / "target" / "debug" / "yesnod"


def _principal(name: str, role: str, token: str) -> str:
    digest = hashlib.sha256(token.encode()).hexdigest()
    return f'[[auth.principal]]\nname = "{name}"\nrole = "{role}"\ntoken_sha256 = "{digest}"\n'


@pytest.fixture
def server_factory(tmp_path: Path) -> Iterator[object]:
    processes: list[subprocess.Popen[str]] = []

    def start(*, authenticated: bool = False, mutual_tls: bool = False) -> RunningServer:
        binary = _server_binary()
        if not binary.is_file():
            pytest.skip("build yesnod first, or set YESNODB_TEST_SERVER")
        port = _free_port()
        instance = tmp_path / f"server-{len(processes)}"
        instance.mkdir()
        data = instance / "data"
        config = instance / "yesnod.toml"
        tls_dir = instance / "tls" if mutual_tls else None
        if tls_dir is not None:
            subprocess.run(
                [str(ROOT / "yesno-server" / "dist" / "gen-dev-certs.sh"), str(tls_dir)],
                cwd=ROOT,
                check=True,
                stdout=subprocess.DEVNULL,
            )
        anonymous = "none" if authenticated or mutual_tls else "read"
        text = (
            "[server]\n"
            f'data_dir = "{data.as_posix()}"\n'
            'role = "leader"\n'
            "\n[server.flight]\n"
            f'listen = "127.0.0.1:{port}"\n'
            "\n[server.metrics]\n"
            'listen = ""\n'
            "\n[auth]\n"
            f'anonymous = "{anonymous}"\n'
        )
        if authenticated:
            text += _principal("reader", "reader", "read-token")
            text += _principal("writer", "writer", "write-token")
            text += _principal("admin", "admin", "admin-token")
        if tls_dir is not None:
            fingerprint = (tls_dir / "client.sha256").read_text().strip()
            server_cert = (tls_dir / "server.pem").as_posix()
            server_key = (tls_dir / "server.key").as_posix()
            client_ca = (tls_dir / "ca.pem").as_posix()
            text += (
                "[[auth.principal]]\n"
                'name = "client"\n'
                'role = "writer"\n'
                f'cert_sha256 = "{fingerprint}"\n'
                "\n[server.flight.tls]\n"
                f'cert = "{server_cert}"\n'
                f'key = "{server_key}"\n'
                f'client_ca = "{client_ca}"\n'
                "require_client_auth = true\n"
            )
        config.write_text(text)
        process = subprocess.Popen(
            [str(binary), "--config", str(config), "--log", "warn"],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        processes.append(process)
        scheme = "https" if mutual_tls else "http"
        endpoint = f"{scheme}://127.0.0.1:{port}"
        deadline = time.monotonic() + 10
        while True:
            if process.poll() is not None:
                output = process.stdout.read() if process.stdout is not None else ""
                pytest.fail(f"yesnod exited during startup ({process.returncode}):\n{output}")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                if time.monotonic() >= deadline:
                    pytest.fail("yesnod did not become available within 10 seconds")
        return RunningServer(endpoint, process, tls_dir)

    yield start

    for process in reversed(processes):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
