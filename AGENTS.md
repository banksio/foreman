# Working on Foreman

## Git workflow

- Always use [Conventional Commits](https://www.conventionalcommits.org/) for commits and squash merges. Describe observable changes with a type and optional scope, such as `fix(reboot): preserve a pending cycle across restarts`.
- Use Git Flow where repository permissions and branch protection allow it. Keep `main` releasable and use `develop` to integrate work. Branch features and routine fixes from `develop` as `feature/<name>` or `fix/<name>`, then open a pull request into `develop`.
- Cut `release/<version>` from `develop` for stabilization. Merge it into `main` and back into `develop`. Cut `hotfix/<version>` from `main` for urgent production fixes and merge into both branches. Use the repository's pull request and review rules for those merges.
- Always tag releases on the corresponding `main` commit with an annotated semantic version tag, such as `v0.1.0`. Push the tag explicitly. Keep the tag, Cargo manifest, lockfile, changelog, and installer default in agreement. Treat pushed release tags and assets as immutable.
- If only the protected default branch is available, work on a short-lived branch and open a pull request into that branch. Resume the Git Flow model when the integration branch is available.

## Code and validation

- Read the operator documentation and affected provider before editing. Preserve preview semantics, the execution lock, provider dependency rules, persisted schema compatibility, and the distinction between operator intent and observed state.
- Run `cargo fmt --all -- --check`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo test --locked`. Use a disposable Linux host for tests that install packages, alter systemd, manage Docker, or reboot.
- Run `npm ci` and `npm run build` from `website/` for documentation changes. Update the Markdown guides when CLI behavior, configuration, paths, providers, or compatibility change.
- Keep credentials in protected files. Avoid storing them in fixture output, command lines, logs, and JSON.
- Explain release-affecting changes and unverified platform checks in the pull request. A green container smoke test verifies package lifecycle and CLI entry points. A real boot and post-reboot continuation need a systemd host.
