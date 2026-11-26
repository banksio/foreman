---
title: Troubleshooting, migration, and removal
description: Recover pending work, move from Python Foreman, and retain history on removal.
---

## Inspect a blocked or waiting run

```bash
foreman status
foreman schedule list
systemctl list-timers 'foreman-*'
journalctl -u foreman-resume.service
```

Read the earliest blocked or failed phase. Resolve the reported host condition before retrying. Examples include an active package manager, insufficient free space, an unready service, or untrusted Compose input. For an APT Release metadata change, review the repository and its intended release with the host's package manager.

If work is parked at a reboot window, let it resume in the window or use `sudo foreman run` to re-evaluate it. `sudo foreman cycle abort` closes an inactive cycle while preserving the host's actual reboot requirement.

## Back up or reset observations

Before editing or restoring Foreman's data, disable its scheduled jobs and notification timer and wait for live work to finish. Resolve any handoff, then abort an inactive pending cycle if you intend to retire it. Back up both `/etc/foreman` and `/var/lib/foreman`.

Review outstanding image holds and unresolved rollback records before moving or removing observations. The state under `/var/lib/foreman` can include protection for a previous Docker image as well as history. Save the previous binary and its compatible data together if you intend to restore an older package.

## Migrate from Python Foreman

1. Back up the Python configuration, state and any units that trigger it. Review and resolve pending Python maintenance before retiring its timers.
2. Identify legacy jobs with `systemctl list-timers --all` and inspect their service definitions. Python uses `foreman-window.timer` and may install `/etc/systemd/system/foreman-resume.service`. Disable only the confirmed Python units. Archive the legacy resume unit before enabling Rust continuation.
3. Install Foreman and use `foreman --version` to confirm which binary you are calling. Check `command -v foreman` in case a locally installed Python command still takes precedence.
4. Review the Python settings against the shipped example. Re-create schedules and explicitly enroll each Compose project intended for Foreman maintenance. Use `foreman docker stacks` to inspect discovery and eligibility.
5. Enable the boot resume unit if needed and run a preview before enabling scheduled actions.

```bash
sudo systemctl daemon-reload
sudo systemctl enable foreman-resume.service
sudo foreman run --dry-run --no-reboot
```

## Upgrade or remove

For standalone upgrades, use `sudo foreman self update`. For package-managed installations, use APT with the new tagged Debian package. Resolve parked maintenance or unresolved rollback work before replacing the binary. `foreman self version` shows the installed version. The following removal commands apply to package-managed installations.

```bash
sudo apt-get remove foreman
# Or remove package-owned configuration as well
sudo apt-get purge foreman
```

Removal stops Foreman's jobs and removes its generated units. Removal retains the package example configuration. Purge removes the example and package-owned timer state. Both preserve operator intent and observed state in the shared Foreman directories. Review those contents before deleting them manually. After reinstalling, explicitly disable and re-enable any saved schedules you want to reactivate.


## Switch from the Debian package to standalone

Resolve pending maintenance or Docker rollback work before switching.

```bash
sudo apt-get install -y ca-certificates curl openssl jq netcat-openbsd
curl -fsSL https://raw.githubusercontent.com/banksio/foreman/v0.2.1/install.sh | bash -s -- --migrate-deb
hash -r
foreman self update --check
```

Migration verifies the standalone release, backs up configuration and observations, installs the replacement, and removes only the Foreman package. Your configuration, Docker enrollments and schedule definitions stay in place. Package removal stops the old timers. Use `sudo foreman schedule enable NAME` to resume a saved schedule, and re-enable the optional notification timer if you used it. If dpkg cannot finish removing the package, resolve its error and rerun the migration command.

## Recover an interrupted standalone update

Run `sudo foreman self update` again to reconcile an interrupted activation. Foreman verifies the installed candidate before committing the receipt, or restores the previous executable. The updater validates configuration and observations in read-only mode. Its previous binary and state archive are protected under `/usr/local/lib/foreman`.

If publisher trust needs to be reset after loss or compromise of the root signing key, use the separately authenticated project repository and verify the new public key fingerprint with the maintainer before reinstalling. A standalone binary rejects release metadata outside its pinned root of trust.
