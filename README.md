# foreman

Unattended maintenance for Debian-family hosts, built first for Raspberry Pi OS Lite 64-bit.

One cycle runs these steps in order, and carries on by itself after the reboot:

```text
preflight → full-upgrade → reboot → confirm new kernel → remove old kernels → update Compose → disk report
```

## Install

foreman needs root, so install it system-wide. `--managed-python` matters: without it uv picks up the system Python, which `full-upgrade` may replace while foreman is running.

```bash
sudo env UV_TOOL_DIR=/opt/foreman/tools UV_TOOL_BIN_DIR=/usr/local/bin UV_PYTHON_INSTALL_DIR=/opt/foreman/python uv tool install --managed-python --python 3.13 git+<your-repo>
```

Optional settings go in `/etc/foreman/config.toml`. See [config.example.toml](config.example.toml) for all of them and their defaults.

## Use

Start with a dry run. It changes nothing and doesn't need root.

```bash
foreman run --dry-run
```

| Command | What it does |
|---|---|
| `sudo foreman run` | Full cycle, or continue one that is waiting |
| `sudo foreman run compose` | One step: `preflight`, `upgrade`, `reboot`, `kernels`, `compose` or `disk` |
| `sudo foreman run --no-reboot` | Full cycle without rebooting |
| `sudo foreman run --reboot-now` | Ignore the reboot window, but keep every other reboot check |
| `foreman status` | Last run, pending reboot, disk, stacks and warnings |
| `sudo foreman stacks` | What foreman will do with each running stack |
| `sudo foreman stacks approve NAME` | Allow a new stack held for review to be updated |

A step's output after a reboot is in the system journal:

```bash
journalctl -u foreman-resume
```

## Behaviour worth knowing

- **Reboots** only start inside the window, 03:00–05:00 by default. They are blocked by errors in the run, an invalid `sshd` config, a watched service not enabled at boot, or a kernel that didn't take effect last time. If a cycle has to wait, foreman schedules itself for the start of the window.
- **Old kernels** are removed only once the newest has booted. The running kernel and one previous are kept, per flavour. If a simulation shows the removal would affect anything else, foreman leaves everything alone.
- **Databases** are detected by image name and data directory, and aren't updated unless listed in `allow_databases`.
- **New stacks** are held for review if they appear after foreman's first real Compose run.
- **Failed updates** roll back to the previous images. Rollback restores images, not data.
- **Undervoltage** happening now postpones the run. Undervoltage earlier since boot is only a warning.

## Develop

```bash
uv run pytest
```
