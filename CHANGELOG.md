# Changelog

## 0.2.1

- Make the signed standalone build the default and only quick-installer target.
- Add an explicit migration from existing Debian packages that keeps operator configuration and state.

## 0.2.0

- Show live APT download and package application progress in interactive runs.
- Add standalone Linux installs with receipt-based, signed release verification and operator-triggered self-update on amd64 and arm64.

## 0.1.1

- Added an administrator-controlled, per-project exception for user-owned Docker Compose inputs. Foreman warns when the exception is active.

## 0.1.0

- Added the Rust Foreman CLI and bundled host, systemd, and Docker providers.
- Added maintenance previews, saved reboot continuation, and named maintenance schedules.
- Added explicit Compose enrollment, image maintenance, and rollback tracking.
- Added versioned JSON output, webhooks, and scheduled-job heartbeats.
- Added Debian packages for arm64 and amd64, a Bash installer, and a GitHub Pages documentation site.
