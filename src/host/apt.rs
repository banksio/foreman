use crate::{AppResult, Context, Error, Outcome, Report, process};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::Path,
    sync::LazyLock,
    thread,
    time::Duration,
};

use super::{BOOT, MIB, enforce_space, filesystem, record_kernel_upgrade};

const APT_ENV: &[(&str, &str)] = &[
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("APT_LISTCHANGES_FRONTEND", "none"),
    ("NEEDRESTART_MODE", "a"),
];
const NO_CACHES: &[&str] = &[
    "-o",
    "Dir::Cache::pkgcache=",
    "-o",
    "Dir::Cache::srcpkgcache=",
];
const SECURE: &[&str] = &[
    "-o",
    "Acquire::AllowInsecureRepositories=false",
    "-o",
    "Acquire::AllowWeakRepositories=false",
    "-o",
    "Acquire::AllowDowngradeToInsecureRepositories=false",
    "-o",
    "APT::Get::AllowUnauthenticated=false",
    "-o",
    "APT::Get::allow-downgrades=false",
    "-o",
    "APT::Get::allow-remove-essential=false",
    "-o",
    "APT::Get::allow-change-held-packages=false",
];
const LOCK_WAIT: &[&str] = &["-o", "DPkg::Lock::Timeout=120"];
const UPDATE: &[&str] = &[
    "-o",
    "APT::Update::Error-Mode=any",
    "-o",
    "Acquire::Retries=1",
    "-o",
    "Acquire::http::Timeout=20",
    "-o",
    "Acquire::https::Timeout=20",
    "-o",
    "Acquire::AllowReleaseInfoChange=false",
    "-o",
    "Acquire::AllowReleaseInfoChange::Origin=false",
    "-o",
    "Acquire::AllowReleaseInfoChange::Label=false",
    "-o",
    "Acquire::AllowReleaseInfoChange::Version=false",
    "-o",
    "Acquire::AllowReleaseInfoChange::Suite=false",
    "-o",
    "Acquire::AllowReleaseInfoChange::Codename=false",
    "-o",
    "Acquire::AllowReleaseInfoChange::DefaultPin=false",
    "update",
];

const PROTECTED: &[&str] = &[
    "apt",
    "apt-utils",
    "dpkg",
    "systemd",
    "systemd-sysv",
    "libc6",
    "init",
    "openssh-server",
    "openssh-client",
    "iproute2",
    "netcat-openbsd",
    "ifupdown",
    "network-manager",
    "systemd-networkd",
    "dhcpcd",
    "dhcpcd-base",
    "docker.io",
    "docker-ce",
    "docker-ce-cli",
    "containerd",
    "containerd.io",
    "runc",
    "foreman",
    "kernel-common",
    "linux-base",
    "raspberrypi-bootloader",
    "raspberrypi-kernel",
    "raspi-firmware",
    "rpi-eeprom",
    "firmware-brcm80211",
    "grub-common",
    "grub2-common",
    "grub-pc",
    "grub-efi-arm64",
    "grub-efi-amd64",
    "shim-signed",
    "initramfs-tools",
    "initramfs-tools-core",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    Unpack,
    Configure,
    Remove,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct Action {
    pub package: String,
    pub architecture: String,
    pub kind: Kind,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug)]
struct Installed {
    architecture: String,
    version: String,
    protected: bool,
    held: bool,
}

type Inventory = BTreeMap<String, Vec<Installed>>;

static SIM_ACTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(Inst|Conf|Remv|Purg) ([a-z0-9][a-z0-9+.-]*)(?::([a-z0-9-]+))?(?: \[([^\s\]]+)\])?(?: \(([^\s)]+)[^)]*? \[([a-z0-9-]+)\]\))?(?: \[.*\])?$")
        .expect("valid APT simulation expression")
});
static SIM_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^([0-9]+) upgraded, ([0-9]+) newly installed, (?:[0-9]+ reinstalled, )?(?:[0-9]+ downgraded, )?([0-9]+) to remove and [0-9]+ not upgraded\.$")
        .expect("valid APT simulation summary")
});
static DEBIAN_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[0-9]+:)?[0-9][A-Za-z0-9.+~\-]*(?:-[A-Za-z0-9.+~]+)?$")
        .expect("valid Debian package version expression")
});

fn apt(args: &[&str], seconds: u64) -> AppResult<process::Output> {
    apt_run(args, seconds, None)
}

fn apt_run(args: &[&str], seconds: u64, progress: Option<&str>) -> AppResult<process::Output> {
    let mut command = SECURE.to_vec();
    command.extend_from_slice(NO_CACHES);
    command.extend_from_slice(args);
    let timeout = Duration::from_secs(seconds);
    match progress {
        Some(label) => process::apt_with_progress(&command, timeout, APT_ENV, label),
        None => process::run_env("apt-get", &command, timeout, APT_ENV),
    }
}

