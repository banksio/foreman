---
title: Schedules
description: Create, inspect, pause, and resume named maintenance jobs.
---

Schedules use systemd calendar expressions. Each named job runs the full cycle or a single maintenance step through the same provider and lock path as a manual invocation.

```bash
sudo foreman schedule add weekly --calendar 'Sun *-*-* 03:30:00'
sudo foreman schedule add disk-daily --calendar '*-*-* 08:00:00' --run disk --reboot never
foreman schedule list
foreman schedule show weekly
```

On add or a calendar edit, Foreman validates the expression and previews the next occurrences. The examples use the host's current local time.

## Edit or pause a schedule

```bash
sudo foreman schedule edit weekly --calendar 'Sat *-*-* 03:30:00'
sudo foreman schedule edit weekly --run cycle --reboot inherit
sudo foreman schedule disable weekly
sudo foreman schedule enable weekly
sudo foreman schedule remove weekly
```

Disabling a job preserves its definition. Removing it stops future triggers and deletes its definition. A cycle that already delegated its continuation to systemd can continue after the initiating job is paused or removed.

## Missed runs and reboots

An enabled schedule can catch up once after host downtime. Pausing and re-enabling a schedule clears its previous firing state so it starts at the next future occurrence. A mutating job that reaches an occupied lock or a cycle waiting to resume is recorded as busy. Disk jobs can report independently.

The calendar controls when work starts. Host policy still decides whether and when a reboot can occur. Use `--reboot never` for jobs that must stay online.

## Inspect runs

```bash
foreman schedule list --json
systemctl list-timers 'foreman-*'
journalctl -u foreman-job-weekly.service
journalctl -u foreman-resume.service
```

The list includes the schedule action, calendar, enabled state and available outcome information. For troubleshooting missed triggers, inspect the generated timer as well as the service journal.
