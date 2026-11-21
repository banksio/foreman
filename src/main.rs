mod config;
mod cycle;
mod docker;
mod host;
mod model;
mod notify;
mod process;
mod providers;
mod schedule;
mod store;
mod systemd;

use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Args, ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use model::RebootMode;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub use config::Config;
pub use model::{AppResult, Error, Outcome, Report, Step};
pub use store::Paths;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD));

/// Local maintenance for Debian-family hosts.
#[derive(Debug, Parser)]
#[command(version, arg_required_else_help = true, styles = STYLES)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Produce versioned JSON for supported commands"
    )]
    json: bool,
    #[arg(
        long,
        short,
        global = true,
        help = "Show provider and operation details"
    )]
    verbose: bool,
    #[arg(long, global = true, value_enum, default_value = "auto")]
    color: Colors,
    #[arg(long, global = true, help = "Suppress activity indicators")]
    no_progress: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum Colors {
    #[default]
    Auto,
    Always,
    Never,
}

impl Colors {
    fn enabled(self, interactive: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => {
                if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
                    return false;
                }
                if let Some(force) = std::env::var_os("FORCE_COLOR")
                    && !force.is_empty()
                    && force != "0"
                {
                    return true;
                }
                interactive && std::env::var_os("TERM").is_none_or(|value| value != "dumb")
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a maintenance cycle or one phase.
    Run(Run),
    /// Manage pending maintenance.
    Cycle {
        #[command(subcommand)]
        command: CycleCommand,
    },
    /// Show local host and maintenance status.
    Status,
    /// List and manage recurring maintenance.
    Schedule {
        #[command(subcommand)]
        command: Option<schedule::Action>,
    },
    /// Discover and manage Docker Compose enrollments.
    Docker {
        #[command(subcommand)]
        command: DockerCommand,
    },
    /// List or configure bundled providers.
    Modules {
        #[command(subcommand)]
        command: Option<ModuleCommand>,
    },
    /// Show the installed version and update instructions.
    #[command(name = "self")]
    SelfManage {
        #[command(subcommand)]
        command: SelfCommand,
    },
    #[command(name = "__resume", hide = true)]
    Resume,
    #[command(name = "__scheduled", hide = true)]
    Scheduled { name: String },
    #[command(name = "__reconcile", hide = true)]
    Reconcile { name: String },
    #[command(name = "__reconcile-resume", hide = true)]
    ReconcileResume,
    #[command(name = "__notify", hide = true)]
    Notify,
    #[command(name = "apt-verify-plan", hide = true)]
    AptVerifyPlan { path: PathBuf },
    #[command(name = "__schemas", hide = true)]
    Schemas { directory: PathBuf },
}

#[derive(Args, Debug)]
struct Run {
    #[arg(value_enum, default_value = "cycle")]
    step: Step,
    #[arg(long, help = "Preview using read-only checks and simulations")]
    dry_run: bool,
    #[arg(
        long,
        conflicts_with = "reboot_now",
        help = "Suppress reboot for this invocation"
    )]
    no_reboot: bool,
    #[arg(long, help = "Ignore the reboot window after all other checks")]
    reboot_now: bool,
    #[arg(long, help = "Limit run compose to one enrolled stack")]
    stack: Option<String>,
}

#[derive(Debug, Subcommand)]
enum CycleCommand {
    /// Abort an inactive saved cycle and cancel its continuation.
    Abort,
}

#[derive(Debug, Subcommand)]
enum DockerCommand {
    /// List enrolled and discovered Compose projects.
    Stacks,
    /// Manage one Compose enrollment.
    Stack {
        #[command(subcommand)]
        command: docker::DockerAction,
    },
}

#[derive(Debug, Subcommand)]
enum ModuleCommand {
    /// List bundled modules with selection reasons.
    List,
    /// Enable an available module.
    Enable { id: String },
    /// Disable a module while preserving its settings.
    Disable { id: String },
}

#[derive(Debug, Subcommand)]
enum SelfCommand {
    /// Print the running Foreman version.
    Version,
    /// Show the package-managed update path.
    Update {
        #[arg(long)]
        check: bool,
    },
}

