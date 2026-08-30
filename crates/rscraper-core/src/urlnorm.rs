//! URL identity and navigation policy for bounded crawls.

use crate::{Error, Result};
use std::hash::{Hash, Hasher};
use url::{Host, Url};

const DESTRUCTIVE_ACTIONS: &[&str] = &[
    "logout",
    "signout",
    "delete",
    "remove",
    "destroy",
    "deactivate",
    "unsubscribe",
    "terminate",
];
const ACTION_QUERY_KEYS: &[&str] = &["action", "act", "do", "cmd", "command", "operation"];
const MAX_PERCENT_DECODE_PASSES: usize = 4;

/// Canonicalize an HTTP(S) crawl URL without changing path or query meaning.
pub fn normalize_url(url: &Url) -> Result<Url> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Policy("only HTTP(S) URLs are permitted".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Policy("URL credentials are not permitted".into()));
    }
    if url.host().is_none() {
        return Err(Error::Policy("URL host is required".into()));
    }

    let mut normalized = url.clone();
    normalized.set_fragment(None);
    if let Some(Host::Domain(domain)) = normalized.host() {
        let canonical = domain.trim_end_matches('.').to_ascii_lowercase();
        if canonical.is_empty() {
            return Err(Error::Policy("URL host is required".into()));
        }
        normalized
            .set_host(Some(&canonical))
            .map_err(|_| Error::InvalidInput("invalid URL host".into()))?;
    }
    let default_port = match normalized.scheme() {
        "http" => 80,
        "https" => 443,
        _ => unreachable!("scheme checked above"),
    };
    if normalized.port() == Some(default_port) {
        normalized
            .set_port(None)
            .map_err(|_| Error::InvalidInput("invalid URL port".into()))?;
    }
    Ok(normalized)
}

/// Resolve a reference against the fetched page's final URL and canonicalize it.
pub fn resolve_and_normalize(base: &Url, reference: &str) -> Result<Url> {
    let resolved = base
        .join(reference)
        .map_err(|_| Error::InvalidInput("invalid crawl link".into()))?;
    normalize_url(&resolved)
}

/// Whether two URLs have the same scheme, canonical host, and effective port.
pub fn same_origin(left: &Url, right: &Url) -> bool {
    normalized_origin_parts(left) == normalized_origin_parts(right)
}

/// Apply the same-origin boundary, optionally broadening only the host to subdomains.
pub fn within_origin_scope(base: &Url, candidate: &Url, include_subdomains: bool) -> bool {
    let Some((base_scheme, base_host, base_port)) = normalized_origin_parts(base) else {
        return false;
    };
    let Some((candidate_scheme, candidate_host, candidate_port)) =
        normalized_origin_parts(candidate)
    else {
        return false;
    };
    if base_scheme != candidate_scheme || base_port != candidate_port {
        return false;
    }
    if base_host == candidate_host {
        return true;
    }
    include_subdomains
        && candidate_host
            .strip_suffix(&base_host)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Reject links whose path or action query describes a destructive operation.
pub fn is_destructive_url(url: &Url) -> bool {
    if url.path_segments().is_some_and(|segments| {
        segments.into_iter().any(|segment| {
            decode_repeatedly(segment).is_none_or(|decoded| contains_destructive_action(&decoded))
        })
    }) {
        return true;
    }

    url.query().is_some_and(|query| {
        decode_repeatedly(query).is_none_or(|decoded| destructive_query(&decoded))
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Origin {
    key: String,
    robots_url: Url,
}

impl Origin {
    pub(crate) fn from_url(url: &Url) -> Result<Self> {
        let mut robots_url = normalize_url(url)?;
        robots_url.set_path("/robots.txt");
        robots_url.set_query(None);
        robots_url.set_fragment(None);
        let key = robots_url.origin().ascii_serialization();
        Ok(Self { key, robots_url })
    }

    pub(crate) fn robots_url(&self) -> Url {
        self.robots_url.clone()
    }
}

impl PartialEq for Origin {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Origin {}

impl Hash for Origin {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

fn normalized_origin_parts(url: &Url) -> Option<(String, String, u16)> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = match url.host()? {
        Host::Domain(domain) => domain.trim_end_matches('.').to_ascii_lowercase(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => address.to_string(),
    };
    Some((
        url.scheme().to_ascii_lowercase(),
        host,
        url.port_or_known_default()?,
    ))
}

fn contains_destructive_action(value: &str) -> bool {
    let mut previous = None;
    for token in value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
    {
        if DESTRUCTIVE_ACTIONS.contains(&token.as_str())
            || previous.as_deref().is_some_and(|previous| {
                matches!((previous, token.as_str()), ("log", "out") | ("sign", "out"))
            })
        {
            return true;
        }
        previous = Some(token);
    }
    false
}

fn destructive_query(query: &str) -> bool {
    query.split(['&', ';']).any(|field| {
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        let key = key.to_ascii_lowercase();
        contains_destructive_action(&key)
            || (has_action_query_root(&key) && contains_destructive_action(value))
    })
}

/// Accept a configured action key followed only by complete bracket
/// components. `foo[action]` deliberately remains a benign `foo` root.
fn has_action_query_root(key: &str) -> bool {
    let root_end = key.find('[').unwrap_or(key.len());
    if !ACTION_QUERY_KEYS.contains(&&key[..root_end]) {
        return false;
    }
    if root_end == key.len() {
        return true;
    }

    let bytes = key.as_bytes();
    let mut index = root_end;
    while index < bytes.len() {
        if bytes[index] != b'[' {
            return false;
        }
        index += 1;
        while index < bytes.len() && bytes[index] != b']' {
            if bytes[index] == b'[' {
                return false;
            }
            index += 1;
        }
        if index == bytes.len() {
            return false;
        }
        index += 1;
    }
    true
}

/// Decode a bounded number of layers. If another valid escape remains after
/// the cap, classify the value as ambiguous so policy callers can fail closed.
fn decode_repeatedly(value: &str) -> Option<String> {
    let mut current = value.to_owned();
    for _ in 0..MAX_PERCENT_DECODE_PASSES {
        let (decoded, changed) = percent_decode_once(&current);
        current = decoded;
        if !changed {
            return Some(current);
        }
    }
    if contains_percent_escape(&current) {
        None
    } else {
        Some(current)
    }
}

fn percent_decode_once(value: &str) -> (String, bool) {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut changed = false;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                changed = true;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    (String::from_utf8_lossy(&decoded).into_owned(), changed)
}

fn contains_percent_escape(value: &str) -> bool {
    value.as_bytes().windows(3).any(|window| {
        window[0] == b'%' && hex_value(window[1]).is_some() && hex_value(window[2]).is_some()
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
