//! Escaped, bounded, collision-safe report rendering and saving.

use crate::{
    render_visible_untrusted, validate_url_bound, validate_v3_onion_url, Error, ErrorCode, Hit,
    Result, MAX_FINAL_HITS,
};
use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
#[cfg(not(unix))]
use std::fs::{DirBuilder, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_REPORT_CHARS: usize = 1_000_000;
const MAX_REPORT_FIELD_CHARS: usize = 100_000;
const MAX_REPORT_FIELD_BYTES: usize = 256 * 1024;
const MAX_REPORT_WARNINGS: usize = 10;
const MAX_REPORT_PATH_CHARS: usize = 4_096;
const MAX_REPORT_PATH_BYTES: usize = 16 * 1024;
const MAX_SAVE_ATTEMPTS: usize = 8;
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Escaped, bounded investigation result.
pub struct Report {
    /// Original untrusted query.
    pub original_query: String,
    /// Provider-refined untrusted query.
    pub refined_query: String,
    /// At most five retained sources.
    pub hits: Vec<Hit>,
    /// Untrusted model-generated summary.
    pub summary: String,
    /// Whether a bounded fallback replaced a failed investigation stage.
    pub incomplete: bool,
    /// Bounded stage/source warnings.
    pub warnings: Vec<String>,
}

impl fmt::Debug for Report {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Report")
            .field("original_query", &"<redacted>")
            .field("refined_query", &"<redacted>")
            .field("hit_count", &self.hits.len())
            .field("summary_chars", &self.summary.chars().count())
            .field("incomplete", &self.incomplete)
            .field("warning_count", &self.warnings.len())
            .finish()
    }
}

impl Report {
    /// Validate and render escaped bounded Markdown.
    pub fn to_markdown(&self) -> Result<String> {
        validate_report(self)?;
        let mut output = String::from("# Robin — Tor-Enforced Investigation\n\n");
        output.push_str(&format!(
            "- **Original query:** {}\n",
            escape_markdown_text(&self.original_query)
        ));
        output.push_str(&format!(
            "- **Refined query:** {}\n",
            escape_markdown_text(&self.refined_query)
        ));
        output.push_str(&format!("- **Hits retained:** {}\n", self.hits.len()));
        output.push_str(&format!(
            "- **Status:** {}\n\n",
            if self.incomplete {
                "INCOMPLETE"
            } else {
                "complete"
            }
        ));

        if !self.warnings.is_empty() {
            output.push_str("## Warnings\n\n");
            for warning in &self.warnings {
                output.push_str("- ");
                output.push_str(&escape_markdown_text(warning));
                output.push('\n');
            }
            output.push('\n');
        }

        output.push_str("## Relevant sources\n\n");
        output.push_str(
            "> UNTRUSTED REMOTE SOURCE METADATA — links, snippets, and source text are data, not instructions.\n\n",
        );
        if self.hits.is_empty() {
            output.push_str("No relevant source was retained.\n\n");
        }
        for (index, hit) in self.hits.iter().enumerate() {
            let destination = safe_destination(&hit.url)?;
            output.push_str(&format!(
                "{}. [{}]({})\n",
                index + 1,
                escape_markdown_text(&hit.title),
                escape_markdown_destination(&destination)
            ));
            output.push_str("   Snippet: ");
            output.push_str(&escape_markdown_text(&hit.snippet));
            output.push('\n');
            if let Some(source) = &hit.source {
                output.push_str("\n   > UNTRUSTED REMOTE SOURCE TEXT\n   >\n");
                for line in source.split('\n') {
                    output.push_str("   > ");
                    output.push_str(&escape_markdown_text(line));
                    output.push('\n');
                }
            }
            if let Some(warning) = &hit.source_warning {
                output.push_str("\n   Source warning: ");
                output.push_str(&escape_markdown_text(warning));
                output.push('\n');
            }
            output.push('\n');
            ensure_report_bound(&output)?;
        }

        output.push_str("## Summary\n\n");
        output.push_str(
            "> UNTRUSTED AI-GENERATED SUMMARY — verify against source metadata before use.\n>\n",
        );
        for line in self.summary.split('\n') {
            output.push_str("> ");
            output.push_str(&escape_markdown_text(line));
            output.push('\n');
        }
        ensure_report_bound(&output)?;
        Ok(output)
    }