pub struct Context<'a> {
    pub config: &'a Config,
    pub paths: &'a Paths,
    pub dry_run: bool,
    pub verbose: bool,
    pub host: &'a host::Host,
    color: bool,
    progress: bool,
}

impl Context<'_> {
    pub fn module_enabled(&self, id: &str) -> bool {
        providers::active(self, id)
    }
}

fn main() -> ExitCode {
    if is_root() {
        // Set this before starting children or creating root-owned intent and observation directories.
        unsafe {
            libc::umask(0o022);
        }
    }
    let args: Vec<_> = std::env::args_os().collect();
    let colors = args
        .iter()
        .enumerate()
        .take_while(|(_, arg)| *arg != "--")
        .filter_map(|(index, arg)| {
            let arg = arg.to_str()?;
            let value = if arg == "--color" {
                args.get(index + 1)?.to_str()?
            } else {
                arg.strip_prefix("--color=")?
            };
            Colors::from_str(value, true).ok()
        })
        .last()
        .unwrap_or_default();
    let mut command = Cli::command().color(
        if matches!(colors, Colors::Always)
            || (matches!(colors, Colors::Auto)
                && colors.enabled(io::stdout().is_terminal() || io::stderr().is_terminal()))
        {
            ColorChoice::Always
        } else {
            ColorChoice::Never
        },
    );
    if args.len() == 1 {
        let _ = command.print_help();
        println!();
        return ExitCode::SUCCESS;
    }
    let matches = command
        .try_get_matches_from(args)
        .unwrap_or_else(|error| error.exit());
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let error_color = !cli.json && cli.color.enabled(io::stderr().is_terminal());
    match execute(cli) {
        Ok(outcome) => ExitCode::from(outcome.exit()),
        Err(error) => {
            eprintln!(
                "{} {}",
                colored(error.outcome, error_color),
                literal(&error.message)
            );
            ExitCode::from(error.outcome.exit())
        }
    }
}

