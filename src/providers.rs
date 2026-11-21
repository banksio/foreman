use crate::{AppResult, Context, Error, Outcome, Report, Step, docker, host, process, store};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Clone, Copy)]
struct Module {
    id: &'static str,
    phases: &'static [Step],
    requires: &'static [&'static str],
    command_group: Option<&'static str>,
    resource: Option<&'static str>,
}

const MODULES: &[Module] = &[
    Module {
        id: "linux",
        phases: &[Step::Preflight, Step::Disk],
        requires: &[],
        command_group: None,
        resource: None,
    },
    Module {
        id: "systemd",
        phases: &[Step::Reboot],
        requires: &["linux"],
        command_group: None,
        resource: Some("systemd-boot-and-timers"),
    },
    Module {
        id: "debian",
        phases: &[Step::Preflight, Step::Upgrade, Step::Kernels],
        requires: &["linux"],
        command_group: None,
        resource: Some("host-packages"),
    },
    Module {
        id: "raspberry-pi-os",
        phases: &[Step::Preflight, Step::Reboot, Step::Kernels],
        requires: &["debian"],
        command_group: None,
        resource: None,
    },
    Module {
        id: "docker",
        phases: &[Step::Workloads, Step::Compose, Step::Disk],
        requires: &["linux"],
        command_group: Some("docker"),
        resource: Some("local-docker-workloads"),
    },
    Module {
        id: "webhook",
        phases: &[],
        requires: &["linux"],
        command_group: None,
        resource: None,
    },
    Module {
        id: "heartbeat",
        phases: &[],
        requires: &["linux", "systemd"],
        command_group: None,
        resource: None,
    },
];

pub fn known(id: &str) -> bool {
    MODULES.iter().any(|module| module.id == id)
}

#[derive(Serialize, JsonSchema)]
pub struct ModuleStatus {
    id: &'static str,
    version: &'static str,
    phases: &'static [Step],
    requires: &'static [&'static str],
    command_group: Option<&'static str>,
    exclusive_resource: Option<&'static str>,
    enabled: bool,
    available: bool,
    reason: &'static str,
}

impl ModuleStatus {
    pub fn line(&self) -> String {
        format!(
            "{:18} {:9} {}  {}",
            self.id,
            if self.enabled { "enabled" } else { "inactive" },
            self.version,
            self.reason
        )
    }
}

pub fn list(ctx: &Context<'_>) -> Vec<ModuleStatus> {
    MODULES
        .iter()
        .map(|module| {
            let available = available(ctx, module.id);
            let enabled = active(ctx, module.id);
            let reason = if ctx.config.modules.get(module.id) == Some(&false) {
                "disabled_in_config"
            } else if !available {
                "host_requirements_unavailable"
            } else if !enabled {
                "dependency_unavailable"
            } else {
                "selected_for_host"
            };
            ModuleStatus {
                id: module.id,
                version: env!("CARGO_PKG_VERSION"),
                phases: module.phases,
                requires: module.requires,
                command_group: module.command_group,
                exclusive_resource: module.resource,
                enabled,
                available,
                reason,
            }
        })
        .collect()
}

fn available(ctx: &Context<'_>, id: &str) -> bool {
    match id {
        "linux" => cfg!(target_os = "linux"),
        "systemd" => ctx.host.systemd,
        "debian" => ctx.host.is_debian(),
        "raspberry-pi-os" => ctx.host.profile == "raspberry-pi-os",
        "docker" => {
            static DOCKER: OnceLock<bool> = OnceLock::new();
            ctx.config.compose.enabled && *DOCKER.get_or_init(docker::available)
        }
        "webhook" => {
            ctx.config.notifications.webhook_url_file.is_some() && process::available("curl")
        }
        "heartbeat" => {
            ctx.config.notifications.heartbeat_url_file.is_some() && process::available("curl")
        }
        _ => false,
    }
}

pub fn active(ctx: &Context<'_>, id: &str) -> bool {
    ctx.config.modules.get(id) != Some(&false)
        && available(ctx, id)
        && MODULES
            .iter()
            .find(|module| module.id == id)
            .is_some_and(|module| module.requires.iter().all(|required| active(ctx, required)))
}

