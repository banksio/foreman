use crate::{
    AppResult, Context, Error, Outcome, Report, Step, model::RebootMode, process, store, systemd,
};
use chrono::{DateTime, Local, Utc};
use clap::{Subcommand, ValueEnum};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use toml_edit::{DocumentMut, value};

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum JobReboot {
    #[default]
    Inherit,
    Never,
}

impl JobReboot {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Never => "never",
        }
    }

    pub fn mode(self, host: RebootMode) -> RebootMode {
        match self {
            Self::Inherit => host,
            Self::Never => RebootMode::Never,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Action {
    /// List maintenance schedules.
    List,
    /// Show the calendar, next start, and last attempt for a schedule.
    Show { name: String },
    /// Add and enable a named maintenance schedule.
    Add {
        name: String,
        #[arg(long)]
        calendar: String,
        #[arg(long, value_enum, default_value = "cycle")]
        run: Step,
        #[arg(long, value_enum, default_value = "inherit")]
        reboot: JobReboot,
    },
    /// Change an existing schedule.
    Edit {
        name: String,
        #[arg(long)]
        calendar: Option<String>,
        #[arg(long, value_enum)]
        run: Option<Step>,
        #[arg(long, value_enum)]
        reboot: Option<JobReboot>,
    },
    /// Enable a paused schedule.
    Enable { name: String },
    /// Pause future starts.
    Disable { name: String },
    /// Remove a schedule and its timer.
    Remove { name: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub schema_version: u32,
    pub generation: String,
    pub calendar: String,
    pub run: Step,
    pub reboot: JobReboot,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Attempt {
    pub name: String,
    pub generation: String,
    pub invocation_id: Option<String>,
    pub started: DateTime<Utc>,
    pub result: Option<Report>,
}

#[derive(Serialize, JsonSchema)]
pub struct Entry {
    pub name: String,
    #[serde(flatten)]
    pub job: Job,
    pub next_run: Option<String>,
    pub last_attempt: Option<Attempt>,
}

pub fn validate_name(name: &str) -> AppResult<()> {
    if name.is_empty()
        || name.len() > 48
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(Error::invalid(
            "Schedule names must start with a lowercase letter and contain 1–48 lowercase letters, digits, or hyphens",
        ));
    }
    Ok(())
}

fn config_path(ctx: &Context<'_>, name: &str) -> PathBuf {
    ctx.paths
        .config_dir
        .join("schedules")
        .join(format!("{name}.toml"))
}

fn result_path(ctx: &Context<'_>, name: &str) -> PathBuf {
    ctx.paths
        .observation("schedule-results")
        .join(format!("{name}.json"))
}

fn unit_name(name: &str, extension: &str) -> String {
    format!("foreman-job-{name}.{extension}")
}

pub fn list(ctx: &Context<'_>) -> AppResult<Vec<Entry>> {
    let dir = ctx.paths.config_dir.join("schedules");
    let files = match std::fs::read_dir(dir) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };

    let mut schedules = Vec::new();
    for file in files {
        let path = file?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::invalid("Invalid schedule filename"))?;
            schedules.push(entry(ctx, name)?);
        }
    }
    schedules.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(schedules)
}

pub fn entry(ctx: &Context<'_>, name: &str) -> AppResult<Entry> {
    let job = load(ctx, name)?;
    let next_run = if job.enabled && ctx.host.systemd {
        systemd::systemctl(&[
            "show",
            "-p",
            "NextElapseUSecRealtime",
            "--value",
            &unit_name(name, "timer"),
        ])
        .ok()
        .filter(|output| output.success)
        .map(|output| output.stdout.trim().to_owned())
        .filter(|text| !matches!(text.as_str(), "" | "n/a" | "0"))
    } else {
        None
    };
    let last_attempt: Option<Attempt> = store::read_json(&result_path(ctx, name))?;
    Ok(Entry {
        name: name.into(),
        next_run,
        last_attempt: last_attempt.filter(|attempt| attempt.generation == job.generation),
        job,
    })
}