fn execute(cli: Cli) -> AppResult<Outcome> {
    if cli.json
        && !matches!(
            &cli.command,
            Command::Status
                | Command::Schedule {
                    command: None | Some(schedule::Action::List)
                }
                | Command::Docker {
                    command: DockerCommand::Stacks
                }
                | Command::Modules {
                    command: None | Some(ModuleCommand::List)
                }
        )
    {
        return Err(Error::invalid(
            "--json is available for status, schedule list, docker stacks, and modules list",
        ));
    }

    match &cli.command {
        Command::SelfManage {
            command: SelfCommand::Version,
        } => {
            println!("foreman {VERSION}");
            return Ok(Outcome::Completed);
        }
        Command::SelfManage {
            command: SelfCommand::Update { .. },
        } => {
            return Err(Error::blocked(
                "Use install.sh with the desired release tag to upgrade the Debian package",
            ));
        }
        Command::AptVerifyPlan { path } => {
            host::verify_apt_plan(path)?;
            return Ok(Outcome::Completed);
        }
        Command::Schemas { directory } => {
            write_schemas(directory)?;
            return Ok(Outcome::Completed);
        }
        _ => {}
    }
    process::install_signal_handlers()?;
    let paths = Paths::default();
    if cfg!(target_os = "linux") {
        store::check_protected_parent(&paths.config_dir)?;
        store::check_protected_parent(&paths.state_dir)?;
    }
    let config = Config::load(&paths)?;
    let host = host::Host::detect();
    let is_preview = matches!(&cli.command, Command::Run(Run { dry_run: true, .. }));
    let ctx = Context {
        config: &config,
        paths: &paths,
        host: &host,
        dry_run: is_preview
            || (matches!(
                &cli.command,
                Command::Run(Run {
                    step: Step::Disk,
                    ..
                })
            ) && !is_root()),
        verbose: cli.verbose,
        color: !cli.json && cli.color.enabled(io::stdout().is_terminal()),
        progress: !cli.no_progress && !cli.json && io::stderr().is_terminal(),
    };

    let report = match cli.command {
        Command::Status => {
            let status = status(&ctx)?;
            if cli.json {
                print_json(&status)?;
            } else {
                print_status(&status, ctx.verbose)?;
            }
            return Ok(Outcome::Completed);
        }
        Command::Run(run) => {
            if run.stack.is_some() && run.step != Step::Compose {
                return Err(Error::invalid("--stack requires foreman run compose"));
            }
            let options = cycle::RebootOptions {
                no_reboot: run.no_reboot,
                reboot_now: run.reboot_now,
            };
            let _guard = if ctx.dry_run || run.step == Step::Disk {
                None
            } else {
                Some(admin_lock(&ctx)?)
            };
            cycle::run(
                &ctx,
                run.step,
                run.stack.as_deref(),
                options,
                None,
                config.reboot.mode,
            )?
        }
        Command::Cycle {
            command: CycleCommand::Abort,
        } => {
            let _guard = admin_lock(&ctx)?;
            cycle::abort(&ctx)?
        }
        Command::Schedule {
            command: None | Some(schedule::Action::List),
        } => {
            let schedules = schedule::list(&ctx)?;
            if cli.json {
                print_json(&ScheduleList {
                    schema_version: 1,
                    timezone: schedule::timezone(),
                    schedules: &schedules,
                })?;
            } else {
                print_schedules(&schedules);
            }
            return Ok(Outcome::Completed);
        }
        Command::Schedule {
            command: Some(schedule::Action::Show { name }),
        } => {
            let entry = schedule::entry(&ctx, &name)?;
            print_schedules(std::slice::from_ref(&entry));
            println!("Reboot policy  {:?}", entry.job.reboot);
            if let Some(attempt) = entry.last_attempt {
                println!("Last start  {}", attempt.started);
                if let Some(report) = attempt.result {
                    print_report(&ctx, &report, true);
                }
            }
            return Ok(Outcome::Completed);
        }
        Command::Schedule {
            command: Some(command),
        } => {
            let _guard = admin_lock(&ctx)?;
            schedule::command(&ctx, command)?
        }
        Command::Modules {
            command: None | Some(ModuleCommand::List),
        } => {
            let modules = providers::list(&ctx);
            if cli.json {
                print_json(&ModuleList {
                    schema_version: 1,
                    modules: &modules,
                })?;
            } else {
                for module in &modules {
                    println!("{}", module.line());
                }
            }
            return Ok(Outcome::Completed);
        }
        Command::Modules {
            command: Some(command),
        } => {
            let _guard = admin_lock(&ctx)?;
            cycle::reject_pending(&ctx)?;
            match command {
                ModuleCommand::Enable { id } => providers::change(&ctx, &id, true)?,
                ModuleCommand::Disable { id } => providers::change(&ctx, &id, false)?,
                ModuleCommand::List => unreachable!(),
            }
        }
        Command::Docker {
            command: DockerCommand::Stacks,
        } => {
            let stacks = docker::list(&ctx)?;
            if cli.json {
                print_json(&stacks)?;
            } else {
                print_docker(&stacks)?;
            }
            return Ok(Outcome::Completed);
        }
        Command::Docker {
            command: DockerCommand::Stack { command },
        } => {
            let _guard = if matches!(&command, docker::DockerAction::Show { .. }) {
                None
            } else {
                let guard = admin_lock(&ctx)?;
                cycle::reject_pending(&ctx)?;
                Some(guard)
            };
            docker::command(&ctx, command)?
        }
        Command::Resume => {
            let _guard = admin_lock(&ctx)?;
            cycle::resume(&ctx)?
        }
        Command::Scheduled { name } => scheduled(&ctx, name)?,
        Command::Reconcile { name } => {
            require_root()?;
            schedule::reconcile(&ctx, &name)?;
            return Ok(Outcome::Completed);
        }
        Command::ReconcileResume => {
            require_root()?;
            cycle::reconcile_resume(&ctx)?;
            return Ok(Outcome::Completed);
        }
        Command::Notify => {
            require_root()?;
            notify::tick(&ctx)?;
            return Ok(Outcome::Completed);
        }
        _ => unreachable!(),
    };

    print_report(&ctx, &report, true);
    Ok(report.outcome)
}

