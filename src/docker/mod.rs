mod engine;
mod input;
mod update;

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use clap::Subcommand;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use self::engine::{Container, Engine};
use crate::{AppResult, Context, Error, Outcome, Report, store};

const SCHEMA_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerConfig {
    pub enabled: bool,
    pub exclude: Vec<String>,
    pub allow_databases: Vec<String>,
    pub allow_user_owned_projects: Vec<String>,
    pub registry_config: Option<PathBuf>,
    pub health_timeout: u64,
    pub settle: u64,
    pub boot_settle: u64,
    pub prune_dangling: bool,
    pub prune_tagged_days: u64,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            exclude: vec![],
            allow_databases: vec![],
            allow_user_owned_projects: vec![],
            registry_config: None,
            health_timeout: 180,
            settle: 15,
            boot_settle: 60,
            prune_dangling: true,
            prune_tagged_days: 0,
        }
    }
}

impl DockerConfig {
    pub fn validate(&self) -> AppResult<()> {
        for project in &self.allow_user_owned_projects {
            checked_id(project)?;
        }
        if self.settle == 0
            || self.settle >= self.health_timeout
            || self.health_timeout > 1800
            || self.boot_settle > 600
            || self.prune_tagged_days > 36500
        {
            return Err(Error::invalid(
                "Invalid Compose health, settle, or pruning settings",
            ));
        }
        if let Some(directory) = &self.registry_config {
            input::absolute(Path::new("/"), directory)?;
            if !directory.is_absolute() {
                return Err(Error::invalid("compose.registry_config must be absolute"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Subcommand)]
pub enum DockerAction {
    /// Enroll a local Compose project without changing its containers.
    Enroll {
        file: PathBuf,
        /// Add an override Compose file. Repeat in deployment order.
        #[arg(long = "file")]
        overrides: Vec<PathBuf>,
        /// Match an explicitly named Compose project.
        #[arg(long)]
        project: Option<String>,
        /// Match the project directory used at deployment.
        #[arg(long)]
        project_directory: Option<PathBuf>,
        /// Use this file for Compose interpolation.
        #[arg(long)]
        env_file: Option<PathBuf>,
        /// Activate a Compose profile. Repeat for additional profiles.
        #[arg(long = "profile")]
        profiles: Vec<String>,
    },
    /// Show eligibility and the most recent outcome of an enrolled project.
    Show { id: String },
    /// Enable maintenance for an enrolled project.
    Enable { id: String },
    /// Pause maintenance for an enrolled project.
    Disable { id: String },
    /// Clear a remembered image failure after any rollback recovery is verified.
    Retry { id: String },
    /// Remove enrollment while retaining containers, images, volumes, and history.
    Unenroll { id: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    schema_version: u32,
    project: String,
    workdir: PathBuf,
    files: Vec<PathBuf>,
    env_file: Option<PathBuf>,
    #[serde(default)]
    profiles: Vec<String>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    warn_on_config_drift: bool,
    registry_config: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    observed_inputs: Vec<input::TrustedInput>,
    #[serde(skip)]
    relax_input_ownership: bool,
}

fn yes() -> bool {
    true
}

impl Enrollment {
    fn apply_input_policy(&mut self, ctx: &Context<'_>) {
        self.relax_input_ownership = ctx
            .config
            .compose
            .allow_user_owned_projects
            .contains(&self.project);
    }

    fn input_warning(&self) -> Option<String> {
        self.relax_input_ownership.then(|| format!(
            "{} allows user-owned or writable Compose inputs. Anyone who can edit or replace them can influence root Docker maintenance.",
            self.project
        ))
    }

    fn compose_args<'a, const N: usize>(&'a self, args: [&'a str; N]) -> Vec<&'a str> {
        let mut base = vec![
            "compose",
            "--ansi",
            "never",
            "--project-directory",
            self.workdir.to_str().unwrap_or(""),
            "--env-file",
            self.env_file
                .as_deref()
                .and_then(Path::to_str)
                .unwrap_or("/dev/null"),
        ];
        if !self.project.is_empty() {
            base.extend(["--project-name", &self.project]);
        }
        for file in &self.files {
            base.extend(["--file", file.to_str().unwrap_or("")]);
        }
        for profile in &self.profiles {
            base.extend(["--profile", profile]);
        }
        base.extend(args);
        base
    }

    fn matches(&self, containers: &[Container]) -> bool {
        containers.iter().all(|container| {
            container.project() == Some(&self.project)
                && container.workdir() == Some(&self.workdir)
                && container.files().as_ref() == Some(&self.files)
        })
    }

    fn registry<'a>(&'a self, ctx: &'a Context<'_>) -> Option<&'a Path> {
        self.registry_config
            .as_deref()
            .or(ctx.config.compose.registry_config.as_deref())
    }
}

#[derive(Serialize, JsonSchema)]
pub struct StackList {
    pub schema_version: u32,
    pub stacks: Vec<StackStatus>,
    pub discovery_error: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct StackStatus {
    id: String,
    project: String,
    workdir: Option<PathBuf>,
    files: Vec<PathBuf>,
    services: Vec<String>,
    enrolled: bool,
    enabled: bool,
    migration_candidate: bool,
    enroll_command: Option<Vec<String>>,
    eligibility: &'static str,
    reason: Option<String>,
    warnings: Vec<String>,
    last_result: Option<update::StackResult>,
    recovery_pending: bool,
}

fn checked_id(id: &str) -> AppResult<()> {
    if !input::project_name(id) {
        return Err(Error::invalid("Invalid Docker stack ID"));
    }
    Ok(())
}

fn enrollments_dir(ctx: &Context) -> PathBuf {
    ctx.paths.config_dir.join("stacks.d")
}

fn enrollment_path(ctx: &Context, id: &str) -> PathBuf {
    enrollments_dir(ctx).join(format!("{id}.toml"))
}

fn get(ctx: &Context, id: &str) -> AppResult<Option<Enrollment>> {
    checked_id(id)?;
    let path = enrollment_path(ctx, id);
    if !input::exists(&path)? {
        return Ok(None);
    }
    input::trusted(&path, false)?;
    let contents =
        store::read(&path)?.ok_or_else(|| Error::failed("Docker enrollment disappeared"))?;
    let mut enrollment: Enrollment = toml::from_str(&contents)
        .map_err(|_| Error::invalid(format!("Invalid Docker enrollment {}", path.display())))?;
    if enrollment.project != id || enrollment.schema_version != SCHEMA_VERSION {
        return Err(Error::invalid(format!(
            "Docker enrollment {} has an unsupported identity or schema",
            path.display()
        )));
    }
    input::validate_options(&enrollment)?;
    enrollment.apply_input_policy(ctx);
    Ok(Some(enrollment))
}

fn all(ctx: &Context) -> AppResult<BTreeMap<String, Enrollment>> {
    let directory = enrollments_dir(ctx);
    if !input::exists(&directory)? {
        return Ok(BTreeMap::new());
    }
    input::trusted(&directory, true)?;
    let mut all = BTreeMap::new();
    for file in fs::read_dir(directory)? {
        let path = file?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::invalid("Invalid Docker enrollment filename"))?;
            if let Some(enrollment) = get(ctx, name)? {
                all.insert(name.to_owned(), enrollment);
            }
        }
    }
    Ok(all)
}

pub fn available() -> bool {
    crate::process::available("docker")
        && Engine::new(None)
            .and_then(|engine| engine.exec(&["compose", "version"], 15))
            .is_ok_and(|output| output.success)
}

pub fn ready() -> bool {
    Engine::new(None)
        .and_then(|engine| engine.exec(&["info", "--format", "{{.ServerVersion}}"], 10))
        .is_ok_and(|output| output.success && !output.stdout.trim().is_empty())
}

fn disabled(ctx: &Context) -> bool {
    !ctx.module_enabled("docker") || !ctx.config.compose.enabled
}

struct Eligibility {
    code: &'static str,
    reason: Option<String>,
}

impl Eligibility {
    fn ready() -> Self {
        Self {
            code: "eligible",
            reason: None,
        }
    }
    fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: Some(reason.into()),
        }
    }
}

fn eligibility(
    ctx: &Context,
    id: &str,
    enrollment: Option<&Enrollment>,
    containers: &[Container],
    recovery: bool,
    discovery_error: Option<&str>,
) -> Eligibility {
    let workdir = enrollment
        .map(|enrollment| enrollment.workdir.as_path())
        .or_else(|| containers.first().and_then(Container::workdir));
    let matches = |items: &[String]| {
        items
            .iter()
            .any(|item| item == id || workdir == Some(Path::new(item)))
    };
    if matches(&ctx.config.compose.exclude) || containers.iter().any(Container::excluded) {
        return Eligibility::new("excluded", "Explicit Docker exclusion");
    }
    let Some(enrollment) = enrollment else {
        let local = containers
            .first()
            .and_then(Container::files)
            .is_some_and(|files| {
                files
                    .iter()
                    .all(|file| input::exists(file).unwrap_or(false))
            });
        return if local {
            Eligibility::new("not_enrolled", "Explicit enrollment is required")
        } else {
            Eligibility::new("external", "Compose inputs are unavailable on this host")
        };
    };
    if !enrollment.enabled {
        return Eligibility::new("paused", "Stack maintenance is paused");
    }
    if disabled(ctx) {
        return Eligibility::new("disabled", "Docker module is disabled");
    }
    if recovery {
        return Eligibility::new(
            "recovery_pending",
            "A previous image operation needs operator recovery before it can be retried",
        );
    }
    if let Err(error) = input::audit(enrollment) {
        return Eligibility::new("untrusted_inputs", error.message);
    }
    if let Some(error) = discovery_error {
        return Eligibility::new("docker_unavailable", error);
    }
    if containers.iter().all(|container| !container.running()) {
        return Eligibility::new("stopped", "There are no running replicas");
    }
    if !enrollment.matches(containers) {
        return Eligibility::new(
            "identity_mismatch",
            "Running Compose labels do not match the enrolled project, workdir and ordered files",
        );
    }
    if !matches(&ctx.config.compose.allow_databases) && containers.iter().any(Container::database) {
        return Eligibility::new(
            "database",
            "Likely database stack requires an administrator opt-in",
        );
    }
    Eligibility::ready()
}

#[derive(Deserialize)]
struct LegacyState {
    known_stacks: Option<Vec<String>>,
}

fn migration_candidates(ctx: &Context) -> BTreeSet<String> {
    store::read_json::<LegacyState>(&ctx.paths.state_dir.join("state.json"))
        .ok()
        .flatten()
        .and_then(|state| state.known_stacks)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn enroll_command(id: &str, workdir: &Path, files: &[PathBuf]) -> Option<Vec<String>> {
    let mut command = vec![
        "sudo".to_owned(),
        "foreman".to_owned(),
        "docker".to_owned(),
        "stack".to_owned(),
        "enroll".to_owned(),
        files.first()?.to_str()?.to_owned(),
        "--project".to_owned(),
        id.to_owned(),
        "--project-directory".to_owned(),
        workdir.to_str()?.to_owned(),
    ];
    for file in &files[1..] {
        command.extend(["--file".to_owned(), file.to_str()?.to_owned()]);
    }
    Some(command)
}

pub fn list(ctx: &Context) -> AppResult<StackList> {
    let enrollments = all(ctx)?;
    let discovered = Engine::new(None).and_then(|engine| engine.projects());
    let (projects, discovery_error) = match discovered {
        Ok(projects) => (projects, None),
        Err(error) => (BTreeMap::new(), Some(error.message)),
    };
    let candidates = migration_candidates(ctx);
    let ids: BTreeSet<_> = enrollments.keys().chain(projects.keys()).collect();
    let mut stacks = Vec::new();
    for id in ids {
        let enrollment = enrollments.get(id);
        let containers = projects.get(id).map(Vec::as_slice).unwrap_or_default();
        let state = update::load(ctx, id)?;
        let mut eligibility = eligibility(
            ctx,
            id,
            enrollment,
            containers,
            state.recovery.is_some(),
            discovery_error.as_deref(),
        );
        let mut warnings: Vec<_> = enrollment
            .and_then(Enrollment::input_warning)
            .into_iter()
            .collect();
        if let Some(enrollment) = enrollment.filter(|_| eligibility.code == "eligible") {
            match Engine::new(enrollment.registry(ctx))
                .and_then(|engine| update::check_drift(&engine, enrollment, containers))
            {
                Ok(warning) => {
                    warnings.extend(warning);
                }
                Err(error) => {
                    eligibility = Eligibility::new("config_drift", error.message);
                }
            }
        }
        let workdir = enrollment
            .map(|enrollment| enrollment.workdir.clone())
            .or_else(|| {
                containers
                    .first()
                    .and_then(Container::workdir)
                    .map(Path::to_owned)
            });
        let files = enrollment
            .map(|enrollment| enrollment.files.clone())
            .or_else(|| containers.first().and_then(Container::files))
            .unwrap_or_default();
        let migration_candidate = enrollment.is_none() && candidates.contains(id);
        let enroll_command = if enrollment.is_none() && eligibility.code == "not_enrolled" {
            workdir
                .as_deref()
                .and_then(|workdir| enroll_command(id, workdir, &files))
        } else {
            None
        };
        stacks.push(StackStatus {
            id: id.clone(),
            project: id.clone(),
            workdir,
            files,
            services: containers
                .iter()
                .filter_map(Container::service)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            enrolled: enrollment.is_some(),
            enabled: enrollment.is_some_and(|enrollment| enrollment.enabled),
            migration_candidate,
            enroll_command,
            eligibility: eligibility.code,
            reason: eligibility.reason,
            warnings,
            last_result: state.last_result,
            recovery_pending: state.recovery.is_some(),
        });
    }
    Ok(StackList {
        schema_version: SCHEMA_VERSION,
        stacks,
        discovery_error,
    })
}

pub fn command(ctx: &Context, action: DockerAction) -> AppResult<Report> {
    match action {
        DockerAction::Enroll {
            file,
            overrides,
            project,
            project_directory,
            env_file,
            profiles,
        } => enroll(
            ctx,
            file,
            overrides,
            project,
            project_directory,
            env_file,
            profiles,
        ),
        DockerAction::Show { id } => {
            let enrollment = get(ctx, &id)?
                .ok_or_else(|| Error::invalid(format!("Unknown enrolled Docker stack {id}")))?;
            let status = list(ctx)?
                .stacks
                .into_iter()
                .find(|stack| stack.id == id)
                .ok_or_else(|| Error::failed("Docker stack disappeared during inspection"))?;
            let trusted_inputs = input::audit(&enrollment).unwrap_or_default();
            let state = update::load(ctx, &id)?;
            Ok(Report::new(Outcome::Completed, format!("{id}: {}", status.eligibility)).detail(
                serde_json::json!({"status": status, "enrollment": enrollment, "trusted_inputs": trusted_inputs,
                    "recovery": state.recovery, "holds": state.holds})))
        }
        DockerAction::Enable { id } => set_enabled(ctx, &id, true),
        DockerAction::Disable { id } => set_enabled(ctx, &id, false),
        DockerAction::Retry { id } => {
            let enrollment = get(ctx, &id)?
                .ok_or_else(|| Error::invalid(format!("Unknown enrolled Docker stack {id}")))?;
            update::retry(ctx, &enrollment)
        }
        DockerAction::Unenroll { id } => {
            get(ctx, &id)?
                .ok_or_else(|| Error::invalid(format!("Unknown enrolled Docker stack {id}")))?;
            store::remove(&enrollment_path(ctx, &id))?;
            Ok(Report::new(
                Outcome::Completed,
                format!("Unenrolled Docker stack {id}"),
            ))
        }
    }
}

fn enroll(
    ctx: &Context,
    file: PathBuf,
    overrides: Vec<PathBuf>,
    project: Option<String>,
    project_directory: Option<PathBuf>,
    env_file: Option<PathBuf>,
    profiles: Vec<String>,
) -> AppResult<Report> {
    if disabled(ctx) {
        return Err(Error::blocked("Docker module is disabled"));
    }
    let cwd = env::current_dir()?;
    let file = input::absolute(&cwd, &file)?;
    let workdir = match project_directory {
        Some(directory) => input::absolute(&cwd, &directory)?,
        None => file
            .parent()
            .ok_or_else(|| Error::invalid("Compose file needs a parent directory"))?
            .to_owned(),
    };
    let mut files = vec![file];
    for file in overrides {
        files.push(input::absolute(&cwd, &file)?);
    }
    let env_file = match env_file {
        Some(file) => Some(input::absolute(&cwd, &file)?),
        None => {
            let dotenv = workdir.join(".env");
            input::exists(&dotenv)?.then_some(dotenv)
        }
    };
    let mut enrollment = Enrollment {
        schema_version: SCHEMA_VERSION,
        project: project.unwrap_or_default(),
        workdir,
        files,
        env_file,
        profiles,
        enabled: true,
        warn_on_config_drift: false,
        registry_config: None,
        observed_inputs: vec![],
        relax_input_ownership: false,
    };
    input::validate_options(&enrollment)?;
    let engine = Engine::new(enrollment.registry(ctx))?;
    let projects = engine.projects()?;
    let matches: Vec<_> = projects
        .iter()
        .filter(|(_, containers)| {
            containers.iter().all(|container| {
                container.workdir() == Some(&enrollment.workdir)
                    && container.files().as_ref() == Some(&enrollment.files)
            })
        })
        .map(|(name, _)| name)
        .collect();
    if enrollment.project.is_empty() && matches.len() == 1 {
        enrollment.project = (*matches[0]).clone();
    }
    if enrollment.project.is_empty() && matches.len() > 1 {
        return Err(Error::blocked(
            "Multiple running projects use these inputs. Specify --project to choose one.",
        ));
    }
    if !matches.is_empty() && !matches.contains(&&enrollment.project) {
        return Err(Error::blocked(
            "These Compose inputs belong to a different running project. Check --project.",
        ));
    }
    enrollment.apply_input_policy(ctx);
    let mut warnings: Vec<_> = enrollment.input_warning().into_iter().collect();
    match input::audit(&enrollment) {
        Ok(inputs) => {
            let resolved = input::render(&engine, &enrollment)?.name;
            if enrollment.relax_input_ownership && resolved != enrollment.project {
                return Err(Error::blocked(
                    "Compose returned a different project than the configured ownership exception",
                ));
            }
            enrollment.project = resolved;
            enrollment.observed_inputs = inputs;
        }
        Err(error) if !matches.is_empty() => warnings.push(error.message),
        Err(error) => return Err(error),
    }
    if projects
        .get(&enrollment.project)
        .is_some_and(|containers| !enrollment.matches(containers))
    {
        return Err(Error::blocked(
            "Running Compose identity differs from the requested enrollment. Check its workdir and ordered --file options.",
        ));
    }
    if let Some(existing) = get(ctx, &enrollment.project)? {
        if existing.workdir != enrollment.workdir
            || existing.files != enrollment.files
            || existing.env_file != enrollment.env_file
            || existing.profiles != enrollment.profiles
        {
            return Err(Error::blocked(
                "An enrollment already uses this project name with different inputs. Review and unenroll it before changing identity.",
            ));
        }
        let mut report = Report::new(
            Outcome::Unchanged,
            format!("Docker stack {} is already enrolled", enrollment.project),
        )
        .detail(existing);
        report.warnings = warnings;
        return Ok(report);
    }
    let directory = enrollments_dir(ctx);
    if input::exists(&directory)? {
        input::trusted(&directory, true)?;
    } else {
        input::trusted(&ctx.paths.config_dir, true)?;
    }
    let data = toml::to_string_pretty(&enrollment)
        .map_err(|_| Error::failed("Cannot encode Docker enrollment"))?;
    store::atomic_write(
        &enrollment_path(ctx, &enrollment.project),
        data.as_bytes(),
        0o644,
    )?;
    let mut report = Report::new(
        Outcome::Completed,
        format!("Enrolled Docker stack {}", enrollment.project),
    )
    .detail(enrollment);
    report.warnings = warnings;
    Ok(report)
}

fn set_enabled(ctx: &Context, id: &str, enabled: bool) -> AppResult<Report> {
    let enrollment = get(ctx, id)?
        .ok_or_else(|| Error::invalid(format!("Unknown enrolled Docker stack {id}")))?;
    if enrollment.enabled == enabled {
        return Ok(Report::new(
            Outcome::Unchanged,
            format!(
                "Docker stack {id} is already {}",
                if enabled { "enabled" } else { "paused" }
            ),
        ));
    }
    let path = enrollment_path(ctx, id);
    let raw = store::read(&path)?.ok_or_else(|| Error::failed("Docker enrollment disappeared"))?;
    let mut document = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| Error::invalid("Invalid Docker enrollment TOML"))?;
    if let Some(old) = document
        .get_mut("enabled")
        .and_then(toml_edit::Item::as_value_mut)
    {
        let decorations = old.decor().clone();
        *old = toml_edit::Value::from(enabled);
        *old.decor_mut() = decorations;
    } else {
        document["enabled"] = toml_edit::value(enabled);
    }
    store::atomic_write(&path, document.to_string().as_bytes(), 0o644)?;
    Ok(Report::new(
        Outcome::Completed,
        format!(
            "{} Docker stack {id}",
            if enabled { "Enabled" } else { "Paused" }
        ),
    ))
}

pub fn maintain(ctx: &Context, target: Option<&str>) -> AppResult<Report> {
    if disabled(ctx) {
        let outcome = if target.is_some() {
            Outcome::Blocked
        } else {
            Outcome::Skipped
        };
        return Ok(Report::new(outcome, "Docker module is disabled"));
    }
    if let Some(target) = target {
        checked_id(target)?;
    }
    let enrollments = all(ctx)?;
    if target.is_some_and(|target| !enrollments.contains_key(target)) {
        return Err(Error::invalid("Unknown enrolled Docker stack"));
    }
    if enrollments.is_empty() {
        return Ok(Report::new(
            Outcome::Unchanged,
            "No Docker stacks are enrolled",
        ));
    }
    ctx.config.compose.validate()?;
    if !available() {
        return Err(Error::blocked(
            "Docker Compose is unavailable for enrolled projects",
        ));
    }
    update::maintain(ctx, enrollments, target)
}