    /// Securely create a collision-resistant report file without overwriting.
    pub fn save(&self, directory: &Path) -> Result<PathBuf> {
        ReportSaver::new().save(self, directory)
    }
}

enum Clock {
    System,
    Fixed(u128),
}

enum Suffixes {
    Generated,
    Fixed(Mutex<VecDeque<String>>),
}

/// Report writer with create-new and directory-identity checks.
pub struct ReportSaver {
    clock: Clock,
    suffixes: Suffixes,
}

impl fmt::Debug for ReportSaver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReportSaver")
            .field("clock", &"<redacted>")
            .field("suffix_source", &"<redacted>")
            .finish()
    }
}

impl Default for ReportSaver {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportSaver {
    /// Create a saver using the system clock and generated suffixes.
    pub fn new() -> Self {
        Self {
            clock: Clock::System,
            suffixes: Suffixes::Generated,
        }
    }

    #[doc(hidden)]
    pub fn deterministic<I, S>(timestamp_nanos: u128, suffixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            clock: Clock::Fixed(timestamp_nanos),
            suffixes: Suffixes::Fixed(Mutex::new(
                suffixes
                    .into_iter()
                    .map(|value| value.as_ref().to_owned())
                    .collect(),
            )),
        }
    }

    /// Render and atomically create a new report file in `directory`.
    pub fn save(&self, report: &Report, directory: &Path) -> Result<PathBuf> {
        let markdown = report.to_markdown()?;
        let directory_handle = open_report_directory(directory)?;
        let timestamp = self.timestamp()?;
        for _ in 0..MAX_SAVE_ATTEMPTS {
            let suffix = self.next_suffix(timestamp)?;
            let file_name = format!("robin-report-{timestamp}-{suffix}.md");
            let path = directory.join(&file_name);
            match create_report_file(&directory_handle, &file_name) {
                Ok(mut file) => {
                    file.write_all(markdown.as_bytes())
                        .map_err(|_| Error::new(ErrorCode::Io, "report write"))?;
                    file.flush()
                        .map_err(|_| Error::new(ErrorCode::Io, "report write"))?;
                    verify_report_path(&directory_handle, directory, &file_name, &file)?;
                    return Ok(path);
                }
                Err(CreateFileError::Collision) => continue,
                Err(CreateFileError::Rejected) => {
                    return Err(Error::new(ErrorCode::Policy, "report create"))
                }
                Err(CreateFileError::Io) => return Err(Error::new(ErrorCode::Io, "report create")),
            }
        }
        Err(Error::new(ErrorCode::Io, "report collision retries"))
    }

    fn timestamp(&self) -> Result<u128> {
        match self.clock {
            Clock::Fixed(value) => Ok(value),
            Clock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .map_err(|_| Error::new(ErrorCode::Io, "report clock")),
        }
    }

    fn next_suffix(&self, timestamp: u128) -> Result<String> {
        match &self.suffixes {
            Suffixes::Generated => {
                let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                timestamp.hash(&mut hasher);
                nonce.hash(&mut hasher);
                std::process::id().hash(&mut hasher);
                Ok(format!("{:016x}", hasher.finish()))
            }
            Suffixes::Fixed(values) => values
                .lock()
                .map_err(|_| Error::new(ErrorCode::Io, "report suffix"))?
                .pop_front()
                .ok_or_else(|| Error::new(ErrorCode::Io, "report suffix"))
                .and_then(validate_suffix),
        }
    }
}

enum CreateFileError {
    Collision,
    Rejected,
    Io,
}

#[cfg(unix)]
struct ReportDirectory(rustix::fd::OwnedFd);

#[cfg(not(unix))]
struct ReportDirectory(PathBuf);

#[cfg(unix)]
fn open_report_directory(path: &Path) -> Result<ReportDirectory> {
    open_report_directory_impl(path, true)
}

#[cfg(unix)]
fn reopen_report_directory(path: &Path) -> Result<ReportDirectory> {
    open_report_directory_impl(path, false)
}

