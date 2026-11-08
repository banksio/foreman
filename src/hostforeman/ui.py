"""Terminal output. Text from commands is never interpreted as Rich markup."""

from __future__ import annotations

import shlex
from collections.abc import Sequence
from contextlib import nullcontext

from rich.console import Console
from rich.text import Text

from . import models

STATUS_STYLES = {
    models.OK: "green",
    models.WARN: "yellow",
    models.SKIPPED: "dim",
    models.BLOCKED: "yellow",
    models.ERROR: "bold red",
    models.WAITING: "cyan",
    models.REBOOTING: "cyan",
    "failed": "bold red",
    "running": "cyan",
}

LABELS = {
    "preflight": "Preflight checks",
    "upgrade": "Package upgrade",
    "reboot": "Reboot",
    "verify": "Boot verification",
    "kernels": "Old kernels",
    "compose": "Compose stacks",
    "disk": "Disk usage",
}


def status_text(status: str) -> Text:
    return Text(status, style=STATUS_STYLES.get(status, ""))


class UI:
    def __init__(self, console: Console | None = None, verbose: bool = False) -> None:
        self.console = console or Console()
        self.verbose = verbose

    def heading(self, name: str) -> None:
        self.console.rule(Text(LABELS.get(name, name), style="bold"), align="left")

    def working(self, label: str):
        if self.console.is_terminal:
            return self.console.status(Text(label))
        return nullcontext()

    def note(self, message: str) -> None:
        self.console.print(Text(message))

    def fail(self, message: str) -> None:
        self.console.print(Text(f"Error: {message}", style="bold red"))

    def would(self, args: Sequence[str]) -> None:
        self.console.print(Text(f"would run: {shlex.join(args)}", style="dim"))

    def ran(self, args: Sequence[str]) -> None:
        if self.verbose:
            self.console.print(Text(f"$ {shlex.join(args)}", style="dim"))

    def result(self, result: models.StepResult) -> None:
        line = status_text(result.status)
        line.append(f"  {result.summary}", style="default")
        self.console.print(line)
        for warning in result.warnings:
            self.console.print(Text(f"  ! {warning}", style="yellow"))
        for error in result.errors:
            self.console.print(Text(f"  x {error}", style="red"))
