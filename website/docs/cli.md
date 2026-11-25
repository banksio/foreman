---
title: CLI reference
description: Foreman commands, exit codes, and JSON output.
---

Run `foreman --help` for the global options and a command with `--help` for its accepted arguments. Administrative commands should run through `sudo` or as root. Read access to Docker and some host observations depends on local permissions.

## Commands

| Command | Purpose |
| --- | --- |
| `foreman run` | Start or continue a maintenance cycle. |
| `foreman run preflight` | Check maintenance prerequisites. |
| `foreman run upgrade` | Check prerequisites and upgrade host packages. |
| `foreman run reboot` | Apply the reboot policy to a standalone reboot request. |
| `foreman run kernels` | Review and clean up eligible old kernels. |
| `foreman run workloads` | Run eligible workload providers. |
| `foreman run compose [--stack ID]` | Run Compose maintenance for eligible projects or a single enrollment. |
| `foreman run disk` | Report filesystem and directory usage. |
| `foreman cycle abort` | Close an inactive pending cycle. |
| `foreman status [--json]` | Inspect host, provider, disk, reboot, and recent run information. |
| `foreman schedule [list]` | Show named jobs. Use `show`, `add`, `edit`, `enable`, `disable` or `remove` for one job. |
| `foreman docker stacks` | Discover running Compose projects and list enrollments. |
| `foreman docker stack` | Use `enroll`, `show`, `enable`, `disable`, `retry` or `unenroll` for one project. |
| `foreman modules list` | Inspect installed providers. Use `modules enable` or `modules disable` to change activation where supported. |
| `foreman self version`, `foreman --version` | Print the installed binary's version. |
| `foreman self update [--check]` | Check or install the latest signed stable release for a standalone installation. Updating requires root. Debian installs use the tagged package installer. |

Use `--dry-run` with `run` to preview. The mutually exclusive `--no-reboot` and `--reboot-now` flags apply to `run`. `--verbose` increases detail and `--no-progress` suppresses activity indicators and live APT progress. Interactive runs show download and package application percentages reported by APT. Set `--color auto|always|never` for terminal output. Automatic color respects redirected output and `NO_COLOR`.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Completed, unchanged, ordinary resource skip, or successful reboot handoff. |
| `1` | Execution or reboot verification failed. |
| `2` | Invalid arguments or settings. |
| `3` | A safety or policy check blocked the operation. |
| `4` | Continuation is waiting. |
| `5` | A lock or pending cycle owns the work. |

Foreman's systemd units mark `3`, `4`, and `5` as controlled outcomes. Check Foreman's status for the reason.

## Machine-readable output

```bash
foreman status --json
foreman schedule list --json
foreman docker stacks --json
foreman modules list --json
```

These commands emit one JSON document with a `schema_version` field. Timestamps include a time zone. Consumers should tolerate additional fields within a schema version and check the version before interpreting outcomes. [JSON schemas](https://github.com/banksio/foreman/tree/main/schemas)