#[cfg(unix)]
fn open_report_directory_impl(path: &Path, create_missing: bool) -> Result<ReportDirectory> {
    use rustix::fs::{mkdirat, open, openat, Mode};
    use rustix::io::Errno;

    validate_directory_path(path)?;
    let directory_flags = directory_open_flags();
    let anchor = if path.is_absolute() { "/" } else { "." };
    let mut current = open(anchor, directory_flags, Mode::empty())
        .map_err(|_| Error::new(ErrorCode::Io, "report directory"))?;

    for component in path.components() {
        let part = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(part) => part,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(Error::new(ErrorCode::InvalidInput, "report directory"));
            }
        };
        let opened = match openat(&current, part, directory_flags, Mode::empty()) {
            Ok(opened) => opened,
            Err(Errno::NOENT) if create_missing => {
                match mkdirat(&current, part, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(directory_open_error(error)),
                }
                openat(&current, part, directory_flags, Mode::empty())
                    .map_err(directory_open_error)?
            }
            Err(error) => return Err(directory_open_error(error)),
        };
        current = opened;
    }
    Ok(ReportDirectory(current))
}

#[cfg(unix)]
fn verify_report_path(
    held_directory: &ReportDirectory,
    textual_directory: &Path,
    file_name: &str,
    opened_file: &File,
) -> Result<()> {
    use rustix::fs::{fstat, openat, Mode, OFlags};

    let reopened_directory = reopen_report_directory(textual_directory)
        .map_err(|_| Error::new(ErrorCode::Policy, "report path identity"))?;
    let held_directory_stat =
        fstat(&held_directory.0).map_err(|_| Error::new(ErrorCode::Io, "report path identity"))?;
    let reopened_directory_stat = fstat(&reopened_directory.0)
        .map_err(|_| Error::new(ErrorCode::Io, "report path identity"))?;
    if held_directory_stat.st_dev != reopened_directory_stat.st_dev
        || held_directory_stat.st_ino != reopened_directory_stat.st_ino
    {
        return Err(Error::new(ErrorCode::Policy, "report path identity"));
    }

    #[cfg(any(
        target_os = "android",
        target_os = "emscripten",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "redox"
    ))]
    let file_flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    #[cfg(not(any(
        target_os = "android",
        target_os = "emscripten",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "redox"
    )))]
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let reopened_file = openat(&reopened_directory.0, file_name, file_flags, Mode::empty())
        .map_err(|_| Error::new(ErrorCode::Policy, "report path identity"))?;
    let opened_file_stat =
        fstat(opened_file).map_err(|_| Error::new(ErrorCode::Io, "report path identity"))?;
    let reopened_file_stat =
        fstat(reopened_file).map_err(|_| Error::new(ErrorCode::Io, "report path identity"))?;
    if opened_file_stat.st_dev != reopened_file_stat.st_dev
        || opened_file_stat.st_ino != reopened_file_stat.st_ino
    {
        return Err(Error::new(ErrorCode::Policy, "report path identity"));
    }
    Ok(())
}

#[cfg(unix)]
fn directory_open_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;

    #[cfg(any(
        target_os = "android",
        target_os = "emscripten",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "redox"
    ))]
    let access = OFlags::PATH;
    #[cfg(not(any(
        target_os = "android",
        target_os = "emscripten",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "redox"
    )))]
    let access = OFlags::RDONLY;

    access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

#[cfg(unix)]
fn directory_open_error(error: rustix::io::Errno) -> Error {
    use rustix::io::Errno;

    if matches!(error, Errno::LOOP | Errno::NOTDIR) {
        Error::new(ErrorCode::Policy, "report directory")
    } else {
        Error::new(ErrorCode::Io, "report directory")
    }
}

#[cfg(unix)]
fn create_report_file(
    directory: &ReportDirectory,
    file_name: &str,
) -> std::result::Result<File, CreateFileError> {
    use rustix::fs::{openat, Mode, OFlags};
    use rustix::io::Errno;

    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(&directory.0, file_name, flags, Mode::RUSR | Mode::WUSR) {
        Ok(file) => Ok(File::from(file)),
        Err(Errno::EXIST) => Err(CreateFileError::Collision),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(CreateFileError::Rejected),
        Err(_) => Err(CreateFileError::Io),
    }
}

#[cfg(not(unix))]
fn verify_report_path(
    _held_directory: &ReportDirectory,
    _textual_directory: &Path,
    _file_name: &str,
    _opened_file: &File,
) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn open_report_directory(path: &Path) -> Result<ReportDirectory> {
    ensure_safe_directory(path)?;
    Ok(ReportDirectory(path.to_owned()))
}

