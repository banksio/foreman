mod apt;
mod kernel;

use crate::{AppResult, Context, Error, Outcome, Report, process};
use schemars::JsonSchema;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub use apt::{upgrade, verify_apt_plan};
pub use kernel::kernels;

const MIB: u64 = 1024 * 1024;
const BOOT: &str = "/boot/firmware";
const READ_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct Host {
    pub profile: &'static str,
    pub systemd: bool,
    pub hostname: String,
    pub kernel: String,
    pub architecture: &'static str,
    pub os: String,
    pub pi_model: Option<String>,
    pub bootloader: Option<String>,
}

impl Host {
    pub fn detect() -> Self {
        let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
        let keys: BTreeMap<_, _> = release
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key, value.trim_matches(['"', '\''])))
            .collect();
        let id = keys.get("ID").copied().unwrap_or_default();
        let pi_model = fs::read_to_string("/proc/device-tree/model")
            .ok()
            .map(|model| model.trim_matches('\0').trim().to_owned())
            .filter(|model| model.starts_with("Raspberry Pi"));
        let pi_os = pi_model.is_some()
            && (id == "raspbian"
                || id == "debian"
                    && (Path::new("/etc/rpi-issue").exists()
                        || keys
                            .get("PRETTY_NAME")
                            .is_some_and(|name| name.contains("Raspberry Pi"))));
        let profile = if !cfg!(target_os = "linux") {
            "unsupported"
        } else if pi_os {
            "raspberry-pi-os"
        } else if id == "debian"
            || id == "raspbian"
            || keys.get("ID_LIKE").is_some_and(|families| {
                families.split_whitespace().any(|family| family == "debian")
            })
        {
            "debian"
        } else {
            "linux"
        };
        let bootloader = pi_os
            .then(|| command_text("vcgencmd", &["bootloader_version"]))
            .flatten()
            .and_then(|output| output.lines().next().map(str::to_owned));
        Self {
            profile,
            systemd: cfg!(target_os = "linux") && Path::new("/run/systemd/system").is_dir(),
            hostname: command_text("hostname", &[]).unwrap_or_else(|| "(unknown)".into()),
            kernel: command_text("uname", &["-r"]).unwrap_or_else(|| "(unknown)".into()),
            architecture: std::env::consts::ARCH,
            os: keys
                .get("PRETTY_NAME")
                .copied()
                .unwrap_or(std::env::consts::OS)
                .into(),
            pi_model,
            bootloader,
        }
    }

    pub fn is_debian(&self) -> bool {
        matches!(self.profile, "debian" | "raspberry-pi-os")
    }
}

fn pi_profile(ctx: &Context<'_>) -> bool {
    ctx.host.profile == "raspberry-pi-os" && ctx.module_enabled("raspberry-pi-os")
}

fn command_text(name: &str, args: &[&str]) -> Option<String> {
    process::run(name, args, READ_TIMEOUT)
        .ok()
        .filter(|result| result.success)
        .map(|result| result.stdout.trim().into())
        .filter(|text: &String| !text.is_empty())
}

fn pi_power(ctx: &Context<'_>, warnings: &mut Vec<String>) -> AppResult<()> {
    if !pi_profile(ctx) {
        return Ok(());
    }
    let flags = command_text("vcgencmd", &["get_throttled"]).and_then(|output| {
        output
            .strip_prefix("throttled=0x")
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
    });
    match flags {
        Some(flags) if flags & 1 != 0 => Err(Error::blocked("Raspberry Pi undervoltage is active")),
        Some(flags) if flags & (1 << 16) != 0 => {
            warnings.push("Raspberry Pi recorded undervoltage earlier in this boot".into());
            Ok(())
        }
        Some(_) => Ok(()),
        None => Err(Error::blocked(
            "Cannot verify Raspberry Pi power with vcgencmd get_throttled",
        )),
    }
}

fn synchronized() -> Option<bool> {
    if !process::available("timedatectl") {
        return None;
    }
    command_text("timedatectl", &["show", "-p", "NTPSynchronized", "--value"]).and_then(|value| {
        match value.as_str() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    })
}