fn read_job(path: &Path) -> AppResult<Option<(DocumentMut, Job)>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    if crate::is_root() {
        store::trusted_file(path)?;
    }
    let Some(text) = store::read(path)? else {
        return Ok(None);
    };
    let doc = text
        .parse()
        .map_err(|_| Error::invalid(format!("Invalid schedule TOML in {}", path.display())))?;
    let job: Job = toml::from_str(&text)
        .map_err(|_| Error::invalid(format!("Invalid schedule fields in {}", path.display())))?;
    if job.schema_version != 1 || job.generation.is_empty() {
        return Err(Error::invalid(format!(
            "Unsupported schedule format in {}",
            path.display()
        )));
    }
    Ok(Some((doc, job)))
}

pub fn load(ctx: &Context<'_>, name: &str) -> AppResult<Job> {
    validate_name(name)?;
    read_job(&config_path(ctx, name))?
        .map(|(_, job)| job)
        .ok_or_else(|| Error::invalid(format!("Unknown schedule {name}")))
}

pub fn command(ctx: &Context<'_>, action: Action) -> AppResult<Report> {
    systemd::require(ctx)?;

    let name = match &action {
        Action::Add { name, .. }
        | Action::Edit { name, .. }
        | Action::Enable { name }
        | Action::Disable { name }
        | Action::Remove { name } => name,
        _ => return Err(Error::invalid("Expected a schedule change")),
    };
    validate_name(name)?;
    let path = config_path(ctx, name);
    store::check_protected_parent(&ctx.paths.config_dir.join("schedules"))?;
    store::check_protected_parent(&ctx.paths.units)?;
    let old = read_job(&path)?;
    if matches!(&action, Action::Add { .. }) == old.is_some() {
        return Err(Error::invalid(if old.is_some() {
            format!("Schedule {name} already exists")
        } else {
            format!("Unknown schedule {name}")
        }));
    }

    let removal = matches!(&action, Action::Remove { .. });
    let (mut doc, mut job) = if let Some((doc, job)) = &old {
        (doc.clone(), job.clone())
    } else {
        (
            DocumentMut::new(),
            Job {
                schema_version: 1,
                generation: new_generation()?,
                calendar: String::new(),
                run: Step::Cycle,
                reboot: JobReboot::Inherit,
                enabled: true,
            },
        )
    };
    let original_job = job.clone();
    match &action {
        Action::Add {
            calendar,
            run,
            reboot,
            ..
        } => {
            job.calendar.clone_from(calendar);
            job.run = *run;
            job.reboot = *reboot;
            doc["schema_version"] = value(1);
            doc["generation"] = value(&job.generation);
            doc["calendar"] = value(&job.calendar);
            doc["run"] = value(job.run.name());
            doc["reboot"] = value(job.reboot.as_str());
            doc["enabled"] = value(true);
        }
        Action::Edit {
            calendar,
            run,
            reboot,
            ..
        } => {
            if let Some(calendar) = calendar {
                job.calendar.clone_from(calendar);
                set(&mut doc, "calendar", value(calendar));
            }
            if let Some(run) = run {
                job.run = *run;
                set(&mut doc, "run", value(run.name()));
            }
            if let Some(reboot) = reboot {
                job.reboot = *reboot;
                set(&mut doc, "reboot", value(reboot.as_str()));
            }
        }
        Action::Enable { .. } => {
            job.enabled = true;
            set(&mut doc, "enabled", value(true));
        }
        Action::Disable { .. } => {
            job.enabled = false;
            set(&mut doc, "enabled", value(false));
        }
        Action::Remove { .. } => {}
        _ => unreachable!(),
    }

    if old.is_some()
        && !removal
        && original_job.calendar == job.calendar
        && original_job.run == job.run
        && original_job.reboot == job.reboot
        && original_job.enabled == job.enabled
        && systemd::is_managed(&ctx.paths.units.join(unit_name(name, "timer")))
        && systemd::systemctl(&["is-enabled", "--quiet", &unit_name(name, "timer")])
            .is_ok_and(|status| status.success == job.enabled)
    {
        return Ok(Report::new(
            Outcome::Unchanged,
            format!("Schedule {name} already has those settings"),
        ));
    }

    let preview = if !removal
        && (job.enabled
            || matches!(
                action,
                Action::Edit {
                    calendar: Some(_),
                    ..
                }
            )) {
        if job.enabled && job.run != Step::Disk {
            systemd::require_legacy_resolution(ctx)?;
        }
        crate::providers::check_step(ctx, job.run)?;
        Some(calendar(&job.calendar)?)
    } else {
        None
    };
    replace(
        ctx,
        name,
        &path,
        (!removal).then_some((&doc, &job)),
        old.as_ref(),
    )?;
    let summary = if removal {
        format!("Removed schedule {name}")
    } else {
        format!("Saved schedule {name}")
    };
    let mut report = Report::new(Outcome::Completed, summary);
    if let Some(preview) = preview {
        if matches!(job.run, Step::Cycle | Step::Reboot)
            && job.reboot.mode(ctx.config.reboot.mode) == RebootMode::Auto
            && preview
                .iter()
                .filter_map(|time| occurrence_time(time))
                .any(|time| !ctx.config.reboot.in_window(time).unwrap_or(false))
        {
            report.warnings.push(format!(
                "A calendar occurrence starts outside the reboot window ({})",
                ctx.config.reboot.window
            ));
        }
        report = report.detail(preview);
    }
    Ok(report)
}

