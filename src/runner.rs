//! Run the configured yt-dlp command for one source and keep a ring buffer of log lines.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::config::{Config, DEFAULT_LOG_FILE, expand_path};
use crate::repository::{LAST_CHECK_FILE, Source};

const RING_CAPACITY: usize = 500;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB per file (1 backup -> <= 10 MiB total)

pub struct AppLogger {
    path: PathBuf,
    max_bytes: u64,
    ring: Mutex<VecDeque<String>>,
}

impl AppLogger {
    pub fn new(log_file: Option<&Path>) -> Self {
        let path = log_file
            .map(expand_path)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_FILE));
        Self {
            path,
            max_bytes: MAX_LOG_BYTES,
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
        }
    }

    #[cfg(test)]
    pub fn with_max_bytes(log_file: &Path, max_bytes: u64) -> Self {
        Self {
            path: expand_path(log_file),
            max_bytes,
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
        }
    }

    pub fn info(&self, message: &str) {
        self.write_line("INFO", message);
    }

    pub fn error(&self, message: &str) {
        self.write_line("ERROR", message);
    }

    pub fn recent(&self, limit: usize) -> Vec<String> {
        let guard = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let skip = guard.len().saturating_sub(limit);
        guard.iter().skip(skip).cloned().collect()
    }

    fn rotate_if_needed(&self) {
        if let Ok(meta) = fs::metadata(&self.path)
            && meta.len() >= self.max_bytes
        {
            let mut backup = self.path.as_os_str().to_os_string();
            backup.push(".1");
            let _ = fs::rename(&self.path, PathBuf::from(backup));
        }
    }

    fn write_line(&self, level: &str, message: &str) {
        let ts = format_timestamp();
        let formatted = format!("{ts} {level} {message}");
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        if ring.len() >= RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(formatted.clone());

        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        self.rotate_if_needed();
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{formatted}");
        }
    }
}

fn format_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

pub struct YtDlpRunner {
    command: Vec<String>,
    logger: Arc<AppLogger>,
}

impl YtDlpRunner {
    pub fn new(config: &Config, logger: Arc<AppLogger>) -> Result<Self, io::Error> {
        if config.yt_dlp_command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "yt_dlp_command must not be empty",
            ));
        }
        Ok(Self {
            command: config.yt_dlp_command.clone(),
            logger,
        })
    }

    #[cfg(test)]
    pub fn from_command(command: Vec<String>, logger: Arc<AppLogger>) -> Result<Self, io::Error> {
        if command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "yt_dlp_command must not be empty",
            ));
        }
        Ok(Self { command, logger })
    }

    pub async fn run(&self, source: &Source) -> io::Result<i32> {
        let source_path = &source.path;
        let marker = source_path.join(LAST_CHECK_FILE);
        touch_file(&marker)?;

        let mut argv = self.command.clone();
        argv.push(source.url.clone());
        self.logger.info(&format!(
            "yt-dlp started source={} command={argv:?}",
            source_path.display()
        ));

        let (read_pipe, write_pipe) = create_pipe()?;
        let write_pipe_err = write_pipe.try_clone()?;

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(source_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(write_pipe))
            .stderr(Stdio::from(write_pipe_err));

        // SAFETY: `pre_exec` runs in the forked child before `exec` and only calls `setsid`.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                self.logger.error(&format!(
                    "yt-dlp launch failed source={}: {err}",
                    source_path.display()
                ));
                return Err(err);
            }
        };

        drop(cmd);

        let mut reader = tokio::fs::File::from_std(read_pipe);
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending = Vec::<u8>::new();

        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                        let line_bytes: Vec<u8> = pending.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
                        let line = line.trim_end_matches('\r');
                        self.logger.info(&format!(
                            "yt-dlp output source={} {line}",
                            source_path.display()
                        ));
                    }
                    if pending.len() > 1024 * 1024 {
                        let chunk = String::from_utf8_lossy(&pending);
                        self.logger.info(&format!(
                            "yt-dlp output source={} {chunk}",
                            source_path.display()
                        ));
                        pending.clear();
                    }
                }
                Err(err) => {
                    self.logger.error(&format!(
                        "yt-dlp error source={}: {err}",
                        source_path.display()
                    ));
                    stop_process(&mut child).await;
                    return Err(err);
                }
            }
        }

        if !pending.is_empty() {
            let tail = String::from_utf8_lossy(&pending);
            self.logger.info(&format!(
                "yt-dlp output source={} {tail}",
                source_path.display()
            ));
        }

        let status = child.wait().await?;
        stop_process(&mut child).await;
        let code = status.code().unwrap_or(-1);
        self.logger.info(&format!(
            "yt-dlp exit code={code} source={}",
            source_path.display()
        ));
        self.logger
            .info(&format!("source finished source={}", source_path.display()));
        Ok(code)
    }
}

