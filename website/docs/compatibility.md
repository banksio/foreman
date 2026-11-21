---
title: Compatibility and host profiles
description: Supported package architectures and bundled host profiles.
---

## Compatibility matrix

| Operating system | Architectures | Package | Host profile | Coverage |
| --- | --- | --- | --- | --- |
| Debian 12 (Bookworm) with systemd | `amd64`, `arm64` | Yes | `debian` | Release CI builds and exercises package installation and removal on both architectures. |
| Debian 13 (Trixie) with systemd | `amd64`, `arm64` | Yes | `debian` | Supported target for the Debian 12-built package. |
| Raspberry Pi OS Lite 64-bit (Bookworm or Trixie) on Raspberry Pi 5 | `arm64` | Yes | `raspberry-pi-os` | Target for Pi power, firmware and kernel policy. Validate boot continuation on the device before enabling unattended reboots. |
| Other Debian-family distributions with systemd | `amd64`, `arm64` | May install | Usually `debian` | Detection is based on the local OS description. Validate your distribution before enabling scheduled changes. |
| Other Linux distributions | Source build | — | `linux` | Linux observations and disk reporting. APT-based host maintenance requires the Debian profile. |
| 32-bit Linux, macOS, and Windows | — | — | — | Outside the published package targets. |

The package is built against Debian 12's glibc baseline. Schedules and managed reboot continuation require a running systemd system manager. Docker maintenance requires the local Docker Engine and the Docker Compose plugin.

## Bundled profiles

- **`debian`** combines the shared Linux checks and disk reporting with the Debian APT and kernel providers. systemd features are available when the system manager is running.
- **`raspberry-pi-os`** layers Pi firmware and power observations onto the Debian profile. It uses the same APT provider. `vcgencmd` must be available for the Pi power check.
- **`linux`** provides portable Linux host and filesystem observations for a source build. Provider availability depends on the host.
- **`unsupported`** is reported on a non-Linux source build.

Docker is an optional module across Linux profiles. Inspect selection with `foreman modules list`, and inspect the active host profile with `foreman status`.
