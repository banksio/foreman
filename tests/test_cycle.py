from datetime import datetime

from conftest import K2712

from hostforeman import cycle
from hostforeman.host import Result
from hostforeman.models import REBOOTING

OLD = "6.12.47+rpt-rpi-2712"
OLDEST = "6.12.25+rpt-rpi-2712"
UPGRADE_SIM = f"Inst linux-image-{K2712} (1 x)\nInst curl [1] (2 x)\n"


def after_upgrade(bench, **kwargs):
    """A Pi running an older kernel, with a newer one about to be installed."""
    b = bench(**kwargs)
    b.running = OLD
    b.add_modules(OLDEST, OLD, K2712)
    b.runner.on(["apt-get", "-s", "full-upgrade"], Result(0, UPGRADE_SIM))
    packages = "".join(f"ii  linux-image-{name}\n" for name in (OLDEST, OLD, K2712))
    b.runner.on(["dpkg-query"], Result(0, packages))
    b.runner.on(["apt-get", "-s", "purge"], Result(0, f"Purg linux-image-{OLDEST} [1]\n"))
    return b


def is_real_upgrade(args):
    return "full-upgrade" in args and "-s" not in args


def test_cycle_stops_at_reboot_then_resume_finishes_in_order(bench):
    b = after_upgrade(bench)
    outcome = cycle.run_cycle(b.ctx)

    assert outcome["status"] == "running" and outcome["phase"] == "verify"
    assert b.unit.exists()
    assert not b.runner.called("purge")
    upgrade_at = next(i for i, call in enumerate(b.runner.calls) if is_real_upgrade(call))
    assert upgrade_at < b.runner.index("systemctl", "reboot")

    b.running = K2712  # the machine comes back on the new kernel
    final = cycle.resume(b.ctx)

    assert final["status"] == "ok"
    assert final["steps"]["verify"]["status"] == "ok"
    assert final["steps"]["kernels"]["details"]["targets"] == [f"linux-image-{OLDEST}"]
    assert b.runner.index("systemctl", "reboot") < b.runner.index("-y", "purge")
    assert not b.unit.exists()
    assert b.saved()["cycle"]["status"] == "ok"


def test_state_survives_being_reloaded_after_reboot(bench):
    b = after_upgrade(bench)
    cycle.run_cycle(b.ctx)
    second = bench()
    second.running = K2712
    second.add_modules(K2712)
    assert second.ctx.state["cycle"]["phase"] == "verify"
    assert cycle.resume(second.ctx)["rebooted"] is True


def test_no_reboot_needed_runs_straight_through(bench):
    b = bench()
    b.add_modules(K2712)
    outcome = cycle.run_cycle(b.ctx)
    assert outcome["steps"]["reboot"]["summary"] == "No reboot needed"
    assert outcome["steps"]["verify"]["status"] == "skipped"
    assert "disk" in outcome["steps"] and outcome["finished"]


def test_preflight_block_postpones_everything_but_the_disk_report(bench):
    b = bench()
    b.runner.on(["vcgencmd"], Result(0, "throttled=0x1\n"))
    outcome = cycle.run_cycle(b.ctx)
    assert outcome["status"] == "blocked"
    assert set(outcome["steps"]) == {"preflight", "disk"}
    assert not b.runner.called("apt-get", "update")


def test_failed_upgrade_blocks_reboot_and_kernels_but_not_compose(bench):
    b = after_upgrade(bench)
    b.runner.on(["apt-get", "-o", "DPkg::Lock::Timeout=300", "update"], Result(100, "", "broken"))
    outcome = cycle.run_cycle(b.ctx)
    assert outcome["status"] == "failed"
    assert outcome["steps"]["reboot"]["status"] == "blocked"
    assert outcome["steps"]["kernels"]["status"] == "skipped"
    assert "compose" in outcome["steps"]
    assert not b.runner.called("systemctl", "reboot")


