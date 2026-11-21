---
title: Quick start
slug: /
description: Install Foreman and preview your first maintenance cycle.
---

Foreman maintains the host where it runs. It can preview and apply package upgrades, coordinate a required reboot, update enrolled Compose projects, and report disk usage.

## Install

Use a [supported 64-bit Debian-family host](./compatibility.md) with Bash, curl and administrative access. On a minimal installation, refresh APT and install curl first.

```bash
sudo apt-get update
sudo apt-get install -y ca-certificates curl
curl -fsSL https://raw.githubusercontent.com/banksio/foreman/v0.1.0/install.sh | bash
```

The installer selects the package using `dpkg --print-architecture`, verifies the SHA-256 checksum from the tagged release and passes the package to APT. It refuses to replace a newer Foreman package with an older release.

To inspect the installer before running it, download it to a file and run `bash install.sh` after review. You can also download a `.deb` and `SHA256SUMS` from the release, verify the artifact and install the local file through APT. [GitHub Releases](https://github.com/banksio/foreman/releases)

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

The package starts with an empty schedule list. Add a named job when you are ready to run unattended.

```bash
sudo foreman schedule add weekly --calendar 'Sun *-*-* 03:30:00'
foreman schedule list
```

Read the [schedule guide](./schedules.md) for local times, missed runs, and job-specific reboot policy. For Compose workloads, follow [the enrollment guide](./docker.md).
