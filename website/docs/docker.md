---
title: Docker Compose maintenance
description: Enroll a trusted Compose project and review update eligibility and rollback behavior.
---

Foreman discovers running Compose projects and compares them with saved enrollments. A project becomes eligible for updates after explicit enrollment and a successful safety check.

## Enroll a running project

```bash
foreman docker stacks
sudo foreman docker stack enroll /srv/example/compose.yaml
foreman docker stack show example
sudo foreman run compose --stack example --dry-run
```

Use the ID returned by enrollment. For a project originally started with overrides, provide the same inputs during enrollment. Run `foreman docker stack enroll --help` for the full argument list.

```bash
sudo foreman docker stack enroll /srv/example/compose.yaml \
  --file /srv/example/compose.production.yaml \
  --project example --profile production
```

Compose and environment inputs, including their containing paths, need to be root-owned and protected from group or world writes. Keep the full project in a dedicated administrative path such as `/srv`, with direct paths. Foreman checks the live project identity against the enrolled file paths, project directory and name. Keep `.env` and other environment inputs with the reviewed deployment. The trust checks in this release require statically identifiable local inputs and block Compose `include`, `extends`, and service providers.

For a project whose maintainers you already trust with root Docker access, add its exact Compose project name to a root-owned Foreman config file, such as `/etc/foreman/conf.d/90-docker-ownership.toml`.

```toml
[compose]
allow_user_owned_projects = ["public_caddy"]
```

Foreman warns during enrollment, status, and maintenance. Anyone who can edit or replace these inputs can influence root Docker maintenance. Direct paths, explicit enrollment, live project identity, config drift checks, and root-only enrollment and registry settings still apply. Remove the name from the list to restore the ownership requirement. If Foreman has already recorded the enrollment, changing the list is sufficient. If you maintain the list in more than one drop-in, the last definition replaces the whole list.

## Eligibility and health checks

Explicit exclusions take priority over enrollment and database opt-ins. Foreman skips paused and stopped projects. Database-like images and mount points require an administrator opt-in through `compose.allow_databases`.

Foreman checks the rendered Compose configuration against live service hashes before updating. Review unexpected drift in the Compose project before changing its Foreman policy. For private registries, provision a root-controlled Docker configuration directory and use `compose.registry_config` so timer runs use the same credentials. A preview can report an unknown image change when the remote reference cannot be compared reliably.

After pulling, Foreman targets changed services and observes their intended image, restart behavior and available health checks. An application health check gives Foreman more information than running process state alone. Foreman protects previous image IDs for rollback. Rollback can restore container images and still requires a separate recovery plan for data and migrations.

## Maintain or retire a project

```bash
sudo foreman run compose --stack example
sudo foreman docker stack disable example
sudo foreman docker stack enable example
sudo foreman docker stack retry example
sudo foreman docker stack unenroll example
```

Retry clears the remembered candidate failure. Start a fresh run to attempt it again through normal eligibility checks. Unenrolling leaves the project's Docker resources in place.

An eligible multi-project run can clean up dangling images across the daemon. Age-based tagged image cleanup requires configuration. Foreman excludes rollback holds from image cleanup. Targeted `--stack` runs skip daemon-wide cleanup.
