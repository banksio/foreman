use crate::{AppResult, Error};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
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
    if interrupted() {
        return Err(Error::failed("Foreman was interrupted"));
    }

    let child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .env("HOME", "/nonexistent")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
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
            overflow |= drain(&mut stdout, &mut out)? | drain(&mut stderr, &mut err)?;
            if let Some(status) = child.try_wait()? {
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
