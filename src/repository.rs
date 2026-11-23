//! Filesystem sources and pending requests; owned exclusively by `vdl serve`.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use url::Url;

use crate::config::expand_path;

pub const SOURCE_FILE: &str = ".source";
pub const DISABLED_SOURCE_FILE: &str = ".source.del";
pub const LAST_CHECK_FILE: &str = ".last-check";
pub const DOWNLOAD_NOW_FILE: &str = ".download-now";
pub const MAX_URL_LENGTH: usize = 4096;

#[derive(Debug)]
pub enum RepoError {
    Value(String),
    NotFound(String),
    AlreadyExists(String),
    Io(io::Error),
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoError::Value(msg) | RepoError::NotFound(msg) | RepoError::AlreadyExists(msg) => {
                f.write_str(msg)
            }
            RepoError::Io(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for RepoError {}

impl From<io::Error> for RepoError {
    fn from(err: io::Error) -> Self {
        RepoError::Io(err)
    }
}

pub fn normalize_url(value: &str) -> Result<String, RepoError> {
    if value.trim().is_empty() {
        return Err(RepoError::Value(
            "source URL must be a non-empty string".to_string(),
        ));
    }
    if value.chars().count() > MAX_URL_LENGTH {
        return Err(RepoError::Value(format!(
            "source URL must not exceed {MAX_URL_LENGTH} characters"
        )));
    }
    let trimmed = value.trim();
    let url = trimmed
        .split_once('?')
        .map_or(trimmed, |(before, _)| before);
    if url
        .chars()
        .any(|c| c.is_whitespace() || (c as u32) < 32 || (c as u32) == 127)
    {
        return Err(RepoError::Value(
            "source URL must not contain spaces or control characters".to_string(),
        ));
    }
    if url.contains('\\') {
        return Err(RepoError::Value(
            "source URL must not contain credentials or backslashes".to_string(),
        ));
    }
    let parsed = Url::parse(url).map_err(|_| {
        RepoError::Value(
            "source URL must start with http:// or https:// and include a hostname".to_string(),
        )
    })?;
    let scheme = parsed.scheme();
    if (scheme != "http" && scheme != "https") || parsed.host_str().is_none() {
        return Err(RepoError::Value(
            "source URL must start with http:// or https:// and include a hostname".to_string(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || has_userinfo_at(url) {
        return Err(RepoError::Value(
            "source URL must not contain credentials or backslashes".to_string(),
        ));
    }
    Ok(url.to_string())
}

fn has_userinfo_at(url: &str) -> bool {
    if let Some((_, after_scheme)) = url.split_once("://") {
        let authority = after_scheme
            .split(['/', '?', '#'])
            .next()
            .unwrap_or(after_scheme);
        return authority.contains('@');
    }
    false
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub service: String,
    pub account: String,
    pub path: PathBuf,
    pub url: String,
    pub active: bool,
    pub last_check: Option<f64>,
    pub download_requested: bool,
    pub queued_at: Option<f64>,
}

impl Source {
    pub fn id(&self) -> String {
        format!("{}/{}", self.service, self.account)
    }

    pub fn sort_key(&self) -> (String, String, String) {
        (
            self.service.to_lowercase(),
            self.account.to_lowercase(),
            self.path.to_string_lossy().to_lowercase(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct AddResult {
    pub source: Source,
    pub created: bool,
}

#[derive(Debug, Clone)]
pub struct SourceRepository {
    pub download_root: PathBuf,
    mutation_lock: Arc<Mutex<()>>,
    busy: Arc<Mutex<Option<String>>>,
}

impl SourceRepository {
    pub fn new(download_root: impl AsRef<Path>) -> Self {
        Self {
            download_root: expand_path(download_root.as_ref()),
            mutation_lock: Arc::new(Mutex::new(())),
            busy: Arc::new(Mutex::new(None)),
        }
    }

    pub fn current_download(&self) -> Option<String> {
        self.busy.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_busy(&self, label: Option<String>) {
        *self.busy.lock().unwrap_or_else(|e| e.into_inner()) = label;
    }

    pub fn list_sources(&self) -> Vec<Source> {
        if !self.download_root.is_dir() {
            return Vec::new();
        }
        let mut discovered = Vec::new();
        for service_dir in sorted_directories(&self.download_root) {
            for source_dir in sorted_directories(&service_dir) {
                if let Some(source) = read_source(&service_dir, &source_dir) {
                    discovered.push(source);
                }
            }
        }
        discovered.sort_by_key(Source::sort_key);
        discovered
    }

    fn source_at(&self, path: &Path) -> Result<Option<Source>, RepoError> {
        let root = canonicalize_best_effort(&self.download_root);
        let resolved = canonicalize_best_effort(path);
        let path_is_symlink = fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let parent_is_symlink = path
            .parent()
            .and_then(|p| fs::symlink_metadata(p).ok())
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !resolved.starts_with(&root) || path_is_symlink || parent_is_symlink {
            return Err(RepoError::Value(
                "source directory must remain inside download_root".to_string(),
            ));
        }
        if !path.is_dir() {
            return Ok(None);
        }
        let Some(parent) = path.parent() else {
            return Ok(None);
        };
        Ok(read_source(parent, path))
    }

    pub fn add_source(&self, raw_url: &str) -> Result<AddResult, RepoError> {
        let url = normalize_url(raw_url)?;
        let (service, account) = derive_source_location(&url)?;
        let candidate = self.download_root.join(&service).join(&account);
        let _lock = self.mutation_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = self.source_at(&candidate)? {
            if existing.url == url {
                return Ok(AddResult {
                    source: existing,
                    created: false,
                });
            }
            return Err(RepoError::AlreadyExists(format!(
                "{} already contains a different source URL",
                candidate.display()
            )));
        }
        fs::create_dir_all(&candidate)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(candidate.join(SOURCE_FILE))?;
        writeln!(file, "{url}")?;
        drop(file);
        let Some(source) = self.source_at(&candidate)? else {
            return Err(RepoError::Io(io::Error::other(
                "could not read newly created source",
            )));
        };
        Ok(AddResult {
            source,
            created: true,
        })
    }

    pub fn disable(&self, source: &Source) -> Result<Source, RepoError> {
        let _lock = self.mutation_lock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = self.source_at(&source.path)? else {
            return Err(RepoError::NotFound("source marker disappeared".to_string()));
        };
        if current.active {
            fs::rename(
                current.path.join(SOURCE_FILE),
                current.path.join(DISABLED_SOURCE_FILE),
            )?;
        }
        remove_if_exists(&current.path.join(DOWNLOAD_NOW_FILE))?;
        let Some(updated) = self.source_at(&current.path)? else {
            return Err(RepoError::Io(io::Error::other("source marker disappeared")));
        };
        Ok(updated)
    }

    pub fn request_download(&self, source: &Source) -> Result<Source, RepoError> {
        let _lock = self.mutation_lock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = self.source_at(&source.path)? else {
            return Err(RepoError::NotFound("source marker disappeared".to_string()));
        };
        if !current.active {
            return Err(RepoError::Value("source is inactive".to_string()));
        }
        if self.current_download().as_deref() != Some(current.id().as_str()) {
            let marker = current.path.join(DOWNLOAD_NOW_FILE);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
            {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(RepoError::Io(e)),
            }
        }
        let Some(updated) = self.source_at(&current.path)? else {
            return Err(RepoError::Io(io::Error::other("source marker disappeared")));
        };
        Ok(updated)
    }

    pub fn claim_next<F>(&self, select: F) -> Option<Source>
    where
        F: FnOnce(&[Source]) -> Option<Source>,
    {
        let _lock = self.mutation_lock.lock().unwrap_or_else(|e| e.into_inner());
        let sources = self.list_sources();
        let selected = select(&sources);
        if let Some(ref source) = selected {
            self.set_busy(Some(source.id()));
            let _ = remove_if_exists(&source.path.join(DOWNLOAD_NOW_FILE));
        }
        selected
    }

    pub fn snapshot_sources(&self) -> (Vec<Source>, Option<String>) {
        let _lock = self.mutation_lock.lock().unwrap_or_else(|e| e.into_inner());
        (self.list_sources(), self.current_download())
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut existing = abs.clone();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_os_string) else {
            break;
        };
        suffix.push(name);
        if !existing.pop() {
            break;
        }
    }
    let mut base = fs::canonicalize(&existing).unwrap_or(existing);
    for part in suffix.into_iter().rev() {
        base.push(part);
    }
    base
}

fn read_source(service_dir: &Path, source_dir: &Path) -> Option<Source> {
    let active_marker = source_dir.join(SOURCE_FILE);
    let inactive_marker = source_dir.join(DISABLED_SOURCE_FILE);
    let (marker, active) = if active_marker.is_file() {
        (active_marker, true)
    } else if inactive_marker.is_file() {
        (inactive_marker, false)
    } else {
        return None;
    };
    let url = fs::read(&marker)
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default();
    let last_check = file_mtime(&source_dir.join(LAST_CHECK_FILE));
    let requested_at = file_mtime(&source_dir.join(DOWNLOAD_NOW_FILE));
    let queued_at = requested_at.or_else(|| {
        if last_check.is_none() {
            file_mtime(&marker)
        } else {
            None
        }
    });
    let service = service_dir.file_name()?.to_string_lossy().into_owned();
    let account = source_dir.file_name()?.to_string_lossy().into_owned();
    Some(Source {
        service,
        account,
        path: source_dir.to_path_buf(),
        url,
        active,
        last_check,
        download_requested: requested_at.is_some(),
        queued_at,
    })
}

fn file_mtime(path: &Path) -> Option<f64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_secs_f64()),
        Err(e) => Some(-e.duration().as_secs_f64()),
    }
}

fn sorted_directories(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
            children.push(path);
        }
    }
    children.sort_by(|a, b| {
        let an = a.file_name().unwrap_or_default().to_string_lossy();
        let bn = b.file_name().unwrap_or_default().to_string_lossy();
        (an.to_lowercase(), an.as_ref()).cmp(&(bn.to_lowercase(), bn.as_ref()))
    });
    children
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn sanitize_component(value: &str, fallback: &str) -> String {
    let decoded = percent_decode(value);
    let trimmed = decoded.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_dash = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            if ch == '-' {
                if !prev_dash {
                    out.push('-');
                    prev_dash = true;
                }
            } else {
                out.push(ch);
                prev_dash = false;
            }
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let cleaned = out.trim_matches(['.', '-', '_']);
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

fn hostname_service(hostname: &str) -> String {
    let host = hostname.to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    if host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com") {
        return "youtube".to_string();
    }
    if host == "tiktok.com" || host.ends_with(".tiktok.com") {
        return "tiktok".to_string();
    }
    if host == "instagram.com" || host.ends_with(".instagram.com") {
        return "instagram".to_string();
    }
    let host = host.strip_prefix("www.").unwrap_or(host);
    sanitize_component(&host.replace('.', "-"), "site").to_ascii_lowercase()
}

pub fn derive_source_location(url: &str) -> Result<(String, String), RepoError> {
    if url.trim().is_empty() {
        return Err(RepoError::Value(
            "source URL must be a non-empty string".to_string(),
        ));
    }
    let parsed = Url::parse(url.trim()).map_err(|_| {
        RepoError::Value("source URL must include a scheme and hostname".to_string())
    })?;
    let Some(hostname) = parsed.host_str() else {
        return Err(RepoError::Value(
            "source URL must include a hostname".to_string(),
        ));
    };
    let service = hostname_service(hostname);
    let mut segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() > 1
        && let Some(last) = segments.last()
        && matches!(
            last.to_ascii_lowercase().as_str(),
            "posts" | "reels" | "shorts" | "videos"
        )
    {
        segments.pop();
    }
    let component = segments.last().copied().unwrap_or(hostname);
    let account = sanitize_component(component.strip_prefix('@').unwrap_or(component), "source");
    Ok((service, account))
}

pub fn display_service(service: &str) -> String {
    match service.to_lowercase().as_str() {
        "tiktok" => "TikTok".to_string(),
        "youtube" => "YouTube".to_string(),
        _ => title_case(&service.replace(['-', '_'], " ")),
    }
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_cased = false;
    for ch in s.chars() {
        if ch.is_alphabetic() {
            if prev_cased {
                for lc in ch.to_lowercase() {
                    out.push(lc);
                }
            } else {
                for uc in ch.to_uppercase() {
                    out.push(uc);
                }
            }
            prev_cased = true;
        } else {
            out.push(ch);
            prev_cased = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_expected_service_and_account() {
        for (url, expected) in [
            (
                "https://www.tiktok.com/@foo?tab=videos#top",
                ("tiktok", "foo"),
            ),
            ("https://www.youtube.com/@foo/", ("youtube", "foo")),
            ("https://www.youtube.com/@foo/videos", ("youtube", "foo")),
            ("https://www.instagram.com/foo/", ("instagram", "foo")),
            (
                "https://video.example.org/@some user/",
                ("video-example-org", "some-user"),
            ),
        ] {
            let (svc, acc) = derive_source_location(url).unwrap();
            assert_eq!((svc.as_str(), acc.as_str()), expected);
        }
    }

    #[test]
    fn displays_service_names() {
        assert_eq!(display_service("tiktok"), "TikTok");
        assert_eq!(display_service("youtube"), "YouTube");
        assert_eq!(display_service("video-example-org"), "Video Example Org");
    }
}
