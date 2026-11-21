use crate::{
    AppResult, Context, Error, Outcome, Report, Step, docker, host, model::RebootMode, notify,
    process, providers, schedule, store, systemd,
};
use chrono::{DateTime, Local, TimeDelta, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const CYCLE: &str = "cycle-v1.json";
const REBOOT: &str = "reboot-v1.json";
const BOOT_GRACE: i64 = 180;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Preflight,
    Upgrade,
    Reboot,
    Verify,
    Kernels,
    Workloads,
    Disk,
    Finished,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Upgrade => "upgrade",
            Self::Reboot => "reboot",
            Self::Verify => "verify",
            Self::Kernels => "kernels",
            Self::Workloads => "workloads",
            Self::Disk => "disk",
            Self::Finished => "finished",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Cycle {
    schema_version: u32,
    executable_version: String,
    configuration: String,
    started: DateTime<Utc>,
    finished: Option<DateTime<Utc>>,
    phase: Phase,
    in_flight: bool,
    #[serde(default)]
    resume_invocation_id: Option<String>,
    mode: RebootMode,
    outcome: Option<Outcome>,
    schedule: Option<schedule::Attempt>,
    handoff: Option<Handoff>,
    reports: BTreeMap<String, Report>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct Handoff {
    at: DateTime<Utc>,
    boot_id: String,
    #[serde(default)]
    boot_uptime: Option<u64>,
    expected_kernel: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct RebootRecord {
    schema_version: u32,
    attempted: DateTime<Utc>,
}

#[derive(Clone, Copy, Default)]
pub struct RebootOptions {
    pub no_reboot: bool,
    pub reboot_now: bool,
}

pub fn read(ctx: &Context<'_>) -> AppResult<Option<Cycle>> {
    let cycle: Option<Cycle> = store::read_json(&ctx.paths.observation(CYCLE))?;
    if cycle
        .as_ref()
        .is_some_and(|cycle| cycle.schema_version != 1)
    {
        return Err(Error::invalid("Unsupported cycle state"));
    }
    Ok(cycle)
}

impl Cycle {
    pub fn line(&self) -> String {
        let progress = match self.outcome {
            Some(outcome) => outcome.label(),
            None if self.in_flight => "phase started",
            None => "pending",
        };
        format!(
            "Cycle     {progress}  {}  {}",
            self.phase.name(),
            self.started
        )
    }
}

pub fn scheduled_pending(ctx: &Context<'_>, attempt: &schedule::Attempt) -> AppResult<bool> {
    Ok(read(ctx)?.is_some_and(|cycle| {
        cycle.finished.is_none()
            && cycle.schedule.is_some_and(|started| {
                started.started == attempt.started && started.generation == attempt.generation
            })
    }))
}

pub fn pending(ctx: &Context<'_>) -> AppResult<bool> {
    Ok(read(ctx)?.is_some_and(|cycle| cycle.finished.is_none()))
}

pub fn reject_pending(ctx: &Context<'_>) -> AppResult<()> {
    if pending(ctx)? {
        return Err(Error::busy(
            "A saved cycle owns maintenance. Run foreman run to continue, or foreman cycle abort",
        ));
    }
    Ok(())
}

pub fn last_steps(ctx: &Context<'_>) -> AppResult<BTreeMap<String, Report>> {
    let mut records = BTreeMap::new();
    let path = ctx.paths.observation("last-steps-v1.json");
    if let Some(saved) = store::read_json(&path)? {
        records = saved;
    }
    if let Some(disk) = store::read_json(&ctx.paths.observation("disk-v1.json"))? {
        records.insert(Step::Disk.name().into(), disk);
    }
    Ok(records)
}

fn record(ctx: &Context<'_>, step: Step, report: &Report) -> AppResult<()> {
    if ctx.dry_run {
        return Ok(());
    }
    if step == Step::Disk {
        let path = ctx.paths.observation("disk-v1.json");
        let _guard = store::open_lock(&path.with_extension("lock"))?;
        let previous: Option<Report> = store::read_json(&path)?;
        if previous.is_none_or(|previous| previous.at <= report.at) {
            store::write_json(&path, report)?;
        }
    } else {
        let path = ctx.paths.observation("last-steps-v1.json");
        let mut steps: BTreeMap<String, Report> = store::read_json(&path)?.unwrap_or_default();
        steps.insert(step.name().into(), report.clone());
        store::write_json(&path, &steps)?;
    }
    Ok(())
}

fn verify(ctx: &Context<'_>, handoff: &Handoff) -> AppResult<Report> {
    let boot_id = host::boot_id()?;
    if boot_id == handoff.boot_id {
        if handoff_in_grace(handoff) {
            return Ok(Report::new(
                Outcome::Waiting,
                "Waiting for a different boot ID after the requested reboot",
            ));
        }
        return Ok(Report::new(
            Outcome::Failed,
            "Reboot verification failed: the boot ID did not change",
        ));
    }
    let report = host::verify(ctx, handoff.expected_kernel.as_deref())?;
    if ctx.module_enabled("docker") && has_enrollments(ctx)? {
        wait_for_docker(ctx)
            .map_err(|error| Error::failed(format!("Post-boot verification: {error}")))?;
    }
    Ok(report)
}

fn boot_uptime() -> Option<u64> {
    std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn handoff_in_grace(handoff: &Handoff) -> bool {
    handoff.boot_uptime.zip(boot_uptime()).map_or_else(
        || Utc::now() - handoff.at < TimeDelta::seconds(BOOT_GRACE),
        |(start, now)| now.saturating_sub(start) < BOOT_GRACE as u64,
    )
}

pub fn abort(ctx: &Context<'_>) -> AppResult<Report> {
    let Some(mut cycle) = read(ctx)?.filter(|cycle| cycle.finished.is_none()) else {
        return Ok(Report::new(Outcome::Unchanged, "No pending cycle"));
    };
    if let Some(handoff) = &cycle.handoff
        && host::boot_id()? == handoff.boot_id
        && handoff_in_grace(handoff)
    {
        return Err(Error::busy(
            "A reboot handoff is still within its grace period",
        ));
    }

    systemd::clear_window(ctx)?;
    let report = Report::new(Outcome::Blocked, "Cycle aborted by the operator");
    cycle.reports.insert("abort".into(), report.clone());
    // An accepted abort is a successful command with a blocked cycle in history.
    finish(ctx, &mut cycle, Outcome::Blocked)?;
    Ok(Report::new(
        Outcome::Completed,
        "Aborted the pending maintenance cycle",
    ))
}

fn save(ctx: &Context<'_>, cycle: &Cycle) -> AppResult<()> {
    if !ctx.dry_run {
        store::write_json(&ctx.paths.observation(CYCLE), cycle)?;
    }
    Ok(())
}

fn finish(ctx: &Context<'_>, cycle: &mut Cycle, outcome: Outcome) -> AppResult<()> {
    cycle.finished = Some(Utc::now());
    cycle.outcome = Some(outcome);
    cycle.phase = Phase::Finished;
    cycle.in_flight = false;
    save(ctx, cycle)?;
    if ctx.dry_run {
        return Ok(());
    }

    if let Err(error) = systemd::clear_window(ctx) {
        crate::warn(&error.message);
    }
    if let Some(attempt) = &mut cycle.schedule {
        attempt.result = Some(Report::new(outcome, "Maintenance cycle finished"));
        schedule::save_attempt(ctx, attempt)?;
    }
    if let Err(error) = notify::cycle(
        ctx,
        outcome,
        cycle.schedule.as_ref().map(|attempt| attempt.name.as_str()),
    ) {
        crate::warn(&format!("Notification state: {error}"));
    }
    Ok(())
}

pub fn run(
    ctx: &Context<'_>,
    step: Step,
    stack: Option<&str>,
    options: RebootOptions,
    attempt: Option<schedule::Attempt>,
    schedule_mode: RebootMode,
) -> AppResult<Report> {
    if step == Step::Cycle {
        return full(ctx, options, attempt, schedule_mode, false);
    }
    if !ctx.dry_run && step != Step::Disk {
        reject_pending(ctx)?;
    }

    if !ctx.dry_run
        && matches!(
            step,
            Step::Upgrade | Step::Kernels | Step::Compose | Step::Workloads | Step::Reboot
        )
    {
        systemd::require_legacy_resolution(ctx)?;
    }
    let report = match step {
        Step::Upgrade => {
            let preflight = Report::from_result(providers::run(ctx, Step::Preflight, None));
            crate::phase_result(ctx, Step::Preflight.name(), &preflight);
            record(ctx, Step::Preflight, &preflight)?;
            if preflight.outcome.exit() != 0 {
                preflight
            } else {
                upgraded(ctx)?
            }
        }
        Step::Reboot => reboot(
            ctx,
            options,
            effective_mode(ctx, options, schedule_mode),
            None,
        )?,
        _ => Report::from_result(providers::run(ctx, step, stack)),
    };
    if matches!(step, Step::Compose | Step::Workloads)
        && let Err(error) = notify::rollback(ctx, &report)
    {
        crate::warn(&format!("Rollback notification state: {error}"));
    }
    record(ctx, step, &report)?;
    if !ctx.dry_run
        && step != Step::Disk
        && let Err(error) = notify::tick(ctx)
    {
        crate::warn(&format!("Notification state: {error}"));
    }
    Ok(report)
}

pub fn resume(ctx: &Context<'_>) -> AppResult<Report> {
    full(
        ctx,
        RebootOptions::default(),
        None,
        ctx.config.reboot.mode,
        true,
    )
}

pub fn reconcile_resume(ctx: &Context<'_>) -> AppResult<()> {
    let reason = std::env::var("SERVICE_RESULT").unwrap_or_default();
    if reason.is_empty() || reason == "success" {
        return Ok(());
    }
    let Some(id) = std::env::var("INVOCATION_ID").ok() else {
        return Ok(());
    };
    let _guard = store::lock(&ctx.paths.lock)?;
    if let Some(mut cycle) = read(ctx)?.filter(|cycle| {
        cycle.finished.is_none() && cycle.resume_invocation_id.as_deref() == Some(&id)
    }) {
        let report = Report::new(
            Outcome::Failed,
            format!("systemd stopped cycle continuation ({reason})"),
        );
        cycle.reports.insert("interruption".into(), report);
        save(ctx, &cycle)?;
    }
    Ok(())
}

fn effective_mode(
    ctx: &Context<'_>,
    options: RebootOptions,
    schedule_mode: RebootMode,
) -> RebootMode {
    if ctx.config.reboot.mode == RebootMode::Never
        || schedule_mode == RebootMode::Never
        || options.no_reboot
    {
        RebootMode::Never
    } else {
        RebootMode::Auto
    }
}

fn full(
    ctx: &Context<'_>,
    options: RebootOptions,
    attempt: Option<schedule::Attempt>,
    schedule_mode: RebootMode,
    resume_only: bool,
) -> AppResult<Report> {
    let prior = if ctx.dry_run {
        None
    } else {
        read(ctx)?.filter(|cycle| cycle.finished.is_none())
    };
    if prior.is_some() && attempt.is_some() {
        return Err(Error::busy("Another full cycle owns pending continuation"));
    }
    if prior.is_none() && resume_only {
        systemd::clear_window(ctx)?;
        return Ok(Report::new(Outcome::Unchanged, "No cycle to resume"));
    }
    let fingerprint = ctx.config.fingerprint()?;
    let mut cycle = match prior {
        Some(mut cycle) => {
            if cycle.configuration != fingerprint
                || cycle.executable_version != env!("CARGO_PKG_VERSION")
            {
                return Err(Error::blocked(
                    "Configuration or Foreman version changed during the pending cycle. Restore it or abort the cycle",
                ));
            }
            if cycle.in_flight
                && !matches!(cycle.phase, Phase::Preflight | Phase::Disk | Phase::Verify)
            {
                return Err(Error::blocked(format!(
                    "{} was interrupted. Review the host before running foreman cycle abort",
                    cycle.phase.name()
                )));
            }
            if options.no_reboot {
                cycle.mode = RebootMode::Never;
            }
            if host::reboot_clock_safety(ctx).is_ok()
                && Utc::now() - cycle.started
                    >= TimeDelta::days(i64::from(ctx.config.reboot.pending_days))
            {
                finish(ctx, &mut cycle, Outcome::Blocked)?;
                return Ok(Report::new(Outcome::Blocked, "Pending maintenance expired"));
            }
            cycle
        }
        None => Cycle {
            schema_version: 1,
            executable_version: env!("CARGO_PKG_VERSION").into(),
            configuration: fingerprint,
            started: Utc::now(),
            finished: None,
            phase: Phase::Preflight,
            in_flight: false,
            resume_invocation_id: None,
            mode: effective_mode(ctx, options, schedule_mode),
            outcome: None,
            schedule: attempt,
            handoff: None,
            reports: BTreeMap::new(),
        },
    };

    if !ctx.dry_run {
        systemd::require_legacy_resolution(ctx)?;
    }
    cycle.resume_invocation_id = if resume_only {
        std::env::var("INVOCATION_ID").ok()
    } else {
        None
    };
    cycle.reports.remove("interruption");
    loop {
        if process::interrupted() {
            return Err(Error::failed(
                "Cycle interrupted; its checkpoint has been retained",
            ));
        }
        let phase = cycle.phase;
        if phase == Phase::Finished {
            break;
        }
        cycle.in_flight = !matches!(phase, Phase::Reboot | Phase::Verify);
        save(ctx, &cycle)?;
        let report = match phase {
            Phase::Preflight => Report::from_result(providers::run(ctx, Step::Preflight, None)),
            Phase::Upgrade => Report::from_result(upgraded(ctx)),
            Phase::Reboot => {
                let mode = cycle.mode;
                Report::from_result(reboot(ctx, options, mode, Some(&mut cycle)))
            }
            Phase::Verify => match &cycle.handoff {
                Some(handoff) => Report::from_result(verify(ctx, handoff)),
                None => Report::new(Outcome::Failed, "Missing reboot verification checkpoint"),
            },
            Phase::Kernels => Report::from_result(providers::run(ctx, Step::Kernels, None)),
            Phase::Workloads => {
                if !ctx.module_enabled("docker") {
                    Report::new(Outcome::Skipped, "No active workload provider")
                } else if let Err(error) = workload_ready(ctx, &cycle) {
                    Report::new(error.outcome, error.message)
                } else {
                    Report::from_result(providers::run(ctx, Step::Workloads, None))
                }
            }
            Phase::Disk => Report::from_result(providers::run(ctx, Step::Disk, None)),
            Phase::Finished => unreachable!(),
        };
        crate::phase_result(ctx, phase.name(), &report);
        if phase == Phase::Workloads
            && let Err(error) = notify::rollback(ctx, &report)
        {
            crate::warn(&format!("Rollback notification state: {error}"));
        }
        let step = match phase {
            Phase::Preflight => Some(Step::Preflight),
            Phase::Upgrade => Some(Step::Upgrade),
            Phase::Reboot => Some(Step::Reboot),
            Phase::Kernels => Some(Step::Kernels),
            Phase::Workloads => Some(Step::Workloads),
            Phase::Disk => Some(Step::Disk),
            _ => None,
        };
        let waited = report.outcome == Outcome::Waiting;
        let problem = report.outcome.exit() != 0;
        let failed = report.outcome == Outcome::Failed;
        cycle.reports.insert(phase.name().into(), report.clone());
        // On an interrupted phase, retain in-flight state unless it was read-only.
        if !process::interrupted() {
            cycle.in_flight = false;
        }
        if let Some(step) = step {
            record(ctx, step, &report)?;
        }
        if process::interrupted() {
            save(ctx, &cycle)?;
            return Err(Error::failed(
                "Cycle interrupted; its checkpoint has been retained",
            ));
        }
        // A successful reboot request leaves the verification checkpoint in place.
        if phase == Phase::Reboot && cycle.phase == Phase::Verify && !problem {
            save(ctx, &cycle)?;
            return Ok(report);
        }
        if waited {
            save(ctx, &cycle)?;
            if let Err(error) = notify::tick(ctx) {
                crate::warn(&format!("Notification state: {error}"));
            }
            return Ok(report);
        }

        cycle.phase = match phase {
            Phase::Preflight if problem => Phase::Disk,
            Phase::Preflight => Phase::Upgrade,
            Phase::Upgrade if failed && ctx.config.maintenance.workloads_after_host_failure => {
                Phase::Workloads
            }
            Phase::Upgrade if problem => Phase::Disk,
            Phase::Upgrade => Phase::Reboot,
            Phase::Reboot | Phase::Verify if problem => Phase::Disk,
            Phase::Reboot | Phase::Verify => Phase::Kernels,
            Phase::Kernels => Phase::Workloads,
            Phase::Workloads => Phase::Disk,
            Phase::Disk => Phase::Finished,
            Phase::Finished => unreachable!(),
        };
        save(ctx, &cycle)?;
    }
    let overall = summarize(cycle.reports.values().map(|report| report.outcome));
    finish(ctx, &mut cycle, overall)?;
    Ok(Report::new(
        overall,
        if ctx.dry_run {
            "Maintenance preview finished"
        } else {
            "Maintenance cycle finished"
        },
    ))
}

fn summarize(outcomes: impl IntoIterator<Item = Outcome>) -> Outcome {
    let mut overall = Outcome::Completed;
    for outcome in outcomes {
        match outcome {
            Outcome::Failed => return Outcome::Failed,
            Outcome::Invalid if overall != Outcome::Failed => overall = Outcome::Invalid,
            Outcome::Blocked if overall != Outcome::Invalid => overall = Outcome::Blocked,
            _ => {}
        }
    }
    overall
}

fn upgraded(ctx: &Context<'_>) -> AppResult<Report> {
    providers::run(ctx, Step::Upgrade, None)
}

fn workload_ready(ctx: &Context<'_>, cycle: &Cycle) -> AppResult<()> {
    if ctx.dry_run {
        return Ok(());
    }
    if let Some(upgrade) = cycle.reports.get("upgrade") {
        if upgrade.outcome == Outcome::Failed {
            let audit = process::run("dpkg", &["--audit"], Duration::from_secs(20))?;
            if !audit.success || !audit.stdout.trim().is_empty() || !docker::ready() {
                return Err(Error::blocked(
                    "Package recovery or Docker readiness check failed",
                ));
            }
        }
        if cycle.handoff.is_none() && upgrade.details["docker_updated"].as_bool() == Some(true) {
            return wait_for_docker(ctx);
        }
    }
    Ok(())
}

fn has_enrollments(ctx: &Context<'_>) -> AppResult<bool> {
    let files = match std::fs::read_dir(ctx.paths.config_dir.join("stacks.d")) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for file in files {
        if file?.path().extension().is_some_and(|ext| ext == "toml") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn wait_for_docker(ctx: &Context<'_>) -> AppResult<()> {
    let settle = Duration::from_secs(ctx.config.compose.boot_settle);
    let deadline = Instant::now() + Duration::from_secs(120) + settle;
    let mut ready_since = None;
    while Instant::now() < deadline && !process::interrupted() {
        if docker::ready() {
            if ready_since.get_or_insert_with(Instant::now).elapsed() >= settle {
                return Ok(());
            }
        } else {
            ready_since = None;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(Error::blocked("Docker did not become and stay ready"))
}

fn reboot(
    ctx: &Context<'_>,
    options: RebootOptions,
    mode: RebootMode,
    mut cycle: Option<&mut Cycle>,
) -> AppResult<Report> {
    let observation = host::reboot_status(ctx);
    if !observation.needed {
        return Ok(Report::new(
            Outcome::Unchanged,
            if ctx.dry_run {
                "No reboot is currently required; a package upgrade may change that"
            } else {
                "No reboot required"
            },
        ));
    }
    if mode == RebootMode::Never {
        let mut report = Report::new(Outcome::Skipped, "Reboot required; reboot mode is never");
        report.warnings.push(observation.reason);
        return Ok(report);
    }
    systemd::require(ctx)?;
    if ctx.module_enabled("raspberry-pi-os")
        && let Err(error) = host::reboot_clock_safety(ctx)
    {
        let mut report = Report::new(
            if ctx.dry_run {
                Outcome::Unchanged
            } else {
                Outcome::Waiting
            },
            "Reboot is deferred while the host clock synchronizes",
        );
        report.warnings.push(error.message);
        if !ctx.dry_run
            && cycle.is_some()
            && let Err(error) =
                systemd::install_resume(ctx).and_then(|()| systemd::arm_delay(ctx, 300))
        {
            report
                .warnings
                .push(format!("Automatic continuation setup failed: {error}"));
        }
        return Ok(report);
    }
    host::reboot_safety(ctx)?;
    let now = Utc::now();
    let path = ctx.paths.observation(REBOOT);
    if let Some(record) = store::read_json::<RebootRecord>(&path)? {
        if record.schema_version != 1 {
            return Err(Error::invalid("Unsupported reboot record"));
        }
        if now < record.attempted + TimeDelta::hours(i64::from(ctx.config.reboot.cooldown_hours)) {
            return Err(Error::blocked(
                "A previous Foreman reboot attempt is still in its cooldown period",
            ));
        }
    }
    if !options.reboot_now && !ctx.config.reboot.in_window(Local::now().time())? {
        let mut report = Report::new(
            if ctx.dry_run {
                Outcome::Unchanged
            } else {
                Outcome::Waiting
            },
            format!(
                "{} the reboot window ({})",
                if ctx.dry_run {
                    "Would wait for"
                } else {
                    "Waiting for"
                },
                ctx.config.reboot.window
            ),
        );
        if !ctx.dry_run
            && cycle.is_some()
            && let Some((start, _)) = ctx.config.reboot.bounds()?
            && let Err(error) = systemd::arm_window(ctx, start)
        {
            report.warnings.push(format!(
                "Automatic continuation setup failed. Run foreman run in the window: {error}"
            ));
        }
        return Ok(report);
    }

    if ctx.dry_run {
        return Ok(Report::new(
            Outcome::Unchanged,
            format!("Would request reboot: {}", observation.reason),
        ));
    }

    let mut watchdog_warning = None;
    if let Some(cycle) = cycle.as_mut() {
        if cycle.handoff.is_some() {
            return Err(Error::blocked("This cycle has used its reboot attempt"));
        }
        systemd::install_resume(ctx)?;
        cycle.handoff = Some(Handoff {
            at: now,
            boot_id: host::boot_id()?,
            boot_uptime: boot_uptime(),
            expected_kernel: observation.expected_kernel,
        });
        cycle.phase = Phase::Verify;
        cycle.in_flight = false;
        save(ctx, cycle)?;
        if let Err(error) = systemd::arm_delay(ctx, BOOT_GRACE + 15) {
            watchdog_warning = Some(format!(
                "Reboot watchdog could not start. Run foreman run if the host does not restart: {error}"
            ));
        }
    }
    store::write_json(
        &path,
        &RebootRecord {
            schema_version: 1,
            attempted: now,
        },
    )?;
    systemd::checked(&["reboot", "--no-block"])?;
    let mut report = Report::new(
        Outcome::Completed,
        format!("Reboot handed off to systemd: {}", observation.reason),
    );
    report.warnings.extend(watchdog_warning);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_errors_dominate_blocked_and_warning_outcomes() {
        assert_eq!(
            summarize([Outcome::Blocked, Outcome::Failed, Outcome::Unchanged]),
            Outcome::Failed
        );
        assert_eq!(
            summarize([Outcome::Skipped, Outcome::Blocked]),
            Outcome::Blocked
        );
    }
}
