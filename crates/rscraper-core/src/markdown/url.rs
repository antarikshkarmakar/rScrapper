use crate::{Error, Result};
use url::{Position, Url};

pub(super) struct BoundedDestination {
    pub(super) text: String,
    pub(super) chars: usize,
}

pub(super) fn safe_destination(raw: &str, base: Option<&Url>) -> Option<String> {
    normalize_destination(raw, base, usize::MAX, usize::MAX)
        .ok()
        .flatten()
        .map(|destination| destination.text)
}

pub(super) fn bounded_destination(
    raw: &str,
    base: Option<&Url>,
    available: usize,
    limit: usize,
) -> Result<Option<BoundedDestination>> {
    if available == usize::MAX {
        return Ok(safe_destination(raw, base).map(|text| BoundedDestination {
            chars: text.len(),
            text,
        }));
    }
    normalize_destination(raw, base, available, limit)
}

pub(super) fn destination_is_allowed_with_budget(
    raw: &str,
    base: Option<&Url>,
    available: usize,
    limit: usize,
) -> Result<bool> {
    Ok(normalize_destination(raw, base, available, limit)?.is_some())
}

fn normalize_destination(
    raw: &str,
    base: Option<&Url>,
    available: usize,
    limit: usize,
) -> Result<Option<BoundedDestination>> {
    if raw.is_empty() {
        return Ok(None);
    }

    let scheme = reference_scheme(raw);
    if let Some(scheme) = scheme {
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Ok(None);
        }
    } else if first_path_segment(raw).contains(':')
        || (leading_special_separators(raw) >= 2 && base.is_none())
    {
        return Ok(None);
    }

    if raw.chars().any(char::is_control) || !has_valid_percent_escapes(raw) {
        return Ok(None);
    }

    if let Some(scheme) = scheme {
        let remainder = &raw[scheme.len() + 1..];
        if let Some(base) = base.filter(|base| safe_absolute_url(base)) {
            if scheme.eq_ignore_ascii_case(base.scheme())
                && leading_special_separators(remainder) < 2
            {
                return normalize_relative_to_base(remainder, base, available, limit);
            }
        }
        return normalize_absolute(remainder, scheme, available, limit);
    }
    if leading_special_separators(raw) >= 2 {
        let Some(base) = base.filter(|base| safe_absolute_url(base)) else {
            return Ok(None);
        };
        return normalize_scheme_relative(raw, base.scheme(), available, limit);
    }
    if let Some(base) = base {
        if !safe_absolute_url(base) {
            return Ok(None);
        }
        return normalize_relative_to_base(raw, base, available, limit);
    }

    encode_destination(raw, available, limit).map(Some)
}

fn normalize_absolute(
    remainder: &str,
    scheme: &str,
    available: usize,
    limit: usize,
) -> Result<Option<BoundedDestination>> {
    let authority_and_rest = &remainder[leading_special_separators(remainder)..];
    let authority_end = authority_and_rest
        .find(['/', '\\', '?', '#'])
        .unwrap_or(authority_and_rest.len());
    let authority = &authority_and_rest[..authority_end];
    let rest = &authority_and_rest[authority_end..];
    let Some(prefix) = normalize_authority(scheme, authority, available, limit)? else {
        return Ok(None);
    };
    finish_absolute_reference(&prefix, None, rest, available, limit).map(Some)
}

fn normalize_scheme_relative(
    raw: &str,
    scheme: &str,
    available: usize,
    limit: usize,
) -> Result<Option<BoundedDestination>> {
    let authority_and_rest = &raw[leading_special_separators(raw)..];
    let authority_end = authority_and_rest
        .find(['/', '\\', '?', '#'])
        .unwrap_or(authority_and_rest.len());
    let authority = &authority_and_rest[..authority_end];
    let rest = &authority_and_rest[authority_end..];
    let Some(prefix) = normalize_authority(scheme, authority, available, limit)? else {
        return Ok(None);
    };
    finish_absolute_reference(&prefix, None, rest, available, limit).map(Some)
}

fn normalize_relative_to_base(
    raw: &str,
    base: &Url,
    available: usize,
    limit: usize,
) -> Result<Option<BoundedDestination>> {
    let prefix = &base[..Position::BeforePath];
    if prefix.len() > available {
        return Err(Error::BodyLimit { limit });
    }
    record_normalization_size(prefix.len());

    let (raw_path, raw_query, raw_fragment) = split_reference(raw);
    let preserve_base_path = raw_path.is_empty();
    let base_path = if raw_path.starts_with(['/', '\\']) {
        None
    } else if preserve_base_path {
        Some(base.path())
    } else {
        Some(base_directory(base.path()))
    };
    let path = normalize_path(base_path, raw_path, available - prefix.len(), limit)?;
    let mut candidate = bounded_string(prefix, available, limit)?;
    push_bounded(&mut candidate, &path, available, limit)?;

    match raw_query {
        Some(query) => {
            push_bounded(&mut candidate, "?", available, limit)?;
            push_bounded(&mut candidate, query, available, limit)?;
        }
        None if preserve_base_path => {
            if let Some(query) = base.query() {
                push_bounded(&mut candidate, "?", available, limit)?;
                push_bounded(&mut candidate, query, available, limit)?;
            }
        }
        None => {}
    }
    if let Some(fragment) = raw_fragment {
        push_bounded(&mut candidate, "#", available, limit)?;
        push_bounded(&mut candidate, fragment, available, limit)?;
    }

    finalize_candidate(candidate, available, limit).map(Some)
}

