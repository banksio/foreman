---
title: Development and releases
description: Build Foreman and the documentation site from the source checkout.
---

## Build and test

```bash
git clone https://github.com/banksio/foreman.git
cd foreman
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

CI uses Rust 1.100.0. Use a disposable Debian or Raspberry Pi OS host for package, reboot, timer and Docker integration tests. The container package test verifies installation, reinstallation, removal and purge on Debian 12.

If a machine-readable output type changes, regenerate its schema with `cargo run --locked -- __schemas schemas` and include the schema diff with the change.

## Preview the documentation

Use Node.js 24 and npm.

```bash
cd website
npm ci
npm run start
```

`npm run build` makes the production static site. The Pages workflow publishes the built site from `main` using the repository's GitHub Actions Pages source.

## Release

Use Conventional Commits and the Git Flow branch model in the repository's agent guide. After merging a release into `main`, push its annotated `vMAJOR.MINOR.PATCH` tag. The release workflow tests and builds both architectures against Debian 12, validates the packages and publishes their checksums with the binaries. [Repository agent guide](https://github.com/banksio/foreman/blob/main/AGENTS.md)