fn clock_safety(ctx: &Context<'_>, wait: Duration) -> AppResult<()> {
    if !ctx.config.preflight.require_clock_sync {
        return Ok(());
    }
    let deadline = Instant::now() + wait;
    loop {
        match synchronized() {
            Some(true) => return Ok(()),
            None if !pi_profile(ctx) && !ctx.host.systemd => return Ok(()),
            _ if Instant::now() >= deadline => {
                return Err(Error::blocked(
                    "Cannot verify that the system clock is synchronized",
                ));
            }
            _ => thread::sleep(Duration::from_secs(2).min(deadline - Instant::now())),
        }
    }
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Endpoint {
    host: String,
    port: u16,
}

fn endpoint(input: &str, default_port: u16) -> Option<Endpoint> {
    let authority = input.split('/').next()?.rsplit('@').next()?;
    let (host, port) = if let Some(ipv6) = authority.strip_prefix('[') {
        let (host, suffix) = ipv6.split_once(']')?;
        let port = if suffix.is_empty() {
            default_port
        } else {
            suffix.strip_prefix(':')?.parse().ok()?
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host, port.parse().ok()?)
    } else {
        (authority, default_port)
    };
    if host.is_empty()
        || port == 0
        || host.starts_with('-')
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".:-_".contains(&byte))
    {
        return None;
    }
    Some(Endpoint {
        host: host.into(),
        port,
    })
}

fn endpoints(ctx: &Context<'_>) -> AppResult<BTreeSet<Endpoint>> {
    let mut targets = BTreeSet::new();
    for configured in &ctx.config.preflight.network_hosts {
        targets.insert(endpoint(configured, 443).ok_or_else(|| {
            Error::invalid("preflight.network_hosts requires hostnames or host:port entries")
        })?);
    }
    if !ctx.host.is_debian() {
        return Ok(targets);
    }
    let result = process::run(
        "apt-get",
        &[
            "-o",
            "Dir::Cache::pkgcache=",
            "-o",
            "Dir::Cache::srcpkgcache=",
            "indextargets",
            "--no-release-info",
            "--format",
            "$(REPO_URI)",
        ],
        Duration::from_secs(30),
    )?;
    if !result.success {
        return Err(Error::blocked(
            "APT could not enumerate configured repository targets",
        ));
    }
    for repository in result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if repository.starts_with("file:") || repository.starts_with("copy:") {
            continue;
        }
        let (scheme, address) = repository
            .split_once("://")
            .ok_or_else(|| Error::blocked("APT repository transport could not be checked"))?;
        let port = match scheme {
            "http" => 80,
            "https" => 443,
            "ftp" => 21,
            _ => {
                return Err(Error::blocked(
                    "APT repository transport could not be checked",
                ));
            }
        };
        targets.insert(endpoint(address, port).ok_or_else(|| {
            Error::blocked("APT repository network address could not be checked")
        })?);
    }
    Ok(targets)
}

fn network_safety(ctx: &Context<'_>, wait: Duration) -> AppResult<()> {
    let targets = endpoints(ctx)?;
    if !targets.is_empty() && !process::available("nc") {
        return Err(Error::blocked(
            "Repository reachability requires a netcat TCP probe",
        ));
    }
    let deadline = Instant::now() + wait;
    let mut unreachable = Vec::new();
    for target in targets {
        let port = target.port.to_string();
        loop {
            let result = process::run(
                "nc",
                &["-z", "-w", "5", &target.host, &port],
                Duration::from_secs(7),
            );
            if result.is_ok_and(|status| status.success) {
                break;
            }
            if Instant::now() >= deadline {
                unreachable.push(format!("{}:{}", target.host, target.port));
                break;
            }
            thread::sleep(Duration::from_secs(2).min(deadline - Instant::now()));
        }
    }
    if unreachable.is_empty() {
        Ok(())
    } else {
        Err(Error::blocked(format!(
            "Cannot reach {}",
            unreachable.join(", ")
        )))
    }
}

