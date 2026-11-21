use crate::{AppResult, Context, Error, process, store};
use chrono::NaiveTime;
use std::path::Path;
use std::time::Duration;

pub const MANAGED: &str = "# Managed by Foreman";
pub const RUNNER: &str = "/usr/bin/foreman";
pub const RESUME: &str = "foreman-resume.service";
pub const WINDOW: &str = "foreman-window.timer";

pub fn systemctl(args: &[&str]) -> AppResult<process::Output> {
    process::run("systemctl", args, Duration::from_secs(45))
}

pub fn checked(args: &[&str]) -> AppResult<()> {
    let output = systemctl(args)?;
    if !output.success {
        let reason = output
            .stderr
            .lines()
            .last()
            .unwrap_or("systemctl did not report a reason");
        let status = output
            .code
            .map_or_else(|| "terminated".into(), |code| format!("exit {code}"));
        return Err(Error::failed(format!(
            "systemctl {} failed ({status}): {reason}",
            args.join(" ")
        )));
    }
    Ok(())
}

pub fn require(ctx: &Context<'_>) -> AppResult<()> {
    if ctx.host.systemd && ctx.module_enabled("systemd") {
        Ok(())
    } else {
        Err(Error::blocked("No active systemd provider on this host"))
    }
}

pub fn require_legacy_resolution(ctx: &Context<'_>) -> AppResult<()> {
    let units = legacy_units(ctx);
    if units.is_empty() {
        return Ok(());
    }
    Err(Error::blocked(format!(
        "Resolve legacy Foreman units first: {}",
        units.join(", ")
    )))
}

pub fn legacy_units(ctx: &Context<'_>) -> Vec<String> {
    let mut legacy = Vec::new();
    if ctx.paths.units.join(RESUME).exists() {
        legacy.push(RESUME.into());
    }

    let window = ctx.paths.units.join(WINDOW);
    if !is_managed(&window)
        && (window.exists()
            || systemctl(&["is-active", "--quiet", WINDOW]).is_ok_and(|output| output.success))
    {
        legacy.push(WINDOW.into());
    }

    if ctx.host.systemd {
        let timers = systemctl(&[
            "list-unit-files",
            "--type=timer",
            "--no-legend",
            "--no-pager",
            "*foreman*",
        ]);
        if let Ok(output) = timers {
            for line in output.stdout.lines() {
                let mut fields = line.split_whitespace();
                let Some(unit) = fields.next() else { continue };
                if !unit.starts_with("foreman-job-")
                    && !matches!(unit, WINDOW | "foreman-notify.timer")
                    && (fields
                        .next()
                        .is_some_and(|state| state.starts_with("enabled"))
                        || systemctl(&["is-active", "--quiet", unit])
                            .is_ok_and(|output| output.success))
                {
                    legacy.push(unit.into());
                }
            }
        }
    }
    legacy
}

pub fn is_managed(path: &Path) -> bool {
    store::read(path)
        .ok()
        .flatten()
        .is_some_and(|data| data.lines().next() == Some(MANAGED))
}

pub fn check_owned(path: &Path) -> AppResult<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || !is_managed(path) => Err(Error::blocked(format!(
            "Refusing to replace unowned unit {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn install_resume(ctx: &Context<'_>) -> AppResult<()> {
    require(ctx)?;
    require_legacy_resolution(ctx)?;
    checked(&["enable", RESUME])
}

pub fn arm_window(ctx: &Context<'_>, at: NaiveTime) -> AppResult<()> {
    install_resume(ctx)?;
    arm_continuation(ctx, &format!("OnCalendar=*-*-* {}", at.format("%H:%M:%S")))
}

pub fn arm_delay(ctx: &Context<'_>, seconds: i64) -> AppResult<()> {
    arm_continuation(ctx, &format!("OnActiveSec={seconds}s"))
}

fn arm_continuation(ctx: &Context<'_>, trigger: &str) -> AppResult<()> {
    let path = ctx.paths.units.join(WINDOW);
    check_owned(&path)?;
    let unit = format!(
        "{MANAGED}\n[Unit]\nDescription=Resume saved Foreman maintenance\n\n[Timer]\n{trigger}\nAccuracySec=1s\nUnit={RESUME}\n\n[Install]\nWantedBy=timers.target\n"
    );
    store::atomic_write(&path, unit.as_bytes(), 0o644)?;
    checked(&["daemon-reload"])?;
    checked(&["enable", WINDOW])?;
    checked(&["restart", WINDOW])
}

pub fn clear_window(ctx: &Context<'_>) -> AppResult<()> {
    let path = ctx.paths.units.join(WINDOW);
    if !is_managed(&path) {
        return Ok(());
    }
    if ctx.host.systemd {
        // A timer can stop while its already-started service completes.
        checked(&["disable", "--now", WINDOW])?;
    }
    store::remove(&path)?;
    if ctx.host.systemd {
        checked(&["daemon-reload"])?;
    }
    Ok(())
}