fn occurrence_time(line: &str) -> Option<chrono::NaiveTime> {
    line.split_whitespace()
        .find_map(|word| chrono::NaiveTime::parse_from_str(word, "%H:%M:%S").ok())
}

fn set(doc: &mut DocumentMut, key: &str, mut item: toml_edit::Item) {
    if let (Some(old), Some(new)) = (
        doc.get(key).and_then(toml_edit::Item::as_value),
        item.as_value_mut(),
    ) {
        *new.decor_mut() = old.decor().clone();
    }
    doc[key] = item;
}

fn new_generation() -> AppResult<String> {
    // The kernel provides an independent UUID for each read.
    let id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")?;
    Ok(id.trim().to_owned())
}

fn calendar(expression: &str) -> AppResult<Vec<String>> {
    if expression.is_empty()
        || expression.len() > 512
        || expression.contains(['%', '\\'])
        || !expression
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
    {
        return Err(Error::invalid(
            "Calendar must be one printable systemd calendar expression",
        ));
    }

    let output = process::run(
        "systemd-analyze",
        &["calendar", "--iterations=3", "--", expression],
        Duration::from_secs(15),
    )?;
    if !output.success {
        return Err(Error::invalid(format!(
            "Invalid systemd calendar expression: {expression}"
        )));
    }
    let occurrences: Vec<_> = output
        .stdout
        .lines()
        .filter_map(|line| {
            let (label, date) = line.trim().split_once(':')?;
            (label == "Next elapse" || label.starts_with("Iter. #")).then(|| date.trim().to_owned())
        })
        .filter(|date| date != "never")
        .collect();
    if occurrences.is_empty() {
        return Err(Error::invalid("Calendar has no future occurrences"));
    }
    Ok(occurrences)
}

