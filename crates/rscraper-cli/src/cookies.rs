//! Secure platform-cookie loading and private state-file helpers.

use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::HeaderValue;
use rscraper_core::{Error, Result};
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_COOKIE_FILE_BYTES: u64 = 64 * 1024;
const MAX_COOKIE_COUNT: usize = 128;

/// Cookie state whose diagnostics never expose the contained session secrets.
///
/// The wrapper intentionally implements [`CookieStore`] so callers retain the
/// normal request-header ergonomics without receiving the raw jar.
pub struct PlatformCookieJar {
    inner: Jar,
}

impl fmt::Debug for PlatformCookieJar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted platform cookies>")
    }
}

impl CookieStore for PlatformCookieJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        self.inner.set_cookies(cookie_headers, url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        self.inner.cookies(url)
    }
}

pub enum CookieSource {
    RawHeader(String),
    NameValue(String),
    Netscape(String),
}

impl fmt::Debug for CookieSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted cookie source>")
    }
}

#[derive(Clone)]
struct CookieSpec {
    name: String,
    value: String,
    domain: Option<String>,
    path: String,
    secure: bool,
    http_only: bool,
    max_age_secs: Option<u64>,
}

pub fn load_platform_cookies(path: &Path, platform_origin: &Url) -> Result<PlatformCookieJar> {
    validate_origin(platform_origin)?;
    let bytes = read_cookie_file(path)?;
    let raw = String::from_utf8(bytes)
        .map_err(|_| Error::InvalidInput("cookie file must be UTF-8 text".into()))?;
    if raw.contains('\r') {
        return Err(Error::InvalidInput(
            "cookie file contains forbidden carriage returns".into(),
        ));
    }
    let source = detect_source(raw)?;
    let cookies = parse_source(&source, platform_origin)?;
    if cookies.is_empty() {
        return Err(Error::InvalidInput(
            "cookie file contains no usable cookies".into(),
        ));
    }
    if cookies.len() > MAX_COOKIE_COUNT {
        return Err(Error::InvalidInput(format!(
            "cookie file exceeds the {MAX_COOKIE_COUNT}-cookie limit"
        )));
    }

    let jar = Jar::default();
    for cookie in cookies {
        let mut set_cookie = format!("{}={}; Path={}", cookie.name, cookie.value, cookie.path);
        if let Some(domain) = cookie.domain {
            set_cookie.push_str("; Domain=");
            set_cookie.push_str(&domain);
        }
        if cookie.secure {
            set_cookie.push_str("; Secure");
        }
        if cookie.http_only {
            set_cookie.push_str("; HttpOnly");
        }
        if let Some(max_age_secs) = cookie.max_age_secs {
            set_cookie.push_str("; Max-Age=");
            set_cookie.push_str(&max_age_secs.to_string());
        }
        jar.add_cookie_str(&set_cookie, platform_origin);
    }
    Ok(PlatformCookieJar { inner: jar })
}

pub fn validate_cookie_file(path: &Path) -> Result<()> {
    let file = open_cookie_file(path)?;
    validate_cookie_metadata(&file.metadata().map_err(safe_io_error)?)
}

fn read_cookie_file(path: &Path) -> Result<Vec<u8>> {
    read_cookie_file_after_open(path, || {})
}

fn read_cookie_file_after_open<F>(path: &Path, after_open: F) -> Result<Vec<u8>>
where
    F: FnOnce(),
{
    let file = open_cookie_file(path)?;
    after_open();
    let metadata = file.metadata().map_err(safe_io_error)?;
    validate_cookie_metadata(&metadata)?;
    if metadata.len() > MAX_COOKIE_FILE_BYTES {
        return Err(Error::InvalidInput(format!(
            "cookie file exceeds the {MAX_COOKIE_FILE_BYTES}-byte limit"
        )));
    }

    let mut bytes = Vec::new();
    file.take(MAX_COOKIE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(safe_io_error)?;
    if bytes.len() as u64 > MAX_COOKIE_FILE_BYTES {
        return Err(Error::InvalidInput(format!(
            "cookie file exceeds the {MAX_COOKIE_FILE_BYTES}-byte limit"
        )));
    }
    Ok(bytes)
}

fn open_cookie_file(path: &Path) -> Result<File> {
    #[cfg(not(unix))]
    {
        let metadata = fs::symlink_metadata(path).map_err(safe_io_error)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::Policy("cookie file must not be a symlink".into()));
        }
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return Error::Policy("cookie file must not be a symlink".into());
        }
        safe_io_error(error)
    })
}

