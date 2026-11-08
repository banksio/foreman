"""Reboot window parsing, as "HH:MM-HH:MM" in local time. It may span midnight."""

from __future__ import annotations

import re
from datetime import datetime, time

_FORMAT = re.compile(r"^([01]\d|2[0-3]):([0-5]\d)-([01]\d|2[0-3]):([0-5]\d)$")


def parse(window: str) -> tuple[time, time]:
    match = _FORMAT.match(window)
    if not match:
        raise ValueError(f"reboot window must look like 03:00-05:00, not {window!r}")
    hour1, minute1, hour2, minute2 = map(int, match.groups())
    start, end = time(hour1, minute1), time(hour2, minute2)
    if start == end:
        raise ValueError("reboot window must not start and end at the same time")
    return start, end


def contains(window: str, now: datetime) -> bool:
    """An empty window means any time."""
    if not window:
        return True
    start, end = parse(window)
    current = now.time()
    if start < end:
        return start <= current < end
    return current >= start or current < end
