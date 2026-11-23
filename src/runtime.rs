//! Small cross-process coordination files under vdl's runtime directory.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub dir: PathBuf,
    pub busy: PathBuf,
    pub owner_lock: PathBuf,
    pub mutation_lock: PathBuf,
    pub download_lock: PathBuf,
}

impl RuntimePaths {
    pub fn resolve() -> io::Result<Self> {
        let uid = unsafe { libc::getuid() };
        let mut candidates = Vec::new();
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR")
            && !xdg.is_empty()
        {
            candidates.push(PathBuf::from(xdg).join("vdl"));
        }
        candidates.push(PathBuf::from(format!("/tmp/vdl-{uid}")));

        for candidate in candidates {
            if fs::create_dir_all(&candidate).is_err() {
                continue;
            }
            let Ok(meta) = fs::metadata(&candidate) else {
                continue;
            };
            if meta.uid() != uid {
                continue;
            }
            if fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).is_err() {
                continue;
            }
            return Ok(Self::from_dir(candidate));
        }
        Err(io::Error::other("could not create vdl runtime directory"))
    }

    pub fn from_dir(dir: PathBuf) -> Self {
        Self {
            busy: dir.join("busy"),
            owner_lock: dir.join("owner.lock"),
            mutation_lock: dir.join("mutation.lock"),
            download_lock: dir.join("download.lock"),
            dir,
        }
    }
}

pub struct FileLock {
    file: File,
}

impl FileLock {
    pub fn acquire(path: &Path, blocking: bool) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let op = if blocking {
            libc::LOCK_EX
        } else {
            libc::LOCK_EX | libc::LOCK_NB
        };
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

pub fn lock_is_held(path: &Path) -> bool {
    match FileLock::acquire(path, false) {
        Ok(_guard) => false,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

pub fn set_busy(paths: &RuntimePaths, label: Option<&str>) -> io::Result<()> {
    match label {
        None => match fs::remove_file(&paths.busy) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
        Some(text) => {
            fs::create_dir_all(&paths.dir)?;
            let tmp = paths.dir.join(format!(".busy-{}", std::process::id()));
            fs::write(&tmp, format!("{text}\n"))?;
            fs::rename(&tmp, &paths.busy)?;
            Ok(())
        }
    }
}

pub fn read_busy(paths: &RuntimePaths) -> Option<String> {
    let text = fs::read_to_string(&paths.busy).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn current_download(paths: &RuntimePaths) -> Option<String> {
    if lock_is_held(&paths.download_lock) {
        read_busy(paths)
    } else {
        None
    }
}
