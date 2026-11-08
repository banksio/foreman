"""Items 3 and 8: the ordered cycle, which carries on across a reboot, and single steps."""

from __future__ import annotations

from collections.abc import Callable

from . import apt, compose, disk, kernels, preflight, reboot
from .models import BLOCKED, ERROR, OK, REBOOTING, WAITING, WARN, Context, StepResult, record

Step = Callable[[Context, dict | None], StepResult]

PHASES: dict[str, Step] = {
    "preflight": preflight.step,
    "upgrade": apt.step,
    "reboot": reboot.step,
    "verify": reboot.verify,
    "kernels": kernels.step,
    "compose": compose.step,
    "disk": disk.step,
}
SINGLE_STEPS = ("preflight", "upgrade", "reboot", "kernels", "compose", "disk")
_ACTIVE = ("running", "waiting")


def _new_cycle(ctx: Context) -> dict:
    return {
        "status": "running",
        "phase": "preflight",
        "started": ctx.host.now().isoformat(timespec="seconds"),
        "finished": None,
        "steps": {},
        "rebooted": False,
        "expected_kernel": None,
    }


def _call(ctx: Context, name: str, cycle: dict | None) -> StepResult:
    try:
        return PHASES[name](ctx, cycle)
    except Exception as exc:  # One broken step must not stop the remaining ones or lose state.
        return StepResult(name, ERROR, "The step crashed", errors=[f"{type(exc).__name__}: {exc}"])


def _final_status(cycle: dict, halted: bool) -> str:
    steps = cycle["steps"].values()
    if any(step["errors"] or step["status"] == ERROR for step in steps):
        return "failed"
    if halted:
        return BLOCKED
    if any(step["warnings"] or step["status"] in (WARN, BLOCKED) for step in steps):
        return WARN
    return OK


def run_cycle(ctx: Context) -> dict:
    """Runs or continues a full cycle. Returns early if it is waiting for or doing a reboot."""
    cycle = ctx.state["cycle"]
    if ctx.dry_run or not cycle or cycle["status"] not in _ACTIVE:
        cycle = _new_cycle(ctx)
        ctx.state["cycle"] = cycle
    else:
        ctx.ui.note(f"Continuing the cycle from {cycle['started']}, at step {cycle['phase']}")
    cycle["status"] = "running"
    ctx.state.save()

    names = list(PHASES)
    halted = False
    for name in names[names.index(cycle["phase"]):]:
        if halted and name != "disk":
            continue
        cycle["phase"] = name
        ctx.state.save()
        ctx.ui.heading(name)
        result = _call(ctx, name, cycle)
        ctx.ui.result(result)
        if result.status == REBOOTING:
            return cycle  # reboot.step has already saved the state and the next phase.
        record(ctx, result, cycle)
        if result.status == WAITING:
            cycle["status"] = "waiting"
            ctx.state.save()
            return cycle
        if name == "preflight" and result.status in (BLOCKED, ERROR):
            halted = True

    cycle["status"] = _final_status(cycle, halted)
    cycle["finished"] = ctx.host.now().isoformat(timespec="seconds")
    ctx.state.save()
    return cycle


def resume(ctx: Context) -> dict | None:
    """Called at boot, and when a reboot window opens. Does nothing if no cycle is pending."""
    cycle = ctx.state["cycle"]
    if not cycle or cycle["status"] not in _ACTIVE:
        reboot.remove_resume_unit(ctx)
        return None
    outcome = None
    try:
        outcome = run_cycle(ctx)
        return outcome
    finally:
        # Status "running" here means another reboot has just been set up, which needs the unit.
        if outcome is None or outcome["status"] != "running":
            reboot.remove_resume_unit(ctx)


def run_single(ctx: Context, name: str) -> StepResult:
    """Runs one step outside any cycle. An upgrade still goes through preflight first."""
    if name == "upgrade":
        ctx.ui.heading("preflight")
        checks = _call(ctx, "preflight", None)
        ctx.ui.result(checks)
        record(ctx, checks)
        if checks.status in (BLOCKED, ERROR):
            return checks
    ctx.ui.heading(name)
    result = _call(ctx, name, None)
    ctx.ui.result(result)
    if result.status != REBOOTING:  # Already recorded before the reboot began.
        record(ctx, result)
    return result
