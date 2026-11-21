use crate::{AppResult, Error, Paths, docker::DockerConfig, model::RebootMode, store};
use chrono::NaiveTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml::Value;

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub apt: AptConfig,
    pub preflight: PreflightConfig,
    pub reboot: RebootConfig,
    pub kernels: KernelConfig,
    pub compose: DockerConfig,
    pub disk: DiskConfig,
    pub maintenance: MaintenanceConfig,
    pub notifications: NotificationsConfig,
    pub modules: BTreeMap<String, bool>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AptConfig {
    pub max_removals: usize,
    pub protected_packages: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreflightConfig {
    pub min_free_mb: u64,
    pub min_free_boot_mb: u64,
    pub network_hosts: Vec<String>,
    pub require_clock_sync: bool,
}

impl Default for PreflightConfig {
    fn default() -> Self {
        Self {
            min_free_mb: 500,
            min_free_boot_mb: 100,
            network_hosts: vec![],
            require_clock_sync: true,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RebootConfig {
    pub mode: RebootMode,
    pub window: String,
    pub watch_services: Vec<String>,
    pub cooldown_hours: u32,
    pub pending_days: u32,
}

impl Default for RebootConfig {
    fn default() -> Self {
        Self {
            mode: RebootMode::Auto,
            window: "03:00-05:00".into(),
            watch_services: vec!["ssh".into()],
            cooldown_hours: 24,
            pending_days: 7,
        }
    }
}

impl RebootConfig {
    pub fn bounds(&self) -> AppResult<Option<(NaiveTime, NaiveTime)>> {
        if self.window.is_empty() {
            return Ok(None);
        }

        let times = self
            .window
            .split_once('-')
            .and_then(|(start, end)| {
                if start.len() != 5 || end.len() != 5 {
                    return None;
                }
                Some((
                    NaiveTime::parse_from_str(start, "%H:%M").ok()?,
                    NaiveTime::parse_from_str(end, "%H:%M").ok()?,
                ))
            })
            .filter(|(start, end)| start != end);
        times.map(Some).ok_or_else(|| {
            Error::invalid(
                "reboot.window must be empty or HH:MM-HH:MM with different start and end",
            )
        })
    }

    pub fn in_window(&self, time: NaiveTime) -> AppResult<bool> {
        Ok(self.bounds()?.is_none_or(|(start, end)| {
            if start < end {
                start <= time && time < end
            } else {
                start <= time || time < end
            }
        }))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct KernelConfig {
    pub remove_old: bool,
    pub keep_previous: usize,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            remove_old: true,
            keep_previous: 1,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiskConfig {
    pub paths: Vec<String>,
    pub warn_percent: u64,
    pub critical_percent: u64,
    pub consumers: Vec<String>,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            paths: vec!["/".into(), "/boot/firmware".into()],
            warn_percent: 85,
            critical_percent: 95,
            consumers: [
                "/var/cache/apt",
                "/var/log",
                "/var/lib/docker",
                "/home",
                "/root",
                "/tmp",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceConfig {
    pub workloads_after_host_failure: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub webhook_url_file: Option<PathBuf>,
    pub heartbeat_url_file: Option<PathBuf>,
    pub heartbeat_schedule: Option<String>,
    pub stale_reboot_days: u32,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            webhook_url_file: None,
            heartbeat_url_file: None,
            heartbeat_schedule: None,
            stale_reboot_days: 3,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> AppResult<Self> {
        let base = paths.config_dir.join("config.toml");
        let mut value = Value::Table(Default::default());
        if let Some(config) = read_table(&base)? {
            merge(&mut value, config);
        }

        let dir = paths.config_dir.join("conf.d");
        match std::fs::read_dir(&dir) {
            Ok(files) => {
                let mut files = files
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<Result<Vec<_>, _>>()?;
                files.sort();
                for path in files
                    .iter()
                    .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                {
                    if let Some(config) = read_table(path)? {
                        merge(&mut value, config);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let modules = paths.config_dir.join("modules.toml");
        if let Some(config) = read_table(&modules)? {
            let table = config
                .as_table()
                .ok_or_else(|| Error::invalid("Invalid modules.toml"))?;
            if table.keys().any(|key| key != "modules") {
                return Err(Error::invalid(
                    "modules.toml may only contain the [modules] table",
                ));
            }
            merge(&mut value, config);
        }

        let config: Self = value
            .try_into()
            .map_err(|error| Error::invalid(format!("Invalid configuration: {error}")))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> AppResult<()> {
        self.reboot.bounds()?;
        if self.reboot.pending_days == 0
            || self.reboot.pending_days > 365
            || self.reboot.cooldown_hours > 8760
        {
            return Err(Error::invalid(
                "reboot.pending_days must be 1–365 and cooldown_hours must be 0–8760",
            ));
        }
        if self.preflight.min_free_mb > u64::MAX / 1_048_576
            || self.preflight.min_free_boot_mb > u64::MAX / 1_048_576
        {
            return Err(Error::invalid(
                "preflight space floors exceed the supported size",
            ));
        }
        if self.disk.warn_percent == 0
            || self.disk.critical_percent > 100
            || self.disk.warn_percent > self.disk.critical_percent
        {
            return Err(Error::invalid(
                "Disk thresholds must be in ascending order within 1–100",
            ));
        }
        if !self
            .disk
            .paths
            .iter()
            .chain(&self.disk.consumers)
            .all(|path| Path::new(path).is_absolute())
        {
            return Err(Error::invalid("Disk paths and consumers must be absolute"));
        }
        if self.notifications.heartbeat_url_file.is_some()
            != self.notifications.heartbeat_schedule.is_some()
        {
            return Err(Error::invalid(
                "Configure heartbeat_url_file and heartbeat_schedule together",
            ));
        }
        if let Some(name) = &self.notifications.heartbeat_schedule {
            crate::schedule::validate_name(name)?;
        }
        for path in [
            &self.notifications.webhook_url_file,
            &self.notifications.heartbeat_url_file,
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_absolute() {
                return Err(Error::invalid(
                    "Notification credential paths must be absolute",
                ));
            }
        }
        if let Some(id) = self.modules.keys().find(|id| !crate::providers::known(id)) {
            return Err(Error::invalid(format!("Unknown bundled module {id}")));
        }
        self.compose.validate()?;
        Ok(())
    }

    pub fn fingerprint(&self) -> AppResult<String> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

fn read_table(path: &Path) -> AppResult<Option<Value>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    if crate::is_root() {
        store::trusted_file(path)?;
    }
    store::read(path)?
        .map(|text| {
            toml::from_str(&text).map_err(|error| {
                // TOML diagnostics can include source lines. Keep values out of logs.
                Error::invalid(format!(
                    "Invalid TOML in {} ({})",
                    path.display(),
                    error.message().lines().next().unwrap_or("parse error")
                ))
            })
        })
        .transpose()
}

fn merge(base: &mut Value, newer: Value) {
    if let (Value::Table(base), Value::Table(newer)) = (&mut *base, &newer) {
        for (key, value) in newer {
            match base.get_mut(key) {
                Some(old) => merge(old, value.clone()),
                None => {
                    base.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        *base = newer;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reboot_window_is_half_open_and_can_cross_midnight() {
        let reboot = RebootConfig {
            window: "23:00-02:00".into(),
            ..Default::default()
        };
        for (time, expected) in [
            ("22:59", false),
            ("23:00", true),
            ("00:00", true),
            ("02:00", false),
        ] {
            assert_eq!(
                reboot
                    .in_window(NaiveTime::parse_from_str(time, "%H:%M").unwrap())
                    .unwrap(),
                expected
            );
        }
    }
}
