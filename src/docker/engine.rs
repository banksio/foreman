use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{AppResult, Error, process};

use super::input;

const PROJECT: &str = "com.docker.compose.project";
const SERVICE: &str = "com.docker.compose.service";
const WORKDIR: &str = "com.docker.compose.project.working_dir";
const FILES: &str = "com.docker.compose.project.config_files";
const HASH: &str = "com.docker.compose.config-hash";
const ONE_OFF: &str = "com.docker.compose.oneoff";

pub(super) struct Engine {
    config: String,
    deadline: Instant,
}

impl Engine {
    pub fn new(config: Option<&Path>) -> AppResult<Self> {
        let path = config.unwrap_or(Path::new("/etc/foreman/anonymous-registry"));
        if config.is_some() || input::exists(path)? {
            input::registry_config(path)?;
        }
        let config = path
            .to_str()
            .ok_or_else(|| Error::invalid("Docker registry configuration needs a UTF-8 path"))?
            .to_owned();
        Ok(Self {
            config,
            deadline: Instant::now() + Duration::from_secs(300),
        })
    }

    pub fn with_budget(mut self, seconds: u64) -> Self {
        self.deadline = Instant::now() + Duration::from_secs(seconds);
        self
    }

    pub fn exec(&self, args: &[&str], seconds: u64) -> AppResult<process::Output> {
        let mut command = vec![
            "--config",
            &self.config,
            "--host",
            "unix:///var/run/docker.sock",
        ];
        command.extend_from_slice(args);
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| Error::failed("Docker operation reached its time limit"))?;
        process::run_env(
            "docker",
            &command,
            Duration::from_secs(seconds).min(remaining),
            &[("COMPOSE_DISABLE_ENV_FILE", "1")],
        )
        .map_err(|_| Error::failed("Docker command could not be executed"))
    }

    pub fn run(&self, args: &[&str], seconds: u64, message: &str) -> AppResult<String> {
        let output = self.exec(args, seconds)?;
        if output.success {
            return Ok(output.stdout);
        }
        // Docker and Compose diagnostics can contain resolved environment values.
        Err(Error::failed(message.to_owned()))
    }

    pub fn json<T: DeserializeOwned>(
        &self,
        args: &[&str],
        seconds: u64,
        message: &str,
    ) -> AppResult<T> {
        serde_json::from_str(&self.run(args, seconds, message)?)
            .map_err(|_| Error::failed(format!("{message}: unexpected Docker JSON")))
    }

    pub fn containers(
        &self,
        project: Option<&str>,
        all: bool,
        timeout: u64,
    ) -> AppResult<Vec<Container>> {
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let selector = project.map_or_else(
            || format!("label={PROJECT}"),
            |project| format!("label={PROJECT}={project}"),
        );
        let mut args = vec![
            "container",
            "ls",
            "--quiet",
            "--no-trunc",
            "--filter",
            &selector,
        ];
        if all {
            args.push("--all");
        }
        let output = self.run(
            &args,
            timeout,
            "Cannot list local Docker Compose containers",
        )?;
        let ids: Vec<_> = output.split_whitespace().collect();
        if ids.len() > 2048 || ids.iter().any(|id| !container_id(id)) {
            return Err(Error::failed(
                "Docker returned too many containers or an invalid ID",
            ));
        }
        let mut containers: Vec<Container> = Vec::new();
        for chunk in ids.chunks(16) {
            let seconds = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    Error::failed("Docker container observation reached its time limit")
                })?
                .as_secs()
                .max(1);
            let mut args = vec!["container", "inspect", "--"];
            args.extend_from_slice(chunk);
            let observed: Vec<Container> =
                self.json(&args, seconds, "Cannot inspect Docker Compose containers")?;
            if observed.len() != chunk.len()
                || observed
                    .iter()
                    .any(|container| !chunk.contains(&container.id.as_str()))
            {
                return Err(Error::failed(
                    "Docker container list changed during observation",
                ));
            }
            containers.extend(observed.into_iter().filter(|container| {
                !container
                    .label(ONE_OFF)
                    .is_some_and(|value| value.eq_ignore_ascii_case("true"))
            }));
        }
        Ok(containers)
    }

    pub fn projects(&self) -> AppResult<BTreeMap<String, Vec<Container>>> {
        let mut projects = BTreeMap::<String, Vec<Container>>::new();
        for container in self.containers(None, false, 25)? {
            let name = container.project().unwrap_or("");
            if input::project_name(name) {
                projects.entry(name.to_owned()).or_default().push(container);
            }
        }
        Ok(projects)
    }

    pub fn image(&self, reference: &str) -> AppResult<Image> {
        let mut images: Vec<Image> = self.json(
            &["image", "inspect", "--", reference],
            30,
            "Cannot inspect Docker image",
        )?;
        if images.len() != 1 || !image_id(&images[0].id) {
            return Err(Error::failed(
                "Docker returned an invalid image observation",
            ));
        }
        Ok(images.remove(0))
    }

    pub fn manifest(&self, reference: &str, local: &Image) -> AppResult<RemoteImage> {
        let response: ManifestResponse = self.json(
            &["manifest", "inspect", "--verbose", "--", reference], 60,
            "Registry manifest unavailable. Check registry credentials, TLS, network access, and rate limits",
        )?;
        response.select(local).ok_or_else(|| {
            Error::blocked(
                "Registry response has no uniquely comparable image for the running platform",
            )
        })
    }

    pub fn oom_events(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        project: &str,
    ) -> AppResult<bool> {
        let since = since.to_rfc3339();
        let until = until.to_rfc3339();
        let selector = format!("label={PROJECT}={project}");
        let result = self.run(
            &[
                "events",
                "--since",
                &since,
                "--until",
                &until,
                "--filter",
                "type=container",
                "--filter",
                "event=oom",
                "--filter",
                &selector,
                "--format",
                "{{json .}}",
            ],
            15,
            "Cannot verify Docker OOM events",
        )?;
        Ok(!result.trim().is_empty())
    }

    pub fn image_ids(&self) -> AppResult<BTreeSet<String>> {
        let result = self.run(
            &["image", "ls", "--all", "--no-trunc", "--quiet"],
            45,
            "Cannot list local Docker images",
        )?;
        let ids: BTreeSet<_> = result.split_whitespace().map(str::to_owned).collect();
        if ids.len() > 1024 || ids.iter().any(|id| !image_id(id)) {
            return Err(Error::failed(
                "Docker returned too many images or an invalid ID",
            ));
        }
        Ok(ids)
    }
}