pub fn preflight(ctx: &Context<'_>) -> AppResult<Report> {
    let mut warnings = Vec::new();
    if ctx.host.is_debian() {
        for tool in ["apt-get", "apt-cache", "apt-mark", "dpkg", "dpkg-query"] {
            if !process::available(tool) {
                return Err(Error::blocked(format!(
                    "The Debian provider requires {tool}"
                )));
            }
        }
        if ctx.host.systemd
            && [
                "apt-daily.service",
                "apt-daily-upgrade.service",
                "unattended-upgrades.service",
            ]
            .iter()
            .any(|unit| {
                command_text("systemctl", &["is-active", unit])
                    .is_some_and(|state| state == "active")
            })
        {
            warnings.push("An unattended APT service is active. Package operations may need to wait for its lock.".into());
        }
    }
    pi_power(ctx, &mut warnings)?;
    clock_safety(ctx, Duration::ZERO)?;
    network_safety(ctx, Duration::ZERO)?;
    let required = ctx.config.preflight.min_free_mb.saturating_mul(MIB);
    let mut root = filesystem("/")?;
    if root.free_bytes < required && ctx.host.is_debian() {
        if ctx.dry_run {
            warnings.push(
                "An APT cache clean may make enough space. Preview leaves the cache intact.".into(),
            );
        } else {
            apt::clean()?;
            root = filesystem("/")?;
        }
    }
    enforce_space("/", root, required)?;
    if pi_profile(ctx) && Path::new(BOOT).is_dir() {
        enforce_space(
            BOOT,
            filesystem(BOOT)?,
            ctx.config.preflight.min_free_boot_mb.saturating_mul(MIB),
        )?;
    }
    let mut report = Report::new(Outcome::Completed, "Preflight checks passed");
    report.warnings = warnings;
    Ok(report)
}

pub fn boot_id() -> AppResult<String> {
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| Error::blocked("Cannot read the Linux boot ID"))?;
    let id = id.trim();
    if id.is_empty() {
        return Err(Error::blocked("The Linux boot ID is empty"));
    }
    Ok(id.into())
}

pub fn record_kernel_upgrade(ctx: &Context<'_>) -> AppResult<()> {
    let id = boot_id()?;
    crate::store::atomic_write(
        &ctx.paths.observation("kernel-updated-boot"),
        id.as_bytes(),
        0o644,
    )
}

#[derive(Debug, JsonSchema, Serialize)]
pub struct RebootStatus {
    pub needed: bool,
    pub reason: String,
    pub expected_kernel: Option<String>,
}

pub fn reboot_status(ctx: &Context<'_>) -> RebootStatus {
    let newest = ctx
        .host
        .is_debian()
        .then(|| kernel::newest_bootable(&ctx.host.kernel))
        .flatten();
    let newer = newest
        .as_ref()
        .is_some_and(|version| version != &ctx.host.kernel);
    let kernel_updated = boot_id().ok().is_some_and(|id| {
        fs::read_to_string(ctx.paths.state_dir.join("kernel-updated-boot"))
            .is_ok_and(|marker| marker.trim() == id)
    });
    let reason = if Path::new("/run/reboot-required").exists() {
        "The system flagged a reboot as required".into()
    } else if pi_profile(ctx)
        && [BOOT, "/boot"].iter().any(|base| {
            Path::new(base).join("pieeprom.upd").is_file()
                && Path::new(base).join("pieeprom.sig").is_file()
        })
    {
        "A Raspberry Pi bootloader update is staged".into()
    } else if kernel_updated {
        format!("The package for {} was updated", ctx.host.kernel)
    } else if newer {
        format!(
            "Running {}, newest bootable kernel is {}",
            ctx.host.kernel,
            newest.as_deref().unwrap_or_default()
        )
    } else {
        String::new()
    };
    RebootStatus {
        needed: !reason.is_empty(),
        reason,
        expected_kernel: newest
            .or_else(|| (ctx.host.kernel != "(unknown)").then(|| ctx.host.kernel.clone())),
    }
}

pub fn reboot_clock_safety(ctx: &Context<'_>) -> AppResult<()> {
    clock_safety(
        ctx,
        if pi_profile(ctx) {
            Duration::from_secs(60)
        } else {
            Duration::ZERO
        },
    )
}

pub fn reboot_safety(ctx: &Context<'_>) -> AppResult<()> {
    if !ctx.host.systemd {
        return Err(Error::blocked("Reboot requires a running systemd host"));
    }
    pi_power(ctx, &mut Vec::new())?;
    reboot_clock_safety(ctx)?;
    for service in &ctx.config.reboot.watch_services {
        if !valid_unit(service) {
            return Err(Error::invalid(
                "reboot.watch_services contains an invalid systemd unit name",
            ));
        }
        if !enabled_at_boot(service)
            && (service.contains('.') || !enabled_at_boot(&format!("{service}.socket")))
        {
            return Err(Error::blocked(format!("{service} is not enabled at boot")));
        }
    }
    if process::available("sshd") && !process::run("sshd", &["-t"], READ_TIMEOUT)?.success {
        return Err(Error::blocked("sshd configuration failed validation"));
    }
    Ok(())
}

