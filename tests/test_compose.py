import json

from hostforeman import compose
from hostforeman.host import Result
from hostforeman.models import ERROR, OK, SKIPPED, WARN

OLD = "sha256:old"
NEW = "sha256:new"


class FakeDocker:
    """A tiny Docker: pull moves tags to `pulled`, and up recreates containers from the tags."""

    def __init__(self, bench_instance, compose_dir="/srv/app"):
        self.b = bench_instance
        self.dir = compose_dir
        self.containers = []
        self.tags = {}
        self.pulled = {}
        self.broken = set()  # image ids that never become healthy
        self.up_calls = []
        b = bench_instance
        b.touch(f"{compose_dir}/compose.yaml")
        b.runner.on(["docker", "--version"], Result(0, "Docker 29"))
        b.runner.on(["docker", "compose"], self._compose)
        b.runner.on(["docker", "ps"], self._ps)
        b.runner.on(["docker", "inspect"], self._inspect)
        b.runner.on(["docker", "image", "inspect"], self._image)
        b.runner.on(["docker", "image", "prune"], Result(0))
        b.runner.on(["docker", "tag"], self._tag)

    def add(self, project, service, ref, image_id=OLD, mounts=(), health=None, workdir=None):
        workdir = workdir or self.dir
        self.tags[ref] = image_id
        self.containers.append(
            {
                "Id": f"{project}-{service}",
                "Image": image_id,
                "State": {"Running": True, "Restarting": False, "Health": {"Status": health} if health else None},
                "Mounts": [{"Destination": mount} for mount in mounts],
                "Config": {
                    "Image": ref,
                    "Labels": {
                        "com.docker.compose.project": project,
                        "com.docker.compose.service": service,
                        "com.docker.compose.project.working_dir": workdir,
                        "com.docker.compose.project.config_files": f"{workdir}/compose.yaml",
                    },
                },
            }
        )

    def _ps(self, args):
        filter_value = args[args.index("--filter") + 1]
        wanted = filter_value.removeprefix("label=")
        project = wanted.split("=", 1)[1] if "=" in wanted else None
        ids = [
            c["Id"]
            for c in self.containers
            if project is None or c["Config"]["Labels"]["com.docker.compose.project"] == project
        ]
        return Result(0, "\n".join(ids))

    def _inspect(self, args):
        selected = [c for c in self.containers if c["Id"] in args[2:]]
        return Result(0, json.dumps(selected))

    def _image(self, args):
        return Result(0, self.tags.get(args[-1], "") + "\n")

    def _tag(self, args):
        self.tags[args[3]] = args[2]
        return Result(0)

    def _compose(self, args):
        if "pull" in args:
            self.tags.update(self.pulled)
        if "up" in args:
            services = args[args.index("--no-build") + 1 :]
            project = args[args.index("-p") + 1]
            self.up_calls.append((project, services))
            for container in self.containers:
                labels = container["Config"]["Labels"]
                if labels["com.docker.compose.project"] == project and labels["com.docker.compose.service"] in services:
                    container["Image"] = self.tags[container["Config"]["Image"]]
                    container["State"]["Running"] = container["Image"] not in self.broken
        return Result(0)


def setup(bench, raw=None, **kwargs):
    b = bench(raw, **kwargs)
    return b, FakeDocker(b)


