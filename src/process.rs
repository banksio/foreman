use crate::{AppResult, Error};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin:/usr/local/sbin:/usr/local/bin";
const STREAM_LIMIT: usize = 8 * 1024 * 1024;
static INTERRUPTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
}

pub fn install_signal_handlers() -> AppResult<()> {
    let interrupted = INTERRUPTED.get_or_init(|| Arc::new(AtomicBool::new(false)));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, interrupted.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone())?;
    Ok(())
}

pub fn interrupted() -> bool {
    INTERRUPTED
        .get()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
}

pub fn available(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    PATH.split(':').any(|directory| {
        let path = Path::new(directory).join(program);
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

pub fn run(program: &str, args: &[&str], timeout: Duration) -> AppResult<Output> {
    run_env(program, args, timeout, &[])
}

pub fn run_env(
    program: &str,
    args: &[&str],
    timeout: Duration,
    env: &[(&str, &str)],
) -> AppResult<Output> {
    run_command(program, args, timeout, env, None)
}

pub fn apt_with_progress(
    args: &[&str],
    timeout: Duration,
    env: &[(&str, &str)],
    label: &str,
) -> AppResult<Output> {
    let mut args = args.to_vec();
    args.extend(["-o", "APT::Status-Fd=3"]);
    run_command("apt-get", &args, timeout, env, Some(label))
}

fn run_command(
    program: &str,
    args: &[&str],
    timeout: Duration,
    env: &[(&str, &str)],
    progress: Option<&str>,
) -> AppResult<Output> {
    if interrupted() {
        return Err(Error::failed("Foreman was interrupted"));
    }

    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .env("HOME", "/nonexistent")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut writer = None;
    let mut progress = if let Some(label) = progress {
        let (input, output) = UnixStream::pair()?;
        input.set_nonblocking(true)?;
        let fd = output.as_raw_fd();
        // Only async-signal-safe operations run after fork and before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        writer = Some(output);
        Some(AptProgress {
            input,
            label,
            line: Vec::new(),
            dropping: false,
            last: None,
        })
    } else {
        None
    };
    let child = command.spawn();
    drop(writer);
    let mut child = match child {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Output {
                stdout: String::new(),
                stderr: format!("{program} is unavailable"),
                success: false,
                code: Some(127),
            });
        }
        Err(error) => return Err(Error::failed(format!("Start {program}: {error}"))),
    };

    let group = -(child.id() as libc::pid_t);
    let mut monitor = || -> AppResult<Output> {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::failed("Missing command stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::failed("Missing command stderr"))?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut overflow = false;
        let start = Instant::now();
        let mut stop = None;
        let package_transaction = program.ends_with("dpkg")
            || (program.ends_with("apt-get")
                && !args.iter().any(|arg| matches!(*arg, "-s" | "--simulate"))
                && args.iter().any(|arg| {
                    matches!(
                        *arg,
                        "full-upgrade" | "dist-upgrade" | "purge" | "install" | "upgrade"
                    )
                }));

        loop {
            if let Some(progress) = &mut progress {
                progress.read_available();
            }
            overflow |= drain(&mut stdout, &mut out)? | drain(&mut stderr, &mut err)?;
            if let Some(status) = child.try_wait()? {
                if let Some(progress) = &mut progress {
                    progress.read_available();
                }
                overflow |= drain(&mut stdout, &mut out)? | drain(&mut stderr, &mut err)?;
                if interrupted() || stop.is_some() {
                    return Err(Error::failed(format!(
                        "{program} was interrupted or timed out"
                    )));
                }
                if overflow {
                    return Err(Error::failed(format!(
                        "{program} exceeded its output limit"
                    )));
                }
                if status.success()
                    && let Some(progress) = &mut progress
                    && let Some((stage, _)) = progress.last
                {
                    progress.render(stage, 100);
                }
                return Ok(Output {
                    stdout: String::from_utf8_lossy(&out).into_owned(),
                    stderr: String::from_utf8_lossy(&err).into_owned(),
                    success: status.success(),
                    code: status.code(),
                });
            }

            let should_stop =
                interrupted() || start.elapsed() >= timeout || (overflow && !package_transaction);
            if should_stop && stop.is_none() {
                let grace = if package_transaction {
                    Duration::from_secs(120)
                } else {
                    unsafe {
                        libc::kill(group, libc::SIGTERM);
                    }
                    Duration::from_secs(2)
                };
                stop = Some(Instant::now() + grace);
            }
            if stop.is_some_and(|until| Instant::now() >= until) {
                return Err(Error::failed(format!(
                    "{program} exceeded its deadline or interruption grace period"
                )));
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    };

    match monitor() {
        Ok(output) => Ok(output),
        Err(error) => {
            // A separate process group keeps the supervisor in charge of child termination.
            unsafe {
                libc::kill(group, libc::SIGKILL);
            }
            let _ = child.wait();
            Err(error)
        }
    }
}

fn nonblocking(pipe: &impl AsRawFd) -> AppResult<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn drain(pipe: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 32768];
    let mut overflow = false;
    for _ in 0..32 {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(STREAM_LIMIT.saturating_sub(output.len()));
                output.extend_from_slice(&buffer[..keep]);
                overflow |= keep != count;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(overflow)
}

struct AptProgress<'a> {
    input: UnixStream,
    label: &'a str,
    line: Vec<u8>,
    dropping: bool,
    last: Option<(&'static str, u8)>,
}

impl AptProgress<'_> {
    fn read_available(&mut self) {
        let mut chunk = [0; 4096];
        for _ in 0..16 {
            let Ok(count) = self.input.read(&mut chunk) else {
                break;
            };
            if count == 0 {
                break;
            }
            for &byte in &chunk[..count] {
                if byte == b'\n' {
                    if !self.dropping
                        && let Some((stage, percent)) = apt_position(&self.line)
                    {
                        self.render(stage, percent);
                    }
                    self.line.clear();
                    self.dropping = false;
                } else if self.line.len() < 4096 {
                    self.line.push(byte);
                } else {
                    self.dropping = true;
                }
            }
        }
    }

    fn render(&mut self, stage: &'static str, percent: u8) {
        if self.last == Some((stage, percent)) {
            return;
        }
        self.last = Some((stage, percent));
        let mut stderr = io::stderr().lock();
        let filled = usize::from(percent / 5);
        let _ = write!(
            stderr,
            "\r{} {stage:<11} [{}{}] {percent:>3}%",
            self.label,
            "#".repeat(filled),
            "-".repeat(20 - filled)
        );
        let _ = stderr.flush();
    }
}

impl Drop for AptProgress<'_> {
    fn drop(&mut self) {
        if self.last.is_some() {
            let _ = writeln!(io::stderr().lock());
        }
    }
}

fn apt_position(line: &[u8]) -> Option<(&'static str, u8)> {
    let mut fields = std::str::from_utf8(line).ok()?.split(':');
    let stage = match fields.next()? {
        "dlstatus" => "downloading",
        "pmstatus" => "applying",
        _ => return None,
    };
    fields.next()?;
    let field = fields.next()?;
    let percent: f64 = field.parse().ok().or_else(|| fields.next()?.parse().ok())?;
    (0.0..=100.0)
        .contains(&percent)
        .then(|| (stage, percent.round() as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_apt_download_and_arch_qualified_dpkg_positions() {
        assert_eq!(
            apt_position(b"dlstatus:1:33.25:Fetching"),
            Some(("downloading", 33))
        );
        assert_eq!(
            apt_position(b"pmstatus:base-files:arm64:74.5:Preparing"),
            Some(("applying", 75))
        );
        assert_eq!(apt_position(b"pmerror:1:12.0:failed"), None);
        assert_eq!(apt_position(b"pmstatus:1:NaN:invalid"), None);
    }
}
