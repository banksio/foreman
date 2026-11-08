from datetime import datetime

import pytest
from conftest import K2712

from hostforeman import reboot, window
from hostforeman.host import Result
from hostforeman.models import BLOCKED, ERROR, OK, REBOOTING, SKIPPED, WAITING, WARN

OLD = "6.12.47+rpt-rpi-2712"


def needs_reboot(bench, **kwargs):
    b = bench(**kwargs)
    b.running = OLD
    b.add_modules(OLD, K2712)
    return b


def fresh_cycle():
    return {"steps": {}, "phase": "reboot", "rebooted": False, "expected_kernel": None}


def test_no_reboot_when_running_the_newest(bench):
    b = bench()
    b.add_modules(OLD, K2712)
    assert not reboot.inspect(b.ctx).required


def test_flag_file_means_reboot(bench):
    b = bench()
    b.touch("/run/reboot-required")
    assert reboot.inspect(b.ctx).required


def test_newer_kernel_of_the_same_flavour_means_reboot(bench):
    info = reboot.inspect(needs_reboot(bench).ctx)
    assert info.required and info.newest == K2712


def test_newer_kernel_of_another_flavour_is_ignored(bench):
    b = bench()
    b.add_modules(K2712, "6.12.99+rpt-rpi-v8")
    assert not reboot.inspect(b.ctx).required


def test_running_kernel_rebuilt_in_place_means_reboot_this_boot_only(bench):
    b = bench()
    b.add_modules(K2712)
    b.root.joinpath("proc/sys/kernel/random").mkdir(parents=True)
    boot_id = b.root / "proc/sys/kernel/random/boot_id"
    boot_id.write_text("first\n")
    b.ctx.state["kernel_updated_boot"] = "first"
    assert reboot.inspect(b.ctx).required
    boot_id.write_text("second\n")
    assert not reboot.inspect(b.ctx).required


@pytest.mark.parametrize(
    "spec,hour,minute,expected",
    [
        ("03:00-05:00", 3, 0, True),
        ("03:00-05:00", 4, 59, True),
        ("03:00-05:00", 5, 0, False),
        ("03:00-05:00", 2, 59, False),
        ("23:00-02:00", 23, 30, True),
        ("23:00-02:00", 1, 0, True),
        ("23:00-02:00", 12, 0, False),
        ("", 12, 0, True),
    ],
)
def test_window(spec, hour, minute, expected):
    assert window.contains(spec, datetime(2026, 1, 1, hour, minute)) is expected


def test_reboot_saves_everything_before_rebooting(bench):
    b = needs_reboot(bench)
    cycle = fresh_cycle()
    b.ctx.state["cycle"] = cycle
    seen = {}

    def on_reboot(args):
        seen["state"] = b.saved()
        seen["unit"] = b.unit.exists()
        return Result(0)

    b.runner.on(["systemctl", "reboot"], on_reboot)
    assert reboot.step(b.ctx, cycle).status == REBOOTING
    assert seen["unit"]
    assert seen["state"]["cycle"]["phase"] == "verify"
    assert seen["state"]["cycle"]["expected_kernel"] == K2712
    assert seen["state"]["rebooted_for"] == K2712
    assert seen["state"]["cycle"]["steps"]["reboot"]["status"] == REBOOTING


def test_resume_unit_calls_foreman_resume(bench):
    b = needs_reboot(bench)
    cycle = fresh_cycle()
    b.ctx.state["cycle"] = cycle
    reboot.step(b.ctx, cycle)
    assert 'resume\n' in b.unit.read_text()
    assert b.runner.called("systemctl", "enable", "foreman-resume.service")


def test_failed_reboot_cleans_up_and_is_an_error(bench):
    b = needs_reboot(bench)
    cycle = fresh_cycle()
    b.ctx.state["cycle"] = cycle
    b.runner.on(["systemctl", "reboot"], Result(1, "", "denied"))
    assert reboot.step(b.ctx, cycle).status == ERROR
    assert not b.unit.exists()
    assert cycle["phase"] == "reboot" and cycle["rebooted"] is False


