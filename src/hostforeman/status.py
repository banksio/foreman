"""Item 7: what `foreman status` shows. It only reads, so it doesn't need root."""

from __future__ import annotations

import socket

from rich.columns import Columns
from rich.table import Table
from rich.text import Text

from . import __version__, disk, reboot
from .cycle import PHASES
from .models import Context
from .ui import LABELS, status_text


def _percent_text(percent: int, ctx: Context) -> Text:
    settings = ctx.config.disk
    style = "green"
    if percent >= settings.critical_percent:
        style = "bold red"
    elif percent >= settings.warn_percent:
        style = "yellow"
    return Text(f"{percent}%", style=style)


def _overview(ctx: Context) -> Table:
    info = reboot.inspect(ctx)
    table = Table.grid(padding=(0, 2))
    table.add_column(style="bold")
    table.add_column()
    table.add_row("Host", Text(socket.gethostname()))
    table.add_row("Kernel", Text(info.running))
    needed = Text("no", style="green")
    if info.required:
        needed = Text(f"yes, {info.reason}", style="yellow")
    table.add_row("Reboot needed", needed)
    table.add_row("Reboot window", Text(ctx.config.reboot.window or "any time"))
    cycle = ctx.state["cycle"]
    if cycle:
        table.add_row("Last cycle", status_text(cycle["status"]))
        table.add_row("Started", Text(cycle["started"]))
        if cycle["finished"]:
            table.add_row("Finished", Text(cycle["finished"]))
        else:
            table.add_row("Currently at", Text(LABELS.get(cycle["phase"], cycle["phase"])))
    else:
        table.add_row("Last cycle", Text("none recorded", style="dim"))
    return table


def _steps(ctx: Context) -> Table | None:
    results = ctx.state["last_steps"]
    if not results:
        return None
    table = Table(title="Latest result of each step", title_justify="left")
    table.add_column("Step")
    table.add_column("Status")
    table.add_column("When")
    table.add_column("Summary", overflow="fold")
    for name in PHASES:
        if name in results:
            entry = results[name]
            table.add_row(
                LABELS[name], status_text(entry["status"]), Text(entry["at"]), Text(entry["summary"])
            )
    return table


def _problems(ctx: Context) -> list[Text]:
    lines: list[Text] = []
    for entry in ctx.state["last_steps"].values():
        lines += [Text(f"! {item}", style="yellow") for item in entry["warnings"]]
        lines += [Text(f"x {item}", style="red") for item in entry["errors"]]
    return lines


def _disk(ctx: Context) -> list:
    report = ctx.state["disk"] or {}
    paths = list(ctx.config.disk.paths)
    paths += [row["path"] for row in report.get("rows", []) if row["path"] not in paths]
    usage = Table(title="Disk usage now", title_justify="left")
    usage.add_column("Path")
    usage.add_column("Used", justify="right")
    usage.add_column("Free", justify="right")
    usage.add_column("Size", justify="right")
    for row in disk.usage_rows(ctx, paths):
        usage.add_row(
            Text(row["path"]),
            _percent_text(row["percent"], ctx),
            f"{row['free_gb']} GB",
            f"{row['total_gb']} GB",
        )
    tables = [usage]
    if report.get("consumers"):
        consumers = Table(title=f"Biggest users at {report['at']}", title_justify="left")
        consumers.add_column("Path")
        consumers.add_column("Size", justify="right")
        for row in report["consumers"]:
            consumers.add_row(Text(row["path"]), f"{row['gb']} GB")
        tables.append(consumers)
    return tables


def _stacks(ctx: Context) -> Table | None:
    rows = ctx.state["stacks"]
    if not rows:
        return None
    table = Table(title="Compose stacks at the last run", title_justify="left")
    table.add_column("Project")
    table.add_column("Outcome")
    table.add_column("Detail", overflow="fold")
    for row in rows:
        table.add_row(Text(row["project"]), Text(row["outcome"].replace("_", " ")), Text(row["message"]))
    return table


def render(ctx: Context) -> None:
    console = ctx.ui.console
    console.rule(Text(f"foreman {__version__}", style="bold"), align="left")
    console.print(_overview(ctx))
    for table in (_steps(ctx), _stacks(ctx)):
        if table:
            console.print()
            console.print(table)
    problems = _problems(ctx)
    if problems:
        console.print()
        console.print(Text("Needs attention", style="bold"))
        for line in problems:
            console.print(line)
    console.print()
    console.print(Columns(_disk(ctx)))
