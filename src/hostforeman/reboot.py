"""Items 2, 3, 11 and 12: deciding on a reboot, performing it, and confirming it worked."""

from __future__ import annotations

import shutil
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from . import window as window_module
from .kernels import installed_module_releases, newest_of_flavour
from .models import (
    BLOCKED,
    ERROR,
    OK,
    REBOOTING,
    SKIPPED,
    WAITING,
    WARN,
    Context,
    StepResult,
    cycle_errors,
    record,
)

RESUME_UNIT = "/etc/systemd/system/foreman-resume.service"
WINDOW_UNIT = "foreman-window"
_NETWORK_WAIT = 300
_DOCKER_WAIT = 120


@dataclass(frozen=True)
class RebootInfo:
    required: bool
    reason: str
    running: str
    newest: str | None


def inspect(ctx: Context) -> RebootInfo:
    running = ctx.host.running_kernel()
    newest = newest_of_flavour(installed_module_releases(ctx), running)
    if ctx.host.path("/run/reboot-required").exists():
        return RebootInfo(True, "the system has flagged a reboot as required", running, newest)
    marker = ctx.state["kernel_updated_boot"]
    if marker and marker == ctx.host.boot_id():
        return RebootInfo(True, f"the package for {running} was updated", running, newest)
    if newest and newest != running:
        return RebootInfo(True, f"running {running}, newest is {newest}", running, newest)
    return RebootInfo(False, "", running, newest)


def _self_path() -> str:
    return shutil.which("foreman") or str(Path(sys.argv[0]).resolve())


def _service_enabled(ctx: Context, service: str) -> bool:
    if ctx.host.run(["systemctl", "is-enabled", service]).ok:
        return True
    # SSH can be socket-activated, in which case only the socket is enabled.
    return "." not in service and ctx.host.run(["systemctl", "is-enabled", f"{service}.socket"]).ok


def _blocker(ctx: Context, info: RebootInfo, cycle: dict | None) -> str | None:
    errors = cycle_errors(cycle)
    if errors:
        return f"{len(errors)} error(s) earlier in this cycle"
    for service in ctx.config.reboot.watch_services:
        if not _service_enabled(ctx, service):
            return f"{service} is not enabled at boot"
    flagged = ctx.host.path("/run/reboot-required").exists()
    already_tried = info.newest and ctx.state["rebooted_for"] == info.newest
    if not flagged and already_tried and info.newest != info.running:
        return f"already rebooted once for {info.newest}, and it still isn't running"
    sshd = ctx.host.run(["sshd", "-t"])
    if not sshd.ok and not sshd.missing:
        return f"sshd's configuration is invalid: {sshd.tail(2)}"
    return None


def _install_resume_unit(ctx: Context) -> bool:
    unit = (
        "[Unit]\n"
        "Description=Resume the foreman cycle after a reboot\n"
        "After=network-online.target docker.service time-sync.target\n"
        "Wants=network-online.target\n\n"
        "[Service]\n"
        "Type=exec\n"
        f'ExecStart="{_self_path()}" resume\n\n'
        "[Install]\n"
        "WantedBy=multi-user.target\n"
    )
    path = ctx.host.path(RESUME_UNIT)
    path.write_text(unit)
    run = ctx.host.run
    if run(["systemctl", "daemon-reload"], mutates=True).ok and run(
        ["systemctl", "enable", "foreman-resume.service"], mutates=True
    ).ok:
        return True
    remove_resume_unit(ctx)
    return False


def remove_resume_unit(ctx: Context) -> None:
    path = ctx.host.path(RESUME_UNIT)
    if not path.exists():
        return
    # Removes only the boot link and file. A resume in progress keeps running.
    ctx.host.run(["systemctl", "disable", "foreman-resume.service"], mutates=True)
    path.unlink(missing_ok=True)
    ctx.host.run(["systemctl", "daemon-reload"], mutates=True)


def _schedule_window_start(ctx: Context) -> bool:
    start, _ = window_module.parse(ctx.config.reboot.window)
    run = ctx.host.run
    run(["systemctl", "stop", f"{WINDOW_UNIT}.timer"], mutates=True)
    return run(
        [
            "systemd-run",
            f"--unit={WINDOW_UNIT}",
            f"--on-calendar=*-*-* {start:%H:%M}:00",
            _self_path(),
            "resume",
        ],
        mutates=True,
    ).ok


