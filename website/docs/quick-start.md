---
title: Quick start
description: Install Foreman and preview your first maintenance cycle.
---

Foreman maintains the host where it runs. It can preview and apply package upgrades, coordinate a required reboot, update enrolled Compose projects, and report disk usage.

## Install

Use a [supported 64-bit Debian-family host](./compatibility.md) with administrative access.

```bash
sudo apt-get update
sudo apt-get install -y ca-certificates curl openssl jq netcat-openbsd
curl -fsSL https://raw.githubusercontent.com/banksio/foreman/v0.2.1/install.sh | bash
```

The installer verifies the signed release and puts Foreman in `/usr/local/bin` with a root-owned receipt. The installer can also be downloaded and reviewed before running `bash install.sh`.

If you installed Foreman through APT or an earlier quick installer, [switch the existing package to standalone](./recovery.md#switch-from-the-debian-package-to-standalone).

## Update Foreman

```bash
foreman self update --check
sudo foreman self update
```

Self-update verifies the publisher signature, manifest expiry, and executable checksum. Applying an update takes the maintenance lock, checks compatibility with saved state, and keeps a backup before activation. Run it when you want to upgrade.

For explicitly package-managed deployments, verify and install the `.deb` from the tagged release with APT. [GitHub Releases](https://github.com/banksio/foreman/releases)

## Check the host

```bash
foreman --version
foreman status
sudo foreman run --dry-run --no-reboot
```

Status shows the selected profile and provider availability. Preview uses existing local APT metadata and can run with elevated privileges when Docker and other host observations require it. A real run refreshes the package indexes and evaluates the resulting plan again.

## Run maintenance

```bash
sudo foreman run --no-reboot
foreman status
```

The flag prevents reboot for this run. If you are operating remotely, start with this mode and arrange a reboot separately after reviewing the result. [Reboot policy](./maintenance.md#reboots-and-continuation)

## Schedule recurring work

A fresh installation starts with an empty schedule list. Add a named job when you are ready to run unattended.

```bash
sudo foreman schedule add weekly --calendar 'Sun *-*-* 03:30:00'
foreman schedule list
```

Read the [schedule guide](./schedules.md) for local times, missed runs, and job-specific reboot policy. For Compose workloads, follow [the enrollment guide](./docker.md).
