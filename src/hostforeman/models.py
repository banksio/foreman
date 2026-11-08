"""The result type every step returns, and the context every step receives."""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import TYPE_CHECKING

from .config import Config
from .host import Host
from .state import State

if TYPE_CHECKING:
    from .ui import UI

OK = "ok"
WARN = "warning"
SKIPPED = "skipped"
BLOCKED = "blocked"
ERROR = "error"
WAITING = "waiting"
REBOOTING = "rebooting"


@dataclass
class StepResult:
    name: str
    status: str
    summary: str
    warnings: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    details: dict = field(default_factory=dict)


@dataclass
class Context:
    config: Config
    host: Host
    state: State
    ui: UI
    dry_run: bool = False
    no_reboot: bool = False
    reboot_now: bool = False


def record(ctx: Context, result: StepResult, cycle: dict | None = None) -> None:
    """Stores a step's result and saves, so the outcome survives whatever happens next."""
    entry = {**asdict(result), "at": ctx.host.now().isoformat(timespec="seconds")}
    ctx.state["last_steps"][result.name] = entry
    if cycle is not None:
        cycle["steps"][result.name] = entry
    ctx.state.save()


def cycle_errors(cycle: dict | None) -> list[str]:
    if not cycle:
        return []
    return [error for step in cycle["steps"].values() for error in step["errors"]]
