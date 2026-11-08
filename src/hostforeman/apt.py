"""Item 1: apt update and full-upgrade, unattended."""

from __future__ import annotations

import re

from .models import ERROR, OK, WARN, Context, StepResult

APT_ENV = {
    "DEBIAN_FRONTEND": "noninteractive",
    "APT_LISTCHANGES_FRONTEND": "none",
    "NEEDRESTART_MODE": "a",
}
LOCK_WAIT = ["-o", "DPkg::Lock::Timeout=300"]
KEEP_CONFIG = [
    "-o",
    "Dpkg::Options::=--force-confdef",
    "-o",
    "Dpkg::Options::=--force-confold",
]
_DPKG_RECOVER = ["dpkg", "--force-confdef", "--force-confold", "--configure", "-a"]
_SIMULATED = re.compile(r"^(Inst|Remv|Purg) (\S+)", re.MULTILINE)


def parse_simulation(output: str) -> tuple[list[str], list[str]]:
    """Returns the packages a simulated run would (install or upgrade, remove)."""
    changed: list[str] = []
    removed: list[str] = []
    for action, package in _SIMULATED.findall(output):
        (changed if action == "Inst" else removed).append(package)
    return changed, removed


def _dpkg_interrupted(ctx: Context) -> bool:
    updates = ctx.host.path("/var/lib/dpkg/updates")
    return updates.is_dir() and any(updates.iterdir())


def _recover(ctx: Context) -> None:
    ctx.host.run(_DPKG_RECOVER, mutates=True, timeout=1800, env=APT_ENV)


def step(ctx: Context, cycle: dict | None = None) -> StepResult:
    run = ctx.host.run
    warnings: list[str] = []

    if _dpkg_interrupted(ctx):
        warnings.append("A previous dpkg run was interrupted, so it was finished first")
        _recover(ctx)

    update = run(["apt-get", *LOCK_WAIT, "update"], mutates=True, timeout=900, env=APT_ENV)
    if not update.ok:
        return StepResult("upgrade", ERROR, "apt-get update failed", errors=[update.tail()])

    simulated = run(["apt-get", "-s", "full-upgrade"], timeout=300, env=APT_ENV)
    if not simulated.ok:
        return StepResult(
            "upgrade", ERROR, "Could not work out the upgrade", errors=[simulated.tail()]
        )
    changed, removed = parse_simulation(simulated.out)
    details = {"changed": changed, "removed": removed}

    if not changed and not removed:
        status = WARN if warnings else OK
        return StepResult("upgrade", status, "Already up to date", warnings, details=details)

    plan = f"{len(changed)} to upgrade or install, {len(removed)} to remove"
    if ctx.dry_run:
        return StepResult("upgrade", OK, f"Would apply: {plan}", warnings, details=details)

    upgrade = ["apt-get", "-y", "-q", *LOCK_WAIT, *KEEP_CONFIG, "full-upgrade"]
    with ctx.ui.working(f"Upgrading packages ({plan})"):
        applied = run(upgrade, mutates=True, timeout=7200, env=APT_ENV)
        if not applied.ok:
            warnings.append("The first upgrade attempt failed, so dpkg was repaired and it was retried")
            _recover(ctx)
            applied = run(upgrade, mutates=True, timeout=7200, env=APT_ENV)
    if not applied.ok:
        return StepResult(
            "upgrade", ERROR, "apt-get full-upgrade failed", warnings, [applied.tail()], details
        )

    # A kernel can be rebuilt under the same release name, which no version comparison would see.
    if f"linux-image-{ctx.host.running_kernel()}" in changed:
        ctx.state["kernel_updated_boot"] = ctx.host.boot_id()
        ctx.state.save()

    status = WARN if warnings else OK
    return StepResult("upgrade", status, f"Applied: {plan}", warnings, details=details)
