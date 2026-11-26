use crate::{
    AppResult, Context, Error, Outcome, Report, cycle, docker, host, notify, process, schedule,
    store, systemd,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const EXECUTABLE: &str = "/usr/local/bin/foreman";
const HOME: &str = "/usr/local/lib/foreman";
const UNITS: &str = "/usr/local/lib/systemd/system";
const RELEASES: &str = "https://github.com/banksio/foreman/releases";
const ROOT: &[u8] = include_bytes!("../signing/root.pem");
const METADATA_LIMIT: u64 = 64 * 1024;
const BINARY_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    executable: PathBuf,
    version: String,
    sha256: String,
    revision: u64,
    keyring_revision: u64,
    keyring_sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Transaction {
    previous: Option<Receipt>,
    next: Receipt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keyring {
    schema_version: u32,
    revision: u64,
    keys: Vec<Key>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    id: String,
    public_key: String,
    expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    key_id: String,
    keyring_revision: u64,
    version: String,
    revision: u64,
    expires_at: DateTime<Utc>,
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    platform: String,
    file: String,
    sha256: String,
}

fn local(name: &str) -> PathBuf {
    Path::new(HOME).join(name)
}

fn version_revision(version: &str) -> Option<u64> {
    let values: Vec<_> = version
        .split('.')
        .map(|part| {
            let value = part.parse::<u64>().ok()?;
            (value < 1_000_000 && part == value.to_string()).then_some(value)
        })
        .collect::<Option<_>>()?;
    let [major, minor, patch] = values.as_slice() else {
        return None;
    };
    Some(major * 1_000_000_000_000 + minor * 1_000_000 + patch)
}

fn platform() -> AppResult<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        _ => Err(Error::blocked(
            "Standalone Foreman supports Linux arm64 and amd64",
        )),
    }
}

fn is_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn exists(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn bytes(path: &Path, limit: u64) -> AppResult<Vec<u8>> {
    let mut result = Vec::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::blocked(
            "Standalone release input must be a regular file",
        ));
    }
    file.take(limit + 1).read_to_end(&mut result)?;
    if result.len() as u64 > limit {
        return Err(Error::blocked(
            "Standalone release file exceeds its size limit",
        ));
    }
    Ok(result)
}

fn json<T: DeserializeOwned>(path: &Path) -> AppResult<T> {
    serde_json::from_slice(&bytes(path, METADATA_LIMIT)?)
        .map_err(|_| Error::blocked(format!("Invalid standalone metadata in {}", path.display())))
}

