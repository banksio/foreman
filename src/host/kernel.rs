use super::apt;
use crate::{AppResult, Context, Error, Outcome, Report, process};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::LazyLock,
    time::Duration,
};

static RELEASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]+(?:\.[0-9]+)+(?:[-+](?:rpt[0-9]*|deb[0-9]+(?:\.[0-9]+)*|bpo|[0-9]+))*)[-+]([a-zA-Z][a-zA-Z0-9.+-]*)$")
        .expect("valid kernel release expression")
});

#[derive(Debug, Eq, PartialEq)]
struct Release<'a> {
    name: &'a str,
    base: &'a str,
    flavour: &'a str,
}

impl<'a> Release<'a> {
    fn parse(name: &'a str) -> Option<Self> {
        let fields = RELEASE.captures(name)?;
        Some(Self {
            name,
            base: fields.get(1)?.as_str(),
            flavour: fields.get(2)?.as_str(),
        })
    }
}

fn newer<'a>(current: &'a str, installed: impl Iterator<Item = &'a str>) -> Option<Release<'a>> {
    let running = Release::parse(current)?;
    installed
        .filter_map(Release::parse)
        .filter(|release| release.flavour == running.flavour)
        .max_by(|left, right| apt::debian_version_cmp(left.base, right.base))
}

pub(super) fn newest_bootable(running: &str) -> Option<String> {
    let modules = fs::read_dir("/lib/modules").ok()?;
    let installed: Vec<_> = modules
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            path.join("modules.builtin").is_file() || path.join("modules.dep").is_file()
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    newer(
        running,
        installed
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(running)),
    )
    .map(|release| release.name.into())
}

fn inventory() -> AppResult<BTreeMap<String, bool>> {
    let result = process::run(
        "dpkg-query",
        &["-W", "-f", "${db:Status-Abbrev}\t${Package}\n"],
        Duration::from_secs(60),
    )?;
    if !result.success {
        return Err(Error::blocked("Cannot determine installed kernel packages"));
    }
    let mut packages = BTreeMap::new();
    for line in result.stdout.lines() {
        let (status, package) = line
            .split_once('\t')
            .ok_or_else(|| Error::blocked("Unrecognized dpkg kernel inventory"))?;
        if package.starts_with("linux-") && matches!(status.trim_end(), "ii" | "hi") {
            packages.insert(package.to_owned(), status.starts_with('h'));
        }
    }
    Ok(packages)
}

fn installed_release(package: &str) -> Option<Release<'_>> {
    let name = package.strip_prefix("linux-image-")?;
    Release::parse(name.strip_suffix("-unsigned").unwrap_or(name))
}

pub(super) fn versioned_image(package: &str) -> bool {
    installed_release(package).is_some()
}

fn removal_targets(
    packages: &BTreeMap<String, bool>,
    releases: &[Release<'_>],
    kept: &BTreeSet<&str>,
) -> Vec<String> {
    let kept_bases: BTreeSet<_> = releases
        .iter()
        .filter(|release| kept.contains(release.name))
        .map(|release| release.base)
        .collect();
    let mut possible = BTreeSet::new();
    for release in releases
        .iter()
        .filter(|release| !kept.contains(release.name))
    {
        for kind in ["image", "headers", "modules", "modules-extra"] {
            possible.insert(format!("linux-{kind}-{}", release.name));
        }
        possible.insert(format!("linux-image-{}-unsigned", release.name));
        if !kept_bases.contains(release.base) {
            possible.extend([
                format!("linux-headers-{}-common", release.base),
                format!("linux-headers-{}-common-rpi", release.base),
                format!("linux-kbuild-{}", release.base),
            ]);
        }
    }
    possible
        .into_iter()
        .filter(|package| packages.get(package) == Some(&false))
        .collect()
}

fn retained<'a>(
    packages: &BTreeMap<String, bool>,
    releases: &[Release<'a>],
    running: &'a str,
    keep_previous: usize,
) -> BTreeSet<&'a str> {
    let mut groups: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for release in releases {
        groups.entry(release.flavour).or_default().push(release);
    }
    let mut kept = BTreeSet::from([running]);
    for mut group in groups.into_values() {
        group.sort_by(|left, right| apt::debian_version_cmp(right.base, left.base));
        group.dedup_by_key(|release| release.name);
        kept.extend(
            group
                .into_iter()
                .take(keep_previous.saturating_add(1))
                .map(|release| release.name),
        );
    }
    for release in releases {
        let held_release = ["image", "headers", "modules", "modules-extra"]
            .iter()
            .any(|kind| packages.get(&format!("linux-{kind}-{}", release.name)) == Some(&true))
            || packages.get(&format!("linux-image-{}-unsigned", release.name)) == Some(&true);
        if held_release {
            kept.insert(release.name);
        }
    }
    kept
}

