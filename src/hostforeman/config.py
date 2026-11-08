"""Settings from a TOML file. It is parsed as data and never executed."""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field, fields
from pathlib import Path

from . import window as window_module

DEFAULT_CONFIG = "/etc/foreman/config.toml"
DEFAULT_STATE_DIR = "/var/lib/foreman"
DEFAULT_LOCK = "/run/foreman.lock"


class ConfigError(Exception):
    pass


@dataclass
class PreflightConfig:
    min_free_mb: int = 500
    min_free_boot_mb: int = 100
    network_hosts: list[str] = field(
        default_factory=lambda: ["deb.debian.org", "archive.raspberrypi.com"]
    )
    require_clock_sync: bool = True


@dataclass
class RebootConfig:
    window: str = "03:00-05:00"
    watch_services: list[str] = field(default_factory=lambda: ["ssh"])


@dataclass
class KernelsConfig:
    remove_old: bool = True
    keep_previous: int = 1


@dataclass
class ComposeConfig:
    enabled: bool = True
    exclude: list[str] = field(default_factory=list)
    allow_databases: list[str] = field(default_factory=list)
    health_timeout: int = 180
    settle: int = 15
    boot_settle: int = 60
    prune_dangling: bool = True
    prune_tagged_days: int = 0


@dataclass
class DiskConfig:
    paths: list[str] = field(default_factory=lambda: ["/", "/boot/firmware"])
    warn_percent: int = 85
    critical_percent: int = 95
    consumers: list[str] = field(
        default_factory=lambda: [
            "/var/cache/apt",
            "/var/log",
            "/var/lib/docker",
            "/home",
            "/root",
            "/tmp",
        ]
    )


@dataclass
class Config:
    preflight: PreflightConfig = field(default_factory=PreflightConfig)
    reboot: RebootConfig = field(default_factory=RebootConfig)
    kernels: KernelsConfig = field(default_factory=KernelsConfig)
    compose: ComposeConfig = field(default_factory=ComposeConfig)
    disk: DiskConfig = field(default_factory=DiskConfig)


def _section(cls: type, name: str, raw: object):
    if not isinstance(raw, dict):
        raise ConfigError(f"[{name}] must be a table")
    defaults = cls()
    names = {item.name for item in fields(cls)}
    for key, value in raw.items():
        if key not in names:
            raise ConfigError(f"unknown setting {name}.{key}")
        expected = type(getattr(defaults, key))
        wrong_type = not isinstance(value, expected) or (
            expected is int and isinstance(value, bool)
        )
        if wrong_type:
            raise ConfigError(f"{name}.{key} must be {expected.__name__}")
        if expected is list and not all(isinstance(item, str) for item in value):
            raise ConfigError(f"{name}.{key} must be a list of strings")
        if expected is int and value < 0:
            raise ConfigError(f"{name}.{key} must not be negative")
    return cls(**raw)


def from_dict(raw: dict) -> Config:
    sections = {item.name: item.default_factory for item in fields(Config)}
    for name in raw:
        if name not in sections:
            raise ConfigError(f"unknown section [{name}]")
    config = Config(
        **{name: _section(cls, name, raw.get(name, {})) for name, cls in sections.items()}
    )
    if config.reboot.window:
        try:
            window_module.parse(config.reboot.window)
        except ValueError as exc:
            raise ConfigError(str(exc)) from exc
    return config


def load(path: Path) -> Config:
    """A missing file simply means the defaults."""
    try:
        with path.open("rb") as handle:
            raw = tomllib.load(handle)
    except FileNotFoundError:
        return Config()
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"{path}: {exc}") from exc
    try:
        return from_dict(raw)
    except ConfigError as exc:
        raise ConfigError(f"{path}: {exc}") from exc


def config_path() -> Path:
    return Path(os.environ.get("FOREMAN_CONFIG", DEFAULT_CONFIG))


def state_path() -> Path:
    return Path(os.environ.get("FOREMAN_STATE_DIR", DEFAULT_STATE_DIR)) / "state.json"


def lock_path() -> Path:
    return Path(os.environ.get("FOREMAN_LOCK", DEFAULT_LOCK))