fn sha256(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn verify_signature(key: &Path, data: &Path, signature: &Path) -> AppResult<()> {
    if bytes(signature, 64)?.len() != 64 {
        return Err(Error::blocked("Invalid Ed25519 release signature"));
    }
    let key = as_arg(key)?;
    let data = as_arg(data)?;
    let signature = as_arg(signature)?;
    let output = process::run(
        "/usr/bin/openssl",
        &[
            "pkeyutl", "-verify", "-pubin", "-rawin", "-inkey", key, "-in", data, "-sigfile",
            signature,
        ],
        Duration::from_secs(15),
    )?;
    if !output.success {
        return Err(Error::blocked(
            "Standalone signature verification failed. OpenSSL 3 is required",
        ));
    }
    Ok(())
}

fn as_arg(path: &Path) -> AppResult<&str> {
    path.to_str()
        .ok_or_else(|| Error::invalid("Standalone operation requires UTF-8 paths"))
}

fn verify_metadata(
    dir: &Path,
    tag: Option<&str>,
    now: DateTime<Utc>,
) -> AppResult<(Receipt, String)> {
    let root = dir.join("root.pem");
    store::atomic_write(&root, ROOT, 0o600)?;
    let keyring_path = dir.join("keyring.json");
    let manifest_path = dir.join("manifest.json");
    let keyring: Keyring = json(&keyring_path)?;
    verify_signature(&root, &keyring_path, &dir.join("keyring.sig"))?;
    let manifest: Manifest = json(&manifest_path)?;
    if keyring.schema_version != 1
        || keyring.revision == 0
        || manifest.schema_version != 1
        || keyring.revision != manifest.keyring_revision
        || manifest.expires_at <= now
        || version_revision(&manifest.version) != Some(manifest.revision)
        || manifest.revision == 0
        || tag.is_some_and(|tag| tag.strip_prefix('v') != Some(&manifest.version))
    {
        return Err(Error::blocked(
            "Standalone release metadata has an unsupported version, expiry, or identity",
        ));
    }
    let mut keys = keyring.keys.iter().filter(|key| key.id == manifest.key_id);
    let key = keys
        .next()
        .filter(|key| {
            keys.next().is_none() && key.expires_at > now && manifest.expires_at <= key.expires_at
        })
        .ok_or_else(|| {
            Error::blocked("Standalone release signing key is unavailable or expired")
        })?;
    let signer = dir.join("signer.pem");
    store::atomic_write(&signer, key.public_key.as_bytes(), 0o600)?;
    verify_signature(&signer, &manifest_path, &dir.join("manifest.sig"))?;

    let platform = platform()?;
    let name = format!("foreman-{platform}");
    let mut candidates = manifest
        .artifacts
        .iter()
        .filter(|asset| asset.platform == platform);
    let asset = candidates
        .next()
        .filter(|asset| candidates.next().is_none() && asset.file == name && is_hex(&asset.sha256))
        .ok_or_else(|| Error::blocked("No signed standalone binary for this platform"))?;
    Ok((
        Receipt {
            schema_version: 1,
            executable: EXECUTABLE.into(),
            version: manifest.version,
            sha256: asset.sha256.clone(),
            revision: manifest.revision,
            keyring_revision: manifest.keyring_revision,
            keyring_sha256: sha256(&bytes(&keyring_path, METADATA_LIMIT)?),
        },
        name,
    ))
}

fn fetch(base: &str, name: &str, destination: &Path, limit: u64) -> AppResult<()> {
    let url = format!("{base}/{name}");
    let limit = limit.to_string();
    let output = process::run(
        "/usr/bin/curl",
        &[
            "--disable",
            "--fail",
            "--silent",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--connect-timeout",
            "10",
            "--max-time",
            "90",
            "--max-filesize",
            &limit,
            "--output",
            as_arg(destination)?,
            &url,
        ],
        Duration::from_secs(95),
    )?;
    if !output.success {
        return Err(Error::failed(format!(
            "Download failed for {name}. Check HTTPS connectivity and the GitHub release"
        )));
    }
    Ok(())
}

fn download_metadata(dir: &Path, tag: Option<&str>) -> AppResult<(Receipt, String)> {
    let endpoint = tag.map_or_else(
        || format!("{RELEASES}/latest/download"),
        |tag| format!("{RELEASES}/download/{tag}"),
    );
    for (name, limit) in [
        ("keyring.json", METADATA_LIMIT),
        ("keyring.sig", 64),
        ("manifest.json", METADATA_LIMIT),
        ("manifest.sig", 64),
    ] {
        fetch(&endpoint, name, &dir.join(name), limit)?;
    }
    verify_metadata(dir, tag, Utc::now())
}

fn receipt() -> AppResult<Receipt> {
    let path = local("receipt.json");
    if !exists(&path)? {
        return Err(Error::blocked(
            "No standalone installation receipt. Use the official installer with --standalone",
        ));
    }
    store::trusted_file(&path)?;
    let receipt: Receipt = json(&path)?;
    if receipt.schema_version != 1
        || receipt.executable != Path::new(EXECUTABLE)
        || version_revision(&receipt.version) != Some(receipt.revision)
        || receipt.keyring_revision == 0
        || !is_hex(&receipt.sha256)
        || !is_hex(&receipt.keyring_sha256)
    {
        return Err(Error::blocked(
            "Unsupported or mismatched standalone installation receipt",
        ));
    }
    Ok(receipt)
}

fn running_at_install_path() -> bool {
    std::env::current_exe().is_ok_and(|path| path == Path::new(EXECUTABLE))
}

pub fn uses_local_binary() -> bool {
    running_at_install_path() && receipt().is_ok()
}

fn debian_package() -> AppResult<Option<String>> {
    let path = Path::new("/usr/bin/foreman");
    if process::available("dpkg-query") {
        let query = process::run(
            "/usr/bin/dpkg-query",
            &["-S", "--", "/usr/bin/foreman"],
            Duration::from_secs(10),
        )?;
        match query.code {
            Some(0)
                if matches!(
                    query.stdout.trim(),
                    "foreman: /usr/bin/foreman"
                        | "foreman:arm64: /usr/bin/foreman"
                        | "foreman:amd64: /usr/bin/foreman"
                ) =>
            {
                let version = process::run(
                    "/usr/bin/dpkg-query",
                    &["-W", "-f=${Version}", "foreman"],
                    Duration::from_secs(10),
                )?;
                if !version.success || version.stdout.trim().is_empty() {
                    return Err(Error::blocked(
                        "Could not determine the installed Foreman package version",
                    ));
                }
                return Ok(Some(version.stdout.trim().to_owned()));
            }
            Some(1) => {}
            _ => {
                return Err(Error::blocked(
                    "Could not verify that the Foreman package owns /usr/bin/foreman",
                ));
            }
        }
    }
    if exists(path)? {
        return Err(Error::blocked(
            "An executable at /usr/bin/foreman is outside the supported package migration path",
        ));
    }
    Ok(None)
}

fn migration_required() -> Error {
    Error::blocked(
        "This Foreman installation is managed by dpkg. Run the latest install.sh with --migrate-deb to enable self-update",
    )
}

fn reject_debian_install() -> AppResult<()> {
    if debian_package()?.is_some() {
        return Err(migration_required());
    }
    Ok(())
}

fn check_migration_version(previous: &str, next: &str) -> AppResult<()> {
    let downgrade = process::run(
        "/usr/bin/dpkg",
        &["--compare-versions", previous, "gt", next],
        Duration::from_secs(10),
    )?;
    match downgrade.code {
        Some(0) => Err(Error::blocked(
            "The Debian Foreman package is newer than the standalone candidate",
        )),
        Some(1) => Ok(()),
        _ => Err(Error::blocked(
            "Could not compare the package and standalone release versions",
        )),
    }
}

fn remove_debian_package() -> AppResult<()> {
    let removal = process::run(
        "/usr/bin/dpkg",
        &["--remove", "foreman"],
        Duration::from_secs(120),
    )?;
    if !removal.success {
        return Err(Error::failed(format!(
            "Standalone Foreman is staged, but removing the Debian package failed. Resolve the dpkg error and rerun the installer with --migrate-deb. {}",
            removal.stderr.trim()
        )));
    }
    reject_debian_install()
}

pub fn verify_state(ctx: &Context<'_>) -> AppResult<()> {
    cycle::read(ctx)?;
    cycle::last_steps(ctx)?;
    notify::status(ctx)?;
    schedule::list(ctx)?;
    docker::validate_update_state(ctx)?;
    Ok(())
}

fn gate(ctx: &Context<'_>, upgrading: bool) -> AppResult<()> {
    cycle::reject_pending(ctx)?;
    if upgrading && host::reboot_status(ctx).needed {
        return Err(Error::blocked(
            "Resolve the pending host reboot before replacing standalone Foreman",
        ));
    }
    verify_state(ctx)?;
    Ok(())
}

fn check_runnable(path: &Path, version: &str) -> AppResult<()> {
    let path = as_arg(path)?;
    let output = process::run(path, &["--version"], Duration::from_secs(12))?;
    if !output.success || output.stdout.trim() != format!("foreman {version}") {
        return Err(Error::blocked(
            "Verified standalone candidate did not report the signed release version",
        ));
    }
    let output = process::run(path, &["__validate-update-state"], Duration::from_secs(180))?;
    if !output.success {
        return Err(Error::blocked(
            "Verified standalone candidate could not read the current settings and state",
        ));
    }
    Ok(())
}

fn persist_receipt(receipt: &Receipt) -> AppResult<()> {
    store::atomic_write(
        &local("receipt.json"),
        &serde_json::to_vec_pretty(receipt)?,
        0o644,
    )
}

fn journal() -> PathBuf {
    local("transaction.json")
}

fn recover() -> AppResult<bool> {
    if !exists(&journal())? {
        return Ok(false);
    }
    store::trusted_file(&journal())?;
    let tx: Transaction = json(&journal())?;
    let selected = Path::new(EXECUTABLE);
    let active = if exists(selected)? {
        store::trusted_file(selected)?;
        Some(sha256(&bytes(selected, BINARY_LIMIT)?))
    } else {
        None
    };
    let selected_next = active.as_deref() == Some(&tx.next.sha256);
    if selected_next && check_runnable(selected, &tx.next.version).is_ok() {
        persist_receipt(&tx.next)?;
        store::remove(&journal())?;
        return Ok(true);
    }
    if active.is_some()
        && !selected_next
        && active.as_deref() != tx.previous.as_ref().map(|r| r.sha256.as_str())
    {
        return Err(Error::blocked(
            "Interrupted update found an unexpected executable. Review the protected transaction and backup",
        ));
    }
    if let Some(previous) = tx.previous {
        let backup = local("previous.bin");
        store::trusted_file(&backup)?;
        let old = bytes(&backup, BINARY_LIMIT)?;
        if sha256(&old) != previous.sha256 {
            return Err(Error::blocked(
                "Previous executable backup failed verification",
            ));
        }
        store::atomic_write(selected, &old, 0o755)?;
        persist_receipt(&previous)?;
    } else {
        store::remove(selected)?;
        store::remove(&local("receipt.json"))?;
    }
    store::remove(&journal())?;
    Err(Error::failed(
        "Standalone activation failed. Restored the previous installation",
    ))
}

fn backup_state(ctx: &Context<'_>) -> AppResult<()> {
    let mut args = vec!["-C", "/", "-cf"];
    let mut file = tempfile::NamedTempFile::new_in(HOME)?;
    args.push(as_arg(file.path())?);
    for directory in [&ctx.paths.config_dir, &ctx.paths.state_dir] {
        if exists(directory)? {
            args.push(as_arg(directory.strip_prefix("/").unwrap_or(directory))?);
        }
    }
    if args.len() == 4 {
        return Ok(());
    }
    let output = process::run("tar", &args, Duration::from_secs(90))?;
    if !output.success {
        return Err(Error::failed(
            "Could not back up Foreman settings and state",
        ));
    }
    let listing = process::run(
        "tar",
        &["-tf", as_arg(file.path())?],
        Duration::from_secs(90),
    )?;
    if !listing.success {
        return Err(Error::failed(
            "Could not verify Foreman settings and state backup",
        ));
    }
    // Read and fsync the file written by tar, then move it into the protected install directory.
    file.as_file_mut().sync_all()?;
    file.persist(local("previous-state.tar"))
        .map_err(|_| Error::failed("Could not retain the state backup"))?;
    fs::File::open(HOME)?.sync_all()?;
    Ok(())
}

fn activate(
    ctx: &Context<'_>,
    candidate: &Path,
    previous: Option<Receipt>,
    next: Receipt,
) -> AppResult<()> {
    let data = bytes(candidate, BINARY_LIMIT)?;
    if sha256(&data) != next.sha256 {
        return Err(Error::blocked(
            "Standalone artifact SHA-256 does not match the signed manifest",
        ));
    }
    fs::set_permissions(candidate, fs::Permissions::from_mode(0o700))?;
    check_runnable(candidate, &next.version)?;
    store::check_protected_parent(Path::new(HOME))?;
    fs::create_dir_all(HOME)?;
    if let Some(previous) = &previous {
        let old = bytes(Path::new(EXECUTABLE), BINARY_LIMIT)?;
        if sha256(&old) != previous.sha256 {
            return Err(Error::blocked(
                "Current standalone binary no longer matches its receipt",
            ));
        }
        store::atomic_write(&local("previous.bin"), &old, 0o600)?;
        backup_state(ctx)?;
    }
    let tx = Transaction { previous, next };
    store::atomic_write(&journal(), &serde_json::to_vec(&tx)?, 0o600)?;
    store::atomic_write(Path::new(EXECUTABLE), &data, 0o755)?;
    recover()?;
    Ok(())
}

fn compare(current: &Receipt, next: &Receipt) -> AppResult<bool> {
    if next.revision < current.revision
        || next.keyring_revision < current.keyring_revision
        || (next.revision == current.revision && next.sha256 != current.sha256)
        || (next.keyring_revision == current.keyring_revision
            && next.keyring_sha256 != current.keyring_sha256)
    {
        return Err(Error::blocked(
            "Standalone release metadata attempts a downgrade, replay, or replacement of an existing release",
        ));
    }
    Ok(next.revision > current.revision)
}

pub fn check_update_installation() -> AppResult<()> {
    platform()?;
    reject_debian_install()?;
    if !running_at_install_path() {
        return Err(Error::blocked(format!(
            "Self-update must run from the standalone install at {EXECUTABLE}"
        )));
    }
    Ok(())
}

pub fn update(ctx: &Context<'_>, check: bool) -> AppResult<Report> {
    if !check {
        gate(ctx, true)?;
        if recover()? {
            let active = receipt()?;
            return Ok(Report::new(
                Outcome::Completed,
                format!(
                    "Recovered standalone activation for Foreman {}",
                    active.version
                ),
            ));
        }
    } else if exists(&journal())? {
        return Err(Error::blocked(
            "A standalone activation needs recovery. Run sudo foreman self update",
        ));
    }
    let current = receipt()?;
    let running = Path::new(EXECUTABLE);
    store::trusted_file(running)?;
    if current.version != env!("CARGO_PKG_VERSION")
        || sha256(&bytes(running, BINARY_LIMIT)?) != current.sha256
    {
        return Err(Error::blocked(
            "Standalone executable differs from its installation receipt",
        ));
    }
    let temp = tempfile::tempdir_in("/tmp")?;
    let (next, artifact) = download_metadata(temp.path(), None)?;
    if !compare(&current, &next)? {
        if !check && next.keyring_revision > current.keyring_revision {
            persist_receipt(&next)?;
        }
        return Ok(Report::new(
            Outcome::Unchanged,
            format!("Foreman {} is current", current.version),
        ));
    }
    if check {
        return Ok(Report::new(
            Outcome::Completed,
            format!("Foreman {} is available", next.version),
        ));
    }
    let source = format!("{RELEASES}/download/v{}", next.version);
    let candidate = temp.path().join(&artifact);
    fetch(&source, &artifact, &candidate, BINARY_LIMIT)?;
    let version = next.version.clone();
    activate(ctx, &candidate, Some(current), next)?;
    Ok(Report::new(
        Outcome::Completed,
        format!("Updated standalone Foreman to {version}"),
    ))
}

fn setup_units(ctx: &Context<'_>) -> AppResult<()> {
    store::check_protected_parent(Path::new(UNITS))?;
    for (name, unit) in [
        (
            "foreman-resume.service",
            include_str!("../packaging/debian/foreman-resume.service"),
        ),
        (
            "foreman-notify.service",
            include_str!("../packaging/debian/foreman-notify.service"),
        ),
        (
            "foreman-notify.timer",
            include_str!("../packaging/debian/foreman-notify.timer"),
        ),
    ] {
        let path = Path::new(UNITS).join(name);
        systemd::check_owned(&path)?;
        let data = format!(
            "{}\n{}",
            systemd::MANAGED,
            unit.replace(systemd::RUNNER, EXECUTABLE)
        );
        store::atomic_write(&path, data.as_bytes(), 0o644)?;
    }
    if ctx.host.systemd {
        systemd::checked(&["daemon-reload"])?;
        systemd::install_resume(ctx)?;
    }
    Ok(())
}

pub fn install(ctx: &Context<'_>, bundle: &Path, migrate_deb: bool) -> AppResult<Report> {
    platform()?;
    let debian = debian_package()?;
    if debian.is_some() && !migrate_deb {
        return Err(migration_required());
    }
    gate(ctx, false)?;
    systemd::require_legacy_resolution(ctx)?;
    store::check_protected_parent(Path::new(EXECUTABLE).parent().unwrap())?;
    store::check_protected_parent(Path::new(HOME))?;
    store::check_protected_parent(Path::new(UNITS))?;
    for name in [
        "foreman-resume.service",
        "foreman-notify.service",
        "foreman-notify.timer",
    ] {
        systemd::check_owned(&Path::new(UNITS).join(name))?;
    }

    let mut migration_warnings = Vec::new();
    if debian.is_some() {
        for schedule in schedule::list(ctx)?
            .into_iter()
            .filter(|schedule| schedule.job.enabled)
        {
            migration_warnings.push(format!(
                "Restore the saved schedule with sudo foreman schedule enable {}",
                schedule.name
            ));
        }
        if ctx.host.systemd
            && systemd::systemctl(&["is-enabled", "--quiet", "foreman-notify.timer"])
                .is_ok_and(|output| output.success)
        {
            migration_warnings.push("Restore the notification timer with sudo systemctl enable --now foreman-notify.timer".into());
        }
    }

    if exists(&journal())? {
        recover()?;
    }
    if exists(Path::new(EXECUTABLE))? {
        let installed = receipt()?;
        store::trusted_file(Path::new(EXECUTABLE))?;
        if sha256(&bytes(Path::new(EXECUTABLE), BINARY_LIMIT)?) != installed.sha256 {
            return Err(Error::blocked(
                "The existing standalone executable differs from its installation receipt",
            ));
        }
        check_runnable(Path::new(EXECUTABLE), &installed.version)?;
        if let Some(previous) = &debian {
            check_migration_version(previous, &installed.version)?;
            remove_debian_package()?;
        }
        return finish_install(
            ctx,
            &installed.version,
            debian.is_some(),
            migration_warnings,
        );
    }
    if exists(&local("receipt.json"))? {
        return Err(Error::blocked(
            "A standalone receipt already exists without its executable. Review the previous installation",
        ));
    }
    // Copy input files into a private staging directory before validating them.
    let temp = tempfile::tempdir_in("/tmp")?;
    let file = format!("foreman-{}", platform()?);
    for (name, limit) in [
        ("keyring.json", METADATA_LIMIT),
        ("keyring.sig", 64),
        ("manifest.json", METADATA_LIMIT),
        ("manifest.sig", 64),
        (&file, BINARY_LIMIT),
    ] {
        store::atomic_write(
            &temp.path().join(name),
            &bytes(&bundle.join(name), limit)?,
            0o600,
        )?;
    }
    let (next, _) = verify_metadata(temp.path(), None, Utc::now())?;
    if next.version != env!("CARGO_PKG_VERSION") {
        return Err(Error::blocked(
            "Bootstrap executable differs in version from the verified installation bundle",
        ));
    }
    let version = next.version.clone();
    if let Some(previous) = &debian {
        check_migration_version(previous, &version)?;
        fs::create_dir_all(HOME)?;
        backup_state(ctx)?;
    }
    // Activate the signed replacement before removing the package, so the executable
    // remains available if dpkg cannot complete removal.
    activate(ctx, &temp.path().join(&file), None, next)?;
    if debian.is_some() {
        remove_debian_package()?;
    }
    finish_install(ctx, &version, debian.is_some(), migration_warnings)
}

fn finish_install(
    ctx: &Context<'_>,
    version: &str,
    migrated: bool,
    warnings: Vec<String>,
) -> AppResult<Report> {
    setup_units(ctx)?;
    let summary = if migrated {
        format!("Migrated the Debian package to standalone Foreman {version} at {EXECUTABLE}")
    } else {
        format!("Standalone Foreman {version} is installed at {EXECUTABLE}")
    };
    let mut report = Report::new(Outcome::Completed, summary);
    report.warnings = warnings;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(version: &str, keyring_revision: u64) -> Receipt {
        Receipt {
            schema_version: 1,
            executable: EXECUTABLE.into(),
            version: version.into(),
            sha256: "a".repeat(64),
            revision: version_revision(version).unwrap(),
            keyring_revision,
            keyring_sha256: "c".repeat(64),
        }
    }

    #[test]
    fn stable_versions_are_ordered_and_unambiguous() {
        assert!(version_revision("1.0.0") > version_revision("0.999999.999999"));
        for invalid in ["1.2", "1.02.3", "1.2.3-dev", "1.1000000.0", "+1.0.0"] {
            assert_eq!(version_revision(invalid), None);
        }
    }

    #[test]
    fn rejects_release_and_keyring_replay_and_same_version_replacement() {
        let current = record("0.2.1", 2);
        assert!(compare(&current, &record("0.2.2", 2)).unwrap());
        assert!(!compare(&current, &record("0.2.1", 2)).unwrap());
        assert!(compare(&current, &record("0.2.0", 2)).is_err());
        assert!(compare(&current, &record("0.2.2", 1)).is_err());
        let mut changed = record("0.2.1", 2);
        changed.sha256 = "b".repeat(64);
        assert!(compare(&current, &changed).is_err());
        let mut changed = record("0.2.2", 2);
        changed.keyring_sha256 = "d".repeat(64);
        assert!(compare(&current, &changed).is_err());
    }
}