fn enabled_at_boot(unit: &str) -> bool {
    let state = command_text("systemctl", &["is-enabled", "--", unit]);
    match state.as_deref() {
        Some("enabled") => true,
        Some("alias") => {
            command_text("systemctl", &["show", "-p", "Id", "--value", "--", unit])
                .filter(|id| id != unit && valid_unit(id))
                .and_then(|id| command_text("systemctl", &["is-enabled", "--", &id]))
                .as_deref()
                == Some("enabled")
        }
        _ => false,
    }
}

fn valid_unit(unit: &str) -> bool {
    !unit.is_empty()
        && !unit.starts_with('-')
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-:@".contains(&b))
}

pub fn verify(ctx: &Context<'_>, expected: Option<&str>) -> AppResult<Report> {
    if let Some(expected) = expected
        && expected != ctx.host.kernel
    {
        return Err(Error::failed(format!(
            "Booted {}, expected {expected}",
            ctx.host.kernel
        )));
    }
    clock_safety(ctx, Duration::from_secs(60))
        .map_err(|_| Error::failed("Clock never synchronized after reboot"))?;
    network_safety(ctx, Duration::from_secs(120))
        .map_err(|_| Error::failed("Required network endpoints did not recover after reboot"))?;
    for service in &ctx.config.reboot.watch_services {
        if !valid_unit(service) {
            return Err(Error::invalid(
                "reboot.watch_services contains an invalid systemd unit name",
            ));
        }
        let active = |unit| {
            command_text("systemctl", &["is-active", "--", unit])
                .is_some_and(|state| state == "active")
        };
        if !active(service) && (service.contains('.') || !active(&format!("{service}.socket"))) {
            return Err(Error::failed(format!("{service} is inactive after reboot")));
        }
    }
    Ok(Report::new(
        Outcome::Completed,
        format!("Verified boot of {}", ctx.host.kernel),
    ))
}

#[derive(Debug, Serialize)]
struct Filesystem {
    path: String,
    #[serde(skip)]
    device: u64,
    total_bytes: u64,
    free_bytes: u64,
    used_bytes: u64,
    used_percent: u64,
    inode_count: Option<u64>,
    free_inodes: Option<u64>,
    inode_used_percent: Option<u64>,
}

fn usage_percent(used: u64, available: u64) -> u64 {
    let total = u128::from(used) + u128::from(available);
    if total == 0 {
        0
    } else {
        (u128::from(used) * 100).div_ceil(total) as u64
    }
}

