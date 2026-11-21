use crate::{AppResult, Context, Error, Outcome, Report, host, process, store};
use chrono::{DateTime, TimeDelta, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Cycle,
    Rollback,
    StaleReboot,
    Heartbeat,
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct Event {
    kind: Kind,
    outcome: Option<Outcome>,
    schedule: Option<String>,
    at: DateTime<Utc>,
    attempts: u8,
    last_attempt: Option<DateTime<Utc>>,
    delivered: bool,
}

impl Event {
    fn new(kind: Kind, outcome: Option<Outcome>, schedule: Option<String>) -> Self {
        Self {
            kind,
            outcome,
            schedule,
            at: Utc::now(),
            attempts: 0,
            last_attempt: None,
            delivered: false,
        }
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct State {
    schema_version: u32,
    reboot_observed_at: Option<DateTime<Utc>>,
    stale_queued: bool,
    events: Vec<Event>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 1,
            reboot_observed_at: None,
            stale_queued: false,
            events: vec![],
        }
    }
}

impl State {
    pub fn reboot_observed_at(&self) -> Option<DateTime<Utc>> {
        self.reboot_observed_at
    }

    pub fn last_delivery(&self) -> Option<String> {
        let event = self.events.last()?;
        let kind = match event.kind {
            Kind::Cycle => "Cycle",
            Kind::Rollback => "Rollback",
            Kind::StaleReboot => "Stale reboot",
            Kind::Heartbeat => "Heartbeat",
        };
        let outcome = if event.delivered {
            "delivered"
        } else if event.attempts > 0 {
            "delivery failed"
        } else {
            "queued"
        };
        Some(format!(
            "{kind} {outcome} ({})",
            event.last_attempt.unwrap_or(event.at)
        ))
    }
}

pub fn status(ctx: &Context<'_>) -> AppResult<State> {
    let state: State =
        store::read_json(&ctx.paths.observation("notifications-v1.json"))?.unwrap_or_default();
    if state.schema_version != 1 {
        return Err(Error::invalid("Unsupported notification state"));
    }
    Ok(state)
}

pub fn cycle(ctx: &Context<'_>, outcome: Outcome, schedule: Option<&str>) -> AppResult<()> {
    update(ctx, |state| {
        if ctx.module_enabled("webhook") {
            state.events.push(Event::new(
                Kind::Cycle,
                Some(outcome),
                schedule.map(str::to_owned),
            ));
        }
        if outcome.exit() == 0 {
            push_heartbeat(ctx, state, schedule);
        }
    })
}

pub fn scheduled_step(ctx: &Context<'_>, outcome: Outcome, schedule: &str) -> AppResult<()> {
    update(ctx, |state| {
        if outcome.exit() == 0 && outcome != Outcome::Skipped {
            push_heartbeat(ctx, state, Some(schedule));
        }
    })
}

fn push_heartbeat(ctx: &Context<'_>, state: &mut State, schedule: Option<&str>) {
    if ctx.module_enabled("heartbeat")
        && schedule.is_some()
        && ctx.config.notifications.heartbeat_schedule.as_deref() == schedule
    {
        state.events.push(Event::new(
            Kind::Heartbeat,
            Some(Outcome::Completed),
            schedule.map(str::to_owned),
        ));
    }
}

pub fn rollback(ctx: &Context<'_>, report: &Report) -> AppResult<()> {
    if !ctx.module_enabled("webhook") || !contains_rollback(&report.details) {
        return Ok(());
    }
    update(ctx, |state| {
        state
            .events
            .push(Event::new(Kind::Rollback, Some(report.outcome), None));
    })
}

fn contains_rollback(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => matches!(
            text.as_str(),
            "rolled_back" | "rollback_failed" | "rolled-back" | "rollback-failed"
        ),
        serde_json::Value::Array(values) => values.iter().any(contains_rollback),
        serde_json::Value::Object(fields) => fields.values().any(contains_rollback),
        _ => false,
    }
}

