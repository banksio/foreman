import os

import pytest
from conftest import GB, K2712, Usage
from typer.testing import CliRunner

from hostforeman import apt, config, disk, preflight
from hostforeman.cli import app
from hostforeman.host import Result, Runner
from hostforeman.models import BLOCKED, ERROR, OK, WARN
from hostforeman.state import LockBusy, State, run_lock

LOW = Usage(100 * GB, 99 * GB + 800 * 1024**2, 200 * 1024**2)


# --- apt


def test_upgrade_is_unattended_and_keeps_config_files(bench):
    b = bench()
    b.runner.on(["apt-get", "-s", "full-upgrade"], Result(0, "Inst curl [1] (2 x)\n"))
    assert apt.step(b.ctx).status == OK
    real = next(call for call in b.runner.calls if "full-upgrade" in call and "-y" in call)
    assert "Dpkg::Options::=--force-confold" in real
    assert "DPkg::Lock::Timeout=300" in real


def test_up_to_date_does_not_upgrade(bench):
    b = bench()
    assert apt.step(b.ctx).summary == "Already up to date"
    assert not b.runner.called("-y", "full-upgrade")


def test_interrupted_dpkg_is_finished_first(bench):
    b = bench()
    b.touch("/var/lib/dpkg/updates/0001")
    result = apt.step(b.ctx)
    assert result.status == WARN
    assert b.runner.index("--configure", "-a") < b.runner.index("update")


def test_failed_upgrade_repairs_and_retries_once(bench):
    b = bench()
    b.runner.on(["apt-get", "-s", "full-upgrade"], Result(0, "Inst curl [1] (2 x)\n"))
    attempts = []

    def flaky(args):
        attempts.append(args)
        return Result(100, "", "dpkg died") if len(attempts) == 1 else Result(0)

    b.runner.on(["apt-get", "-y"], flaky)
    assert apt.step(b.ctx).status == WARN
    assert len(attempts) == 2 and b.runner.called("--configure", "-a")


def test_upgrade_that_fails_twice_is_an_error(bench):
    b = bench()
    b.runner.on(["apt-get", "-s", "full-upgrade"], Result(0, "Inst curl [1] (2 x)\n"))
    b.runner.on(["apt-get", "-y"], Result(100, "", "dpkg died"))
    assert apt.step(b.ctx).status == ERROR


def test_rebuilt_running_kernel_is_remembered_for_this_boot(bench):
    b = bench()
    b.touch("/proc/sys/kernel/random/boot_id")
    (b.root / "proc/sys/kernel/random/boot_id").write_text("abc\n")
    b.runner.on(["apt-get", "-s", "full-upgrade"], Result(0, f"Inst linux-image-{K2712} [1] (2 x)\n"))
    apt.step(b.ctx)
    assert b.ctx.state["kernel_updated_boot"] == "abc"


def test_parse_simulation():
    output = "Inst a [1] (2 x)\nConf a (2 x)\nRemv b [1]\nPurg c [1]\n"
    assert apt.parse_simulation(output) == (["a"], ["b", "c"])


# --- preflight


def test_preflight_passes(bench):
    assert preflight.step(bench().ctx).status == OK


def test_current_undervoltage_postpones(bench):
    b = bench()
    b.runner.on(["vcgencmd"], Result(0, "throttled=0x50005\n"))
    assert preflight.step(b.ctx).status == BLOCKED


def test_past_undervoltage_only_warns(bench):
    b = bench()
    b.runner.on(["vcgencmd"], Result(0, "throttled=0x50000\n"))
    assert preflight.step(b.ctx).status == WARN


def test_no_vcgencmd_is_fine_on_other_hardware(bench):
    b = bench()
    b.runner.on(["vcgencmd"], Result(127))
    assert preflight.step(b.ctx).status == OK


def test_unsynced_clock_postpones(bench):
    b = bench()
    b.runner.on(["timedatectl"], Result(0, "no\n"))
    assert "clock" in preflight.step(b.ctx).summary