fn validate_sources() -> AppResult<()> {
    let result = apt(
        &[
            "indextargets",
            "--no-release-info",
            "--format",
            "$(SOURCESENTRY)",
        ],
        30,
    )?;
    if !result.success {
        return Err(Error::blocked(
            "APT could not enumerate active source definitions",
        ));
    }
    let mut inspected = BTreeSet::new();
    for entry in result.stdout.lines().filter(|entry| !entry.is_empty()) {
        let source = entry
            .rsplit_once(':')
            .and_then(|(path, position)| {
                position.parse::<usize>().ok()?;
                Path::new(path).is_absolute().then_some(Path::new(path))
            })
            .ok_or_else(|| Error::blocked("APT did not identify an active source definition"))?;
        if !inspected.insert(source) {
            continue;
        }
        let text = fs::read_to_string(source).map_err(|_| {
            Error::blocked(format!(
                "Cannot inspect APT source file {}",
                source.display()
            ))
        })?;
        let deb822 = source
            .extension()
            .is_some_and(|extension| extension == "sources");
        if has_insecure_source_options(&text, deb822) {
            return Err(Error::blocked(format!(
                "APT source file {} explicitly permits unsigned or insecure repositories",
                source.display()
            )));
        }
    }
    Ok(())
}

fn has_insecure_source_options(text: &str, deb822: bool) -> bool {
    fn false_value(text: &str) -> bool {
        matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "no" | "false" | "0" | "off" | "disable" | "disabled"
        )
    }
    fn insecure(key: &str, value: &str) -> bool {
        matches!(
            key.trim().to_ascii_lowercase().as_str(),
            "trusted" | "allow-insecure" | "allow-weak" | "allow-downgrade-to-insecure"
        ) && !false_value(value)
    }
    if !deb822 {
        return text.lines().any(|line| {
            let content = line.split('#').next().unwrap_or_default().trim();
            let Some((_, rest)) = content.split_once(char::is_whitespace) else {
                return false;
            };
            let Some(options) = rest
                .trim_start()
                .strip_prefix('[')
                .and_then(|rest| rest.split_once(']').map(|(options, _)| options))
            else {
                return false;
            };
            options
                .split_whitespace()
                .filter_map(|option| option.split_once('='))
                .any(|(key, value)| insecure(key, value))
        });
    }
    // APT's deb822 format permits folded values, including for Enabled.
    fn active_insecure(fields: &[(String, String)]) -> bool {
        !fields
            .iter()
            .any(|(key, value)| key.eq_ignore_ascii_case("enabled") && false_value(value))
            && fields.iter().any(|(key, value)| insecure(key, value))
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if line.trim().is_empty() {
            if active_insecure(&fields) {
                return true;
            }
            fields.clear();
        } else if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else if let Some((key, value)) = line.split_once(':') {
            fields.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    active_insecure(&fields)
}

fn refresh(progress: bool) -> AppResult<()> {
    let mut args = LOCK_WAIT.to_vec();
    args.extend_from_slice(UPDATE);
    for retry in 0..=2 {
        let result = apt_run(&args, 600, progress.then_some("APT indexes"))?;
        if result.success {
            return Ok(());
        }
        if result.stderr.contains("Could not get lock")
            || result.stderr.contains("Unable to acquire")
        {
            if retry < 2 {
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            return Err(Error::blocked(
                "The APT indexes are locked by another package manager",
            ));
        }
        if let Some(detail) = result.stderr.lines().find(|line| {
            (line.contains(" changed its ") && line.contains(" from "))
                || line.contains("must be accepted explicitly")
        }) {
            return Err(Error::blocked(format!(
                "APT Release metadata needs operator approval. {}",
                redact_urls(detail)
            )));
        }
        return Err(Error::failed(
            "APT index refresh failed. Review the repositories with apt-get update.",
        ));
    }
    unreachable!("bounded refresh loop returns on its final attempt")
}

fn redact_urls(text: &str) -> String {
    static URL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"([a-zA-Z][a-zA-Z0-9+.-]*://)[^\s'"]+"#)
            .expect("valid repository URL expression")
    });
    URL.replace_all(text, |caps: &regex::Captures<'_>| {
        let authority = caps[0].split_once("://").map_or("", |(_, right)| {
            right
                .split('/')
                .next()
                .unwrap_or_default()
                .rsplit('@')
                .next()
                .unwrap_or_default()
        });
        format!("{}{authority}", &caps[1])
    })
    .into_owned()
}