fn finish_absolute_reference(
    prefix: &str,
    base_path: Option<&str>,
    rest: &str,
    available: usize,
    limit: usize,
) -> Result<BoundedDestination> {
    let (path, query, fragment) = split_reference(rest);
    let path = normalize_path(
        base_path,
        path,
        available.saturating_sub(prefix.len()),
        limit,
    )?;
    let mut candidate = bounded_string(prefix, available, limit)?;
    push_bounded(&mut candidate, &path, available, limit)?;
    if let Some(query) = query {
        push_bounded(&mut candidate, "?", available, limit)?;
        push_bounded(&mut candidate, query, available, limit)?;
    }
    if let Some(fragment) = fragment {
        push_bounded(&mut candidate, "#", available, limit)?;
        push_bounded(&mut candidate, fragment, available, limit)?;
    }
    finalize_candidate(candidate, available, limit)
}

fn normalize_authority(
    scheme: &str,
    authority: &str,
    available: usize,
    limit: usize,
) -> Result<Option<String>> {
    const MAX_AUTHORITY_BYTES: usize = 1_024;
    if authority.is_empty() || authority.contains('@') {
        return Ok(None);
    }
    if authority.len() > MAX_AUTHORITY_BYTES {
        return Ok(None);
    }
    let required = scheme
        .len()
        .saturating_add(3)
        .saturating_add(authority.len())
        .saturating_add(1);
    let mut probe = String::with_capacity(required);
    probe.push_str(scheme);
    probe.push_str("://");
    probe.push_str(authority);
    probe.push('/');
    record_normalization_size(probe.len());
    let Some(parsed) = Url::parse(&probe).ok().filter(safe_absolute_url) else {
        return Ok(None);
    };
    let prefix = &parsed[..Position::BeforePath];
    if prefix.len() > available {
        return Err(Error::BodyLimit { limit });
    }
    record_normalization_size(prefix.len());
    Ok(Some(prefix.to_owned()))
}

fn normalize_path<'a>(
    base_path: Option<&'a str>,
    raw_path: &'a str,
    available: usize,
    limit: usize,
) -> Result<String> {
    let trailing_slash = if raw_path.is_empty() {
        base_path.is_some_and(|path| path.ends_with('/'))
    } else {
        raw_path.ends_with(['/', '\\'])
            || raw_path
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(is_dot_segment)
    };
    let mut skip = 0usize;
    let mut reverse_segments = Vec::new();
    let mut normalized_len = 1usize;

    retain_reverse_segments(
        raw_path,
        &mut skip,
        &mut reverse_segments,
        &mut normalized_len,
        available,
        limit,
    )?;
    if let Some(base_path) = base_path {
        retain_reverse_segments(
            base_path,
            &mut skip,
            &mut reverse_segments,
            &mut normalized_len,
            available,
            limit,
        )?;
    }

    if trailing_slash && !reverse_segments.is_empty() {
        normalized_len = normalized_len
            .checked_add(1)
            .filter(|length| *length <= available)
            .ok_or(Error::BodyLimit { limit })?;
    }
    let mut normalized = String::with_capacity(normalized_len);
    normalized.push('/');
    for (index, segment) in reverse_segments.iter().rev().enumerate() {
        if index > 0 {
            normalized.push('/');
        }
        normalized.push_str(segment);
    }
    if trailing_slash && !reverse_segments.is_empty() {
        normalized.push('/');
    }
    record_normalization_size(normalized.len());
    Ok(normalized)
}

fn retain_reverse_segments<'a>(
    path: &'a str,
    skip: &mut usize,
    reverse_segments: &mut Vec<&'a str>,
    normalized_len: &mut usize,
    available: usize,
    limit: usize,
) -> Result<()> {
    let mut segments = path.rsplit(['/', '\\']).peekable();
    let mut first = true;
    while let Some(segment) = segments.next() {
        record_url_byte_visits(segment.len().saturating_add(1));
        let edge_empty = segment.is_empty() && (first || segments.peek().is_none());
        first = false;
        if edge_empty || is_single_dot_segment(segment) {
            continue;
        }
        if is_double_dot_segment(segment) {
            *skip = skip.saturating_add(1);
            continue;
        }
        if *skip > 0 {
            *skip -= 1;
            continue;
        }
        let separator = usize::from(!reverse_segments.is_empty());
        *normalized_len = normalized_len
            .checked_add(separator.saturating_add(segment.len()))
            .filter(|length| *length <= available)
            .ok_or(Error::BodyLimit { limit })?;
        reverse_segments.push(segment);
    }
    Ok(())
}

