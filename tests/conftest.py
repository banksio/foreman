from __future__ import annotations

import io
import json
from collections import namedtuple
from datetime import datetime, timedelta
from pathlib import Path

import pytest
from rich.console import Console

from hostforeman import config
from hostforeman.host import Host, Result, Runner
from hostforeman.models import Context
from hostforeman.state import State
from hostforeman.ui import UI

Usage = namedtuple("Usage", "total used free")
GB = 1024**3
PLENTY = Usage(100 * GB, 20 * GB, 80 * GB)
K2712 = "6.12.50+rpt-rpi-2712"


class FakeRunner(Runner):
    """Answers commands from rules. Dry-run handling is the real Runner's."""

    def __init__(self, dry_run: bool = False) -> None:
        self.would: list[list[str]] = []
        super().__init__(dry_run=dry_run, on_would=lambda args: self.would.append(list(args)))
        self.calls: list[list[str]] = []
        self.rules: list[tuple[list[str], object]] = []

    def on(self, prefix: list[str], answer) -> None:
        """Newer rules win. An answer is a Result, or a function of the arguments."""
        self.rules.insert(0, (prefix, answer))

    def _exec(self, args, timeout, env):
        self.calls.append(args)
        for prefix, answer in self.rules:
            if args[: len(prefix)] == prefix:
                return answer(args) if callable(answer) else answer
        return Result(0)

    def called(self, *needle: str) -> bool:
        return self.index(*needle) is not None

    def index(self, *needle: str) -> int | None:
        for position, args in enumerate(self.calls):
            if all(word in args for word in needle):
                return position
        return None


class Bench:
    def __init__(self, base: Path, raw_config=None, dry_run=False, **flags) -> None:
        self.root = base / "root"
        self.root.mkdir(exist_ok=True)
        self.running = K2712
        self.moment = datetime(2026, 11, 8, 3, 30)
        self.elapsed = 0.0
        self.usage: dict[str, Usage] = {}
        self.unreachable: dict[str, str] = {}
        self.runner = FakeRunner(dry_run)
        for path in ("etc/systemd/system", "lib/modules"):
            (self.root / path).mkdir(parents=True, exist_ok=True)
        self.runner.on(["docker"], Result(127))
        self.runner.on(["vcgencmd"], Result(0, "throttled=0x0\n"))
        self.runner.on(["timedatectl"], Result(0, "yes\n"))
        host = Host(
            self.runner,
            root=self.root,
            running_kernel=lambda: self.running,
            now=lambda: self.moment,
            sleep=self._sleep,
            monotonic=lambda: self.elapsed,
            disk_usage=lambda path: self.usage.get(path, PLENTY),
            probe=lambda name: self.unreachable.get(name),
        )
        self.console = io.StringIO()
        self.state_path = base / "state" / "state.json"
        self.ctx = Context(
            config.from_dict(raw_config or {}),
            host,
            State(self.state_path, persist=not dry_run),
            UI(Console(file=self.console, width=160)),
            dry_run=dry_run,
            **flags,
        )

    def _sleep(self, seconds: float) -> None:
        self.elapsed += seconds
        self.moment += timedelta(seconds=seconds)

    def add_modules(self, *releases: str) -> None:
        for release in releases:
            directory = self.root / "lib/modules" / release
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "modules.builtin").touch()

    def touch(self, path: str) -> None:
        target = self.root / path.lstrip("/")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.touch()

    def saved(self) -> dict:
        return json.loads(self.state_path.read_text())

    @property
    def unit(self) -> Path:
        return self.root / "etc/systemd/system/foreman-resume.service"


@pytest.fixture
def bench(tmp_path):
    def build(raw_config=None, dry_run=False, **flags) -> Bench:
        return Bench(tmp_path, raw_config, dry_run, **flags)

    return build
