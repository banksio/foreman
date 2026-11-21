---
title: Configuration
description: Configure host policy, provider settings, and Foreman data paths.
---

Use `/etc/foreman/config.toml` or a TOML drop-in under `/etc/foreman/conf.d/`. A fresh installation uses built-in defaults. The package includes the example at `/etc/foreman/config.example.toml`. [Example configuration](https://github.com/banksio/foreman/blob/main/config.example.toml)

Foreman reads the main file, then drop-ins in lexical order, then the dedicated module enablement file. Later settings override earlier ones at the key level. Arrays are replaced as a unit. Privileged runs require administrator-controlled inputs.

## Defaults

| Section | Key | Default |
| --- | --- | --- |
| `preflight` | `min_free_mb`, `min_free_boot_mb` | 500 MiB on root and 100 MiB on the Pi firmware partition when present. The estimated transaction can raise these requirements. |
| `preflight` | `network_hosts`, `require_clock_sync` | Repository checks plus an empty list of extra hosts. Clock sync is required when the host can report it. |
| `apt` | `max_removals`, `protected_packages` | Zero allowed removals and the profile's protected packages. An operator can add protected names. |
| `reboot` | `mode`, `window` | `"auto"` and `"03:00-05:00"` local time. A window can cross midnight. |
| `reboot` | `watch_services`, `cooldown_hours`, `pending_days` | Watch `ssh`, with 24 hours between managed attempts and a seven-day pending cycle lifetime. |
| `kernels` | `remove_old`, `keep_previous` | Cleanup enabled with the newest and one older kernel per flavour retained. The running kernel remains protected. |
| `compose` | `enabled`, `exclude`, `allow_databases` | Enabled where Docker and Compose are available. Enrollment, exclusions and database opt-ins start empty. |
| `compose` | `registry_config` | Optional absolute path to a root-controlled Docker configuration directory. |
| `compose` | `health_timeout`, `settle`, `boot_settle` | 180 seconds to reach stable health, 15 seconds of observation, and a 60-second Docker boot settle period. |
| `compose` | `prune_dangling`, `prune_tagged_days` | Dangling cleanup enabled for eligible runs. A value of zero disables age-based tagged cleanup. |
| `disk` | `paths`, `warn_percent`, `critical_percent` | Root and Pi firmware path if present, with thresholds at 85% and 95% for both byte and inode use. |
| `disk` | `consumers` | `/var/cache/apt`, `/var/log`, `/var/lib/docker`, `/home`, `/root`, and `/tmp`. |
| `maintenance` | `workloads_after_host_failure` | Disabled. |
| `notifications` | credential file paths and `heartbeat_schedule` | Disabled until configured. |
| `notifications` | `stale_reboot_days` | Three days. |

For example, keep the machine online during every maintenance run and include a data volume in disk reporting.

```toml
[reboot]
mode = "never"

[disk]
paths = ["/", "/srv", "/boot/firmware"]
```

## Data on disk

| Path | Contents |
| --- | --- |
| `/etc/foreman/config.toml`, `/etc/foreman/conf.d/` | Main settings and overrides. |
| `/etc/foreman/modules.toml` | Module enablement. |
| `/etc/foreman/schedules/`, `/etc/foreman/stacks.d/` | Named jobs and explicit Compose enrollments. |
| `/var/lib/foreman/` | Versioned cycle and run records, schedule outcomes, reboot observations, notification state, and image recovery records. |

Use the CLI for schedule, enrollment and module changes. Keep credentials in individually protected files or Docker credentials configured under the administrative trust boundary.