fn audit() -> AppResult<bool> {
    let result = process::run_env("dpkg", &["--audit"], Duration::from_secs(45), APT_ENV)?;
    if !result.success {
        return Err(Error::blocked("dpkg could not verify package consistency"));
    }
    let journal = fs::read_dir("/var/lib/dpkg/updates")
        .ok()
        .is_some_and(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())
            })
        });
    Ok(journal || !result.stdout.trim().is_empty() || !result.stderr.trim().is_empty())
}

fn recover() -> AppResult<()> {
    let result = process::run_env(
        "dpkg",
        &["--force-confdef", "--force-confold", "--configure", "-a"],
        Duration::from_secs(900),
        APT_ENV,
    )?;
    if !result.success || audit()? {
        return Err(Error::failed(
            "Package recovery failed. Review dpkg --audit before retrying.",
        ));
    }
    Ok(())
}

pub(super) fn clean() -> AppResult<()> {
    let mut args = LOCK_WAIT.to_vec();
    args.push("clean");
    if apt(&args, 120)?.success {
        Ok(())
    } else {
        Err(Error::blocked("APT cache cleanup failed"))
    }
}

fn local_inventory() -> AppResult<(String, Inventory)> {
    let architecture = process::run("dpkg", &["--print-architecture"], Duration::from_secs(30))?;
    let native = architecture.stdout.trim().to_owned();
    if !architecture.success || native.is_empty() {
        return Err(Error::blocked("Cannot determine the dpkg architecture"));
    }
    let result = process::run(
        "dpkg-query",
        &[
            "-W",
            "-f",
            "${Package}\t${Architecture}\t${Version}\t${Essential}\t${Protected}\t${db:Status-Abbrev}\n",
        ],
        Duration::from_secs(60),
    )?;
    if !result.success {
        return Err(Error::blocked("Cannot read installed package metadata"));
    }
    let mut inventory = Inventory::new();
    for row in result.stdout.lines() {
        let fields: Vec<_> = row.split('\t').collect();
        let [name, architecture, version, essential, protected, status] = fields.as_slice() else {
            return Err(Error::blocked("Unrecognized installed package metadata"));
        };
        if !matches!(*status, "ii " | "hi " | "ii" | "hi") {
            continue;
        }
        if name.is_empty() || architecture.is_empty() || version.is_empty() {
            return Err(Error::blocked("Incomplete installed package metadata"));
        }
        inventory
            .entry((*name).into())
            .or_default()
            .push(Installed {
                architecture: (*architecture).into(),
                version: (*version).into(),
                protected: *essential == "yes" || *protected == "yes",
                held: status.starts_with('h'),
            });
    }
    Ok((native, inventory))
}

fn simulate(command: &[&str], inventory: &Inventory, native: &str) -> AppResult<Vec<Action>> {
    let mut args = vec!["-s", "-o", "APT::Output-Version=20"];
    args.extend_from_slice(command);
    let result = apt(&args, 180)?;
    if !result.success {
        return Err(Error::blocked(
            "The APT simulation failed. Inspect the plan with apt-get -s.",
        ));
    }
    parse_simulation(&result.stdout, inventory, native)
}

fn parse_simulation(text: &str, inventory: &Inventory, native: &str) -> AppResult<Vec<Action>> {
    let invalid = || Error::blocked("The APT simulation was incomplete or unrecognized");
    let summary = SIM_SUMMARY.captures(text).ok_or_else(invalid)?;
    let expected: Vec<usize> = (1..=3)
        .map(|i| summary[i].parse().map_err(|_| invalid()))
        .collect::<AppResult<_>>()?;
    let mut packages = Vec::new();
    let mut seen = BTreeSet::new();
    let mut counts = [0usize; 3];
    for line in text.lines() {
        if !matches!(
            line.split_whitespace().next(),
            Some("Inst" | "Conf" | "Remv" | "Purg")
        ) {
            continue;
        }
        let fields = SIM_ACTION.captures(line).ok_or_else(invalid)?;
        let name = fields.get(2).ok_or_else(invalid)?.as_str();
        let explicit_arch = fields.get(3).map(|word| word.as_str());
        let from = fields.get(4).map(|word| word.as_str());
        let to = fields.get(5).map(|word| word.as_str());
        let candidate_arch = fields.get(6).map(|word| word.as_str());
        if from
            .into_iter()
            .chain(to)
            .any(|version| !DEBIAN_VERSION.is_match(version))
        {
            return Err(invalid());
        }
        let (kind, architecture) = match &fields[1] {
            "Inst" => {
                counts[usize::from(from.is_none())] += 1;
                let arch = candidate_arch.ok_or_else(invalid)?;
                if from
                    .zip(to)
                    .is_some_and(|(old, new)| debian_version_cmp(new, old).is_lt())
                {
                    return Err(Error::blocked(format!(
                        "The APT plan would downgrade {name}"
                    )));
                }
                (Kind::Unpack, arch)
            }
            "Conf" => (Kind::Configure, candidate_arch.ok_or_else(invalid)?),
            "Remv" | "Purg" => {
                counts[2] += 1;
                let wanted = explicit_arch.unwrap_or(native);
                let arch = inventory
                    .get(name)
                    .and_then(|items| {
                        items.iter().find(|item| {
                            item.version == from.unwrap_or_default()
                                && (item.architecture == wanted
                                    || explicit_arch.is_none() && item.architecture == "all")
                        })
                    })
                    .map(|item| item.architecture.as_str())
                    .ok_or_else(invalid)?;
                (Kind::Remove, arch)
            }
            _ => return Err(invalid()),
        };
        if !seen.insert((name, architecture, kind)) {
            return Err(invalid());
        }
        packages.push(Action {
            package: name.into(),
            architecture: architecture.into(),
            kind,
            from: from.map(str::to_owned),
            to: to.map(str::to_owned),
        });
    }
    if counts != expected.as_slice() {
        return Err(invalid());
    }
    Ok(packages)
}