def test_outside_window_waits_then_resume_reboots_inside_it(bench):
    b = after_upgrade(bench)
    b.moment = datetime(2026, 11, 8, 12, 0)
    outcome = cycle.run_cycle(b.ctx)
    assert outcome["status"] == "waiting" and outcome["phase"] == "reboot"
    assert "compose" not in outcome["steps"]
    assert not b.unit.exists()

    b.moment = datetime(2026, 11, 9, 3, 0)
    outcome = cycle.resume(b.ctx)
    assert b.runner.called("systemctl", "reboot")
    assert outcome["phase"] == "verify"
    assert b.unit.exists(), "the unit must survive this resume, for the coming reboot"


def test_run_continues_a_waiting_cycle_instead_of_upgrading_again(bench):
    b = after_upgrade(bench)
    b.moment = datetime(2026, 11, 8, 12, 0)
    cycle.run_cycle(b.ctx)
    upgrades = sum(is_real_upgrade(call) for call in b.runner.calls)
    b.ctx.reboot_now = True
    cycle.run_cycle(b.ctx)
    assert sum(is_real_upgrade(call) for call in b.runner.calls) == upgrades
    assert b.runner.called("systemctl", "reboot")


def test_finished_cycle_starts_a_new_one(bench):
    b = bench()
    b.add_modules(K2712)
    first = cycle.run_cycle(b.ctx)
    second = cycle.run_cycle(b.ctx)
    assert second is not first and second["status"] != "running"


def test_no_reboot_flag_carries_on_and_leaves_kernels_alone(bench):
    b = after_upgrade(bench, no_reboot=True)
    outcome = cycle.run_cycle(b.ctx)
    assert outcome["status"] == "warning"
    assert outcome["steps"]["kernels"]["status"] == "warning"
    assert not b.runner.called("systemctl", "reboot")
    assert not b.runner.called("-y", "purge")


def test_a_crashing_step_is_recorded_and_the_rest_still_run(bench, monkeypatch):
    b = bench()
    b.add_modules(K2712)

    def explode(ctx, cycle_state):
        raise RuntimeError("boom")

    monkeypatch.setitem(cycle.PHASES, "compose", explode)
    outcome = cycle.run_cycle(b.ctx)
    assert outcome["status"] == "failed"
    assert "boom" in outcome["steps"]["compose"]["errors"][0]
    assert "disk" in outcome["steps"]


def test_dry_run_mutates_nothing_and_saves_nothing(bench):
    b = after_upgrade(bench, dry_run=True)
    outcome = cycle.run_cycle(b.ctx)
    # A warning is right: with no real reboot, the old kernels have to be left alone.
    assert outcome["status"] == "warning"
    assert outcome["steps"]["kernels"]["summary"].startswith("Left alone")
    mutating = ("-y", "reboot", "enable", "update", "daemon-reload", "systemd-run", "sync")
    for call in b.runner.calls:
        assert not set(call) & set(mutating), call
    assert not b.state_path.exists()
    assert not b.unit.exists()
    assert outcome["steps"]["upgrade"]["summary"].startswith("Would apply")
    assert outcome["steps"]["reboot"]["summary"].startswith("Would reboot")


def test_resume_with_nothing_pending_does_nothing(bench):
    b = bench()
    b.touch("/etc/systemd/system/foreman-resume.service")
    assert cycle.resume(b.ctx) is None
    assert not b.unit.exists()
    assert not b.runner.called("apt-get")


def test_single_upgrade_still_runs_preflight(bench):
    b = bench()
    b.unreachable["deb.debian.org"] = "DNS lookup failed"
    result = cycle.run_single(b.ctx, "upgrade")
    assert result.status == "blocked"
    assert not b.runner.called("apt-get", "update")


def test_single_reboot_installs_no_resume_unit(bench):
    b = after_upgrade(bench)
    assert cycle.run_single(b.ctx, "reboot").status == REBOOTING
    assert not b.unit.exists()
    assert b.saved()["last_steps"]["reboot"]["status"] == REBOOTING