fn digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(container_id)
}

pub(super) fn image_id(value: &str) -> bool {
    digest(value)
}

fn container_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn image_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.@:+".contains(&c))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct Container {
    pub id: String,
    #[serde(rename = "Image")]
    pub image_id: String,
    pub restart_count: u64,
    state: ContainerState,
    config: ContainerConfig,
    mounts: Option<Vec<Mount>>,
}

impl Container {
    pub fn label(&self, label: &str) -> Option<&str> {
        self.config.labels.as_ref()?.get(label).map(String::as_str)
    }

    pub fn project(&self) -> Option<&str> {
        self.label(PROJECT)
    }

    pub fn service(&self) -> Option<&str> {
        self.label(SERVICE)
    }

    pub fn workdir(&self) -> Option<&Path> {
        self.label(WORKDIR)
            .map(Path::new)
            .filter(|path| path.is_absolute())
    }

    pub fn files(&self) -> Option<Vec<PathBuf>> {
        let workdir = self.workdir()?;
        let label = self.label(FILES)?;
        if label.is_empty() {
            return None;
        }
        label
            .split(',')
            .map(|file| input::absolute(workdir, Path::new(file)).ok())
            .collect()
    }

    pub fn hash(&self) -> Option<&str> {
        self.label(HASH)
    }

    pub fn reference(&self) -> &str {
        &self.config.image
    }

    pub fn running(&self) -> bool {
        self.state.running && !self.state.paused && !self.state.restarting
    }

    pub fn alive(&self) -> bool {
        self.running()
            && !self.state.oom_killed
            && self
                .state
                .health
                .as_ref()
                .is_none_or(|health| health.status == "healthy")
    }

    pub fn started(&self) -> DateTime<Utc> {
        self.state.started_at
    }

    pub fn database(&self) -> bool {
        database_image(self.reference())
            || self.mounts.iter().flatten().any(|mount| {
                [
                    "/var/lib/postgresql",
                    "/var/lib/mysql",
                    "/data/db",
                    "/var/lib/redis",
                    "/var/lib/influxdb",
                    "/var/lib/clickhouse",
                    "/var/lib/cassandra",
                    "/usr/share/elasticsearch/data",
                    "/opt/couchdb/data",
                    "/var/lib/mongodb",
                ]
                .iter()
                .any(|prefix| {
                    mount.destination == *prefix
                        || mount
                            .destination
                            .strip_prefix(*prefix)
                            .is_some_and(|path| path.starts_with('/'))
                })
            })
    }

    pub fn excluded(&self) -> bool {
        self.label("io.banksio.foreman.exclude")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    }
}