pub(super) fn debian_version_cmp(left: &str, right: &str) -> Ordering {
    fn epoch(version: &str) -> (u64, &str) {
        version
            .split_once(':')
            .map_or((0, version), |(epoch, tail)| {
                (epoch.parse().unwrap_or(u64::MAX), tail)
            })
    }
    fn revision(version: &str) -> (&str, &str) {
        version.rsplit_once('-').unwrap_or((version, "0"))
    }
    let (a_epoch, a) = epoch(left);
    let (b_epoch, b) = epoch(right);
    let (upstream_a, revision_a) = revision(a);
    let (upstream_b, revision_b) = revision(b);
    a_epoch
        .cmp(&b_epoch)
        .then_with(|| version_part_cmp(upstream_a.as_bytes(), upstream_b.as_bytes()))
        .then_with(|| version_part_cmp(revision_a.as_bytes(), revision_b.as_bytes()))
}

fn version_part_cmp(mut left: &[u8], mut right: &[u8]) -> Ordering {
    fn non_digit_order(byte: Option<&u8>) -> i32 {
        match byte {
            Some(b'~') => -1,
            Some(b) if b.is_ascii_alphabetic() => i32::from(*b),
            Some(b) if !b.is_ascii_digit() => i32::from(*b) + 256,
            _ => 0,
        }
    }
    while !left.is_empty() || !right.is_empty() {
        while left.first().is_some_and(|b| !b.is_ascii_digit())
            || right.first().is_some_and(|b| !b.is_ascii_digit())
        {
            let order = non_digit_order(left.first()).cmp(&non_digit_order(right.first()));
            if order != Ordering::Equal {
                return order;
            }
            left = &left[1..];
            right = &right[1..];
        }
        let take_digits = |input: &[u8]| {
            input
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count()
        };
        let (left_number, remaining_left) = left.split_at(take_digits(left));
        let (right_number, remaining_right) = right.split_at(take_digits(right));
        let left_number =
            &left_number[left_number.iter().take_while(|byte| **byte == b'0').count()..];
        let right_number = &right_number[right_number
            .iter()
            .take_while(|byte| **byte == b'0')
            .count()..];
        let order = left_number
            .len()
            .cmp(&right_number.len())
            .then_with(|| left_number.cmp(right_number));
        if order != Ordering::Equal {
            return order;
        }
        left = remaining_left;
        right = remaining_right;
    }
    Ordering::Equal
}

