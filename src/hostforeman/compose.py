"""Items 5, 14, 15 and 16: updating running Compose stacks, with care for their data."""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field

from .models import ERROR, OK, SKIPPED, WARN, Context, StepResult

_DB_IMAGE = re.compile(
    r"postgres|postgis|mysql|mariadb|mongo|redis|valkey|timescale|cockroach|influxdb"
    r"|clickhouse|elasticsearch|opensearch|couchdb|cassandra",
    re.IGNORECASE,
)
_DB_MOUNTS = (
    "/var/lib/postgresql",
    "/var/lib/mysql",
    "/data/db",
    "/var/lib/redis",
    "/var/lib/influxdb",
    "/var/lib/clickhouse",
    "/var/lib/cassandra",
    "/usr/share/elasticsearch/data",
    "/opt/couchdb/data",
)
_PROJECT = "com.docker.compose.project"
_SERVICE = "com.docker.compose.service"
_WORKDIR = "com.docker.compose.project.working_dir"
_FILES = "com.docker.compose.project.config_files"

UPDATE = "update"
EXCLUDED = "excluded"
DATABASE = "database"
HELD = "held"


@dataclass
class Container:
    id: str
    service: str
    image_ref: str
    image_id: str
    running: bool
    health: str | None
    mounts: list[str]

    @property
    def healthy(self) -> bool:
        return self.running and self.health in (None, "none", "healthy")


@dataclass
class Stack:
    project: str
    workdir: str
    files: list[str]
    containers: list[Container] = field(default_factory=list)

    @property
    def services(self) -> list[str]:
        return sorted({container.service for container in self.containers})

    def base_command(self) -> list[str]:
        command = ["docker", "compose", "-p", self.project, "--project-directory", self.workdir]
        for file in self.files:
            command += ["-f", file]
        return command


class DockerError(Exception):
    pass


def _inspect(ctx: Context, ids: list[str]) -> list[dict]:
    result = ctx.host.run(["docker", "inspect", *ids], timeout=120)
    if not result.ok:
        raise DockerError(result.tail())
    return json.loads(result.out)


def _to_container(raw: dict) -> Container:
    state = raw.get("State") or {}
    config = raw.get("Config") or {}
    labels = config.get("Labels") or {}
    return Container(
        id=raw["Id"],
        service=labels.get(_SERVICE, ""),
        image_ref=config.get("Image", ""),
        image_id=raw.get("Image", ""),
        running=bool(state.get("Running")) and not state.get("Restarting"),
        health=(state.get("Health") or {}).get("Status"),
        mounts=[mount.get("Destination", "") for mount in raw.get("Mounts") or []],
    )


def _project_ids(ctx: Context, label: str, *extra: str) -> list[str]:
    result = ctx.host.run(["docker", "ps", "-q", "--filter", label, *extra], timeout=60)
    if not result.ok:
        raise DockerError(result.tail())
    return result.out.split()


def discover(ctx: Context) -> list[Stack]:
    ids = _project_ids(ctx, f"label={_PROJECT}")
    stacks: dict[str, Stack] = {}
    if not ids:
        return []
    for raw in _inspect(ctx, ids):
        labels = (raw.get("Config") or {}).get("Labels") or {}
        project = labels[_PROJECT]
        workdir = labels.get(_WORKDIR, "")
        files = [
            file if file.startswith("/") else f"{workdir}/{file}"
            for file in labels.get(_FILES, "").split(",")
            if file
        ]
        stack = stacks.setdefault(project, Stack(project, workdir, files))
        stack.containers.append(_to_container(raw))
    return sorted(stacks.values(), key=lambda stack: stack.project)


def uses_database(stack: Stack) -> bool:
    for container in stack.containers:
        if _DB_IMAGE.search(container.image_ref):
            return True
        if any(mount.startswith(_DB_MOUNTS) for mount in container.mounts):
            return True
    return False


def classify(ctx: Context, stack: Stack, known: list[str] | None) -> str:
    settings = ctx.config.compose
    names = {stack.project, stack.workdir}
    if names & set(settings.exclude):
        return EXCLUDED
    if uses_database(stack) and not names & set(settings.allow_databases):
        return DATABASE
    if known is not None and stack.project not in known:
        return HELD
    return UPDATE


def _image_id(ctx: Context, ref: str) -> str | None:
    result = ctx.host.run(["docker", "image", "inspect", "--format", "{{.Id}}", ref], timeout=60)
    return result.out.strip() or None if result.ok else None


def _wait_healthy(ctx: Context, stack: Stack, services: list[str]) -> bool:
    """True once every updated container has stayed healthy for the settle period."""
    settings = ctx.config.compose
    deadline = ctx.host.monotonic() + settings.health_timeout
    stable_since: float | None = None
    while True:
        now = ctx.host.monotonic()
        try:
            ids = _project_ids(ctx, f"label={_PROJECT}={stack.project}", "-a")
            containers = [_to_container(raw) for raw in _inspect(ctx, ids)] if ids else []
        except DockerError:
            containers = []
        current = [container for container in containers if container.service in services]
        if {container.service for container in current} >= set(services) and all(
            container.healthy for container in current
        ):
            stable_since = now if stable_since is None else stable_since
            if now - stable_since >= settings.settle:
                return True
        else:
            stable_since = None
        if now >= deadline:
            return False
        ctx.host.sleep(3)


