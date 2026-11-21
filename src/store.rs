use crate::{AppResult, Error};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub lock: PathBuf,
    pub units: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            config_dir: "/etc/foreman".into(),
            state_dir: "/var/lib/foreman".into(),
            lock: "/run/foreman/maintenance.lock".into(),
            units: "/etc/systemd/system".into(),
        }
    }
}

impl Paths {
    pub fn observation(&self, filename: &str) -> PathBuf {
        self.state_dir.join(filename)
    }
}

pub fn read(path: &Path) -> AppResult<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::failed(format!("Read {}: {error}", path.display()))),
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> AppResult<Option<T>> {
    read(path)?
        .map(|data| {
            serde_json::from_str(&data).map_err(|error| {
                Error::invalid(format!("Invalid state in {}: {error}", path.display()))
            })
        })
        .transpose()
}

pub fn write_json(path: &Path, value: &impl Serialize) -> AppResult<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, 0o644)
}

pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("Path has no parent directory"))?;
    check_protected_parent(parent)?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|error| Error::failed(format!("Replace {}: {error}", path.display())))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn remove(path: &Path) -> AppResult<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn lock(path: &Path) -> AppResult<File> {
    lock_file(path, true)
}

pub fn open_lock(path: &Path) -> AppResult<File> {
    lock_file(path, false)
}

fn lock_file(path: &Path, no_wait: bool) -> AppResult<File> {
    if let Some(parent) = path.parent() {
        check_protected_parent(parent)?;
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if no_wait {
        FileExt::try_lock_exclusive(&file).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                Error::busy("Another Foreman operation owns the maintenance lock")
            } else {
                error.into()
            }
        })?;
    } else {
        FileExt::lock_exclusive(&file)?;
    }
    Ok(file)
}

pub fn trusted_file(path: &Path) -> AppResult<()> {
    use std::os::unix::fs::MetadataExt;

    if !path.is_absolute() {
        return Err(Error::invalid(format!(
            "Expected an absolute path: {}",
            path.display()
        )));
    }
    for component in path.ancestors() {
        let metadata = fs::symlink_metadata(component).map_err(|error| {
            Error::invalid(format!(
                "Inspect protected path {}: {error}",
                component.display()
            ))
        })?;
        if metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || (if component == path {
                !metadata.is_file()
            } else {
                !metadata.is_dir()
            })
        {
            return Err(Error::invalid(format!(
                "Untrusted path {}",
                component.display()
            )));
        }
    }
    Ok(())
}

pub fn check_protected_parent(path: &Path) -> AppResult<()> {
    use std::os::unix::fs::MetadataExt;

    if !crate::is_root()
        || ![
            "/etc/foreman",
            "/etc/systemd/system",
            "/var/lib/foreman",
            "/run/foreman",
        ]
        .into_iter()
        .any(|base| path.starts_with(base))
    {
        return Ok(());
    }
    for component in path.ancestors() {
        let metadata = match fs::symlink_metadata(component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Error::blocked(format!(
                "Untrusted storage directory {}",
                component.display()
            )));
        }
    }
    Ok(())
}