def step(ctx: Context, cycle: dict | None = None) -> StepResult:
    info = inspect(ctx)
    if not info.required:
        return StepResult("reboot", OK, "No reboot needed")
    if ctx.no_reboot:
        return StepResult("reboot", WARN, f"Reboot needed, skipped by request: {info.reason}")

    blocker = _blocker(ctx, info, cycle)
    if blocker:
        return StepResult("reboot", BLOCKED, f"Reboot needed but not safe: {blocker}", [blocker])

    window = ctx.config.reboot.window
    if not ctx.reboot_now and not window_module.contains(window, ctx.host.now()):
        if ctx.dry_run:
            return StepResult("reboot", OK, f"Would wait for the reboot window {window}")
        if cycle is None:
            return StepResult(
                "reboot", WAITING, f"Outside the window {window}, use --reboot-now to override"
            )
        if _schedule_window_start(ctx):
            summary = f"Reboot needed, waiting for the window {window}"
            return StepResult("reboot", WAITING, summary, details={"reason": info.reason})
        return StepResult(
            "reboot",
            WAITING,
            f"Reboot needed, waiting for the window {window}",
            ["Could not schedule it, so run foreman again inside the window"],
        )

    if ctx.dry_run:
        return StepResult("reboot", OK, f"Would reboot: {info.reason}")

    if cycle is not None and not _install_resume_unit(ctx):
        return StepResult("reboot", ERROR, "Could not install the resume service, so no reboot")

    result = StepResult("reboot", REBOOTING, f"Rebooting: {info.reason}")
    ctx.state["rebooted_for"] = info.newest
    if cycle is not None:
        cycle["phase"] = "verify"
        cycle["rebooted"] = True
        cycle["expected_kernel"] = info.newest or info.running
    # Everything is on disk before the reboot starts, because this process won't get to finish.
    record(ctx, result, cycle)
    ctx.host.run(["sync"], mutates=True)
    done = ctx.host.run(["systemctl", "reboot"], mutates=True)
    if done.ok:
        return result

    if cycle is not None:
        cycle["phase"] = "reboot"
        cycle["rebooted"] = False
        remove_resume_unit(ctx)
    return StepResult("reboot", ERROR, "systemctl reboot failed", errors=[done.tail()])


def _wait_for(ctx: Context, check: Callable[[], bool], seconds: int) -> bool:
    deadline = ctx.host.monotonic() + seconds
    while True:
        if check():
            return True
        if ctx.host.monotonic() >= deadline:
            return False
        ctx.host.sleep(5)


def verify(ctx: Context, cycle: dict | None = None) -> StepResult:
    """Runs after the reboot, confirming the kernel and waiting for services to return."""
    if not cycle or not cycle.get("rebooted"):
        return StepResult("verify", SKIPPED, "No reboot in this cycle")

    warnings: list[str] = []
    hosts = ctx.config.preflight.network_hosts
    if not _wait_for(
        ctx, lambda: all(ctx.host.probe(host) is None for host in hosts), _NETWORK_WAIT
    ):
        warnings.append(f"The network still wasn't reachable after {_NETWORK_WAIT}s")

    docker = ctx.host.run(["docker", "info"], timeout=30)
    if not docker.missing:
        if _wait_for(
            ctx, lambda: ctx.host.run(["docker", "info"], timeout=30).ok, _DOCKER_WAIT
        ):
            # Containers with a restart policy are started after the daemon answers.
            ctx.host.sleep(ctx.config.compose.boot_settle)
        else:
            warnings.append(f"Docker still wasn't answering after {_DOCKER_WAIT}s")

    running = ctx.host.running_kernel()
    expected = cycle.get("expected_kernel")
    if expected and running != expected:
        warnings.append(f"Booted {running}, but expected {expected}")
        return StepResult("verify", WARN, "The expected kernel did not boot", warnings)
    status = WARN if warnings else OK
    return StepResult("verify", status, f"Booted {running}", warnings)