pub(super) fn database_image(reference: &str) -> bool {
    let reference = reference.to_ascii_lowercase();
    [
        "postgres",
        "postgis",
        "mysql",
        "mariadb",
        "mongo",
        "redis",
        "valkey",
        "timescale",
        "cockroach",
        "influxdb",
        "clickhouse",
        "elasticsearch",
        "opensearch",
        "couchdb",
        "cassandra",
    ]
    .iter()
    .any(|name| reference.contains(name))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerState {
    running: bool,
    paused: bool,
    restarting: bool,
    #[serde(rename = "OOMKilled")]
    oom_killed: bool,
    started_at: DateTime<Utc>,
    health: Option<Health>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Health {
    status: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerConfig {
    image: String,
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Mount {
    destination: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct Image {
    pub id: String,
    pub os: String,
    pub architecture: String,
    #[serde(default)]
    pub variant: String,
    pub created: Option<DateTime<Utc>>,
    pub repo_tags: Option<Vec<String>>,
    pub repo_digests: Option<Vec<String>>,
    config: Option<ImageConfig>,
}

impl Image {
    pub fn version(&self) -> Option<&str> {
        let label = self
            .config
            .as_ref()?
            .labels
            .as_ref()?
            .get("org.opencontainers.image.version")?;
        (!label.is_empty()
            && label.len() <= 120
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".+-_".contains(&b)))
        .then_some(label)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageConfig {
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ManifestResponse {
    Index(Vec<Manifest>),
    Single(Box<Manifest>),
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Manifest {
    descriptor: Option<Descriptor>,
    digest: Option<String>,
    platform: Option<Platform>,
    schema_v2_manifest: Option<ManifestBody>,
    #[serde(rename = "OCIManifest")]
    oci_manifest: Option<ManifestBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    digest: String,
    platform: Option<Platform>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Platform {
    os: String,
    architecture: String,
    #[serde(default)]
    variant: String,
}

#[derive(Deserialize)]
struct ManifestBody {
    config: ManifestConfig,
}

#[derive(Deserialize)]
struct ManifestConfig {
    digest: String,
}

#[derive(Clone)]
pub(super) struct RemoteImage {
    pub manifest: String,
    pub config: String,
}

impl ManifestResponse {
    fn select(self, local: &Image) -> Option<RemoteImage> {
        let manifests = match self {
            Self::Index(manifests) => manifests,
            Self::Single(manifest) => vec![*manifest],
        };
        let mut comparable = manifests
            .into_iter()
            .filter_map(|manifest| manifest.comparable(local));
        let selected = comparable.next()?;
        comparable.next().is_none().then_some(selected)
    }
}

impl Manifest {
    fn comparable(self, local: &Image) -> Option<RemoteImage> {
        let descriptor = self.descriptor.as_ref();
        let platform = descriptor
            .and_then(|value| value.platform.as_ref())
            .or(self.platform.as_ref())?;
        let remote = descriptor
            .map(|value| value.digest.as_str())
            .or(self.digest.as_deref())?;
        let config = self.oci_manifest.or(self.schema_v2_manifest)?.config.digest;
        let normalized = |variant: &str| {
            if local.architecture == "arm64" && (variant.is_empty() || variant == "v8") {
                return "v8".to_owned();
            }
            variant.to_owned()
        };
        if platform.os != local.os
            || platform.architecture != local.architecture
            || normalized(&platform.variant) != normalized(&local.variant)
            || !digest(remote)
            || !digest(&config)
        {
            return None;
        }
        Some(RemoteImage {
            manifest: remote.to_owned(),
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_the_running_platform_manifest() {
        let config = format!("sha256:{}", "a".repeat(64));
        let manifest = format!("sha256:{}", "f".repeat(64));
        let make_manifest = |architecture, variant| {
            serde_json::json!({
                "Descriptor": { "digest": manifest, "platform": {"os": "linux", "architecture": architecture, "variant": variant } },
                "OCIManifest": {"config": {"digest": config}},
            })
        };
        let local = Image {
            os: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: String::new(),
            id: config.clone(),
            created: None,
            repo_tags: None,
            repo_digests: None,
            config: None,
        };
        let response: ManifestResponse = serde_json::from_value(serde_json::json!([
            make_manifest("amd64", ""),
            make_manifest("arm64", "v8"),
        ]))
        .unwrap();
        assert_eq!(response.select(&local).unwrap().config, config);
        let ambiguous: ManifestResponse = serde_json::from_value(serde_json::json!([
            make_manifest("arm64", ""),
            make_manifest("arm64", "v8"),
        ]))
        .unwrap();
        assert!(ambiguous.select(&local).is_none());
    }
}
