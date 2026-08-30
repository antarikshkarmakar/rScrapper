//! Deterministic, tolerant robots.txt policy parsing.

use std::fmt;
use std::time::Duration;
use url::{Position, Url};

#[derive(Clone, Default)]
pub struct RobotsPolicy {
    rules: Vec<Rule>,
    crawl_delay: Option<Duration>,
}

impl fmt::Debug for RobotsPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RobotsPolicy")
            .field("rule_count", &self.rules.len())
            .field("crawl_delay_present", &self.crawl_delay.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
struct Rule {
    allow: bool,
    path: String,
}

#[derive(Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delays: Vec<Duration>,
}

impl RobotsPolicy {
    /// Parse robots.txt and select all equally-most-specific groups for `user_agent`.
    /// Unknown and malformed directives are ignored; an empty policy allows access.
    pub fn parse(contents: &str, user_agent: &str) -> Self {
        let groups = parse_groups(contents);
        let user_agent = user_agent.to_ascii_lowercase();
        let best_specificity = groups
            .iter()
            .filter_map(|group| group_specificity(group, &user_agent))
            .max();
        let Some(best_specificity) = best_specificity else {
            return Self::default();
        };

        let mut policy = Self::default();
        for group in groups
            .into_iter()
            .filter(|group| group_specificity(group, &user_agent) == Some(best_specificity))
        {
            policy.rules.extend(group.rules);
            policy.crawl_delay = group
                .crawl_delays
                .into_iter()
                .chain(policy.crawl_delay)
                .max();
        }
        policy
    }

    /// Apply longest-prefix precedence, with Allow winning an equal-length tie.
    pub fn allows(&self, url: &Url) -> bool {
        let path_and_query = normalize_octets(&url[Position::BeforePath..Position::AfterQuery]);
        self.rules
            .iter()
            .filter(|rule| path_and_query.starts_with(&rule.path))
            .max_by(|left, right| {
                left.path
                    .len()
                    .cmp(&right.path.len())
                    .then_with(|| left.allow.cmp(&right.allow))
            })
            .is_none_or(|rule| rule.allow)
    }

    pub fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay
    }
}

fn parse_groups(contents: &str) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut current = Group::default();
    let mut has_directives = false;

    for raw_line in contents
        .strip_prefix('\u{feff}')
        .unwrap_or(contents)
        .lines()
    {
        let raw_line = raw_line.trim();
        if raw_line.is_empty() {
            if !current.agents.is_empty() {
                groups.push(current);
                current = Group::default();
                has_directives = false;
            }
            continue;
        }
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let field = field.trim().to_ascii_lowercase();
        let value = value.trim();
        match field.as_str() {
            "user-agent" => {
                if has_directives && !current.agents.is_empty() {
                    groups.push(current);
                    current = Group::default();
                    has_directives = false;
                }
                if !value.is_empty() {
                    current.agents.push(value.to_ascii_lowercase());
                }
            }
            "allow" | "disallow" if !current.agents.is_empty() => {
                has_directives = true;
                if !value.is_empty() {
                    current.rules.push(Rule {
                        allow: field == "allow",
                        path: normalize_octets(value),
                    });
                }
            }
            "crawl-delay" if !current.agents.is_empty() => {
                has_directives = true;
                if let Some(delay) = parse_delay(value) {
                    current.crawl_delays.push(delay);
                }
            }
            _ => {}
        }
    }
    if !current.agents.is_empty() {
        groups.push(current);
    }
    groups
}

fn group_specificity(group: &Group, user_agent: &str) -> Option<usize> {
    group
        .agents
        .iter()
        .filter_map(|agent| {
            if agent == "*" {
                Some(0)
            } else if user_agent.starts_with(agent) {
                Some(agent.len())
            } else {
                None
            }
        })
        .max()
}

fn normalize_octets(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut normalized = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                let octet = (high << 4) | low;
                if octet.is_ascii_alphanumeric() || matches!(octet, b'-' | b'.' | b'_' | b'~') {
                    normalized.push(char::from(octet));
                } else {
                    normalized.push('%');
                    normalized.push(hex_digit(octet >> 4));
                    normalized.push(hex_digit(octet & 0x0f));
                }
                index += 3;
                continue;
            }
        }

        let octet = bytes[index];
        if octet.is_ascii() {
            normalized.push(char::from(octet));
        } else {
            normalized.push('%');
            normalized.push(hex_digit(octet >> 4));
            normalized.push(hex_digit(octet & 0x0f));
        }
        index += 1;
    }
    normalized
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_digit(value: u8) -> char {
    char::from(if value < 10 {
        b'0' + value
    } else {
        b'A' + value - 10
    })
}

fn parse_delay(value: &str) -> Option<Duration> {
    let seconds = value.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let delay = Duration::try_from_secs_f64(seconds).ok()?;
    std::time::Instant::now().checked_add(delay)?;
    Some(delay)
}