def test_no_reboot_flag(bench):
    b = needs_reboot(bench, no_reboot=True)
    assert reboot.step(b.ctx).status == WARN
    assert not b.runner.called("systemctl", "reboot")


def blocked(bench_instance, cycle=None):
    result = reboot.step(bench_instance.ctx, cycle)
    assert result.status == BLOCKED
    assert not bench_instance.runner.called("systemctl", "reboot")
    return result.summary


def test_blocked_by_errors_this_cycle(bench):
    assert "error" in blocked(needs_reboot(bench), {"steps": {"upgrade": {"errors": ["x"]}}})


def test_blocked_when_watched_service_is_not_enabled(bench):
    b = needs_reboot(bench)
    b.runner.on(["systemctl", "is-enabled"], Result(1, "disabled\n"))
    assert "ssh is not enabled" in blocked(b)


def test_socket_activated_ssh_counts_as_enabled(bench):
    b = needs_reboot(bench)
    b.runner.on(["systemctl", "is-enabled", "ssh"], Result(1))
    b.runner.on(["systemctl", "is-enabled", "ssh.socket"], Result(0))
    assert reboot.step(b.ctx).status == REBOOTING


def test_blocked_by_invalid_sshd_config(bench):
    b = needs_reboot(bench)
    b.runner.on(["sshd", "-t"], Result(255, "", "bad line"))
    assert "sshd" in blocked(b)


def test_missing_sshd_does_not_block(bench):
    b = needs_reboot(bench)
    b.runner.on(["sshd", "-t"], Result(127))
    assert reboot.step(b.ctx).status == REBOOTING


def test_loop_guard_stops_a_second_reboot_for_the_same_kernel(bench):
    b = needs_reboot(bench)
    b.ctx.state["rebooted_for"] = K2712
    assert "already rebooted" in blocked(b)


def test_loop_guard_does_not_block_an_explicit_flag(bench):
    b = needs_reboot(bench)
    b.ctx.state["rebooted_for"] = K2712
    b.touch("/run/reboot-required")
    assert reboot.step(b.ctx).status == REBOOTING


def test_outside_window_waits_and_schedules_the_window_start(bench):
    b = needs_reboot(bench)
    b.moment = datetime(2026, 11, 8, 12, 0)
    result = reboot.step(b.ctx, fresh_cycle())
    assert result.status == WAITING
    assert b.runner.called("--on-calendar=*-*-* 03:00:00")
    assert not b.runner.called("systemctl", "reboot")


def test_single_step_outside_window_does_not_schedule(bench):
    b = needs_reboot(bench)
    b.moment = datetime(2026, 11, 8, 12, 0)
    assert reboot.step(b.ctx).status == WAITING
    assert not b.runner.called("systemd-run")


def test_reboot_now_overrides_the_window_only(bench):
    b = needs_reboot(bench, reboot_now=True)
    b.moment = datetime(2026, 11, 8, 12, 0)
    assert reboot.step(b.ctx).status == REBOOTING

    blocked_bench = needs_reboot(bench, reboot_now=True)
    blocked_bench.runner.on(["sshd", "-t"], Result(255, "", "bad"))
    assert reboot.step(blocked_bench.ctx).status == BLOCKED


def test_verify_skipped_without_a_reboot(bench):
    assert reboot.verify(bench().ctx, fresh_cycle()).status == SKIPPED


def test_verify_confirms_expected_kernel(bench):
    cycle = {**fresh_cycle(), "rebooted": True, "expected_kernel": K2712}
    assert reboot.verify(bench().ctx, cycle).status == OK


def test_verify_warns_on_wrong_kernel(bench):
    b = bench()
    cycle = {**fresh_cycle(), "rebooted": True, "expected_kernel": "6.99.0+rpt-rpi-2712"}
    assert reboot.verify(b.ctx, cycle).status == WARN


def test_verify_waits_for_the_network(bench):
    b = bench()
    b.unreachable["deb.debian.org"] = "DNS lookup failed"
    cycle = {**fresh_cycle(), "rebooted": True, "expected_kernel": K2712}
    result = reboot.verify(b.ctx, cycle)
    assert result.status == WARN and b.elapsed >= 300