pub fn kernels(ctx: &Context<'_>) -> AppResult<Report> {
    let settings = &ctx.config.kernels;
    if !settings.remove_old {
        return Ok(Report::new(Outcome::Skipped, "Kernel cleanup is disabled"));
    }
    if !ctx.host.is_debian() {
        return Err(Error::blocked("No kernel cleanup provider is active"));
    }
    let running = ctx.host.kernel.as_str();
    if super::reboot_status(ctx).needed {
        return Ok(Report::new(
            Outcome::Skipped,
            "Kernel cleanup is deferred while a reboot is required",
        ));
    }
    let packages = inventory()?;
    let releases: Vec<_> = packages
        .keys()
        .filter_map(|name| installed_release(name))
        .collect();
    if !releases.iter().any(|release| release.name == running) {
        return Ok(Report::new(
            Outcome::Skipped,
            format!("Running kernel {running} is outside the packaged kernel inventory"),
        ));
    }
    if newer(running, releases.iter().map(|release| release.name))
        .is_none_or(|release| release.name != running)
    {
        return Ok(Report::new(
            Outcome::Skipped,
            "Kernel cleanup is deferred until the newest kernel of the running flavour has booted",
        ));
    }
    if !Path::new("/lib/modules").join(running).is_dir() {
        return Err(Error::blocked(
            "Cannot verify the running kernel module directory",
        ));
    }
    let kept = retained(&packages, &releases, running, settings.keep_previous);
    let mut targets = removal_targets(&packages, &releases, &kept);
    targets.retain(|package| !ctx.config.apt.protected_packages.contains(package));
    if targets.is_empty() {
        return Ok(
            Report::new(Outcome::Unchanged, "No old kernel packages to remove")
                .detail(serde_json::json!({"retained": kept})),
        );
    }
    apt::guarded_purge(&targets, ctx)?;
    let verb = if ctx.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    Ok(Report::new(
        Outcome::Completed,
        format!("{verb} {} old kernel packages", targets.len()),
    )
    .detail(serde_json::json!({"retained": kept, "packages": targets})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_running_and_latest_pi_kernels_of_each_flavour_and_retains_shared_files() {
        let packages: BTreeMap<_, _> = [
            ("linux-image-6.12.42+rpt-rpi-2712", false),
            ("linux-image-6.12.41+rpt-rpi-2712", false),
            ("linux-image-6.12.40+rpt-rpi-2712", true),
            ("linux-image-6.12.41+rpt-rpi-v8", false),
            ("linux-image-rpi-v8", false),
            ("linux-headers-6.12.41+rpt-common-rpi", false),
            ("linux-headers-6.12.41+rpt-rpi-2712", false),
        ]
        .into_iter()
        .map(|(name, held)| (name.to_owned(), held))
        .collect();
        let running = "6.12.42+rpt-rpi-2712";
        let releases: Vec<_> = packages
            .keys()
            .filter_map(|name| installed_release(name))
            .collect();
        let kept = retained(&packages, &releases, running, 0);
        assert!(kept.contains("6.12.40+rpt-rpi-2712"));
        assert!(kept.contains("6.12.41+rpt-rpi-v8"));
        assert_eq!(
            removal_targets(&packages, &releases, &kept),
            [
                "linux-headers-6.12.41+rpt-rpi-2712",
                "linux-image-6.12.41+rpt-rpi-2712"
            ]
        );
    }

    #[test]
    fn chooses_the_newest_release_in_the_running_debian_flavour() {
        let releases = ["6.1.0-30-arm64", "6.1.0-3-arm64", "6.1.0-40-cloud-arm64"];
        assert_eq!(
            newer("6.1.0-3-arm64", releases.into_iter()).unwrap().name,
            "6.1.0-30-arm64"
        );
    }
}