fn scheduled(ctx: &Context<'_>, name: String) -> AppResult<Report> {
    require_root()?;
    systemd::require(ctx)?;
    let job = schedule::load(ctx, &name)?;
    let mut attempt = schedule::attempt(name, &job);
    schedule::save_attempt(ctx, &attempt)?;

    let result = (|| -> AppResult<Report> {
        if !job.enabled {
            return Ok(Report::new(Outcome::Skipped, "Schedule is disabled"));
        }
        let _guard = if job.run == Step::Disk {
            None
        } else {
            Some(store::lock(&ctx.paths.lock)?)
        };
        // Re-read intent after taking the maintenance lock, where applicable.
        let current = schedule::load(ctx, &attempt.name)?;
        if !current.enabled
            || current.generation != job.generation
            || current.run != job.run
            || current.reboot != job.reboot
            || current.calendar != job.calendar
        {
            return Ok(Report::new(
                Outcome::Skipped,
                "Schedule changed before it could start",
            ));
        }
        cycle::run(
            ctx,
            job.run,
            None,
            cycle::RebootOptions::default(),
            Some(attempt.clone()),
            job.reboot.mode(ctx.config.reboot.mode),
        )
    })();
    let report = Report::from_result(result);
    if job.run != Step::Cycle
        || report.outcome == Outcome::Waiting
        || !cycle::scheduled_pending(ctx, &attempt)?
    {
        attempt.result = Some(report.clone());
        schedule::save_attempt(ctx, &attempt)?;
    }
    if job.run != Step::Cycle
        && let Err(error) = notify::scheduled_step(ctx, report.outcome, &attempt.name)
    {
        warn(&format!("Heartbeat state: {error}"));
    }
    Ok(report)
}

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn require_root() -> AppResult<()> {
    if is_root() {
        Ok(())
    } else {
        Err(Error::invalid(
            "This operation requires root. Use sudo or add --dry-run for a maintenance preview",
        ))
    }
}

fn admin_lock(ctx: &Context<'_>) -> AppResult<std::fs::File> {
    require_root()?;
    store::lock(&ctx.paths.lock)
}

#[derive(Serialize, JsonSchema)]
struct Status {
    schema_version: u32,
    generated_at: chrono::DateTime<chrono::Utc>,
    foreman_version: &'static str,
    host: host::Host,
    reboot: host::RebootStatus,
    reboot_window: String,
    reboot_mode: RebootMode,
    disk: Report,
    cycle: Option<cycle::Cycle>,
    last_steps: BTreeMap<String, Report>,
    providers: Vec<providers::ModuleStatus>,
    schedules: Vec<schedule::Entry>,
    docker: Option<docker::StackList>,
    notifications: notify::State,
    legacy_units: Vec<String>,
}

fn status(ctx: &Context<'_>) -> AppResult<Status> {
    Ok(Status {
        schema_version: 1,
        generated_at: chrono::Utc::now(),
        foreman_version: VERSION,
        host: ctx.host.clone(),
        reboot: host::reboot_status(ctx),
        reboot_window: ctx.config.reboot.window.clone(),
        reboot_mode: ctx.config.reboot.mode,
        disk: Report::from_result(host::disk_live(ctx)),
        cycle: cycle::read(ctx)?,
        last_steps: cycle::last_steps(ctx)?,
        providers: providers::list(ctx),
        schedules: schedule::list(ctx)?,
        docker: if ctx.module_enabled("docker") {
            Some(docker::list(ctx)?)
        } else {
            None
        },
        notifications: notify::status(ctx)?,
        legacy_units: systemd::legacy_units(ctx),
    })
}

#[derive(Serialize, JsonSchema)]
struct ScheduleList<'a> {
    schema_version: u32,
    timezone: String,
    schedules: &'a [schedule::Entry],
}

#[derive(Serialize, JsonSchema)]
struct ModuleList<'a> {
    schema_version: u32,
    modules: &'a [providers::ModuleStatus],
}

fn print_json(data: &impl Serialize) -> AppResult<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, data)?;
    writeln!(out)?;
    Ok(())
}