fn gate(ctx: &Context<'_>, actions: &[Action], inventory: &Inventory) -> AppResult<()> {
    let result = process::run("apt-mark", &["showhold"], Duration::from_secs(45))?;
    if !result.success {
        return Err(Error::blocked("Cannot determine held packages"));
    }
    let held: BTreeSet<_> = result
        .stdout
        .split_whitespace()
        .map(|package| package.split(':').next().unwrap_or_default())
        .collect();
    let removals = actions
        .iter()
        .filter(|action| action.kind == Kind::Remove)
        .count();
    for action in actions {
        let package = action.package.as_str();
        if held.contains(package) {
            return Err(Error::blocked(format!(
                "The APT plan would change held package {package}"
            )));
        }
        if action.kind != Kind::Remove {
            continue;
        }
        let protected = PROTECTED.contains(&package)
            || ctx
                .config
                .apt
                .protected_packages
                .iter()
                .any(|configured| configured == package)
            || inventory
                .get(package)
                .is_some_and(|items| items.iter().any(|item| item.protected))
            || package.starts_with("linux-image-")
                && (!package["linux-image-".len()..]
                    .starts_with(|character: char| character.is_ascii_digit())
                    || package == format!("linux-image-{}", ctx.host.kernel));
        if protected {
            return Err(Error::blocked(format!(
                "The APT plan would remove protected package {package}"
            )));
        }
    }
    if removals > ctx.config.apt.max_removals {
        return Err(Error::blocked(format!(
            "The APT plan would remove {removals} packages, above the configured limit of {}",
            ctx.config.apt.max_removals
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Default, Serialize)]
struct Estimate {
    download: u64,
    unpack: u64,
    boot: u64,
}

fn estimate(actions: &[Action], native: &str) -> AppResult<Estimate> {
    let mut totals = Estimate::default();
    let installs: Vec<_> = actions
        .iter()
        .filter(|item| item.kind == Kind::Unpack)
        .collect();
    for batch in installs.chunks(48) {
        let packages: Vec<_> = batch
            .iter()
            .map(|item| {
                let version = item.to.as_deref().unwrap_or_default();
                if item.architecture == "all" || item.architecture == native {
                    format!("{}={version}", item.package)
                } else {
                    format!("{}:{}={version}", item.package, item.architecture)
                }
            })
            .collect();
        let mut args = NO_CACHES.to_vec();
        args.push("show");
        args.extend(packages.iter().map(String::as_str));
        let metadata = process::run("apt-cache", &args, Duration::from_secs(60))?;
        if !metadata.success {
            return Err(Error::blocked(
                "APT could not estimate space for the simulated package versions",
            ));
        }
        let paragraphs: Vec<BTreeMap<_, _>> = metadata
            .stdout
            .split("\n\n")
            .map(|paragraph| {
                paragraph
                    .lines()
                    .filter_map(|line| line.split_once(": "))
                    .collect()
            })
            .collect();
        for action in batch {
            let matches = paragraphs.iter().filter(|fields| {
                fields.get("Package") == Some(&action.package.as_str())
                    && fields.get("Architecture") == Some(&action.architecture.as_str())
                    && fields.get("Version").copied() == action.to.as_deref()
            });
            let package = action.package.as_str();
            let unavailable =
                || Error::blocked(format!("APT size metadata is missing for {package}"));
            let mut archive_bytes = 0;
            let mut installed_bytes = 0;
            for fields in matches {
                let size = fields.get("Size").and_then(|size| size.parse::<u64>().ok());
                let unpack = fields
                    .get("Installed-Size")
                    .and_then(|size| size.parse::<u64>().ok());
                if let (Some(size), Some(unpack)) = (size, unpack) {
                    archive_bytes = archive_bytes.max(size);
                    installed_bytes = installed_bytes.max(unpack.saturating_mul(1024));
                }
            }
            if archive_bytes == 0 || installed_bytes == 0 {
                return Err(unavailable());
            }
            totals.download = totals.download.saturating_add(archive_bytes);
            // Reserve the complete unpacked footprint while the currently installed version exists.
            totals.unpack = totals.unpack.saturating_add(installed_bytes);
            if super::kernel::versioned_image(package)
                || matches!(
                    package,
                    "raspberrypi-kernel"
                        | "raspi-firmware"
                        | "raspberrypi-bootloader"
                        | "rpi-eeprom"
                )
            {
                totals.boot = totals.boot.saturating_add(installed_bytes);
            }
        }
    }
    // Updating the kernel, modules, or initramfs tooling can generate a second boot image.
    if totals.boot > 0
        || installs.iter().any(|item| {
            item.package.starts_with("linux-modules-")
                || item.package.starts_with("initramfs-tools")
        })
    {
        let initramfs = fs::read_dir("/boot")
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("initrd.img-")
            })
            .filter_map(|entry| entry.metadata().ok().map(|metadata| metadata.len()))
            .max()
            .unwrap_or(0);
        totals.boot = totals
            .boot
            .saturating_add(initramfs.saturating_mul(2).max(32 * MIB));
    }
    Ok(totals)
}

fn space(ctx: &Context<'_>, required: Estimate) -> AppResult<()> {
    let root = filesystem("/")?;
    let cache = filesystem("/var/cache/apt/archives")?;
    let root_floor = ctx.config.preflight.min_free_mb.saturating_mul(MIB);
    let same_cache = root.device == cache.device;
    if !same_cache {
        enforce_space("/var/cache/apt/archives", cache, required.download)?;
    }
    let root_required = root_floor
        .saturating_add(required.unpack)
        .saturating_add(if same_cache { required.download } else { 0 });
    let root_device = root.device;
    enforce_space("/", root, root_required)?;
    let boot_path = if super::pi_profile(ctx) && Path::new(BOOT).is_dir() {
        BOOT
    } else {
        "/boot"
    };
    if Path::new(boot_path).is_dir() {
        let boot = filesystem(boot_path)?;
        if boot.device != root_device {
            let floor = ctx.config.preflight.min_free_boot_mb.saturating_mul(MIB);
            enforce_space(boot_path, boot, floor.saturating_add(required.boot))?;
        } else if required.boot > 0 {
            enforce_space(
                "/",
                filesystem("/")?,
                root_required.saturating_add(required.boot),
            )?;
        }
    }
    Ok(())
}

pub fn upgrade(ctx: &Context<'_>) -> AppResult<Report> {
    if !ctx.host.is_debian() {
        return Err(Error::blocked("No host package provider is active"));
    }
    validate_sources()?;
    let dirty = audit()?;
    if dirty && ctx.dry_run {
        return Ok(Report::new(
            Outcome::Blocked,
            "Would recover interrupted dpkg before refreshing APT and rebuilding the plan",
        ));
    }
    if dirty {
        recover()?;
    }
    if !ctx.dry_run {
        refresh(ctx.progress)?;
    }
    let mut warnings = Vec::new();
    if dirty {
        warnings.push("Recovered an interrupted dpkg transaction".into());
    }
    if ctx.dry_run {
        warnings.push("Preview uses cached APT indexes. A real upgrade refreshes indexes and revalidates the plan.".into());
    }
    for attempt in 0..=1 {
        let (native, inventory) = local_inventory()?;
        let actions = simulate(&["full-upgrade"], &inventory, &native)?;
        gate(ctx, &actions, &inventory)?;
        let changes = actions
            .iter()
            .filter(|item| item.kind != Kind::Configure)
            .count();
        if actions.is_empty() {
            let mut report = Report::new(Outcome::Unchanged, "Host packages are current");
            report.warnings = warnings;
            return Ok(report);
        }
        let required = estimate(&actions, &native)?;
        if let Err(err) = space(ctx, required) {
            if ctx.dry_run {
                return Err(err);
            }
            clean()?;
            space(ctx, required)?;
        }
        let running_kernel_updated = actions.iter().any(|action| {
            action.kind == Kind::Unpack
                && (action.package == format!("linux-image-{}", ctx.host.kernel)
                    || super::pi_profile(ctx) && action.package == "raspberrypi-kernel")
        });
        let docker_updated = actions.iter().any(|action| {
            action.kind == Kind::Unpack
                && matches!(
                    action.package.as_str(),
                    "docker.io" | "docker-ce" | "containerd" | "containerd.io"
                )
        });
        if !ctx.dry_run {
            // Record before dpkg because even interrupted unpacking can overwrite the on-disk running kernel.
            if running_kernel_updated {
                record_kernel_upgrade(ctx)?;
            }
            let result = guarded_apply(
                &["full-upgrade"],
                &actions,
                ctx.config.apt.max_removals == 0,
                5400,
                ctx.progress.then_some("APT upgrade"),
            )?;
            if !result.success {
                if result
                    .stderr
                    .contains("Foreman refused the APT transaction")
                {
                    return Err(Error::blocked(
                        "The live APT transaction differs from its validated simulation. Run Foreman again to review a fresh plan.",
                    ));
                }
                if attempt == 0 && audit()? {
                    recover()?;
                    warnings.push("Recovered dpkg after the first upgrade attempt and rebuilt the transaction".into());
                    continue;
                }
                return Err(Error::failed(
                    "APT full-upgrade failed. Review /var/log/apt/term.log and dpkg --audit.",
                ));
            }
            if audit()? {
                return Err(Error::failed(
                    "Package verification found incomplete dpkg work after APT returned",
                ));
            }
        }
        let verb = if ctx.dry_run {
            "Would change"
        } else {
            "Changed"
        };
        let mut report = Report::new(Outcome::Completed, format!("{verb} {changes} host packages")).detail(serde_json::json!({
            "actions": actions, "space": required, "running_kernel_updated": running_kernel_updated, "docker_updated": docker_updated
        }));
        report.warnings = warnings;
        return Ok(report);
    }
    unreachable!("bounded upgrade loop returns on its final attempt")
}

pub(super) fn guarded_purge(targets: &[String], ctx: &Context<'_>) -> AppResult<Vec<Action>> {
    validate_sources()?;
    if audit()? {
        return Err(Error::blocked(
            "Pending dpkg recovery prevents kernel cleanup",
        ));
    }
    let (native, inventory) = local_inventory()?;
    let mut command = vec!["purge", "--"];
    command.extend(targets.iter().map(String::as_str));
    let actions = simulate(&command, &inventory, &native)?;
    let planned: BTreeSet<_> = actions
        .iter()
        .map(|action| action.package.as_str())
        .collect();
    let expected: BTreeSet<_> = targets.iter().map(String::as_str).collect();
    if planned != expected
        || actions.len() != targets.len()
        || actions.iter().any(|action| action.kind != Kind::Remove)
    {
        return Err(Error::blocked(
            "Kernel purge simulation did not exactly match the intended package removals",
        ));
    }
    if actions.iter().any(|action| {
        inventory.get(&action.package).is_some_and(|items| {
            items.iter().any(|item| {
                item.architecture == action.architecture && (item.held || item.protected)
            })
        })
    }) {
        return Err(Error::blocked(
            "Kernel purge would remove a held or protected package",
        ));
    }
    if !ctx.dry_run {
        let result = guarded_apply(
            &command,
            &actions,
            false,
            1200,
            ctx.progress.then_some("APT kernel purge"),
        )?;
        if !result.success {
            if result
                .stderr
                .contains("Foreman refused the APT transaction")
            {
                return Err(Error::blocked(
                    "The live kernel purge diverged from its validated simulation",
                ));
            }
            return Err(Error::failed(
                "APT kernel purge failed. Review /var/log/apt/term.log.",
            ));
        }
        if audit()? {
            return Err(Error::failed("Kernel purge left incomplete dpkg work"));
        }
    }
    Ok(actions)
}

fn guarded_apply(
    command: &[&str],
    actions: &[Action],
    no_remove: bool,
    seconds: u64,
    progress: Option<&str>,
) -> AppResult<process::Output> {
    let mut file = tempfile::Builder::new()
        .prefix("foreman-apt-")
        .tempfile_in("/run")
        .map_err(|err| Error::failed(format!("Cannot create an APT transaction guard: {err}")))?;
    let data = serde_json::to_vec(actions)
        .map_err(|err| Error::failed(format!("Encoding APT guard: {err}")))?;
    file.write_all(&data)
        .and_then(|_| file.as_file().sync_all())
        .map_err(|err| Error::failed(format!("Saving APT guard: {err}")))?;
    let executable = format!("/proc/{}/exe", std::process::id());
    let file_path = file
        .path()
        .to_str()
        .ok_or_else(|| Error::failed("Invalid guard file path"))?;
    if !file_path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/-._".contains(&byte))
    {
        return Err(Error::failed("Invalid guard file path"));
    }
    let hook = format!("DPkg::Pre-Install-Pkgs::={executable} apt-verify-plan {file_path}");
    let version = format!("DPkg::Tools::Options::{executable}::Version=3");
    let mut args = LOCK_WAIT.to_vec();
    args.extend_from_slice(&[
        "-y",
        "-o",
        "Dpkg::Use-Pty=false",
        "-o",
        "Debug::NoLocking=false",
        "-o",
        "Dpkg::Options::=--force-confdef",
        "-o",
        "Dpkg::Options::=--force-confold",
        "-o",
        "APT::Get::Download-Only=false",
        "-o",
        "APT::Get::Simulate=false",
        "-o",
        &hook,
        "-o",
        &version,
    ]);
    if no_remove {
        args.push("--no-remove");
    }
    args.extend_from_slice(command);
    apt_run(&args, seconds, progress)
}

const MAX_HOOK_INPUT: u64 = 16 * MIB;

pub fn verify_apt_plan(path: &Path) -> AppResult<()> {
    let check = || {
        if std::env::var("APT_HOOK_INFO_FD").as_deref() != Ok("0") {
            return Err(Error::blocked("APT did not supply its plan on stdin"));
        }
        if path.parent() != Some(Path::new("/run"))
            || !path
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"foreman-apt-"))
        {
            return Err(Error::blocked(
                "The approved transaction file is outside the APT guard directory",
            ));
        }
        crate::store::trusted_file(path)?;
        let mut raw = Vec::new();
        fs::File::open(path)
            .and_then(|file| file.take(MAX_HOOK_INPUT + 1).read_to_end(&mut raw))
            .map_err(|_| Error::blocked("The approved transaction file is unavailable"))?;
        if raw.len() as u64 > MAX_HOOK_INPUT {
            return Err(Error::blocked(
                "The approved transaction file exceeds the input limit",
            ));
        }
        let approved: Vec<Action> = serde_json::from_slice(&raw)
            .map_err(|_| Error::blocked("Invalid approved transaction file"))?;
        let mut payload = String::new();
        std::io::stdin()
            .lock()
            .take(MAX_HOOK_INPUT + 1)
            .read_to_string(&mut payload)
            .map_err(|_| Error::blocked("Cannot read the live transaction"))?;
        if payload.len() as u64 > MAX_HOOK_INPUT {
            return Err(Error::blocked(
                "The live transaction exceeds the input limit",
            ));
        }
        check_hook(&payload, &approved)
    };
    check().map_err(|_| {
        Error::blocked(
            "Foreman refused the APT transaction because the live plan could not be verified",
        )
    })
}