pub fn change(ctx: &Context<'_>, id: &str, enabled: bool) -> AppResult<Report> {
    let module = MODULES
        .iter()
        .find(|module| module.id == id)
        .ok_or_else(|| Error::invalid(format!("Unknown bundled module {id}")))?;
    if enabled
        && (!available(ctx, id) || !module.requires.iter().all(|required| active(ctx, required)))
    {
        return Err(Error::blocked(format!(
            "Cannot enable {id}: host requirements or dependencies are unavailable"
        )));
    }
    if !enabled {
        let dependencies: Vec<_> = MODULES
            .iter()
            .filter(|module| module.requires.contains(&id) && active(ctx, module.id))
            .map(|module| module.id)
            .collect();
        if !dependencies.is_empty() {
            return Err(Error::blocked(format!(
                "Modules depend on {id}: {}",
                dependencies.join(", ")
            )));
        }
        if id == "systemd"
            && crate::schedule::list(ctx)?
                .iter()
                .any(|entry| entry.job.enabled)
        {
            return Err(Error::blocked(
                "Pause enabled schedules before disabling systemd",
            ));
        }
    }
    let path = ctx.paths.config_dir.join("modules.toml");
    let mut doc: DocumentMut = store::read(&path)?
        .unwrap_or_default()
        .parse()
        .map_err(|_| Error::invalid("Invalid modules.toml"))?;
    if !doc.contains_key("modules") {
        doc["modules"] = Item::Table(Table::new());
    }
    if !doc["modules"].is_table() {
        return Err(Error::invalid("Use a [modules] table in modules.toml"));
    }
    let mut setting = value(enabled);
    if let (Some(old), Some(new)) = (
        doc["modules"].get(id).and_then(Item::as_value),
        setting.as_value_mut(),
    ) {
        *new.decor_mut() = old.decor().clone();
    }
    doc["modules"][id] = setting;
    store::atomic_write(&path, doc.to_string().as_bytes(), 0o644)?;
    Ok(Report::new(
        Outcome::Completed,
        format!("{} {id}", if enabled { "Enabled" } else { "Disabled" }),
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Request {
    pub phase: Step,
    pub mode: Mode,
    pub stack: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Preview,
    Apply,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    PhaseStarted {
        module: &'static str,
        phase: Step,
    },
    PhaseFinished {
        module: &'static str,
        phase: Step,
        report: Report,
    },
}

trait Provider {
    fn module(&self) -> &'static str;
    fn perform(&self, ctx: &Context<'_>, request: &Request) -> AppResult<Report>;
}

enum Builtin {
    Debian,
    Linux,
    Docker,
}

impl Provider for Builtin {
    fn module(&self) -> &'static str {
        match self {
            Self::Debian => "debian",
            Self::Linux => "linux",
            Self::Docker => "docker",
        }
    }

    fn perform(&self, ctx: &Context<'_>, request: &Request) -> AppResult<Report> {
        match request.phase {
            Step::Preflight => host::preflight(ctx),
            Step::Upgrade => host::upgrade(ctx),
            Step::Kernels => host::kernels(ctx),
            Step::Workloads | Step::Compose => docker::maintain(ctx, request.stack.as_deref()),
            Step::Disk => host::disk(ctx),
            _ => Err(Error::invalid("The phase is coordinated by the core")),
        }
    }
}

fn provider(step: Step) -> Option<Builtin> {
    match step {
        Step::Preflight | Step::Upgrade | Step::Kernels => Some(Builtin::Debian),
        Step::Compose | Step::Workloads => Some(Builtin::Docker),
        Step::Disk => Some(Builtin::Linux),
        _ => None,
    }
}

pub fn check_step(ctx: &Context<'_>, step: Step) -> AppResult<()> {
    let required = match step {
        Step::Cycle => "debian",
        Step::Reboot => "systemd",
        _ => provider(step)
            .ok_or_else(|| Error::invalid("Unsupported phase"))?
            .module(),
    };
    if active(ctx, required) {
        Ok(())
    } else {
        Err(Error::blocked(format!(
            "{} requires the {required} provider",
            step.name()
        )))
    }
}

pub fn run(ctx: &Context<'_>, phase: Step, stack: Option<&str>) -> AppResult<Report> {
    check_step(ctx, phase)?;
    let provider = provider(phase).ok_or_else(|| Error::invalid("Unsupported provider phase"))?;
    let request = Request {
        phase,
        mode: if ctx.dry_run {
            Mode::Preview
        } else {
            Mode::Apply
        },
        stack: stack.map(str::to_owned),
    };
    crate::emit_provider_event(
        ctx,
        Event::PhaseStarted {
            module: provider.module(),
            phase,
        },
    );
    let report = Report::from_result(provider.perform(ctx, &request));
    crate::emit_provider_event(
        ctx,
        Event::PhaseFinished {
            module: provider.module(),
            phase,
            report: report.clone(),
        },
    );
    Ok(report)
}