fn replace(
    ctx: &Context<'_>,
    name: &str,
    path: &Path,
    next: Option<(&DocumentMut, &Job)>,
    old: Option<&(DocumentMut, Job)>,
) -> AppResult<()> {
    let timer = unit_name(name, "timer");
    let service = unit_name(name, "service");
    let timer_path = ctx.paths.units.join(&timer);
    let service_path = ctx.paths.units.join(&service);
    systemd::check_owned(&timer_path)?;
    systemd::check_owned(&service_path)?;
    let old_timer = store::read(&timer_path)?;
    let old_service = store::read(&service_path)?;

    if old_timer.is_some() {
        systemd::checked(&["disable", "--now", &timer])?;
        // The previous definition stays on disk if its stale firing stamp cannot be cleared.
        systemd::checked(&["clean", "--what=state", &timer])?;
    }
    let change = (|| -> AppResult<()> {
        if let Some((doc, job)) = next {
            let service_contents = format!(
                "{}\n[Unit]\nDescription=Foreman maintenance ({name})\nWants=network-online.target\nAfter=network-online.target time-sync.target\n\n[Service]\nType=oneshot\nUMask=0022\nExecStart={} __scheduled {name}\nExecStopPost={} __reconcile {name}\nTimeoutStartSec=6h\nTimeoutStopSec=3min\nKillMode=mixed\nSuccessExitStatus=3 4 5\n",
                systemd::MANAGED,
                systemd::runner(),
                systemd::runner(),
            );
            let timer_contents = format!(
                "{}\n[Unit]\nDescription=Foreman schedule ({name})\n\n[Timer]\nOnCalendar={}\nPersistent=true\nAccuracySec=1s\nUnit={service}\n\n[Install]\nWantedBy=timers.target\n",
                systemd::MANAGED,
                job.calendar,
            );
            store::atomic_write(&service_path, service_contents.as_bytes(), 0o644)?;
            store::atomic_write(&timer_path, timer_contents.as_bytes(), 0o644)?;
            systemd::checked(&["daemon-reload"])?;
            if old_timer.is_none() {
                systemd::checked(&["clean", "--what=state", &timer])?;
            }
            store::atomic_write(path, doc.to_string().as_bytes(), 0o644)?;
            if job.enabled {
                systemd::checked(&["enable", "--now", &timer])?;
            }
        } else {
            store::remove(&timer_path)?;
            store::remove(&service_path)?;
            store::remove(path)?;
            systemd::checked(&["daemon-reload"])?;
        }
        Ok(())
    })();

    if let Err(error) = change {
        let restore = (|| -> AppResult<()> {
            if timer_path.exists() {
                systemd::checked(&["disable", "--now", &timer])?;
            }
            restore_file(&timer_path, old_timer.as_deref())?;
            restore_file(&service_path, old_service.as_deref())?;
            restore_file(path, old.map(|(doc, _)| doc.to_string()).as_deref())?;
            systemd::checked(&["daemon-reload"])?;
            if old.is_some_and(|(_, job)| job.enabled) && old_timer.is_some() {
                systemd::checked(&["enable", "--now", &timer])?;
            }
            Ok(())
        })();
        if let Err(restore) = restore {
            return Err(Error::failed(format!(
                "{error}. Restoring the previous schedule failed: {restore}"
            )));
        }
        return Err(error);
    }
    Ok(())
}

fn restore_file(path: &Path, contents: Option<&str>) -> AppResult<()> {
    match contents {
        Some(contents) => store::atomic_write(path, contents.as_bytes(), 0o644),
        None => store::remove(path),
    }
}

pub fn attempt(name: String, job: &Job) -> Attempt {
    Attempt {
        name,
        generation: job.generation.clone(),
        invocation_id: std::env::var("INVOCATION_ID").ok(),
        started: Utc::now(),
        result: None,
    }
}

pub fn save_attempt(ctx: &Context<'_>, attempt: &Attempt) -> AppResult<()> {
    let path = result_path(ctx, &attempt.name);
    let _guard = store::open_lock(&path.with_extension("lock"))?;
    let previous: Option<Attempt> = store::read_json(&path)?;
    if previous.is_none_or(|previous| previous.started <= attempt.started) {
        store::write_json(&path, attempt)?;
    }
    Ok(())
}

pub fn reconcile(ctx: &Context<'_>, name: &str) -> AppResult<()> {
    let reason = std::env::var("SERVICE_RESULT").unwrap_or_default();
    if reason.is_empty() || reason == "success" {
        return Ok(());
    }
    let job = match load(ctx, name) {
        Ok(job) => job,
        Err(error) if error.outcome == Outcome::Invalid => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut current = attempt(name.into(), &job);
    let previous: Option<Attempt> = store::read_json(&result_path(ctx, name))?;
    if let Some(previous) =
        previous.filter(|previous| previous.invocation_id == current.invocation_id)
    {
        current = previous;
        if current.result.is_some() {
            return Ok(());
        }
    }
    current.result = Some(Report::new(
        Outcome::Failed,
        format!("systemd stopped the scheduled run ({reason})"),
    ));
    save_attempt(ctx, &current)
}

pub fn timezone() -> String {
    Local::now().format("%Z").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_cannot_escape_unit_or_intent_paths() {
        for name in ["", "a/b", "-a", "A", "a\nExecStart", "a.timer"] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("disk-daily").is_ok());
    }

    #[test]
    fn edits_keep_comments() {
        let mut doc: DocumentMut = "# existing\nenabled = true # inline\n".parse().unwrap();
        set(&mut doc, "enabled", value(false));
        assert_eq!(doc.to_string(), "# existing\nenabled = false # inline\n");
    }
}
