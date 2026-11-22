<img src="website/static/img/foreman.svg" alt="" align="right" width="92">

# Foreman

Foreman runs local maintenance on Debian-family hosts. Preview package changes, schedule maintenance with systemd, coordinate required reboots, and enroll Docker Compose projects for image updates.

## Quick start

On a supported 64-bit Debian-family host with Bash, curl, and administrative access:

```bash
curl -fsSL https://raw.githubusercontent.com/banksio/foreman/v0.1.0/install.sh | bash
foreman --version
foreman status
sudo foreman run --dry-run --no-reboot
```

The installer verifies the release checksum and installs a Debian package through APT. A fresh install has no recurring maintenance jobs.

## Documentation

- [Quick start](https://banksio.github.io/foreman/quick-start/)
- [Compatibility and host profiles](https://banksio.github.io/foreman/compatibility/)
- [Maintenance and reboots](https://banksio.github.io/foreman/maintenance/)
- [Schedules](https://banksio.github.io/foreman/schedules/)
- [Docker Compose](https://banksio.github.io/foreman/docker/)
- [Configuration](https://banksio.github.io/foreman/configuration/)
- [CLI reference](https://banksio.github.io/foreman/cli/)

The documentation source lives in [website/docs](website/docs). Tagged builds and package checksums are available under [GitHub Releases](https://github.com/banksio/foreman/releases). See [AGENTS.md](AGENTS.md) for the repository workflow.
