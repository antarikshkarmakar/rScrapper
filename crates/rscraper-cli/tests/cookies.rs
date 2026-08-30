use reqwest::cookie::CookieStore;
use rscraper_cli::context::AppContext;
use rscraper_cli::cookies::{load_platform_cookies, CookieSource};
use rscraper_cli::social;
use rscraper_core::{FetchClient, NetworkPolicy};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};
use tempfile::TempDir;
use url::Url;

const SECRET: &str = "secret-cookie-value";

#[test]
fn raw_cookie_header_and_name_value_lines_are_loaded_for_the_platform_origin() {
    let directory = TempDir::new().unwrap();
    let origin = Url::parse("https://example.com/app").unwrap();
    let raw = secure_file(
        directory.path(),
        "raw.txt",
        &format!("session={SECRET}; theme=dark"),
    );
    let lines = secure_file(
        directory.path(),
        "lines.txt",
        &format!("# local cookies\n\nsession={SECRET}\ntheme=dark\n"),
    );

    for path in [raw, lines] {
        let jar = load_platform_cookies(&path, &origin).unwrap();
        let header = jar.cookies(&origin).unwrap();
        let header = header.to_str().unwrap();
        assert!(header.contains(&format!("session={SECRET}")));
        assert!(header.contains("theme=dark"));
    }
}

#[test]
fn netscape_cookies_preserve_domain_path_and_secure_scope() {
    let directory = TempDir::new().unwrap();
    let path = secure_file(
        directory.path(),
        "netscape.txt",
        &format!(
            "# Netscape HTTP Cookie File\n#HttpOnly_.example.com\tTRUE\t/private\tTRUE\t2147483647\tsession\t{SECRET}\n.example.com\tTRUE\t/\tFALSE\t2147483647\ttheme\tdark\n"
        ),
    );
    let origin = Url::parse("https://sub.example.com/private/start").unwrap();

    let jar = load_platform_cookies(&path, &origin).unwrap();

    let matching = jar
        .cookies(&Url::parse("https://other.example.com/private/page").unwrap())
        .unwrap();
    let matching = matching.to_str().unwrap();
    assert!(matching.contains(&format!("session={SECRET}")));
    assert!(matching.contains("theme=dark"));
    let wrong_path = jar
        .cookies(&Url::parse("https://sub.example.com/public").unwrap())
        .unwrap();
    assert!(!wrong_path.to_str().unwrap().contains(SECRET));
    let insecure = jar
        .cookies(&Url::parse("http://sub.example.com/private").unwrap())
        .unwrap();
    assert!(!insecure.to_str().unwrap().contains(SECRET));
}

#[test]
fn netscape_host_only_paths_cannot_inject_domain_scope() {
    let directory = TempDir::new().unwrap();
    let origin = Url::parse("https://www.example.com/").unwrap();
    let sibling = Url::parse("https://attacker.example.com/").unwrap();
    let valid = secure_file(
        directory.path(),
        "host-only.txt",
        &format!("www.example.com\tFALSE\t/\tTRUE\t2147483647\tsession\t{SECRET}\n"),
    );
    let cookies = load_platform_cookies(&valid, &origin).unwrap();
    assert!(cookies
        .cookies(&origin)
        .unwrap()
        .to_str()
        .unwrap()
        .contains(SECRET));
    assert!(cookies.cookies(&sibling).is_none());

    for (name, path) in [
        ("attribute-injection.txt", "/; Domain=example.com"),
        ("non-ascii-path.txt", "/café"),
        ("delete-byte-path.txt", "/\u{7f}"),
    ] {
        let malicious = secure_file(
            directory.path(),
            name,
            &format!("www.example.com\tFALSE\t{path}\tTRUE\t2147483647\tsession\t{SECRET}\n"),
        );
        let error = load_platform_cookies(&malicious, &origin).unwrap_err();
        assert!(error.to_string().contains("cookie path is invalid"));
        assert!(!format!("{error:?} {error}").contains(SECRET));
    }
}