#[allow(clippy::unnecessary_cast)]
fn filesystem(path: &str) -> AppResult<Filesystem> {
    let filename = CString::new(Path::new(path).as_os_str().as_bytes())
        .map_err(|_| Error::invalid("Disk path contains a null byte"))?;
    let device = fs::metadata(path)
        .map_err(|err| Error::failed(format!("Cannot read filesystem for {path}: {err}")))?
        .dev();
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs initializes the structure only on success.
    if unsafe { libc::statvfs(filename.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(Error::failed(format!(
            "Cannot measure {path}: {}",
            std::io::Error::last_os_error()
        )));
    }
    let stats = unsafe { stats.assume_init() };
    let block_size = (stats.f_frsize as u64).max(1);
    let total_bytes = (stats.f_blocks as u64).saturating_mul(block_size);
    let used_bytes =
        (stats.f_blocks.saturating_sub(stats.f_bfree) as u64).saturating_mul(block_size);
    let free_bytes = (stats.f_bavail as u64).saturating_mul(block_size);
    let inodes_supported =
        stats.f_files > 0 && stats.f_ffree <= stats.f_files && stats.f_favail <= stats.f_files;
    let inode_count = inodes_supported.then_some(stats.f_files as u64);
    let free_inodes = inodes_supported.then_some(stats.f_favail as u64);
    let inode_used_percent = free_inodes
        .map(|free| usage_percent(stats.f_files.saturating_sub(stats.f_ffree) as u64, free));
    Ok(Filesystem {
        path: path.into(),
        device,
        total_bytes,
        free_bytes,
        used_bytes,
        used_percent: usage_percent(used_bytes, free_bytes),
        inode_count,
        free_inodes,
        inode_used_percent,
    })
}

fn enforce_space(path: &str, space: Filesystem, required: u64) -> AppResult<()> {
    if space.free_bytes < required {
        return Err(Error::blocked(format!(
            "{path} has {} MiB free, needs {} MiB",
            space.free_bytes / MIB,
            required.div_ceil(MIB)
        )));
    }
    if space.free_inodes == Some(0) {
        return Err(Error::blocked(format!("{path} has no free inodes")));
    }
    Ok(())
}

pub fn disk_live(ctx: &Context<'_>) -> AppResult<Report> {
    disk_report(ctx, false)
}

pub fn disk(ctx: &Context<'_>) -> AppResult<Report> {
    disk_report(ctx, true)
}

fn disk_report(ctx: &Context<'_>, measure_consumers: bool) -> AppResult<Report> {
    let settings = &ctx.config.disk;
    let mut paths = settings.paths.clone();
    if ctx.module_enabled("docker")
        && process::available("docker")
        && let Some(path) = command_text("docker", &["info", "--format", "{{.DockerRootDir}}"])
        && Path::new(&path).is_absolute()
    {
        paths.push(path);
    }
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for path in paths {
        match filesystem(&path) {
            Ok(row) if seen.insert(row.device) => {
                for (resource, percent) in [
                    ("space", Some(row.used_percent)),
                    ("inodes", row.inode_used_percent),
                ] {
                    if let Some(percent) =
                        percent.filter(|percent| *percent >= settings.warn_percent)
                    {
                        let severity = if percent >= settings.critical_percent {
                            "critical"
                        } else {
                            "warning"
                        };
                        warnings.push(format!(
                            "{} {resource} usage is {percent}% ({severity})",
                            row.path
                        ));
                    }
                }
                rows.push(row);
            }
            Err(_) if path != BOOT || Path::new(BOOT).exists() => {
                warnings.push(format!("Cannot measure filesystem at {path}"));
            }
            _ => {}
        }
    }
    let mut consumers = Vec::new();
    if measure_consumers {
        let deadline = Instant::now() + Duration::from_secs(180);
        for path in &settings.consumers {
            if !Path::new(path).exists() {
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warnings.push("Directory measurement deadline reached".into());
                break;
            }
            let result = process::run(
                "du",
                &["-sxk", "--", path],
                remaining.min(Duration::from_secs(60)),
            );
            if let Ok(result) = result {
                if let Some(kib) = result
                    .stdout
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    consumers.push((path, kib.saturating_mul(1024)));
                }
                if !result.success {
                    warnings.push(format!(
                        "Directory measurement for {path} may be incomplete"
                    ));
                }
            } else {
                warnings.push(format!("Cannot measure directory {path}"));
            }
        }
        consumers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    }
    let summary = if rows.is_empty() {
        "No readable filesystems".into()
    } else {
        rows.iter()
            .map(|row| format!("{} {}% full", row.path, row.used_percent))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut report = Report::new(Outcome::Completed, summary).detail(serde_json::json!({
        "filesystems": rows,
        "consumers": consumers.into_iter().map(|(path, bytes)| serde_json::json!({"path": path, "bytes": bytes})).collect::<Vec<_>>()
    }));
    report.warnings = warnings;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_repository_credentials_and_handles_custom_ports() {
        assert_eq!(
            endpoint("user:password@[2001:db8::1]:8000/mirror", 443),
            Some(Endpoint {
                host: "2001:db8::1".into(),
                port: 8000
            })
        );
        assert!(endpoint("-bad.example", 443).is_none());
        assert!(endpoint("example.net:0", 443).is_none());
    }

    #[test]
    fn inode_pressure_is_rounded_up_and_handles_overflow() {
        assert_eq!(usage_percent(849, 151), 85);
        assert_eq!(usage_percent(u64::MAX, u64::MAX), 50);
        assert_eq!(usage_percent(0, 0), 0);
    }
}