def test_updates_only_changed_services(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    docker.add("app", "worker", "acme/worker:1")
    docker.pulled = {"nginx:1": NEW}
    result = compose.step(b.ctx)
    assert result.status == OK
    assert docker.up_calls == [("app", ["web"])]


def test_current_stack_is_not_recreated(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    result = compose.step(b.ctx)
    assert docker.up_calls == []
    assert "current" in result.summary


def test_database_by_image_is_excluded_and_never_pulled(bench):
    b, docker = setup(bench)
    docker.add("db", "main", "postgres:16")
    assert "1 database" in compose.step(b.ctx).summary
    assert not b.runner.called("pull")


def test_database_by_data_mount_is_excluded(bench):
    b, docker = setup(bench)
    docker.add("db", "main", "acme/custom-store:1", mounts=["/var/lib/postgresql/data"])
    assert compose.step(b.ctx).details["stacks"][0]["outcome"] == "database"


def test_allow_databases_opts_in(bench):
    b, docker = setup(bench, {"compose": {"allow_databases": ["db"]}})
    docker.add("db", "main", "postgres:16")
    docker.pulled = {"postgres:16": NEW}
    compose.step(b.ctx)
    assert docker.up_calls == [("db", ["main"])]


def test_excluded_by_directory(bench):
    b, docker = setup(bench, {"compose": {"exclude": ["/srv/app"]}})
    docker.add("app", "web", "nginx:1")
    assert compose.step(b.ctx).details["stacks"][0]["outcome"] == "excluded"
    assert not b.runner.called("pull")


def test_first_real_run_adopts_existing_stacks(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    docker.pulled = {"nginx:1": NEW}
    compose.step(b.ctx)
    assert docker.up_calls and b.ctx.state["known_stacks"] == ["app"]


def test_stack_appearing_later_is_held_and_flagged_every_run(bench):
    b, docker = setup(bench)
    b.ctx.state["known_stacks"] = ["app"]
    docker.add("newcomer", "web", "nginx:1")
    docker.pulled = {"nginx:1": NEW}
    for _ in range(2):
        result = compose.step(b.ctx)
        assert result.status == WARN and "newcomer" in result.warnings[0]
    assert docker.up_calls == []


def test_dry_run_does_not_set_the_baseline(bench):
    b, docker = setup(bench, dry_run=True)
    docker.add("app", "web", "nginx:1")
    compose.step(b.ctx)
    assert b.ctx.state["known_stacks"] is None


def test_dry_run_changes_nothing(bench):
    b, docker = setup(bench, dry_run=True)
    docker.add("app", "web", "nginx:1")
    docker.pulled = {"nginx:1": NEW}
    result = compose.step(b.ctx)
    assert result.details["stacks"][0]["outcome"] == "would_update"
    assert docker.up_calls == [] and docker.tags["nginx:1"] == OLD
    assert any("pull" in call for call in b.runner.would)


def test_unhealthy_update_rolls_back_to_old_images(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    docker.pulled = {"nginx:1": NEW}
    docker.broken = {NEW}
    result = compose.step(b.ctx)
    assert result.status == WARN
    assert result.details["stacks"][0]["outcome"] == "rolled_back"
    assert docker.tags["nginx:1"] == OLD
    assert docker.containers[0]["Image"] == OLD
    assert b.ctx.state["bad_images"]["app"] == [f"web={NEW}"]


def test_known_bad_images_are_not_retried(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    docker.pulled = {"nginx:1": NEW}
    b.ctx.state["bad_images"]["app"] = [f"web={NEW}"]
    result = compose.step(b.ctx)
    assert result.details["stacks"][0]["outcome"] == "skipped_bad"
    assert docker.up_calls == [] and docker.tags["nginx:1"] == OLD


def test_stack_already_unhealthy_is_not_rolled_back(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1", health="unhealthy")
    docker.pulled = {"nginx:1": NEW}
    docker.broken = {NEW}
    result = compose.step(b.ctx)
    assert result.status == ERROR
    assert len(docker.up_calls) == 1


def test_container_must_stay_healthy_through_the_settle_period(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    docker.pulled = {"nginx:1": NEW}
    compose.step(b.ctx)
    assert b.elapsed >= b.ctx.config.compose.settle


def test_missing_compose_file_is_an_error_for_that_stack_only(bench):
    b, docker = setup(bench)
    docker.add("gone", "web", "nginx:1", workdir="/srv/deleted")
    docker.add("app", "web", "acme/app:1")
    docker.pulled = {"acme/app:1": NEW}
    result = compose.step(b.ctx)
    assert result.status == ERROR and "gone" in result.errors[0]
    assert docker.up_calls == [("app", ["web"])]


def test_pull_failure_is_reported(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    original = docker._compose
    b.runner.on(
        ["docker", "compose"],
        lambda args: Result(1, "", "no network") if "pull" in args else original(args),
    )
    assert compose.step(b.ctx).status == ERROR


def test_prunes_dangling_by_default_and_tagged_only_if_enabled(bench):
    b, docker = setup(bench)
    docker.add("app", "web", "nginx:1")
    compose.step(b.ctx)
    assert b.runner.called("prune", "-f")
    assert not b.runner.called("prune", "-af")


def test_prunes_old_tagged_images_when_enabled(bench):
    b, docker = setup(bench, {"compose": {"prune_tagged_days": 30}})
    docker.add("app", "web", "nginx:1")
    compose.step(b.ctx)
    assert b.runner.called("prune", "-af", "until=720h")


def test_no_docker_is_skipped(bench):
    assert compose.step(bench().ctx).status == SKIPPED
