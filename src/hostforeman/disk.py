"""Item 6: disk usage and what is using it."""

from __future__ import annotations

from .models import OK, WARN, Context, StepResult

_GB = 1024**3


def usage_rows(ctx: Context, paths: list[str]) -> list[dict]:
    """One row per filesystem. Paths that are missing or share a filesystem are dropped."""
    rows: list[dict] = []
    seen: set[tuple[int, int]] = set()
    for path in paths:
        if not ctx.host.path(path).exists():
            continue
        try:
            usage = ctx.host.disk_usage(path)
        except OSError:
            continue
        identity = (usage.total, usage.free)
        if identity in seen:
            continue
        seen.add(identity)
        # The same sum df uses, since a share of ext4 is reserved for root.
        capacity = usage.used + usage.free
        percent = round(100 * usage.used / capacity) if capacity else 0
        rows.append(
            {
                "path": path,
                "total_gb": round(usage.total / _GB, 1),
                "used_gb": round(usage.used / _GB, 1),
                "free_gb": round(usage.free / _GB, 1),
                "percent": percent,
            }
        )
    return rows


def _docker_root(ctx: Context) -> str | None:
    result = ctx.host.run(["docker", "info", "--format", "{{.DockerRootDir}}"], timeout=30)
    return result.out.strip() or None if result.ok else None


def _consumers(ctx: Context) -> list[dict]:
    rows = []
    for path in ctx.config.disk.consumers:
        if not ctx.host.path(path).exists():
            continue
        result = ctx.host.run(["du", "-sxk", path], timeout=600)
        try:  # du exits non-zero for one unreadable file but still prints a useful total.
            kilobytes = int(result.out.split()[0])
        except (IndexError, ValueError):
            continue
        rows.append({"path": path, "gb": round(kilobytes / 1024**2, 2)})
    return sorted(rows, key=lambda row: row["gb"], reverse=True)


def step(ctx: Context, cycle: dict | None = None) -> StepResult:
    settings = ctx.config.disk
    paths = list(settings.paths)
    docker_root = _docker_root(ctx)
    if docker_root:
        paths.append(docker_root)

    with ctx.ui.working("Measuring disk usage"):
        rows = usage_rows(ctx, paths)
        consumers = _consumers(ctx)

    warnings = []
    for row in rows:
        if row["percent"] >= settings.critical_percent:
            warnings.append(f"{row['path']} is critically full at {row['percent']}%")
        elif row["percent"] >= settings.warn_percent:
            warnings.append(f"{row['path']} is {row['percent']}% full")

    report = {
        "at": ctx.host.now().isoformat(timespec="seconds"),
        "rows": rows,
        "consumers": consumers,
    }
    ctx.state["disk"] = report
    summary = ", ".join(f"{row['path']} {row['percent']}%" for row in rows) or "No disks measured"
    return StepResult("disk", WARN if warnings else OK, summary, warnings, details=report)
