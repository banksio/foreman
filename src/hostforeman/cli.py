"""The `foreman` command."""

from __future__ import annotations

import os
from collections.abc import Callable
from enum import Enum
from typing import Annotated, Optional

import typer
from rich.table import Table
from rich.text import Text

from . import __version__, compose, config, cycle, status
from .host import Host, Runner
from .models import ERROR, Context
from .state import LockBusy, State, StateError, run_lock
from .ui import UI

app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Unattended upgrades, reboots, kernel cleanup and Compose updates.",
)
stacks_app = typer.Typer(help="Review running Compose stacks.", invoke_without_command=True)
app.add_typer(stacks_app, name="stacks")


class StepName(str, Enum):
    preflight = "preflight"
    upgrade = "upgrade"
    reboot = "reboot"
    kernels = "kernels"
    compose = "compose"
    disk = "disk"


def _context(
    *,
    dry_run: bool = False,
    no_reboot: bool = False,
    reboot_now: bool = False,
    verbose: bool = False,
    read_only: bool = False,
) -> Context:
    ui = UI(verbose=verbose)
    try:
        settings = config.load(config.config_path())
        state = State(config.state_path(), persist=not (dry_run or read_only))
    except (config.ConfigError, StateError) as exc:
        ui.fail(str(exc))
        raise typer.Exit(2) from exc
    runner = Runner(dry_run=dry_run, on_would=ui.would, on_run=ui.ran)
    return Context(settings, Host(runner), state, ui, dry_run, no_reboot, reboot_now)


def _require_root(ctx: Context) -> None:
    if os.geteuid() != 0:
        ctx.ui.fail("this needs root, so run it with sudo (--dry-run doesn't need it)")
        raise typer.Exit(2)


def _with_lock(ctx: Context, action: Callable[[], None]) -> None:
    try:
        with run_lock(config.lock_path()):
            action()
    except LockBusy:
        ctx.ui.note("Another foreman run is in progress, so this one is stopping")


def _version(value: bool) -> None:
    if value:
        print(__version__)
        raise typer.Exit()


@app.callback()
def main(
    version: Annotated[
        bool, typer.Option("--version", callback=_version, is_eager=True, help="Show the version.")
    ] = False,
) -> None:
    pass


@app.command()
def run(
    step: Annotated[
        Optional[StepName], typer.Argument(help="Run only this step. Omit for the full cycle.")
    ] = None,
    dry_run: Annotated[
        bool, typer.Option("--dry-run", help="Show what would happen. Changes nothing.")
    ] = False,
    no_reboot: Annotated[bool, typer.Option("--no-reboot", help="Never reboot in this run.")] = False,
    reboot_now: Annotated[
        bool, typer.Option("--reboot-now", help="Ignore the reboot window. Other checks still apply.")
    ] = False,
    verbose: Annotated[bool, typer.Option("--verbose", "-v", help="Show each command.")] = False,
) -> None:
    """Run the full maintenance cycle, or one step of it."""
    ctx = _context(dry_run=dry_run, no_reboot=no_reboot, reboot_now=reboot_now, verbose=verbose)
    failed = False

    def action() -> None:
        nonlocal failed
        if step is None:
            outcome = cycle.run_cycle(ctx)
            ctx.ui.console.rule(Text(f"Cycle: {outcome['status']}", style="bold"), align="left")
            failed = outcome["status"] == "failed"
        else:
            failed = cycle.run_single(ctx, step.value).status == ERROR

    if dry_run:
        action()
    else:
        _require_root(ctx)
        _with_lock(ctx, action)
    if failed:
        raise typer.Exit(1)


@app.command(hidden=True)
def resume() -> None:
    """Continue a pending cycle. Started by systemd."""
    ctx = _context()
    _require_root(ctx)
    _with_lock(ctx, lambda: cycle.resume(ctx))


@app.command("status")
def show_status() -> None:
    """Show the last run, pending reboot, disk, stacks and warnings."""
    status.render(_context(read_only=True))


@stacks_app.callback()
def stacks(context: typer.Context) -> None:
    """List running stacks and what foreman will do with each."""
    if context.invoked_subcommand is not None:
        return
    ctx = _context(read_only=True)
    known = ctx.state["known_stacks"]
    try:
        found = compose.discover(ctx)
    except compose.DockerError as exc:
        ctx.ui.fail(f"could not read Docker ({exc}), try sudo")
        raise typer.Exit(1) from exc
    table = Table()
    for column in ("Project", "Directory", "Services", "Action"):
        table.add_column(column, overflow="fold")
    for stack in found:
        action = compose.classify(ctx, stack, known)
        table.add_row(
            Text(stack.project), Text(stack.workdir), Text(", ".join(stack.services)), Text(action)
        )
    ctx.ui.console.print(table)


@stacks_app.command()
def approve(
    names: Annotated[Optional[list[str]], typer.Argument(help="Project names to approve.")] = None,
    all_held: Annotated[bool, typer.Option("--all", help="Approve every held stack.")] = False,
) -> None:
    """Allow new stacks held for review to be updated."""
    ctx = _context()
    _require_root(ctx)
    known = ctx.state["known_stacks"] or []
    try:
        held = [
            stack.project
            for stack in compose.discover(ctx)
            if compose.classify(ctx, stack, known) == compose.HELD
        ]
    except compose.DockerError as exc:
        ctx.ui.fail(f"could not read Docker: {exc}")
        raise typer.Exit(1) from exc
    chosen = held if all_held else list(names or [])
    unknown = sorted(set(chosen) - set(held))
    if unknown:
        ctx.ui.fail(f"not held for review: {', '.join(unknown)}")
        raise typer.Exit(2)
    if not chosen:
        ctx.ui.note("Nothing to approve")
        return
    ctx.state["known_stacks"] = sorted({*known, *chosen})
    ctx.state.save()
    ctx.ui.note(f"Approved: {', '.join(chosen)}")