fn validate_cookie_metadata(metadata: &Metadata) -> Result<()> {
    if !metadata.file_type().is_file() {
        return Err(Error::Policy("cookie path must be a regular file".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Policy(
                "cookie file permissions are too broad; repair with `chmod 600 -- <cookie-file>`"
                    .into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(safe_io_error)?;
    let metadata = fs::symlink_metadata(path).map_err(safe_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::Policy(
            "state directory must be a real directory, not a symlink".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(safe_io_error)?;
    }
    Ok(())
}

pub(crate) fn create_private_file_if_missing(path: &Path, contents: &[u8]) -> Result<bool> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(safe_io_error(error)),
    };
    file.write_all(contents).map_err(safe_io_error)?;
    file.sync_all().map_err(safe_io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(safe_io_error)?;
    }
    Ok(true)
}

fn detect_source(raw: String) -> Result<CookieSource> {
    let meaningful = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#') || line.starts_with("#HttpOnly_"))
        .collect::<Vec<_>>();
    if meaningful.iter().any(|line| line.contains('\t'))
        || raw.lines().any(|line| {
            line.trim()
                .eq_ignore_ascii_case("# Netscape HTTP Cookie File")
        })
    {
        Ok(CookieSource::Netscape(raw))
    } else if meaningful.len() == 1 && meaningful[0].contains(';') {
        Ok(CookieSource::RawHeader(meaningful[0].to_string()))
    } else {
        Ok(CookieSource::NameValue(raw))
    }
}

fn parse_source(source: &CookieSource, origin: &Url) -> Result<Vec<CookieSpec>> {
    match source {
        CookieSource::RawHeader(raw) => raw
            .split(';')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(host_cookie)
            .collect(),
        CookieSource::NameValue(raw) => raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(host_cookie)
            .collect(),
        CookieSource::Netscape(raw) => parse_netscape(raw, origin),
    }
}

fn host_cookie(line: &str) -> Result<CookieSpec> {
    if line.contains(';') || line.contains('\n') || line.contains('\r') {
        return Err(Error::InvalidInput(
            "cookie line contains forbidden delimiter characters".into(),
        ));
    }
    let (name, value) = parse_name_value(line)?;
    Ok(CookieSpec {
        name: name.to_string(),
        value: value.to_string(),
        domain: None,
        path: "/".into(),
        secure: false,
        http_only: false,
        max_age_secs: None,
    })
}

fn parse_netscape(raw: &str, origin: &Url) -> Result<Vec<CookieSpec>> {
    let origin_host = origin
        .host_str()
        .expect("origin validation ensures a host")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let mut cookies = Vec::new();
    for original_line in raw.lines() {
        let mut line = original_line.trim();
        if line.is_empty() || (line.starts_with('#') && !line.starts_with("#HttpOnly_")) {
            continue;
        }
        let http_only = line.starts_with("#HttpOnly_");
        if http_only {
            line = line.trim_start_matches("#HttpOnly_");
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 7 {
            return Err(Error::InvalidInput(
                "Netscape cookie line must have seven tab-separated fields".into(),
            ));
        }
        let domain = normalize_cookie_domain(fields[0])?;
        let include_subdomains = parse_netscape_bool(fields[1])?;
        if include_subdomains {
            if origin_host != domain && !origin_host.ends_with(&format!(".{domain}")) {
                return Err(Error::Policy(
                    "cookie domain does not match the platform origin".into(),
                ));
            }
        } else if origin_host != domain {
            return Err(Error::Policy(
                "host-only cookie does not match the platform origin".into(),
            ));
        }
        let path = fields[2];
        if !path.starts_with('/') || !path.bytes().all(valid_cookie_path_byte) {
            return Err(Error::InvalidInput("cookie path is invalid".into()));
        }
        let secure = parse_netscape_bool(fields[3])?;
        let expiry = fields[4]
            .parse::<u64>()
            .map_err(|_| Error::InvalidInput("cookie expiry is invalid".into()))?;
        let max_age_secs = (expiry != 0).then(|| expiry.saturating_sub(unix_time_secs()));
        let (name, value) = validate_name_value(fields[5], fields[6])?;
        cookies.push(CookieSpec {
            name: name.to_string(),
            value: value.to_string(),
            domain: include_subdomains.then_some(domain),
            path: path.to_string(),
            secure,
            http_only,
            max_age_secs,
        });
    }
    Ok(cookies)
}

fn unix_time_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_name_value(line: &str) -> Result<(&str, &str)> {
    let (name, value) = line.split_once('=').ok_or_else(|| {
        Error::InvalidInput("cookie entry must use the `name=value` format".into())
    })?;
    validate_name_value(name.trim(), value.trim())
}

fn validate_name_value<'a>(name: &'a str, value: &'a str) -> Result<(&'a str, &'a str)> {
    if name.is_empty() || !name.bytes().all(valid_cookie_name_byte) {
        return Err(Error::InvalidInput("cookie name is invalid".into()));
    }
    if !valid_cookie_value(value) {
        return Err(Error::InvalidInput("cookie value is invalid".into()));
    }
    let semantic_value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    if semantic_value.eq_ignore_ascii_case("<paste>") {
        return Err(Error::InvalidInput(
            "cookie placeholder must be replaced before use".into(),
        ));
    }
    Ok((name, value))
}

fn valid_cookie_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn valid_cookie_value_byte(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

fn valid_cookie_path_byte(byte: u8) -> bool {
    matches!(byte, 0x20..=0x3a | 0x3c..=0x7e)
}

fn valid_cookie_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.first() == Some(&b'"') || bytes.last() == Some(&b'"') {
        return bytes.len() >= 2
            && bytes.first() == Some(&b'"')
            && bytes.last() == Some(&b'"')
            && bytes[1..bytes.len() - 1]
                .iter()
                .copied()
                .all(valid_cookie_value_byte);
    }
    bytes.iter().copied().all(valid_cookie_value_byte)
}

fn normalize_cookie_domain(domain: &str) -> Result<String> {
    let domain = domain
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty()
        || domain.split('.').any(|label| {
            label.is_empty()
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(Error::InvalidInput("cookie domain is invalid".into()));
    }
    Ok(domain)
}

fn parse_netscape_bool(value: &str) -> Result<bool> {
    if value.eq_ignore_ascii_case("TRUE") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("FALSE") {
        Ok(false)
    } else {
        Err(Error::InvalidInput(
            "Netscape cookie boolean field is invalid".into(),
        ))
    }
}

fn validate_origin(origin: &Url) -> Result<()> {
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
    {
        return Err(Error::InvalidInput(
            "platform origin must be an HTTP(S) URL without credentials".into(),
        ));
    }
    Ok(())
}

fn safe_io_error(_error: std::io::Error) -> Error {
    Error::Io(std::io::Error::other(
        "cookie/state filesystem operation failed",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tempfile::TempDir;

    #[test]
    fn descriptor_read_is_stable_when_path_is_replaced_after_open() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("cookies.txt");
        let moved = directory.path().join("opened-cookie.txt");
        let replacement = directory.path().join("replacement.txt");
        fs::write(&path, "session=opened-value").unwrap();
        fs::write(&replacement, "session=replacement-value").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();

        let bytes = read_cookie_file_after_open(&path, || {
            fs::rename(&path, &moved).unwrap();
            symlink(&replacement, &path).unwrap();
        })
        .unwrap();

        assert_eq!(bytes, b"session=opened-value");
    }
}