#[test]
fn expired_netscape_cookies_are_not_loaded() {
    let directory = TempDir::new().unwrap();
    let path = secure_file(
        directory.path(),
        "expired.txt",
        &format!(
            "# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tFALSE\t1\texpired\t{SECRET}\n.example.com\tTRUE\t/\tFALSE\t2147483647\tcurrent\tvalue\n"
        ),
    );
    let origin = Url::parse("https://example.com/").unwrap();

    let jar = load_platform_cookies(&path, &origin).unwrap();
    let header = jar.cookies(&origin).unwrap();
    let header = header.to_str().unwrap();

    assert!(!header.contains("expired="));
    assert!(!header.contains(SECRET));
    assert!(header.contains("current=value"));
}

#[test]
fn malformed_or_injected_cookie_input_is_rejected_without_secret_diagnostics() {
    let directory = TempDir::new().unwrap();
    let origin = Url::parse("https://example.com/").unwrap();
    for (name, contents) in [
        ("crlf.txt", format!("session={SECRET}\r\nX-Evil: yes")),
        ("bad-name.txt", format!("bad name={SECRET}")),
        ("bad-value.txt", format!("session={SECRET};still-secret")),
        ("placeholder.txt", "session=<paste>".into()),
        (
            "bad-net.txt",
            format!("evil.example\tTRUE\t/\tFALSE\t0\tsession\t{SECRET}"),
        ),
    ] {
        let path = secure_file(directory.path(), name, &contents);
        let error = load_platform_cookies(&path, &origin).unwrap_err();
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(SECRET), "{name}: {diagnostic}");
    }
}

#[test]
fn cookie_sources_have_redacted_debug_output() {
    for source in [
        CookieSource::RawHeader(format!("session={SECRET}")),
        CookieSource::NameValue(format!("session={SECRET}")),
        CookieSource::Netscape(format!(
            ".example.com\tTRUE\t/\tFALSE\t0\tsession\t{SECRET}"
        )),
    ] {
        let debug = format!("{source:?}");
        assert_eq!(debug, "<redacted cookie source>");
        assert!(!debug.contains(SECRET));
    }
}

#[test]
fn loaded_platform_cookie_state_has_redacted_debug_and_remains_a_cookie_store() {
    fn assert_cookie_store<T: CookieStore + std::fmt::Debug>(_cookies: &T) {}

    let directory = TempDir::new().unwrap();
    let origin = Url::parse("https://example.com/").unwrap();
    let path = secure_file(
        directory.path(),
        "redacted-state.txt",
        &format!("session={SECRET}"),
    );

    let cookies = load_platform_cookies(&path, &origin).unwrap();
    assert_cookie_store(&cookies);
    assert!(cookies.cookies(&origin).is_some());
    let diagnostic = format!("{cookies:?}");
    assert_eq!(diagnostic, "<redacted platform cookies>");
    assert!(!diagnostic.contains(SECRET));
}

#[test]
fn valid_quoted_cookie_values_work_for_lines_and_netscape_without_relaxing_octets() {
    let directory = TempDir::new().unwrap();
    let origin = Url::parse("https://www.linkedin.com/").unwrap();
    let lines = secure_file(
        directory.path(),
        "quoted-lines.txt",
        "JSESSIONID=\"ajax:123\"",
    );
    let netscape = secure_file(
        directory.path(),
        "quoted-netscape.txt",
        ".linkedin.com\tTRUE\t/\tTRUE\t2147483647\tJSESSIONID\t\"ajax:123\"\n",
    );

    for path in [lines, netscape] {
        let cookies = load_platform_cookies(&path, &origin).unwrap();
        let header = cookies.cookies(&origin).unwrap();
        assert!(header.to_str().unwrap().contains("JSESSIONID=\"ajax:123\""));
    }

    for value in ["\"bad;value\"", "\"bad\\\"quote\"", "\"bad\tvalue\""] {
        let path = secure_file(
            directory.path(),
            &format!("bad-quoted-{}.txt", value.len()),
            &format!("JSESSIONID={value}"),
        );
        assert!(load_platform_cookies(&path, &origin).is_err());
    }
}