fn check_hook(payload: &str, approved: &[Action]) -> AppResult<()> {
    let invalid = || Error::blocked("Invalid or changed live APT transaction");
    let (header, entries) = payload.split_once("\n\n").ok_or_else(invalid)?;
    if header.lines().next() != Some("VERSION 3") {
        return Err(invalid());
    }
    let mut changes = BTreeSet::new();
    for line in entries.lines() {
        let mut fields = line.splitn(9, ' ');
        let parcel: Vec<_> = fields.by_ref().collect();
        let [
            package,
            old_version,
            old_arch,
            _,
            direction,
            new_version,
            new_arch,
            _,
            action,
        ] = parcel.as_slice()
        else {
            return Err(invalid());
        };
        let from = (*old_version != "-").then(|| (*old_version).to_owned());
        let to = (*new_version != "-").then(|| (*new_version).to_owned());
        let (kind, architecture, from, to) = match *action {
            "**CONFIGURE**" if to.is_some() && matches!(*direction, "<" | "=") => {
                (Kind::Configure, new_arch, None, to)
            }
            "**REMOVE**" if from.is_some() && to.is_none() => (Kind::Remove, old_arch, from, None),
            deb if deb.starts_with('/')
                && deb.ends_with(".deb")
                && to.is_some()
                && matches!(*direction, "<" | "=") =>
            {
                (Kind::Unpack, new_arch, from, to)
            }
            _ => return Err(invalid()),
        };
        let actual = Action {
            package: (*package).into(),
            architecture: (*architecture).into(),
            kind,
            from,
            to,
        };
        if !approved.contains(&actual) {
            return Err(invalid());
        }
        if kind != Kind::Configure && !changes.insert(actual) {
            return Err(invalid());
        }
    }
    if approved
        .iter()
        .filter(|action| action.kind != Kind::Configure)
        .any(|action| !changes.contains(action))
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_source_specific_authenticated_apt_overrides() {
        assert!(has_insecure_source_options(
            "deb [arch=arm64 trusted=yes] http://example.org stable main",
            false
        ));
        assert!(!has_insecure_source_options(
            "deb [trusted=no] http://example.org stable main",
            false
        ));
        assert!(has_insecure_source_options(
            "Types: deb\nTrusted:\n yes\n",
            true
        ));
        assert!(!has_insecure_source_options(
            "Types: deb\nEnabled: no\nTrusted: yes\n\nTypes: deb\nSigned-By: /usr/share/keyrings/repository.gpg",
            true
        ));
    }

    #[test]
    fn reads_complete_simulation_and_rejects_truncated_and_downgrade_plans() {
        let simulation = "Reading package lists...\n1 upgraded, 1 newly installed, 0 to remove and 0 not upgraded.\nInst openssl [3.0.1] (3.0.2 Debian:stable [arm64])\nInst ca-certificates (2025 Debian:stable [all])\nConf ca-certificates (2025 Debian:stable [all])\nConf openssl (3.0.2 Debian:stable [arm64])\n";
        let inventory = Inventory::new();
        assert_eq!(
            parse_simulation(simulation, &inventory, "arm64")
                .unwrap()
                .len(),
            4
        );
        assert!(
            parse_simulation(
                &simulation.replace("1 newly installed", "2 newly installed"),
                &inventory,
                "arm64"
            )
            .is_err()
        );
        assert!(
            parse_simulation(&simulation.replace("3.0.2", "2.9.1"), &inventory, "arm64").is_err()
        );
    }

    #[test]
    fn rejects_apply_when_version_arch_or_removal_changes() {
        let approved = parse_simulation(
            "1 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\nInst openssl [3.0.1] (3.0.2 Debian:stable [arm64])\nConf openssl (3.0.2 Debian:stable [arm64])\n",
            &Inventory::new(), "arm64"
        ).unwrap();
        let live = "VERSION 3\nAPT::Architecture=arm64\n\nopenssl 3.0.1 arm64 same < 3.0.2 arm64 same /var/cache/apt/openssl.deb\nopenssl 3.0.1 arm64 same < 3.0.2 arm64 same **CONFIGURE**\n";
        assert!(check_hook(live, &approved).is_ok());
        for change in [
            live.replace("3.0.2", "3.0.3"),
            live.replace("arm64 same", "amd64 same"),
            live.replace("VERSION 3", "VERSION 2"),
            format!("{live}bash 5.2 arm64 no > - - none **REMOVE**\n"),
        ] {
            assert!(check_hook(&change, &approved).is_err());
        }
    }

    #[test]
    fn uses_debian_epoch_revision_and_tilde_order() {
        for (new, old) in [
            ("1:1.0", "9.0"),
            ("2.0", "2.0~rc1"),
            ("3.0-10", "3.0-2"),
            ("6.12.62+rpt-rpi-2712", "6.12.61+rpt-rpi-2712"),
        ] {
            assert!(
                debian_version_cmp(new, old).is_gt(),
                "{new} should be newer than {old}"
            );
        }
        assert_eq!(debian_version_cmp("1.0-0", "1.0"), Ordering::Equal);
    }
}