fn print_status(status: &Status, verbose: bool) -> AppResult<()> {
    println!(
        "{}  {}  {}",
        literal(&status.host.hostname),
        status.host.profile,
        literal(&status.host.os)
    );
    println!("Kernel  {}", literal(&status.host.kernel));
    if let Some(bootloader) = &status.host.bootloader {
        println!("Bootloader  {}", literal(bootloader));
    }
    let reason = if status.reboot.needed {
        &status.reboot.reason
    } else {
        "No current requirement"
    };
    println!("Reboot  {} ({:?})", literal(reason), status.reboot_mode);
    if !status.reboot_window.is_empty() {
        println!("Reboot window  {}", status.reboot_window);
    }
    if let Some(cycle) = &status.cycle {
        println!("{}", cycle.line());
    }
    if !status.legacy_units.is_empty() {
        warn(&format!("Legacy units: {}", status.legacy_units.join(", ")));
    }
    if !status.last_steps.is_empty() {
        println!();
        for (step, report) in &status.last_steps {
            println!(
                "{step:10} {:10} {}",
                report.outcome.label(),
                literal(&report.summary)
            );
            if verbose {
                for warning in &report.warnings {
                    warn(warning);
                }
            }
        }
    }
    for warning in &status.disk.warnings {
        warn(warning);
    }
    print_disk(&status.disk.details);
    if let Some(disk) = status.last_steps.get("disk") {
        print_consumers(&disk.details);
    }
    if let Some(since) = status.notifications.reboot_observed_at() {
        println!("Reboot first observed  {since}");
    }
    if let Some(deliveries) = status.notifications.last_delivery() {
        println!("Notifications  {deliveries}");
    }
    if !status.schedules.is_empty() {
        println!();
        print_schedules(&status.schedules);
    }
    if let Some(docker) = &status.docker {
        println!();
        print_docker(docker)?;
    }
    if verbose {
        println!();
        for module in &status.providers {
            println!("{}", module.line());
        }
    }
    Ok(())
}

fn print_schedules(schedules: &[schedule::Entry]) {
    if schedules.is_empty() {
        println!("No maintenance schedules");
    }
    for entry in schedules {
        let last = entry.last_attempt.as_ref().map_or("-", |attempt| {
            attempt
                .result
                .as_ref()
                .map_or("running", |result| result.outcome.label())
        });
        println!(
            "{:20} {:8} {:9} {}  next {}  last {}",
            entry.name,
            entry.job.run.name(),
            if entry.job.enabled {
                "enabled"
            } else {
                "paused"
            },
            literal(&entry.job.calendar),
            literal(entry.next_run.as_deref().unwrap_or("-")),
            last
        );
    }
}

fn print_docker(stacks: &docker::StackList) -> AppResult<()> {
    let stacks = serde_json::to_value(stacks)?;
    if let Some(error) = stacks["discovery_error"].as_str() {
        warn(error);
    }
    let Some(stacks) = stacks["stacks"].as_array().filter(|rows| !rows.is_empty()) else {
        println!("No Compose projects");
        return Ok(());
    };
    for row in stacks {
        let field = |key: &str| literal(row[key].as_str().unwrap_or("-"));
        println!(
            "{:20} {:20} {:12} {}",
            field("id"),
            field("eligibility"),
            if row["enrolled"] == true {
                "enrolled"
            } else {
                "discovered"
            },
            field("reason"),
        );
        if let Some(workdir) = row["workdir"].as_str() {
            println!("  {}", literal(workdir));
        }
        if let Some(command) = row["enroll_command"].as_array() {
            let parts: Vec<_> = command
                .iter()
                .filter_map(|part| part.as_str())
                .map(shell_word)
                .collect();
            if !parts.is_empty() {
                println!("  {}", parts.join(" "));
            }
        }
        if let Some(result) = row["last_result"]["outcome"].as_str() {
            println!("  Last result  {}", literal(result));
        }
    }
    Ok(())
}