def test_clock_check_can_be_disabled(bench):
    b = bench({"preflight": {"require_clock_sync": False}})
    b.runner.on(["timedatectl"], Result(0, "no\n"))
    assert preflight.step(b.ctx).status == OK


def test_unreachable_network_postpones(bench):
    b = bench()
    b.unreachable["archive.raspberrypi.com"] = "DNS lookup failed"
    assert "archive.raspberrypi.com" in preflight.step(b.ctx).summary


def test_low_space_tries_apt_clean_then_postpones(bench):
    b = bench()
    b.usage["/"] = LOW
    assert preflight.step(b.ctx).status == BLOCKED
    assert b.runner.called("apt-get", "clean")


def test_apt_clean_can_rescue_low_space(bench):
    b = bench()
    b.usage["/"] = LOW

    def clean(args):
        b.usage.pop("/")
        return Result(0)

    b.runner.on(["apt-get", "clean"], clean)
    assert preflight.step(b.ctx).status == OK


def test_low_boot_partition_postpones(bench):
    b = bench()
    (b.root / "boot/firmware").mkdir(parents=True)
    b.usage["/boot/firmware"] = Usage(GB // 2, GB // 2 - 50 * 1024**2, 50 * 1024**2)
    assert "/boot/firmware" in preflight.step(b.ctx).summary


def test_no_apt_is_an_error(bench):
    b = bench()
    b.runner.on(["apt-get", "--version"], Result(127))
    assert preflight.step(b.ctx).status == ERROR


# --- disk


def test_disk_report_flags_full_filesystems(bench):
    b = bench()
    b.usage["/"] = Usage(100 * GB, 90 * GB, 10 * GB)
    result = disk.step(b.ctx)
    assert result.status == WARN and "90%" in result.warnings[0]


def test_disk_critical_threshold(bench):
    b = bench()
    b.usage["/"] = Usage(100 * GB, 97 * GB, 3 * GB)
    assert "critically" in disk.step(b.ctx).warnings[0]


def test_disk_ignores_absent_paths_and_duplicate_filesystems(bench):
    b = bench({"disk": {"paths": ["/", "/var", "/nope"]}})
    (b.root / "var").mkdir()
    assert [row["path"] for row in disk.step(b.ctx).details["rows"]] == ["/"]


def test_disk_includes_docker_root_on_its_own_filesystem(bench):
    b = bench()
    (b.root / "mnt/docker").mkdir(parents=True)
    b.usage["/mnt/docker"] = Usage(500 * GB, 50 * GB, 450 * GB)
    b.runner.on(["docker", "info"], Result(0, "/mnt/docker\n"))
    assert "/mnt/docker" in [row["path"] for row in disk.step(b.ctx).details["rows"]]


def test_disk_consumers_sorted_largest_first(bench):
    b = bench({"disk": {"consumers": ["/var/log", "/home"]}})
    b.touch("/var/log/x")
    b.touch("/home/x")
    b.runner.on(["du", "-sxk", "/var/log"], Result(0, "1048576\t/var/log\n"))
    b.runner.on(["du", "-sxk", "/home"], Result(1, "5242880\t/home\n"))
    consumers = disk.step(b.ctx).details["consumers"]
    assert [row["path"] for row in consumers] == ["/home", "/var/log"]


# --- runner, state, lock and config


def test_dry_run_runner_does_not_execute_mutations():
    announced = []
    runner = Runner(dry_run=True, on_would=announced.append)
    assert runner(["definitely-not-a-command"], mutates=True).ok
    assert announced == [["definitely-not-a-command"]]


def test_runner_reports_missing_commands():
    assert Runner()(["definitely-not-a-command"]).missing


def test_runner_reports_timeouts():
    assert Runner()(["sleep", "5"], timeout=0.05).code == 124


def test_state_round_trip_and_no_persist(tmp_path):
    path = tmp_path / "state.json"
    state = State(path)
    state["rebooted_for"] = "x"
    state.save()
    assert State(path)["rebooted_for"] == "x"
    assert not path.with_suffix(".tmp").exists()

    other = tmp_path / "other.json"
    State(other, persist=False).save()
    assert not other.exists()


def test_second_lock_is_refused_and_released(tmp_path):
    path = tmp_path / "lock"
    with run_lock(path):
        with pytest.raises(LockBusy):
            with run_lock(path):
                pass
    with run_lock(path):
        pass


@pytest.mark.parametrize(
    "raw",
    [
        {"nonsense": {}},
        {"reboot": {"nonsense": 1}},
        {"reboot": {"window": "3am"}},
        {"reboot": {"window": "03:00-03:00"}},
        {"kernels": {"keep_previous": True}},
        {"kernels": {"keep_previous": -1}},
        {"compose": {"exclude": "not-a-list"}},
        {"compose": {"exclude": [1]}},
    ],
)
def test_bad_config_is_rejected(raw):
    with pytest.raises(config.ConfigError):
        config.from_dict(raw)


def test_example_config_matches_the_defaults():
    path = os.path.join(os.path.dirname(__file__), "..", "config.example.toml")
    from pathlib import Path

    assert config.load(Path(path)) == config.Config()


def test_missing_config_means_defaults(tmp_path):
    assert config.load(tmp_path / "absent.toml") == config.Config()


# --- command line


@pytest.fixture
def cli_env(tmp_path, monkeypatch):
    monkeypatch.setenv("FOREMAN_CONFIG", str(tmp_path / "config.toml"))
    monkeypatch.setenv("FOREMAN_STATE_DIR", str(tmp_path / "state"))
    monkeypatch.setenv("FOREMAN_LOCK", str(tmp_path / "lock"))
    return tmp_path


def test_status_with_no_history(cli_env):
    result = CliRunner().invoke(app, ["status"])
    assert result.exit_code == 0, result.output
    assert "none recorded" in result.output


def test_status_shows_a_recorded_step(cli_env):
    state = State(cli_env / "state" / "state.json")
    state["last_steps"]["disk"] = {
        "name": "disk", "status": "warning", "summary": "/ 90%",
        "warnings": ["/ is 90% full"], "errors": [], "details": {}, "at": "2026-11-08T03:30:00",
    }
    state.save()
    result = CliRunner().invoke(app, ["status"])
    assert result.exit_code == 0, result.output
    assert "/ is 90% full" in result.output


def test_status_prints_command_output_literally(cli_env):
    state = State(cli_env / "state" / "state.json")
    state["last_steps"]["disk"] = {
        "name": "disk", "status": "error", "summary": "[/red] [bold]x",
        "warnings": [], "errors": ["[/nonsense]"], "details": {}, "at": "t",
    }
    state.save()
    result = CliRunner().invoke(app, ["status"])
    assert result.exit_code == 0, result.output
    assert "[/nonsense]" in result.output


def test_bad_config_exits_cleanly(cli_env):
    (cli_env / "config.toml").write_text("[nonsense]\n")
    result = CliRunner().invoke(app, ["status"])
    assert result.exit_code == 2
    assert "unknown section" in result.output


@pytest.mark.skipif(os.geteuid() == 0, reason="needs a non-root user")
def test_real_run_needs_root(cli_env):
    result = CliRunner().invoke(app, ["run", "disk"])
    assert result.exit_code == 2
    assert "root" in result.output


def test_unknown_step_is_rejected(cli_env):
    assert CliRunner().invoke(app, ["run", "nonsense"]).exit_code == 2


def test_stacks_callback_lists_without_a_subcommand(cli_env, monkeypatch):
    from hostforeman import compose

    monkeypatch.setattr(compose, "discover", lambda ctx: [])
    result = CliRunner().invoke(app, ["stacks"])
    assert result.exit_code == 0, result.output
    assert "Project" in result.output
