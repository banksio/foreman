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


## Standalone release signing

The release workflow builds both Debian packages and standalone binaries from the existing annotated tag. After the workflow passes, a maintainer downloads the artifacts and signs them on a trusted workstation. Keep the Ed25519 root private key offline and use a separate delegated release private key for routine signing. Publish the packages and binaries together with the manifest, keyring, detached signatures, and checksums.

```bash
tag=v0.2.1
run_id=RELEASE_WORKFLOW_RUN_ID
dist=$(mktemp -d)
gh run download "$run_id" --repo banksio/foreman --dir "$dist"
mv "$dist"/foreman-{arm64,amd64}/* "$dist/"
python3 signing/sign-release.py "${tag#v}" "$dist" /path/to/delegated-release-private.pem
(cd "$dist" && sha256sum *.deb foreman-linux-* keyring.{json,sig} manifest.{json,sig} > SHA256SUMS)
gh release create "$tag" --repo banksio/foreman --verify-tag --generate-notes \
  --title "Foreman ${tag#v}" "$dist"/*.deb "$dist"/foreman-linux-* \
  "$dist"/keyring.{json,sig} "$dist"/manifest.{json,sig} "$dist"/SHA256SUMS
```

The public trust anchor is pinned in the Rust binary and bootstrap installer. The signed keyring in `signing/` delegates authority to release keys. To rotate a delegate, generate a new Ed25519 key, update `signing/keyring.json` with a strictly higher revision, and sign the new file offline with the root. Include both valid delegates during a transition if needed. Leave revoked keys out of the new record. Installations record the highest keyring revision they apply. Use `openssl pkeyutl -sign -rawin -inkey ROOT_PRIVATE_KEY -in signing/keyring.json -out signing/keyring.sig` to sign the new record. The manifest expires within 180 days of signing and by its delegate's expiry date. Publish a new release before the active manifest expires.

A lost or compromised root requires an authenticated maintainer announcement and a fresh trusted bootstrap. Release-key rotation under the existing root leaves the embedded trust anchor intact.