fn shell_word(word: &str) -> String {
    let word = literal(word);
    if word
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
    {
        word
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn print_disk(details: &serde_json::Value) {
    if let Some(filesystems) = details["filesystems"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        println!("Filesystem                  Used    Free GiB    Inodes");
        for row in filesystems {
            let path = literal(row["path"].as_str().unwrap_or("-"));
            let used = row["used_percent"].as_u64().unwrap_or_default();
            let free = row["free_bytes"].as_u64().unwrap_or_default() as f64 / 1_073_741_824.0;
            let inodes = row["inode_used_percent"]
                .as_u64()
                .map_or_else(|| "-".into(), |value| format!("{value}%"));
            println!("{path:24} {used:>6}% {free:>11.1} {inodes:>9}");
        }
    }
}

fn print_consumers(details: &serde_json::Value) {
    if let Some(usage) = details["consumers"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        println!("Directory size");
        for row in usage {
            let path = literal(row["path"].as_str().unwrap_or("-"));
            let size = row["bytes"].as_u64().unwrap_or_default() as f64 / 1_073_741_824.0;
            println!("{path:24} {size:>8.2} GiB");
        }
    }
}

pub fn warn(message: &str) {
    eprintln!("warning  {}", literal(message));
}

pub fn phase_result(ctx: &Context<'_>, phase: &str, report: &Report) {
    println!(
        "{phase:10} {:10} {}",
        colored(report.outcome, ctx.color),
        literal(&report.summary)
    );
    for warning in &report.warnings {
        warn(warning);
    }
    if ctx.verbose || ctx.dry_run || phase == "disk" {
        print_detail(&report.details);
    }
}

fn print_report(ctx: &Context<'_>, report: &Report, detail: bool) {
    println!(
        "{:10} {}",
        colored(report.outcome, ctx.color),
        literal(&report.summary)
    );
    for warning in &report.warnings {
        warn(warning);
    }
    if detail {
        print_detail(&report.details);
    }
}

fn print_detail(details: &serde_json::Value) {
    if details.get("filesystems").is_some() {
        print_disk(details);
        print_consumers(details);
        return;
    }
    match details {
        serde_json::Value::Null => {}
        serde_json::Value::Array(rows) if rows.iter().all(|row| row.is_string()) => {
            for row in rows {
                println!("  {}", literal(row.as_str().unwrap_or_default()));
            }
        }
        _ => {
            if let Ok(text) = serde_json::to_string_pretty(details) {
                println!("{}", literal(&text));
            }
        }
    }
}

fn colored(outcome: Outcome, enabled: bool) -> String {
    let label = outcome.label();
    if !enabled {
        return label.to_owned();
    }
    let color = match outcome {
        Outcome::Completed | Outcome::Unchanged => "32",
        Outcome::Skipped | Outcome::Busy => "90",
        Outcome::Blocked | Outcome::Waiting => "33",
        Outcome::Failed | Outcome::Invalid => "31",
    };
    format!("\x1b[{color}m{label}\x1b[0m")
}

fn literal(text: &str) -> String {
    text.chars()
        .filter(|character| {
            *character == '\n'
                || *character == '\t'
                || (!character.is_control()
                    && !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        })
        .collect()
}

pub fn emit_provider_event(ctx: &Context<'_>, event: providers::Event) {
    match event {
        providers::Event::PhaseStarted { module, phase } if ctx.progress || ctx.verbose => {
            eprintln!("Running {} with {module}", phase.name());
        }
        providers::Event::PhaseFinished {
            module,
            phase,
            report,
        } if ctx.verbose => {
            eprintln!(
                "{module} finished {} ({})",
                phase.name(),
                report.outcome.label()
            );
        }
        _ => {}
    }
}

fn write_schemas(directory: &Path) -> AppResult<()> {
    for (file, schema) in [
        ("status-v1.json", schemars::schema_for!(Status)),
        (
            "schedule-list-v1.json",
            schemars::schema_for!(ScheduleList<'_>),
        ),
        (
            "modules-list-v1.json",
            schemars::schema_for!(ModuleList<'_>),
        ),
        (
            "docker-stacks-v1.json",
            schemars::schema_for!(docker::StackList),
        ),
        (
            "provider-request.json",
            schemars::schema_for!(providers::Request),
        ),
        (
            "provider-event.json",
            schemars::schema_for!(providers::Event),
        ),
    ] {
        store::write_json(&directory.join(file), &schema)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicting_reboot_flags_are_rejected() {
        assert!(Cli::try_parse_from(["foreman", "run", "--no-reboot", "--reboot-now"]).is_err());
    }

    #[test]
    fn terminal_controls_do_not_escape_the_renderer() {
        assert_eq!(literal("x\x1b]52;y\x07\u{202e}z"), "x]52;yz");
    }
}