pub fn tick(ctx: &Context<'_>) -> AppResult<()> {
    update(ctx, |_| {})
}

fn update(ctx: &Context<'_>, enqueue: impl FnOnce(&mut State)) -> AppResult<()> {
    if ctx.dry_run || (!ctx.module_enabled("webhook") && !ctx.module_enabled("heartbeat")) {
        return Ok(());
    }

    if let Some(runtime) = ctx.paths.lock.parent() {
        store::check_protected_parent(runtime)?;
        std::fs::create_dir_all(runtime)?;
    }
    let path = ctx.paths.observation("notifications-v1.json");
    let _guard = store::open_lock(&path.with_extension("lock"))?;
    let mut state = status(ctx)?;
    enqueue(&mut state);
    let now = Utc::now();
    if ctx.module_enabled("webhook") {
        if host::reboot_status(ctx).needed {
            let observed = *state.reboot_observed_at.get_or_insert(now);
            if !state.stale_queued
                && now - observed
                    >= TimeDelta::days(i64::from(ctx.config.notifications.stale_reboot_days))
            {
                state.events.push(Event::new(Kind::StaleReboot, None, None));
                state.stale_queued = true;
            }
        } else {
            state.reboot_observed_at = None;
            state.stale_queued = false;
            state
                .events
                .retain(|event| event.kind != Kind::StaleReboot || event.delivered);
        }
    }

    if state.events.len() > 64 {
        state.events.drain(..state.events.len() - 64);
    }
    store::write_json(&path, &state)?;
    for event in state
        .events
        .iter_mut()
        .rev()
        .filter(|event| {
            !event.delivered
                && event.attempts < 3
                && event
                    .last_attempt
                    .is_none_or(|last| now - last >= TimeDelta::minutes(15))
        })
        .take(3)
    {
        let config = &ctx.config.notifications;
        let (enabled, credential) = if event.kind == Kind::Heartbeat {
            (ctx.module_enabled("heartbeat"), &config.heartbeat_url_file)
        } else {
            (ctx.module_enabled("webhook"), &config.webhook_url_file)
        };
        if !enabled {
            continue;
        }
        if let Some(credential) = credential {
            event.attempts += 1;
            event.last_attempt = Some(now);
            event.delivered = send(event, credential).is_ok();
        }
    }
    store::write_json(&path, &state)
}

fn send(event: &Event, credential: &Path) -> AppResult<()> {
    store::trusted_file(credential)?;
    let metadata = std::fs::metadata(credential)?;
    if metadata.mode() & 0o077 != 0 || metadata.size() > 4096 {
        return Err(Error::invalid(
            "Notification credential must be a small private file",
        ));
    }
    let raw = std::fs::read_to_string(credential)?;
    let url = raw.trim();
    if !url.starts_with("https://")
        || url
            .bytes()
            .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'"' | b'\\'))
    {
        return Err(Error::invalid("Notification endpoint must be an HTTPS URL"));
    }

    let mut config = tempfile::NamedTempFile::new_in("/run/foreman")?;
    writeln!(config, "url = \"{url}\"")?;
    let config_path = config
        .path()
        .to_str()
        .ok_or_else(|| Error::failed("Invalid temporary path"))?;
    let mut args = vec![
        "--disable",
        "--config",
        config_path,
        "--silent",
        "--fail",
        "--max-time",
        "8",
        "--proto",
        "=https",
        "--output",
        "/dev/null",
        "--write-out",
        "%{http_code}",
    ];
    let data = serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "kind": event.kind,
        "outcome": event.outcome,
        "schedule": event.schedule,
        "at": event.at,
    }))?;
    if event.kind != Kind::Heartbeat {
        args.extend([
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            &data,
        ]);
    }
    let response = process::run("curl", &args, Duration::from_secs(10))?;
    let status: u16 = response.stdout.trim().parse().unwrap_or_default();
    if !response.success || !(200..300).contains(&status) {
        return Err(Error::failed("Notification delivery failed"));
    }
    Ok(())
}