fn create_pipe() -> io::Result<(std::fs::File, std::fs::File)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid 2-element array for `pipe2`.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    use std::os::fd::FromRawFd;
    // SAFETY: both descriptors were freshly created by `pipe2`.
    let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((read_end, write_end))
}

fn touch_file(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    file.set_modified(SystemTime::now())?;
    Ok(())
}

async fn stop_process(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };
    let pgid = pid as libc::pid_t;
    // SAFETY: sending SIGTERM to the child's own process group created via setsid.
    let rc = unsafe { libc::killpg(pgid, libc::SIGTERM) };
    if rc != 0 {
        let _ = child.wait().await;
        return;
    }
    for _ in 0..50 {
        // SAFETY: signal 0 probes process group existence.
        if unsafe { libc::killpg(pgid, 0) } != 0 {
            let _ = child.wait().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // SAFETY: escalate to SIGKILL after 5s timeout.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;
    use crate::repository::SourceRepository;

    #[tokio::test]
    async fn runner_executes_command_touches_marker_and_logs_output() {
        let tmp = TempDir::new().unwrap();
        let repo = SourceRepository::new(tmp.path().join("downloads"));
        let source = repo
            .add_source("https://example.test/account")
            .unwrap()
            .source;

        let fake = tmp.path().join("fake-yt-dlp");
        fs::write(
            &fake,
            "#!/bin/sh\n\
             test -f .last-check || exit 9\n\
             touch .archive\n\
             echo \"fake stdout\"\n\
             echo \"fake stderr\" >&2\n\
             echo \"args:$*\"\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let log_file = tmp.path().join("logs").join("vdl.log");
        let logger = Arc::new(AppLogger::new(Some(&log_file)));
        let runner = YtDlpRunner::from_command(
            vec![
                fake.to_string_lossy().into_owned(),
                "--download-archive".to_string(),
                ".archive".to_string(),
            ],
            Arc::clone(&logger),
        )
        .unwrap();

        let code = runner.run(&source).await.unwrap();
        assert_eq!(code, 0);
        assert!(source.path.join(".last-check").exists());
        assert!(source.path.join(".archive").exists());

        let log = fs::read_to_string(&log_file).unwrap();
        assert!(log.contains("fake stdout"));
        assert!(log.contains("fake stderr"));
        assert!(log.contains("args:--download-archive .archive https://example.test/account"));
        assert!(log.contains("yt-dlp exit code=0"));
        assert!(!logger.recent(10).is_empty());
    }

    #[test]
    fn logger_rotates_file_when_exceeding_max_bytes() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("vdl.log");
        let backup_path = tmp.path().join("vdl.log.1");
        let logger = AppLogger::with_max_bytes(&log_path, 128);

        for i in 0..10 {
            logger.info(&format!("line-{i}-padding-to-exceed-threshold"));
        }

        assert!(log_path.exists());
        assert!(backup_path.exists());
        assert!(fs::metadata(&log_path).unwrap().len() < 256);
    }
}