def _rollback(ctx: Context, stack: Stack, changes: dict[str, tuple[str, str, str]]) -> bool:
    for old_id, ref, _ in changes.values():
        ctx.host.run(["docker", "tag", old_id, ref], mutates=True, timeout=60)
    services = sorted(changes)
    up = ctx.host.run(
        [*stack.base_command(), "up", "-d", "--no-deps", "--no-build", *services],
        mutates=True,
        timeout=900,
    )
    return up.ok and _wait_healthy(ctx, stack, services)


def _update(ctx: Context, stack: Stack) -> tuple[str, str]:
    """Returns an (outcome, message) pair for one stack."""
    for file in stack.files:
        if not ctx.host.path(file).exists():
            return "failed", f"{file} no longer exists"
    if not stack.files:
        return "failed", "its Compose file isn't recorded on the containers"

    healthy_before = all(container.healthy for container in stack.containers)
    pull = ctx.host.run(
        [*stack.base_command(), "pull", "--ignore-buildable", "--quiet", *stack.services],
        mutates=True,
        timeout=3600,
    )
    if not pull.ok:
        return "failed", f"pull failed: {pull.tail(3)}"
    if ctx.dry_run:
        return "would_update", "would pull and recreate any changed services"

    changes: dict[str, tuple[str, str, str]] = {}
    for container in stack.containers:
        new_id = _image_id(ctx, container.image_ref)
        if new_id and new_id != container.image_id:
            changes[container.service] = (container.image_id, container.image_ref, new_id)
    if not changes:
        return "current", "already up to date"

    fingerprint = sorted(f"{service}={change[2]}" for service, change in changes.items())
    bad_images = ctx.state["bad_images"]
    if bad_images.get(stack.project) == fingerprint:
        for old_id, ref, _ in changes.values():
            ctx.host.run(["docker", "tag", old_id, ref], mutates=True, timeout=60)
        return "skipped_bad", "the newest images already failed here, so they were not retried"

    services = sorted(changes)
    up = ctx.host.run(
        [*stack.base_command(), "up", "-d", "--no-deps", "--no-build", *services],
        mutates=True,
        timeout=900,
    )
    if up.ok and _wait_healthy(ctx, stack, services):
        bad_images.pop(stack.project, None)
        return "updated", f"updated {', '.join(services)}"

    if not healthy_before:
        return "failed", "the update didn't become healthy, and it was unhealthy before too"
    if _rollback(ctx, stack, changes):
        bad_images[stack.project] = fingerprint
        return "rolled_back", f"{', '.join(services)} failed health checks, old images restored"
    return "failed", "the update failed and the rollback did not recover it"


def _prune(ctx: Context, warnings: list[str]) -> list[str]:
    settings = ctx.config.compose
    done = []
    if settings.prune_dangling:
        if ctx.host.run(["docker", "image", "prune", "-f"], mutates=True, timeout=900).ok:
            done.append("dangling images pruned")
        else:
            warnings.append("Pruning dangling images failed")
    if settings.prune_tagged_days > 0:
        hours = settings.prune_tagged_days * 24
        command = ["docker", "image", "prune", "-af", "--filter", f"until={hours}h"]
        if ctx.host.run(command, mutates=True, timeout=900).ok:
            done.append(f"unused images over {settings.prune_tagged_days} days old pruned")
        else:
            warnings.append("Pruning old tagged images failed")
    return done


def step(ctx: Context, cycle: dict | None = None) -> StepResult:
    if not ctx.config.compose.enabled:
        return StepResult("compose", SKIPPED, "Disabled in the config")
    if ctx.host.run(["docker", "--version"], timeout=15).missing:
        return StepResult("compose", SKIPPED, "Docker isn't installed")
    if not ctx.host.run(["docker", "compose", "version"], timeout=15).ok:
        return StepResult("compose", ERROR, "Docker Compose isn't available", errors=["docker compose failed"])

    try:
        stacks = discover(ctx)
    except (DockerError, ValueError, KeyError) as exc:
        return StepResult("compose", ERROR, "Could not read running stacks", errors=[str(exc)])
    if not stacks:
        return StepResult("compose", OK, "No running stacks")

    known = ctx.state["known_stacks"]
    warnings: list[str] = []
    errors: list[str] = []
    rows: list[dict] = []
    counts: dict[str, int] = {}

    for stack in stacks:
        action = classify(ctx, stack, known)
        if action == UPDATE:
            with ctx.ui.working(f"Updating {stack.project}"):
                outcome, message = _update(ctx, stack)
        else:
            outcome = action
            message = {
                EXCLUDED: "excluded in the config",
                DATABASE: "uses a database, add it to allow_databases to update it",
                HELD: "new stack held for review, see foreman stacks",
            }[action]
        counts[outcome] = counts.get(outcome, 0) + 1
        rows.append({"project": stack.project, "workdir": stack.workdir, "outcome": outcome, "message": message})
        if outcome == "failed":
            errors.append(f"{stack.project}: {message}")
        elif outcome in ("rolled_back", "skipped_bad", HELD):
            warnings.append(f"{stack.project}: {message}")

    pruned = _prune(ctx, warnings)
    if known is None and not ctx.dry_run:
        # The first real run adopts what exists. Only stacks appearing later are held.
        ctx.state["known_stacks"] = [stack.project for stack in stacks]
    ctx.state["stacks"] = rows

    summary = ", ".join(f"{count} {name.replace('_', ' ')}" for name, count in sorted(counts.items()))
    if pruned:
        summary += f"; {', '.join(pruned)}"
    status = ERROR if errors else WARN if warnings else OK
    return StepResult("compose", status, summary, warnings, errors, {"stacks": rows})
