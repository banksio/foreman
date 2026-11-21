use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use super::{Enrollment, engine::Engine};
use crate::{AppResult, Error};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct TrustedInput {
    path: PathBuf,
    owner: u32,
    mode: u32,
}

pub(super) fn project_name(value: &str) -> bool {
    value.len() <= 100
        && value.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && value
            .bytes()
            .all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

pub(super) fn service_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}

pub(super) fn absolute(directory: &Path, path: &Path) -> AppResult<PathBuf> {
    let joined = directory.join(path);
    let path = joined
        .to_str()
        .ok_or_else(|| Error::invalid("Docker input needs a UTF-8 path"))?;
    if !joined.is_absolute()
        || joined
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        || path.contains(',')
        || path.chars().any(char::is_control)
    {
        return Err(Error::invalid(format!(
            "Unsupported Docker input path {path:?}"
        )));
    }
    Ok(joined.components().collect())
}

pub(super) fn trusted(path: &Path, directory: bool) -> AppResult<TrustedInput> {
    let mut input = None;
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|error| {
            Error::blocked(format!(
                "Cannot verify Docker input {}: {error}",
                ancestor.display()
            ))
        })?;
        let expected_type = if directory || ancestor != path {
            metadata.is_dir()
        } else {
            metadata.is_file()
        };
        if !expected_type || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Error::blocked(format!(
                "Docker input {} must be root-owned, may only be writable by root, and cannot traverse symlinks",
                ancestor.display(),
            )));
        }
        if ancestor == path {
            input = Some(TrustedInput {
                path: path.to_owned(),
                owner: metadata.uid(),
                mode: metadata.mode() & 0o7777,
            });
        }
    }
    input.ok_or_else(|| Error::blocked("Docker input path is empty"))
}

pub(super) fn exists(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::blocked(format!(
            "Cannot check {}: {error}",
            path.display()
        ))),
    }
}

fn read(path: &Path) -> AppResult<String> {
    use std::io::Read;
    const LIMIT: u64 = 1024 * 1024;
    let mut contents = String::new();
    fs::File::open(path)
        .and_then(|file| file.take(LIMIT + 1).read_to_string(&mut contents))
        .map_err(|error| {
            Error::blocked(format!(
                "Cannot read Docker input {}: {error}",
                path.display()
            ))
        })?;
    if contents.len() as u64 > LIMIT {
        return Err(Error::blocked(format!(
            "Docker input {} exceeds 1 MiB",
            path.display()
        )));
    }
    Ok(contents)
}

fn trusted_env(path: &Path) -> AppResult<TrustedInput> {
    let metadata = trusted(path, false)?;
    if read(path)?.lines().any(|line| {
        let line = line.trim_start();
        let line = line
            .strip_prefix("export")
            .filter(|s| s.starts_with(char::is_whitespace))
            .unwrap_or(line);
        let key = line
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '=' || c == ':')
            .next()
            .unwrap_or("");
        ["COMPOSE_", "DOCKER_", "BUILDX_", "BUILDKIT_"]
            .iter()
            .any(|prefix| key.starts_with(prefix))
    }) {
        return Err(Error::blocked(format!(
            "Docker and Compose control variables are unsupported in {}",
            path.display()
        )));
    }
    Ok(metadata)
}

fn add_reference(
    result: &mut Vec<TrustedInput>,
    workdir: &Path,
    value: &Value,
    allow_optional: bool,
) -> AppResult<()> {
    if let Value::Tagged(tagged) = value {
        return add_reference(result, workdir, &tagged.value, allow_optional);
    }
    if let Some(entries) = value.as_sequence() {
        for entry in entries {
            add_reference(result, workdir, entry, allow_optional)?;
        }
        return Ok(());
    }
    if value.is_null() {
        return Ok(());
    }
    let (path, required) = match value.as_str() {
        Some(path) => (path, true),
        None if allow_optional => (
            value
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::blocked("Unknown Compose file reference"))?,
            value
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        ),
        None => return Err(Error::blocked("Unknown Compose file reference")),
    };
    if path.is_empty() || path.contains('$') || path.starts_with('~') || path.contains("://") {
        return Err(Error::blocked(
            "Compose file references must use static local paths",
        ));
    }
    let path = absolute(workdir, Path::new(path))?;
    if required || exists(&path)? {
        result.push(trusted(&path, false)?);
    } else {
        for parent in path.ancestors().skip(1) {
            if exists(parent)? {
                result.push(trusted(parent, true)?);
                break;
            }
        }
    }
    Ok(())
}

