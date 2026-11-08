"""Item 4: removes old kernels, conservatively.

A Raspberry Pi has several kernel flavours installed at once (rpi-2712, rpi-v8), so every
comparison is made within one flavour. Only versioned packages are candidates, which keeps
metapackages such as linux-image-rpi-2712 out of reach.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

from .apt import APT_ENV, LOCK_WAIT, parse_simulation
from .models import ERROR, OK, SKIPPED, WARN, Context, StepResult, cycle_errors

_RELEASE = re.compile(r"^((\d+(?:\.\d+)+)(?:[-+](?:rpt\d*|deb\d+(?:\.\d+)*|bpo|\d+))*)[-+]?(.*)$")
_VERSIONED_IMAGE = re.compile(r"^linux-image-(\d.*)$")


@dataclass(frozen=True)
class Release:
    name: str
    base: str
    flavour: str
    key: tuple[int, ...]


def parse_release(name: str) -> Release | None:
    match = _RELEASE.match(name)
    if not match:
        return None
    base = match.group(1)
    return Release(name, base, match.group(3), tuple(map(int, re.findall(r"\d+", base))))


def installed_module_releases(ctx: Context) -> list[str]:
    """Kernel releases in /lib/modules, ignoring directories left behind after a purge."""
    modules = ctx.host.path("/lib/modules")
    if not modules.is_dir():
        return []
    return sorted(
        entry.name
        for entry in modules.iterdir()
        if (entry / "modules.builtin").exists() or (entry / "modules.dep").exists()
    )


def newest_of_flavour(names: list[str], running: str) -> str | None:
    current = parse_release(running)
    if current is None:
        return None
    candidates = [
        release
        for release in map(parse_release, names)
        if release and release.flavour == current.flavour
    ]
    return max(candidates, key=lambda release: release.key).name if candidates else None


def _installed_packages(ctx: Context) -> dict[str, str]:
    """Maps kernel-related package names to their two-letter dpkg status."""
    result = ctx.host.run(
        [
            "dpkg-query",
            "-W",
            "-f",
            "${db:Status-Abbrev} ${Package}\n",
            "linux-image-*",
            "linux-headers-*",
            "linux-modules-*",
            "linux-kbuild-*",
        ]
    )
    packages = {}
    for line in result.out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            packages[parts[1]] = parts[0][:2]
    return packages


def choose_kept(releases: list[Release], running: str, keep_previous: int) -> set[str]:
    """Within each flavour, keeps the newest plus keep_previous older. Always keeps running."""
    kept = {running}
    for flavour in {release.flavour for release in releases}:
        same = sorted(
            (release for release in releases if release.flavour == flavour),
            key=lambda release: release.key,
            reverse=True,
        )
        kept.update(release.name for release in same[: 1 + keep_previous])
    return kept


def removal_targets(
    packages: dict[str, str], releases: list[Release], kept: set[str]
) -> list[str]:
    installed = {name for name, status in packages.items() if status == "ii"}
    kept_bases = {release.base for release in releases if release.name in kept}
    wanted: set[str] = set()
    for release in releases:
        if release.name in kept:
            continue
        wanted.update(
            f"linux-{kind}-{release.name}"
            for kind in ("image", "headers", "modules", "modules-extra")
        )
        # Files shared between flavours can go only once no kept kernel uses that version.
        if release.base not in kept_bases:
            wanted.update(
                {
                    f"linux-headers-{release.base}-common",
                    f"linux-headers-{release.base}-common-rpi",
                    f"linux-kbuild-{release.base}",
                }
            )
    return sorted(wanted & installed)


def step(ctx: Context, cycle: dict | None = None) -> StepResult:
    settings = ctx.config.kernels
    if not settings.remove_old:
        return StepResult("kernels", SKIPPED, "Disabled in the config")
    if cycle_errors(cycle):
        return StepResult("kernels", SKIPPED, "Skipped because of errors earlier in this cycle")

    running = ctx.host.running_kernel()
    if parse_release(running) is None:
        return StepResult("kernels", WARN, "Left alone", [f"Could not parse kernel {running}"])

    packages = _installed_packages(ctx)
    names = [
        match.group(1)
        for package, status in packages.items()
        if status in ("ii", "hi") and (match := _VERSIONED_IMAGE.match(package))
    ]
    releases = [release for release in map(parse_release, names) if release]

    if running not in names:
        return StepResult(
            "kernels",
            WARN,
            "Left alone",
            [f"The running kernel {running} isn't from a package, so nothing was removed"],
        )
    newest = newest_of_flavour(names, running)
    if newest != running:
        return StepResult(
            "kernels",
            WARN,
            "Left alone until the newest kernel has booted",
            [f"Running {running}, but {newest} is installed"],
        )

    kept = choose_kept(releases, running, settings.keep_previous)
    targets = removal_targets(packages, releases, kept)
    details = {"kept": sorted(kept), "targets": targets}
    if not targets:
        return StepResult("kernels", OK, f"Nothing to remove, keeping {len(kept)}", details=details)

    simulated = ctx.host.run(["apt-get", "-s", "purge", *targets], timeout=300, env=APT_ENV)
    if not simulated.ok:
        return StepResult(
            "kernels", ERROR, "Simulation failed", errors=[simulated.tail()], details=details
        )
    installs, removals = parse_simulation(simulated.out)
    extras = sorted(set(removals) - set(targets))
    if installs or extras:
        return StepResult(
            "kernels",
            WARN,
            "Left alone because the removal would affect other packages",
            [f"Simulation would also change: {', '.join(sorted(installs + extras))}"],
            details=details,
        )

    if ctx.dry_run:
        return StepResult("kernels", OK, f"Would remove {len(targets)} packages", details=details)

    command = ["apt-get", "-y", "-q", *LOCK_WAIT, "purge", *targets]
    with ctx.ui.working(f"Removing {len(targets)} old kernel packages"):
        purged = ctx.host.run(command, mutates=True, timeout=1800, env=APT_ENV)
    if not purged.ok:
        return StepResult(
            "kernels", ERROR, "apt-get purge failed", errors=[purged.tail()], details=details
        )
    return StepResult("kernels", OK, f"Removed {len(targets)} packages", details=details)
