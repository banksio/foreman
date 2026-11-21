---
title: Maintenance and reboots
description: Control a maintenance cycle, inspect the package plan, and review reboot policy.
---

## Full cycle

```bash
sudo foreman run --dry-run
sudo foreman run
```

A cycle runs preflight, host upgrades, reboot evaluation, any necessary boot verification, kernel cleanup, workload maintenance, and disk reporting. Reporting remains available after a blocked phase. Foreman serializes system mutations. Wait for an active package transaction to finish before retrying a busy run.

Preflight checks package tools, repository connectivity, clock state, and filesystem space. On Raspberry Pi OS it also checks power and firmware space. APT refresh uses the host's configured repositories and signature verification. Release metadata changes require operator review. The default APT removal budget is zero.

Dry runs use existing local APT indexes and report changes that depend on a future index refresh, image pull, or reboot. They leave maintenance checkpoints and host settings untouched. Foreman checks repository reachability using direct TCP probes. A repository reachable only through an APT proxy may need a deployment-specific network policy before preflight can pass.

## Individual operations

```bash
foreman run preflight --dry-run
sudo foreman run upgrade
sudo foreman run reboot
sudo foreman run kernels
sudo foreman run workloads
foreman run disk
```

The standalone upgrade still runs preflight. Individual steps have the same safety checks and dry-run behavior as their full-cycle equivalents. The disk report includes filesystem byte and inode usage and can measure configured directory consumers.

## Reboots and continuation

Reboots default to automatic mode inside 03:00–05:00 in the host's local time. Watched services and reboot safety checks must pass. For a manual run, `--no-reboot` tightens the policy to `never`. `--reboot-now` bypasses the window and still requires a reboot signal and safety checks. The two flags are mutually exclusive.

A full cycle may wait for a later window. Foreman saves the checkpoint and uses systemd to continue it. Before a managed reboot it records the machine's boot ID, an attempt against the cooldown, and the expected kernel when known. After boot it verifies the new boot and required readiness before dependent work.

For a parked cycle, use `foreman status` to inspect its stage and `sudo foreman run` to resume it manually. Use `sudo foreman cycle abort` to close an inactive cycle whose continuation is no longer wanted. A reboot handoff in progress can prevent an abort until its outcome is known.

Set an empty reboot window to allow a reboot at any local time, or use `mode = "never"` to prohibit Foreman-initiated reboots. The [configuration reference](./configuration.md) lists cooldown and expiry controls.