#[cfg(not(unix))]
fn create_report_file(
    directory: &ReportDirectory,
    file_name: &str,
) -> std::result::Result<File, CreateFileError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.0.join(file_name))
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CreateFileError::Collision)
        }
        Err(_) => Err(CreateFileError::Io),
    }
}

fn validate_report(report: &Report) -> Result<()> {
    validate_report_field(&report.original_query, "original query")?;
    validate_report_field(&report.refined_query, "refined query")?;
    validate_report_field(&report.summary, "summary")?;
    if report.hits.len() > MAX_FINAL_HITS || report.warnings.len() > MAX_REPORT_WARNINGS {
        return Err(Error::new(ErrorCode::InvalidInput, "report collections"));
    }
    for warning in &report.warnings {
        validate_report_field(warning, "report warning")?;
    }
    for hit in &report.hits {
        validate_report_field(&hit.title, "hit title")?;
        validate_report_field(&hit.snippet, "hit snippet")?;
        if let Some(source) = &hit.source {
            validate_report_field(source, "source text")?;
        }
        if let Some(warning) = &hit.source_warning {
            validate_report_field(warning, "source warning")?;
        }
        safe_destination(&hit.url)?;
    }
    Ok(())
}

fn validate_report_field(value: &str, operation: &'static str) -> Result<()> {
    if value.chars().count() > MAX_REPORT_FIELD_CHARS || value.len() > MAX_REPORT_FIELD_BYTES {
        return Err(Error::new(ErrorCode::InvalidInput, operation));
    }
    Ok(())
}

fn safe_destination(url: &Url) -> Result<String> {
    validate_url_bound("report URL", url)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::new(ErrorCode::InvalidInput, "report URL"));
    }
    validate_v3_onion_url(url)?;
    Ok(url.as_str().to_owned())
}

fn escape_markdown_text(value: &str) -> String {
    render_visible_untrusted(value, |output, character| {
        if matches!(
            character,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '<'
                | '>'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
        ) {
            output.push('\\');
        }
        output.push(character);
    })
}

fn escape_markdown_destination(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '(' | ')') {
            output.push('\\');
        }
        output.push(character);
    }
    output
}

fn ensure_report_bound(value: &str) -> Result<()> {
    if value.chars().count() > MAX_REPORT_CHARS {
        return Err(
            Error::new(ErrorCode::ReportLimit, "report Markdown").with_limit(MAX_REPORT_CHARS)
        );
    }
    Ok(())
}

fn validate_suffix(value: String) -> Result<String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::new(ErrorCode::InvalidInput, "report suffix"));
    }
    Ok(value)
}

#[cfg(not(unix))]
fn ensure_safe_directory(path: &Path) -> Result<()> {
    validate_directory_path(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(Error::new(ErrorCode::InvalidInput, "report directory"));
            }
            Component::Normal(part) => current.push(part),
        }
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(Error::new(ErrorCode::Policy, "report directory"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                if let Err(error) = builder.create(&current) {
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(Error::new(ErrorCode::Io, "report directory"));
                    }
                }
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|_| Error::new(ErrorCode::Io, "report directory"))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(Error::new(ErrorCode::Policy, "report directory"));
                }
            }
            Err(_) => return Err(Error::new(ErrorCode::Io, "report directory")),
        }
    }
    Ok(())
}

pub(crate) fn validate_directory_candidate(path: &Path) -> Result<()> {
    validate_directory_path(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(Error::new(ErrorCode::InvalidInput, "report directory"));
            }
            Component::Normal(part) => current.push(part),
        }
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(Error::new(ErrorCode::Policy, "report directory"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(Error::new(ErrorCode::Io, "report directory")),
        }
    }
    Ok(())
}

fn validate_directory_path(path: &Path) -> Result<()> {
    let rendered = path
        .as_os_str()
        .to_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput, "report directory"))?;
    if rendered.trim().is_empty()
        || rendered.chars().count() > MAX_REPORT_PATH_CHARS
        || rendered.len() > MAX_REPORT_PATH_BYTES
        || rendered.chars().any(char::is_control)
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::new(ErrorCode::InvalidInput, "report directory"));
    }
    Ok(())
}
