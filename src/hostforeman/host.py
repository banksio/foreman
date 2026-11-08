"""Everything foreman needs from the machine, gathered here so tests can replace it."""

from __future__ import annotations

import os
import platform
import shutil
import socket
import subprocess
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"


@dataclass(frozen=True)
class Result:
    code: int
    out: str = ""
    err: str = ""

    @property
    def ok(self) -> bool:
        return self.code == 0

    @property
    def missing(self) -> bool:
        return self.code == 127

    def tail(self, lines: int = 8) -> str:
        text = self.err.strip() or self.out.strip()
        return "\n".join(text.splitlines()[-lines:])


class Runner:
    """Runs commands without a shell. In dry-run mode, anything that mutates is only announced."""

    def __init__(
        self,
        dry_run: bool = False,
        on_would: Callable[[Sequence[str]], None] | None = None,
        on_run: Callable[[Sequence[str]], None] | None = None,
    ) -> None:
        self.dry_run = dry_run
        self.on_would = on_would
        self.on_run = on_run

    def __call__(
        self,
        args: Sequence[str],
        *,
        mutates: bool = False,
        timeout: float = 600,
        env: Mapping[str, str] | None = None,
    ) -> Result:
        args = list(args)
        if mutates and self.dry_run:
            if self.on_would:
                self.on_would(args)
            return Result(0)
        if self.on_run:
            self.on_run(args)
        return self._exec(args, timeout, env)

    def _exec(
        self, args: list[str], timeout: float, env: Mapping[str, str] | None
    ) -> Result:
        full_env = {**os.environ, "PATH": PATH, "LC_ALL": "C", **(env or {})}
        try:
            done = subprocess.run(
                args,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                errors="replace",
                timeout=timeout,
                env=full_env,
            )
        except (FileNotFoundError, NotADirectoryError):
            return Result(127, "", f"{args[0]}: not found")
        except subprocess.TimeoutExpired:
            return Result(124, "", f"{args[0]}: timed out after {timeout:.0f}s")
        return Result(done.returncode, done.stdout, done.stderr)


def tcp_probe(host: str, port: int = 443, timeout: float = 10.0) -> str | None:
    """Returns None if the host is reachable, otherwise what went wrong."""
    try:
        with socket.create_connection((host, port), timeout):
            return None
    except socket.gaierror:
        return "DNS lookup failed"
    except OSError as exc:
        return f"could not connect ({exc})"


@dataclass
class Host:
    run: Runner
    root: Path = Path("/")
    running_kernel: Callable[[], str] = platform.release
    now: Callable[[], datetime] = datetime.now
    sleep: Callable[[float], None] = time.sleep
    monotonic: Callable[[], float] = time.monotonic
    disk_usage: Callable[[str], object] = shutil.disk_usage
    probe: Callable[[str], str | None] = tcp_probe

    def path(self, absolute: str) -> Path:
        return self.root / absolute.lstrip("/")

    def boot_id(self) -> str | None:
        try:
            return self.path("/proc/sys/kernel/random/boot_id").read_text().strip()
        except OSError:
            return None