fn is_dot_segment(segment: &str) -> bool {
    is_single_dot_segment(segment) || is_double_dot_segment(segment)
}

fn is_single_dot_segment(segment: &str) -> bool {
    segment == "." || segment.eq_ignore_ascii_case("%2e")
}

fn is_double_dot_segment(segment: &str) -> bool {
    segment == ".."
        || segment.eq_ignore_ascii_case(".%2e")
        || segment.eq_ignore_ascii_case("%2e.")
        || segment.eq_ignore_ascii_case("%2e%2e")
}

fn split_reference(raw: &str) -> (&str, Option<&str>, Option<&str>) {
    let (without_fragment, fragment) = raw
        .split_once('#')
        .map_or((raw, None), |(before, after)| (before, Some(after)));
    let (path, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(before, after)| {
            (before, Some(after))
        });
    (path, query, fragment)
}

fn base_directory(path: &str) -> &str {
    path.rfind('/').map_or("", |separator| &path[..=separator])
}

fn finalize_candidate(
    candidate: String,
    available: usize,
    limit: usize,
) -> Result<BoundedDestination> {
    record_normalization_size(candidate.len());
    let parsed = Url::parse(&candidate).map_err(|_| Error::BodyLimit { limit })?;
    if !safe_absolute_url(&parsed) {
        return Err(Error::BodyLimit { limit });
    }
    record_normalization_size(parsed.as_str().len());
    encode_destination(parsed.as_str(), available, limit)
}

fn bounded_string(value: &str, available: usize, limit: usize) -> Result<String> {
    if value.len() > available {
        return Err(Error::BodyLimit { limit });
    }
    let mut text = String::with_capacity(value.len());
    text.push_str(value);
    record_normalization_size(text.len());
    Ok(text)
}

fn push_bounded(target: &mut String, value: &str, available: usize, limit: usize) -> Result<()> {
    let next = target
        .len()
        .checked_add(value.len())
        .filter(|length| *length <= available)
        .ok_or(Error::BodyLimit { limit })?;
    target.push_str(value);
    record_normalization_size(next);
    Ok(())
}

fn reference_scheme(raw: &str) -> Option<&str> {
    let mut chars = raw.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    for (index, ch) in chars {
        match ch {
            ':' => return Some(&raw[..index]),
            '/' | '?' | '#' => return None,
            _ if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.') => {}
            _ => return None,
        }
    }
    None
}

fn first_path_segment(raw: &str) -> &str {
    let path_end = raw.find(['?', '#']).unwrap_or(raw.len());
    raw[..path_end]
        .split(['/', '\\'])
        .next()
        .unwrap_or_default()
}

fn leading_special_separators(raw: &str) -> usize {
    raw.bytes()
        .take_while(|byte| matches!(byte, b'/' | b'\\'))
        .count()
}

fn has_valid_percent_escapes(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        record_url_byte_visits(1);
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len()
            || !bytes[index + 1].is_ascii_hexdigit()
            || !bytes[index + 2].is_ascii_hexdigit()
        {
            return false;
        }
        index += 3;
    }
    true
}

fn safe_absolute_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
}

fn encode_destination(value: &str, available: usize, limit: usize) -> Result<BoundedDestination> {
    let bytes = value.as_bytes();
    let mut text = String::with_capacity(value.len().min(available));
    let mut chars = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            reserve_encoded_chars(&mut chars, 3, available, limit)?;
            text.push('%');
            text.push(char::from(bytes[index + 1]));
            text.push(char::from(bytes[index + 2]));
            index += 3;
            continue;
        }

        if byte == b'&' {
            reserve_encoded_chars(&mut chars, 5, available, limit)?;
            text.push_str("&amp;");
        } else if is_destination_byte_safe(byte) {
            reserve_encoded_chars(&mut chars, 1, available, limit)?;
            text.push(char::from(byte));
        } else {
            reserve_encoded_chars(&mut chars, 3, available, limit)?;
            use std::fmt::Write as _;
            write!(text, "%{byte:02X}").expect("writing to String cannot fail");
        }
        index += 1;
    }
    record_normalization_size(text.len());
    Ok(BoundedDestination { text, chars })
}

fn reserve_encoded_chars(
    chars: &mut usize,
    additional: usize,
    available: usize,
    limit: usize,
) -> Result<()> {
    *chars = chars
        .checked_add(additional)
        .filter(|chars| *chars <= available)
        .ok_or(Error::BodyLimit { limit })?;
    Ok(())
}

fn is_destination_byte_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'/'
                | b':'
                | b'?'
                | b'#'
                | b'@'
                | b'!'
                | b'$'
                | b'\''
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b'['
                | b']'
        )
}

#[cfg(test)]
fn record_normalization_size(size: usize) {
    super::record_url_normalization_size(size);
}

#[cfg(not(test))]
fn record_normalization_size(_: usize) {}

#[cfg(test)]
fn record_url_byte_visits(count: usize) {
    for _ in 0..count {
        super::record_url_byte_visit();
    }
}

#[cfg(not(test))]
fn record_url_byte_visits(_: usize) {}
