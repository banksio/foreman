---
title: Notifications and heartbeats
description: Deliver maintenance events and report successful scheduled runs to an external monitor.
---

Foreman supports opt-in webhooks and job-specific heartbeats. Put each HTTPS URL in its own root-owned file with mode `0600`, as a single line. Notification URLs commonly contain authentication tokens. Use a protected directory, such as `/etc/foreman/secrets/`, for these files.

```toml
[notifications]
webhook_url_file = "/etc/foreman/secrets/webhook-url"
heartbeat_url_file = "/etc/foreman/secrets/heartbeat-url"
heartbeat_schedule = "weekly"
stale_reboot_days = 3
```

Configure the webhook, the heartbeat, or both. A heartbeat requires both the credential file and the name of an existing schedule. Set up the external monitor to expect that job's cadence plus enough grace for a deferred reboot and continuation. The monitor detects a missed heartbeat while Foreman or the host is offline.

The webhook receives JSON by HTTPS POST. It includes the event kind, outcome if available, schedule name if applicable, and UTC timestamp. Heartbeats use HTTPS GET after the configured job completes successfully. An endpoint must return a 2xx response.

Enable the daily notification timer if you want persistent reboot requirements checked between maintenance runs.

```bash
sudo systemctl enable --now foreman-notify.timer
journalctl -u foreman-notify.service
foreman status
```

Notification delivery outcomes appear separately from maintenance results. To stop the daily check, disable `foreman-notify.timer`. Remove the relevant settings to stop delivering events or heartbeats.
