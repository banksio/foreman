import pytest
from conftest import K2712

from hostforeman import kernels
from hostforeman.host import Result
from hostforeman.models import OK, SKIPPED, WARN

V8 = "6.12.50+rpt-rpi-v8"
PREVIOUS = "6.12.47+rpt-rpi-2712"
OLDEST = "6.12.25+rpt-rpi-2712"
PACKAGES = f"""ii  linux-image-rpi-2712
ii  linux-image-rpi-v8
ii  linux-headers-rpi-2712
ii  linux-image-{K2712}
ii  linux-headers-{K2712}
ii  linux-image-{V8}
ii  linux-image-{PREVIOUS}
ii  linux-image-{OLDEST}
ii  linux-headers-{OLDEST}
ii  linux-headers-6.12.25+rpt-common-rpi
ii  linux-kbuild-6.12.25+rpt
ii  linux-headers-6.12.50+rpt-common-rpi
ii  linux-kbuild-6.12.50+rpt
rc  linux-image-6.6.31+rpt-rpi-2712
"""
OLD_SET = sorted(
    [
        f"linux-image-{OLDEST}",
        f"linux-headers-{OLDEST}",
        "linux-headers-6.12.25+rpt-common-rpi",
        "linux-kbuild-6.12.25+rpt",
    ]
)


def pi(bench, packages=PACKAGES, **kwargs):
    b = bench(**kwargs)
    b.runner.on(["dpkg-query"], Result(0, packages))
    simulated = "\n".join(f"Purg {name} [1.0]" for name in OLD_SET)
    b.runner.on(["apt-get", "-s", "purge"], Result(0, simulated))
    return b


@pytest.mark.parametrize(
    "name,base,flavour",
    [
        ("6.12.47+rpt-rpi-2712", "6.12.47+rpt", "rpi-2712"),
        ("6.12.47+rpt-rpi-v8", "6.12.47+rpt", "rpi-v8"),
        ("6.1.0-25-arm64", "6.1.0-25", "arm64"),
        ("6.1.0-25-rt-arm64", "6.1.0-25", "rt-arm64"),
        ("6.8.0-45-generic", "6.8.0-45", "generic"),
        ("6.12.48+deb13-arm64", "6.12.48+deb13", "arm64"),
        ("6.12.115+deb13.1-arm64", "6.12.115+deb13.1", "arm64"),
        ("6.12.115+deb13.1-rt-arm64", "6.12.115+deb13.1", "rt-arm64"),
        ("6.12.47+rpt-rpi-v8-rt", "6.12.47+rpt", "rpi-v8-rt"),
        ("6.1.21-v8+", "6.1.21", "v8+"),
    ],
)
def test_parse_release(name, base, flavour):
    release = kernels.parse_release(name)
    assert (release.base, release.flavour) == (base, flavour)


def test_versions_sort_numerically_not_alphabetically():
    names = ["6.1.0-9-arm64", "6.1.0-25-arm64"]
    assert kernels.newest_of_flavour(names, "6.1.0-9-arm64") == "6.1.0-25-arm64"


def test_keeps_running_and_previous_and_purges_only_the_oldest(bench):
    b = pi(bench)
    result = kernels.step(b.ctx)
    assert result.status == OK
    purge = next(call for call in b.runner.calls if "purge" in call and "-s" not in call)
    assert sorted(arg for arg in purge if arg.startswith("linux-")) == OLD_SET


def test_never_targets_metapackages_or_kept_kernels(bench):
    targets = kernels.step(pi(bench).ctx).details["targets"]
    for protected in ("linux-image-rpi-2712", "linux-image-rpi-v8", f"linux-image-{K2712}", f"linux-image-{V8}", f"linux-image-{PREVIOUS}"):
        assert protected not in targets


def test_shared_files_stay_while_a_kept_kernel_uses_them(bench):
    targets = kernels.step(pi(bench).ctx).details["targets"]
    assert "linux-kbuild-6.12.50+rpt" not in targets
    assert "linux-headers-6.12.50+rpt-common-rpi" not in targets


def test_held_kernel_is_not_removed(bench):
    b = pi(bench, PACKAGES.replace(f"ii  linux-image-{OLDEST}", f"hi  linux-image-{OLDEST}"))
    assert f"linux-image-{OLDEST}" not in kernels.step(b.ctx).details["targets"]


def test_refuses_when_simulation_touches_other_packages(bench):
    b = pi(bench)
    b.runner.on(["apt-get", "-s", "purge"], Result(0, "Purg some-other-package [1.0]\n"))
    result = kernels.step(b.ctx)
    assert result.status == WARN
    assert not any("purge" in call and "-y" in call for call in b.runner.calls)


def test_waits_until_the_newest_kernel_has_booted(bench):
    b = pi(bench)
    b.running = PREVIOUS
    result = kernels.step(b.ctx)
    assert result.status == WARN
    assert not b.runner.called("-s", "purge")


def test_leaves_alone_a_kernel_not_from_a_package(bench):
    b = pi(bench)
    b.running = "6.12.99+rpt-rpi-2712"
    assert kernels.step(b.ctx).status == WARN
    assert not b.runner.called("purge")


def test_skipped_after_errors_in_the_cycle(bench):
    b = pi(bench)
    cycle = {"steps": {"upgrade": {"errors": ["boom"]}}}
    assert kernels.step(b.ctx, cycle).status == SKIPPED
    assert not b.runner.called("purge")


def test_dry_run_simulates_but_does_not_purge(bench):
    b = pi(bench, dry_run=True)
    assert "Would remove" in kernels.step(b.ctx).summary
    assert not any("-y" in call and "purge" in call for call in b.runner.calls)


def test_leftover_module_directories_are_ignored(bench):
    b = bench()
    b.add_modules(K2712)
    (b.root / "lib/modules/6.12.99+rpt-rpi-2712").mkdir()
    assert kernels.installed_module_releases(b.ctx) == [K2712]
