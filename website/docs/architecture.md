---
title: Provider architecture
description: How Foreman assembles maintenance phases and persists a cycle.
---

Foreman uses bundled Rust providers. The core chooses applicable providers, orders maintenance phases and owns the execution lock, reboot decisions, schedules and history. Provider results carry an outcome and observations the core can use in later phases.

| Provider family | Contribution |
| --- | --- |
| Linux | Host and boot identification, preflight observations, and disk measurements. |
| systemd | Scheduled jobs, deferred continuation and boot-time resume. |
| Debian | APT maintenance, package-related reboot signals, and kernel inventory and cleanup. |
| Raspberry Pi OS | Pi-specific power and firmware checks on top of the Debian package provider. |
| Docker | Compose discovery and enrollment, workload updates, health checks, and image recovery. |
| Notifications | Configured event delivery and scheduled-job heartbeats. |

A manual run, scheduled job, or preview goes through the same phase definitions. Preview tells providers to plan or observe. When Foreman needs to wait or reboot, it persists the cycle and checks the saved context again before returning to mutation. Independent reporting has its own observations.

Operator settings live under `/etc/foreman`. Mutable results and checkpoints live under `/var/lib/foreman`. The state files and JSON command responses have versioned representations. The JSON command output is the supported integration point for this release. [CLI JSON output](./cli.md#machine-readable-output)

This release ships webhook and heartbeat notification providers. Dedicated ntfy and email providers and external executable modules remain planned. Standalone installations support verified operator-triggered self-updates.
