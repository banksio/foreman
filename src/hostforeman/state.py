"""State that has to survive a reboot, and the lock that stops two runs overlapping."""

from __future__ import annotations

import copy
import fcntl
import json
import os
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

_EMPTY = {
    "cycle": None,  # the current or most recent full cycle
    "last_steps": {},  # the latest result of each step
    "disk": None,  # the latest disk report
    "stacks": [],  # the latest Compose outcome per stack
    "known_stacks": None,  # None until the first real Compose run sets the baseline
    "bad_images": {},  # project -> image set that was rolled back
    "rebooted_for": None,  # the kernel foreman last rebooted to get
    "kernel_updated_boot": None,  # boot id during which the running kernel's package changed
}


class StateError(Exception):
    pass


class LockBusy(Exception):
    pass


class State:
    """A JSON file, replaced atomically. With persist=False nothing is ever written."""

    def __init__(self, path: Path, persist: bool = True) -> None:
        self.path = path
        self.persist = persist
        self.data = copy.deepcopy(_EMPTY)
        try:
            self.data.update(json.loads(path.read_text()))
        except FileNotFoundError:
            pass
        except (OSError, ValueError) as exc:
            raise StateError(f"could not read {path}: {exc}") from exc

    def __getitem__(self, key: str):
        return self.data[key]

    def __setitem__(self, key: str, value) -> None:
        self.data[key] = value

    def save(self) -> None:
        if not self.persist:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.path.with_suffix(".tmp")
        # Synced before the rename, because a reboot can follow immediately.
        with temporary.open("w") as handle:
            json.dump(self.data, handle, indent=2)
            handle.flush()
            os.fsync(handle.fileno())
        temporary.chmod(0o644)
        os.replace(temporary, self.path)


@contextmanager
def run_lock(path: Path) -> Iterator[None]:
    descriptor = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise LockBusy from exc
        yield
    finally:
        os.close(descriptor)