fn untag(value: &Value) -> &Value {
    match value {
        Value::Tagged(tagged) => untag(&tagged.value),
        _ => value,
    }
}

fn audit_yaml(path: &Path, workdir: &Path, result: &mut Vec<TrustedInput>) -> AppResult<()> {
    result.push(trusted(path, false)?);
    let mut source: Value = serde_yaml::from_str(&read(path)?).map_err(|_| {
        Error::blocked(format!(
            "Cannot validate Compose YAML in {}",
            path.display()
        ))
    })?;
    source.apply_merge().map_err(|_| {
        Error::blocked(format!(
            "Cannot validate Compose YAML merges in {}",
            path.display()
        ))
    })?;
    let source = untag(&source)
        .as_mapping()
        .ok_or_else(|| Error::blocked("Compose YAML must be a mapping"))?;
    if source.get("include").is_some() {
        return Err(Error::blocked(
            "Compose include is unsupported for unattended maintenance. Enroll local override files explicitly.",
        ));
    }
    if let Some(services) = source
        .get("services")
        .and_then(|value| untag(value).as_mapping())
    {
        for service in services.values().map(untag) {
            if service.get("extends").is_some() {
                return Err(Error::blocked(
                    "Compose extends is unsupported for unattended maintenance",
                ));
            }
            if service.get("provider").is_some() {
                return Err(Error::blocked(
                    "Compose service providers are unsupported for unattended maintenance",
                ));
            }
            for (key, optional) in [("env_file", true), ("label_file", false)] {
                if let Some(file) = service.get(key) {
                    add_reference(result, workdir, file, optional)?;
                }
            }
        }
    }
    for name in ["configs", "secrets"] {
        if let Some(resources) = source.get(name).and_then(|value| untag(value).as_mapping()) {
            for resource in resources.values().map(untag) {
                if let Some(file) = resource.get("file") {
                    add_reference(result, workdir, file, false)?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn audit(enrollment: &Enrollment) -> AppResult<Vec<TrustedInput>> {
    if enrollment.files.is_empty() || !enrollment.workdir.is_absolute() {
        return Err(Error::blocked(
            "Docker enrollment is missing its input paths",
        ));
    }
    if enrollment
        .files
        .iter()
        .any(|file| absolute(&enrollment.workdir, file).as_ref().ok() != Some(file))
        || enrollment
            .env_file
            .as_ref()
            .is_some_and(|file| absolute(&enrollment.workdir, file).as_ref().ok() != Some(file))
    {
        return Err(Error::blocked(
            "Docker enrollment has a non-canonical input path",
        ));
    }
    let mut result = vec![trusted(&enrollment.workdir, true)?];
    for file in &enrollment.files {
        audit_yaml(file, &enrollment.workdir, &mut result)?;
    }
    if let Some(file) = &enrollment.env_file {
        result.push(trusted_env(file)?);
    } else if exists(&enrollment.workdir.join(".env"))? {
        return Err(Error::blocked(
            "A new .env appeared since enrollment. Review and re-enroll the project.",
        ));
    }
    Ok(result)
}

#[derive(Debug, Deserialize, PartialEq)]
pub(super) struct Compose {
    pub name: String,
    pub services: BTreeMap<String, Service>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub(super) struct Service {
    pub image: Option<String>,
    #[serde(default)]
    pub build: Option<serde_json::Value>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

pub(super) fn render(engine: &Engine, enrollment: &Enrollment) -> AppResult<Compose> {
    audit(enrollment)?;
    let model: Compose = engine.json(
        &enrollment.compose_args(["config", "--format", "json"]),
        60,
        "Compose configuration could not be rendered with the enrolled inputs",
    )?;
    if !project_name(&model.name) || model.services.keys().any(|name| !service_name(name)) {
        return Err(Error::blocked(
            "Compose returned an invalid project or service name",
        ));
    }
    Ok(model)
}

pub(super) fn hashes(
    engine: &Engine,
    enrollment: &Enrollment,
) -> AppResult<BTreeMap<String, String>> {
    audit(enrollment)?;
    let output = engine.run(
        &enrollment.compose_args(["config", "--hash=*"]),
        60,
        "Compose service hashes are unavailable",
    )?;
    let mut hashes = BTreeMap::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 2
            || !service_name(parts[0])
            || parts[1].len() != 64
            || !parts[1].bytes().all(|b| b.is_ascii_hexdigit())
            || hashes
                .insert(parts[0].to_owned(), parts[1].to_owned())
                .is_some()
        {
            return Err(Error::blocked(
                "Compose returned unrecognized service hashes",
            ));
        }
    }
    Ok(hashes)
}

pub(super) fn registry_config(path: &Path) -> AppResult<()> {
    if absolute(Path::new("/"), path)? != path {
        return Err(Error::blocked(
            "Docker registry configuration must use an absolute, canonical path",
        ));
    }
    trusted(path, true)?;
    if exists(&path.join("cli-plugins"))? || exists(&path.join("manifests"))? {
        return Err(Error::blocked(
            "The registry credential directory cannot contain CLI plugins or a local manifest cache",
        ));
    }
    let config = path.join("config.json");
    if exists(&config)? {
        let metadata = trusted(&config, false)?;
        let data: serde_json::Value = serde_json::from_str(&read(&config)?)
            .map_err(|_| Error::blocked("Cannot validate Docker registry configuration JSON"))?;
        if !data.is_object()
            || data.get("currentContext").is_some()
            || data.get("cliPluginsExtraDirs").is_some()
        {
            return Err(Error::blocked(
                "Docker registry configuration may not set CLI plugin directories or a Docker context",
            ));
        }
        if metadata.mode & 0o077 != 0
            && data
                .get("auths")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|auths| {
                    auths
                        .values()
                        .any(|value| value.as_object().is_some_and(|entry| !entry.is_empty()))
                })
        {
            return Err(Error::blocked(
                "Docker configuration with inline registry credentials must only be accessible to root",
            ));
        }
        let helpers = data
            .get("credHelpers")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|helpers| helpers.values());
        for helper in data.get("credsStore").into_iter().chain(helpers) {
            if !helper
                .as_str()
                .is_some_and(|name| name.is_empty() || service_name(name))
            {
                return Err(Error::blocked(
                    "Docker registry configuration specifies an invalid credential helper",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_options(enrollment: &Enrollment) -> AppResult<()> {
    if enrollment
        .profiles
        .iter()
        .any(|profile| profile != "*" && !service_name(profile))
        || enrollment.profiles.iter().collect::<BTreeSet<_>>().len() != enrollment.profiles.len()
        || enrollment.files.iter().collect::<BTreeSet<_>>().len() != enrollment.files.len()
        || (!enrollment.project.is_empty() && !project_name(&enrollment.project))
    {
        return Err(Error::invalid(
            "Invalid or duplicate input in Compose project options",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_cannot_be_matched_to_labels() {
        assert!(absolute(Path::new("/srv"), Path::new("foo,bar.yaml")).is_err());
        assert!(absolute(Path::new("/srv"), Path::new("../compose.yaml")).is_err());
        assert_eq!(
            absolute(Path::new("/srv"), Path::new("./compose.yaml")).unwrap(),
            Path::new("/srv/compose.yaml")
        );
    }

    #[test]
    fn validates_cli_identifiers() {
        assert!(project_name("prod-app_2"));
        assert!(service_name("api.v2"));
        assert!(!project_name("-project"));
        assert!(!service_name("--build"));
    }
}
