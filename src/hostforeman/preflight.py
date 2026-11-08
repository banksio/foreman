"""Items 10, 13, 17 and 18: reasons not to start an upgrade right now."""

from __future__ import annotations

from .models import BLOCKED, ERROR, OK, WARN, Context, StepResult

_MB = 1024 * 1024
_UNDERVOLTAGE_NOW = 1 << 0
_UNDERVOLTAGE_SINCE_BOOT = 1 << 16


def _power(ctx: Context, blocks: list[str], warnings: list[str]) -> None:
    result = ctx.host.run(["vcgencmd", "get_throttled"], timeout=15)
    if not result.ok:  # Not a Raspberry Pi, or the firmware tool isn't installed.
        return
    try:
        flags = int(result.out.strip().split("=", 1)[1], 16)
    except (IndexError, ValueError):
        warnings.append(f"Could not read the power state: {result.out.strip()}")
        return
    if flags & _UNDERVOLTAGE_NOW:
        blocks.append("the Pi is undervolted right now, and a power dip can corrupt an upgrade")
    elif flags & _UNDERVOLTAGE_SINCE_BOOT:
        warnings.append("The Pi has been undervolted since boot, so check its power supply")


def _clock(ctx: Context, blocks: list[str], warnings: list[str]) -> None:
    if not ctx.config.preflight.require_clock_sync:
        return
    result = ctx.host.run(["timedatectl", "show", "-p", "NTPSynchronized", "--value"], timeout=15)
    if result.missing:
        return
    if not result.ok:
        warnings.append("Could not read the clock sync state")
    elif result.out.strip() != "yes":
        blocks.append("the clock isn't synchronised, and apt rejects repositories on a wrong date")


def _network(ctx: Context, blocks: list[str]) -> None:
    for host in ctx.config.preflight.network_hosts:
        problem = ctx.host.probe(host)
        if problem:
            blocks.append(f"{host}: {problem}")


def _free_mb(ctx: Context, path: str) -> int:
    return ctx.host.disk_usage(path).free // _MB


def _space(ctx: Context, blocks: list[str], notes: list[str]) -> None:
    settings = ctx.config.preflight
    free = _free_mb(ctx, "/")
    if free < settings.min_free_mb:
        ctx.host.run(["apt-get", "clean"], mutates=True, timeout=120)
        free = _free_mb(ctx, "/")
        notes.append("Ran apt clean to free space")
    if free < settings.min_free_mb:
        blocks.append(f"only {free} MB free on /, and {settings.min_free_mb} MB is required")
    if ctx.host.path("/boot/firmware").is_dir():
        boot_free = _free_mb(ctx, "/boot/firmware")
        if boot_free < settings.min_free_boot_mb:
            blocks.append(
                f"only {boot_free} MB free on /boot/firmware, "
                f"and {settings.min_free_boot_mb} MB is required"
            )


def step(ctx: Context, cycle: dict | None = None) -> StepResult:
    if ctx.host.run(["apt-get", "--version"], timeout=15).missing:
        return StepResult("preflight", ERROR, "apt-get not found", errors=["This host has no apt"])

    blocks: list[str] = []
    warnings: list[str] = []
    notes: list[str] = []
    _power(ctx, blocks, warnings)
    _clock(ctx, blocks, warnings)
    _network(ctx, blocks)
    _space(ctx, blocks, notes)

    if blocks:
        return StepResult("preflight", BLOCKED, "Postponed: " + "; ".join(blocks), warnings)
    status = WARN if warnings else OK
    return StepResult("preflight", status, " ".join(["All checks passed.", *notes]).strip(), warnings)