#[cfg(unix)]
#[test]
fn cookie_loader_uses_one_nofollow_descriptor_instead_of_reopening_the_path() {
    let source = include_str!("../src/cookies.rs");
    assert!(source.contains("O_NOFOLLOW"));
    assert!(source.contains("O_NONBLOCK"));
    assert!(!source.contains("fs::metadata(path)"));
    assert!(!source.contains("fs::read(path)"));
}

#[cfg(unix)]
#[test]
fn fifo_cookie_loader_subprocess_helper() {
    let Some(path) = std::env::var_os("RSCRAPER_FIFO_COOKIE_HELPER") else {
        return;
    };
    let origin = Url::parse("https://example.com/").unwrap();
    let error = load_platform_cookies(Path::new(&path), &origin).unwrap_err();
    assert!(error.to_string().contains("regular file"));
}

#[cfg(unix)]
#[test]
fn unix_fifo_cookie_file_is_rejected_within_a_deadline() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let fifo = directory.path().join("cookies.fifo");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success());
    fs::set_permissions(&fifo, fs::Permissions::from_mode(0o600)).unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fifo_cookie_loader_subprocess_helper",
            "--nocapture",
        ])
        .env("RSCRAPER_FIFO_COOKIE_HELPER", &fifo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "FIFO loader helper failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("cookie loader blocked while opening a FIFO");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn oversized_cookie_files_and_cookie_counts_are_bounded() {
    let directory = TempDir::new().unwrap();
    let origin = Url::parse("https://example.com/").unwrap();
    let huge = secure_file(directory.path(), "huge.txt", &"x".repeat(70 * 1024));
    assert!(load_platform_cookies(&huge, &origin).is_err());

    let many = (0..200)
        .map(|index| format!("cookie{index}=value"))
        .collect::<Vec<_>>()
        .join("\n");
    let many = secure_file(directory.path(), "many.txt", &many);
    assert!(load_platform_cookies(&many, &origin).is_err());
}

#[cfg(unix)]
#[test]
fn unix_cookie_files_require_regular_non_symlink_owner_only_permissions() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let directory = TempDir::new().unwrap();
    let origin = Url::parse("https://example.com/").unwrap();
    let path = secure_file(directory.path(), "cookies.txt", "session=value");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let error = load_platform_cookies(&path, &origin).unwrap_err();
    assert!(error.to_string().contains("chmod 600"));

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    load_platform_cookies(&path, &origin).unwrap();

    let link = directory.path().join("cookies-link.txt");
    symlink(&path, &link).unwrap();
    let error = load_platform_cookies(&link, &origin).unwrap_err();
    assert!(error.to_string().contains("symlink"));
}

#[cfg(unix)]
#[test]
fn setup_validates_platform_before_mutation_and_uses_private_non_overwriting_modes() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let state = directory.path().join("state");
    let context = AppContext {
        fetch: FetchClient::builder()
            .policy(NetworkPolicy::PublicInternet)
            .build()
            .unwrap(),
        browser: None,
        config_dir: state.clone(),
    };

    let error = social::setup(&context, "unknown-platform").unwrap_err();
    assert!(error.to_string().contains("unknown-platform"));
    assert!(!state.exists());

    let setup = social::setup(&context, "twitter").unwrap();
    let cookie_path = setup.cookie_path.unwrap();
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&cookie_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::write(&cookie_path, "do-not-overwrite").unwrap();
    social::setup(&context, "twitter").unwrap();
    assert_eq!(fs::read_to_string(cookie_path).unwrap(), "do-not-overwrite");
}

fn secure_file(directory: &Path, name: &str, contents: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}
