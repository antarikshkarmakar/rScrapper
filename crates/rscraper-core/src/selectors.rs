//! Element selection with CSS selectors, a documented XPath-style subset, and
//! a lightweight element memory layer.
//!
//! CSS selectors are delegated to the `scraper` crate. The XPath-style dialect
//! intentionally supports only `/` and `//` axes, element names or `*`,
//! attribute predicates, `contains(@attr, 'text')`, and one-based positions.

use crate::{Error, Result};
use ego_tree::{iter::Edge, NodeId};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

const DEFAULT_MINIMUM_SCORE: f64 = 0.5;
const TEXT_SNIPPET_CHARS: usize = 80;

#[cfg(test)]
static CANDIDATE_VISITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static COVERAGE_WORK: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
fn reset_candidate_visits() {
    CANDIDATE_VISITS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
fn reset_selector_counters() {
    CANDIDATE_VISITS.store(0, Ordering::Relaxed);
    COVERAGE_WORK.store(0, Ordering::Relaxed);
}

#[cfg(test)]
fn candidate_visits() -> usize {
    CANDIDATE_VISITS.load(Ordering::Relaxed)
}

#[cfg(test)]
fn coverage_work() -> usize {
    COVERAGE_WORK.load(Ordering::Relaxed)
}

#[cfg(test)]
fn record_candidate_visit() {
    CANDIDATE_VISITS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
fn record_candidate_visit() {}

#[cfg(test)]
fn record_coverage_work() {
    COVERAGE_WORK.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
fn record_coverage_work() {}

/// A selector in either dialect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Sel {
    Css(String),
    Xpath(String),
}

impl Sel {
    /// Parse and validate a string into the right dialect based on its prefix.
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(parse_error(
                "selector",
                "selector expression cannot be empty",
            ));
        }

        if trimmed.starts_with('/') {
            parse_xpath(trimmed)?;
            Ok(Sel::Xpath(trimmed.to_string()))
        } else {
            parse_css(trimmed)?;
            Ok(Sel::Css(trimmed.to_string()))
        }
    }

    /// Select elements from an HTML document. Returned refs borrow from `document`.
    pub fn select<'a>(&self, document: &'a Html) -> Result<Vec<ElementRef<'a>>> {
        match self {
            Sel::Css(css) => {
                let selector = parse_css(css)?;
                Ok(document.select(&selector).collect())
            }
            Sel::Xpath(xpath) => {
                let steps = parse_xpath(xpath)?;
                Ok(evaluate_xpath(document, &steps))
            }
        }
    }

    /// First matching element's text, trimmed with whitespace collapsed.
    pub fn first_text(&self, document: &Html) -> Result<Option<String>> {
        Ok(self
            .select(document)?
            .into_iter()
            .next()
            .map(|element| clean_text(element.text())))
    }
}

fn parse_css(input: &str) -> Result<Selector> {
    Selector::parse(input).map_err(|error| parse_error("css", format!("{error:?}")))
}

fn parse_error(kind: &'static str, message: impl Into<String>) -> Error {
    Error::Parse {
        kind,
        message: message.into(),
    }
}

/// Collapse runs of Unicode whitespace and trim.
pub fn clean_text<'a>(text: impl Iterator<Item = &'a str>) -> String {
    text.collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Child,
    Descendant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeTest {
    Any,
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Predicate {
    AttrExists(String),
    AttrEquals { name: String, value: String },
    AttrContains { name: String, needle: String },
    Position(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    axis: Axis,
    node_test: NodeTest,
    predicates: Vec<Predicate>,
}

fn parse_xpath(input: &str) -> Result<Vec<Step>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(parse_error("xpath", "xpath expression cannot be empty"));
    }
    if !trimmed.starts_with('/') {
        return Err(parse_error(
            "xpath",
            "xpath expression must start with '/' or '//'",
        ));
    }

    tokenize_xpath(trimmed)?
        .into_iter()
        .map(|(axis, step)| parse_step(axis, &step))
        .collect()
}

fn tokenize_xpath(input: &str) -> Result<Vec<(Axis, String)>> {
    let chars: Vec<char> = input.chars().collect();
    let mut steps = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        if chars[index] != '/' {
            return Err(parse_error("xpath", "expected step separator"));
        }

        let axis = if chars.get(index + 1) == Some(&'/') {
            index += 2;
            Axis::Descendant
        } else {
            index += 1;
            Axis::Child
        };

        if index >= chars.len() {
            return Err(parse_error("xpath", "missing step after separator"));
        }

        let start = index;
        let mut bracket_depth = 0usize;
        let mut quote = None;

        while index < chars.len() {
            let current = chars[index];

            if let Some(quote_char) = quote {
                if current == quote_char {
                    quote = None;
                }
            } else {
                match current {
                    '\'' | '"' => quote = Some(current),
                    '[' => bracket_depth += 1,
                    ']' => {
                        if bracket_depth == 0 {
                            return Err(parse_error("xpath", "unexpected closing bracket"));
                        }
                        bracket_depth -= 1;
                    }
                    '/' if bracket_depth == 0 => break,
                    _ => {}
                }
            }

            index += 1;
        }

        if quote.is_some() {
            return Err(parse_error("xpath", "unclosed quoted string"));
        }
        if bracket_depth != 0 {
            return Err(parse_error("xpath", "unclosed predicate bracket"));
        }

        let step = chars[start..index]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        if step.is_empty() {
            return Err(parse_error("xpath", "missing step after separator"));
        }
        steps.push((axis, step));
    }

    if steps.is_empty() {
        return Err(parse_error("xpath", "xpath expression has no steps"));
    }

    Ok(steps)
}

fn parse_step(axis: Axis, input: &str) -> Result<Step> {
    let chars: Vec<char> = input.chars().collect();
    let mut node_end = 0;

    while node_end < chars.len() && chars[node_end] != '[' {
        node_end += 1;
    }

    let node_name = chars[..node_end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    let node_test = parse_node_test(&node_name)?;
    let mut predicates = Vec::new();
    let mut index = node_end;

    while index < chars.len() {
        if chars[index] != '[' {
            return Err(parse_error(
                "xpath",
                "unexpected characters after node test",
            ));
        }

        let predicate_start = index + 1;
        index += 1;
        let mut bracket_depth = 1usize;
        let mut quote = None;

        while index < chars.len() {
            let current = chars[index];
            if let Some(quote_char) = quote {
                if current == quote_char {
                    quote = None;
                }
            } else {
                match current {
                    '\'' | '"' => quote = Some(current),
                    '[' => bracket_depth += 1,
                    ']' => {
                        bracket_depth -= 1;
                        if bracket_depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            index += 1;
        }

        if quote.is_some() {
            return Err(parse_error("xpath", "unclosed quoted string"));
        }
        if index >= chars.len() || bracket_depth != 0 {
            return Err(parse_error("xpath", "unclosed predicate bracket"));
        }

        let body = chars[predicate_start..index].iter().collect::<String>();
        predicates.push(parse_predicate(body.trim())?);
        index += 1;
    }

    Ok(Step {
        axis,
        node_test,
        predicates,
    })
}

fn parse_node_test(input: &str) -> Result<NodeTest> {
    if input == "*" {
        return Ok(NodeTest::Any);
    }
    if input.is_empty() {
        return Err(parse_error("xpath", "step is missing an element name"));
    }
    if input
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':'))
    {
        Ok(NodeTest::Name(input.to_ascii_lowercase()))
    } else {
        Err(parse_error(
            "xpath",
            format!("unsupported node test '{input}'"),
        ))
    }
}

fn parse_predicate(input: &str) -> Result<Predicate> {
    if input.is_empty() {
        return Err(parse_error("xpath", "predicate cannot be empty"));
    }

    if input.chars().all(|ch| ch.is_ascii_digit()) {
        let position = input
            .parse::<usize>()
            .map_err(|_| parse_error("xpath", "position predicate is out of range"))?;
        if position == 0 {
            return Err(parse_error("xpath", "position predicate is one-based"));
        }
        return Ok(Predicate::Position(position));
    }

    if let Some(rest) = input.strip_prefix('@') {
        return parse_attribute_predicate(rest);
    }

    if let Some(rest) = input.strip_prefix("contains(") {
        return parse_contains_predicate(rest);
    }

    Err(parse_error(
        "xpath",
        format!("unsupported predicate '{input}'"),
    ))
}

fn parse_attribute_predicate(input: &str) -> Result<Predicate> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(parse_error("xpath", "attribute name cannot be empty"));
    }

    let Some(equals_index) = find_unquoted_char(trimmed, '=') else {
        return validate_attribute_name(trimmed).map(Predicate::AttrExists);
    };

    let name = trimmed[..equals_index].trim();
    validate_attribute_name(name)?;
    let value = parse_quoted_literal(trimmed[equals_index + 1..].trim())?;

    Ok(Predicate::AttrEquals {
        name: name.to_string(),
        value,
    })
}

fn parse_contains_predicate(input: &str) -> Result<Predicate> {
    let trimmed = input.trim();
    if !trimmed.ends_with(')') {
        return Err(parse_error("xpath", "contains predicate is missing ')'"));
    }

    let inner = trimmed[..trimmed.len() - 1].trim();
    let comma_index = find_unquoted_char(inner, ',')
        .ok_or_else(|| parse_error("xpath", "contains predicate requires two arguments"))?;
    let attr_arg = inner[..comma_index].trim();
    let value_arg = inner[comma_index + 1..].trim();
    let attr_name = attr_arg
        .strip_prefix('@')
        .ok_or_else(|| parse_error("xpath", "contains first argument must be @attribute"))?;
    let attr_name = validate_attribute_name(attr_name.trim())?;
    let needle = parse_quoted_literal(value_arg)?;

    Ok(Predicate::AttrContains {
        name: attr_name,
        needle,
    })
}

fn find_unquoted_char(input: &str, target: char) -> Option<usize> {
    let mut quote = None;
    for (index, current) in input.char_indices() {
        if let Some(quote_char) = quote {
            if current == quote_char {
                quote = None;
            }
        } else if matches!(current, '\'' | '"') {
            quote = Some(current);
        } else if current == target {
            return Some(index);
        }
    }
    None
}

fn validate_attribute_name(input: &str) -> Result<String> {
    let name = input.trim();
    if name.is_empty() {
        return Err(parse_error("xpath", "attribute name cannot be empty"));
    }
    if name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':'))
    {
        Ok(name.to_ascii_lowercase())
    } else {
        Err(parse_error(
            "xpath",
            format!("unsupported attribute name '{name}'"),
        ))
    }
}

fn parse_quoted_literal(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let mut chars = trimmed.char_indices();
    let Some((_, quote_char @ ('\'' | '"'))) = chars.next() else {
        return Err(parse_error("xpath", "attribute value must be quoted"));
    };

    for (index, current) in chars {
        if current == quote_char {
            if trimmed[index + current.len_utf8()..].trim().is_empty() {
                return Ok(trimmed[quote_char.len_utf8()..index].to_string());
            }
            return Err(parse_error(
                "xpath",
                "unexpected characters after quoted literal",
            ));
        }
    }

    Err(parse_error("xpath", "unclosed quoted string"))
}

fn evaluate_xpath<'a>(document: &'a Html, steps: &[Step]) -> Vec<ElementRef<'a>> {
    let document_root = document.root_element();
    let mut current = vec![document_root];

    for (index, step) in steps.iter().enumerate() {
        current = evaluate_step(document_root, &current, step, index == 0);
    }

    current
}

fn evaluate_step<'a>(
    document_root: ElementRef<'a>,
    current: &[ElementRef<'a>],
    step: &Step,
    first_step: bool,
) -> Vec<ElementRef<'a>> {
    let position_predicates: Vec<usize> = step
        .predicates
        .iter()
        .filter_map(|predicate| match predicate {
            Predicate::Position(position) => Some(*position),
            _ => None,
        })
        .collect();
    let non_position_predicates: Vec<&Predicate> = step
        .predicates
        .iter()
        .filter(|predicate| !matches!(predicate, Predicate::Position(_)))
        .collect();
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    if first_step && step.axis == Axis::Child {
        for element in current {
            record_coverage_work();
            push_candidate(
                &mut candidates,
                &mut seen,
                *element,
                step,
                &non_position_predicates,
            );
        }
    } else {
        let context_ids = current
            .iter()
            .map(|element| element.id())
            .collect::<HashSet<_>>();
        let mut active_context_depth = 0usize;

        for edge in document_root.traverse() {
            match edge {
                Edge::Open(node) => {
                    let Some(element) = ElementRef::wrap(node) else {
                        continue;
                    };
                    record_coverage_work();

                    let is_context = context_ids.contains(&element.id());
                    let is_candidate = match step.axis {
                        Axis::Child => is_child_of_any_context(&element, &context_ids),
                        Axis::Descendant if first_step => true,
                        Axis::Descendant => active_context_depth > 0,
                    };

                    if is_candidate {
                        push_candidate(
                            &mut candidates,
                            &mut seen,
                            element,
                            step,
                            &non_position_predicates,
                        );
                    }

                    if is_context {
                        active_context_depth += 1;
                    }
                }
                Edge::Close(node) => {
                    if let Some(element) = ElementRef::wrap(node) {
                        if context_ids.contains(&element.id()) {
                            active_context_depth = active_context_depth.saturating_sub(1);
                        }
                    }
                }
            }
        }
    }

    for position in position_predicates {
        candidates = candidates.get(position - 1).copied().into_iter().collect();
    }

    candidates
}

fn is_child_of_any_context(element: &ElementRef<'_>, context_ids: &HashSet<NodeId>) -> bool {
    element
        .parent()
        .and_then(ElementRef::wrap)
        .is_some_and(|parent| context_ids.contains(&parent.id()))
}

fn push_candidate<'a>(
    candidates: &mut Vec<ElementRef<'a>>,
    seen: &mut HashSet<NodeId>,
    element: ElementRef<'a>,
    step: &Step,
    non_position_predicates: &[&Predicate],
) {
    record_candidate_visit();
    if node_matches(&step.node_test, &element)
        && non_position_predicates
            .iter()
            .all(|predicate| predicate_matches(&element, predicate))
        && seen.insert(element.id())
    {
        candidates.push(element);
    }
}

fn node_matches(node_test: &NodeTest, element: &ElementRef<'_>) -> bool {
    match node_test {
        NodeTest::Any => true,
        NodeTest::Name(name) => element.value().name().eq_ignore_ascii_case(name),
    }
}

fn predicate_matches(element: &ElementRef<'_>, predicate: &Predicate) -> bool {
    match predicate {
        Predicate::AttrExists(name) => element.value().attr(name).is_some(),
        Predicate::AttrEquals { name, value } => element.value().attr(name) == Some(value.as_str()),
        Predicate::AttrContains { name, needle } => element
            .value()
            .attr(name)
            .is_some_and(|value| value.contains(needle)),
        Predicate::Position(_) => true,
    }
}

/// A stable-ish fingerprint of an element used to re-find it after layout changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Normalized HTML tag name.
    pub tag: String,
    /// First visible text characters after Unicode whitespace normalization.
    pub text_snippet: String,
    /// Stable attributes worth keying on: id, data-* attributes, and classes.
    pub attrs: HashMap<String, String>,
}

impl Fingerprint {
    fn from_element(element: &ElementRef<'_>) -> Self {
        let mut attrs = HashMap::new();
        for (name, value) in element.value().attrs() {
            if matches!(name, "id" | "class") || name.starts_with("data-") {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    attrs.insert(name.to_string(), trimmed.to_string());
                }
            }
        }

        let text_snippet = clean_text(element.text())
            .chars()
            .take(TEXT_SNIPPET_CHARS)
            .collect();

        Self {
            tag: element.value().name().to_string(),
            text_snippet,
            attrs,
        }
    }

    /// How well this fingerprint matches an element. 1.0 is strongest.
    fn score(&self, element: &ElementRef<'_>) -> f64 {
        if !element.value().name().eq_ignore_ascii_case(&self.tag) {
            return 0.0;
        }

        let mut score = 0.2;
        if let Some(id) = self.attrs.get("id") {
            if element.value().attr("id") == Some(id.as_str()) {
                score += 0.45;
            }
        }

        for (name, value) in self
            .attrs
            .iter()
            .filter(|(name, _)| name.starts_with("data-"))
        {
            if element.value().attr(name) == Some(value.as_str()) {
                score += 0.2;
            }
        }

        score += self.class_score(element) * 0.2;

        let element_text = clean_text(element.text());
        if !self.text_snippet.is_empty() && element_text.starts_with(&self.text_snippet) {
            score += 0.35;
        }

        score.min(1.0)
    }

    fn class_score(&self, element: &ElementRef<'_>) -> f64 {
        let Some(stored_class) = self.attrs.get("class") else {
            return 0.0;
        };
        let stored_tokens = class_tokens(stored_class);
        if stored_tokens.is_empty() {
            return 0.0;
        }

        let element_tokens = element
            .value()
            .attr("class")
            .map(class_tokens)
            .unwrap_or_default();
        let overlap = stored_tokens
            .iter()
            .filter(|token| element_tokens.contains(*token))
            .count();

        overlap as f64 / stored_tokens.len() as f64
    }
}

fn class_tokens(input: &str) -> HashSet<&str> {
    input.split_whitespace().collect()
}

/// Remembers selected elements so they can be re-found after a site redesign.
#[derive(Debug, Serialize, Deserialize)]
pub struct SelectorMemory {
    /// Named fingerprints: `name -> fingerprint`.
    pub entries: HashMap<String, Fingerprint>,
    /// Minimum score required for a fallback match.
    #[serde(default = "default_minimum_score")]
    pub minimum_score: f64,
}

impl Default for SelectorMemory {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            minimum_score: DEFAULT_MINIMUM_SCORE,
        }
    }
}

fn default_minimum_score() -> f64 {
    DEFAULT_MINIMUM_SCORE
}

impl SelectorMemory {
    /// Create empty memory with the default minimum match score.
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember the first element matched by `selector` under a stable `name`.
    pub fn remember(&mut self, name: &str, selector: &Sel, document: &Html) -> Option<()> {
        let element = selector.select(document).ok()?.into_iter().next()?;
        self.entries
            .insert(name.to_string(), Fingerprint::from_element(&element));
        Some(())
    }

    /// Re-find a remembered element in a possibly redesigned document.
    pub fn find<'a>(&self, name: &str, document: &'a Html) -> Option<ElementRef<'a>> {
        if !self.minimum_score.is_finite() {
            return None;
        }

        let fingerprint = self.entries.get(name)?;
        let mut best = None;

        for element in std::iter::once(document.root_element())
            .chain(document.root_element().descendent_elements())
            .filter(|element| {
                element
                    .value()
                    .name()
                    .eq_ignore_ascii_case(&fingerprint.tag)
            })
        {
            let score = fingerprint.score(&element);
            if score >= self.minimum_score
                && best
                    .as_ref()
                    .is_none_or(|(best_score, _): &(f64, ElementRef<'a>)| score > *best_score)
            {
                best = Some((score, element));
            }
        }

        best.map(|(_, element)| element)
    }

    /// Serialize to stable JSON for persistence across runs.
    pub fn to_json(&self) -> anyhow::Result<String> {
        if !self.minimum_score.is_finite() {
            anyhow::bail!("minimum_score must be finite");
        }

        #[derive(Serialize)]
        struct StableFingerprint<'a> {
            tag: &'a str,
            text_snippet: &'a str,
            attrs: BTreeMap<&'a str, &'a str>,
        }

        #[derive(Serialize)]
        struct StableMemory<'a> {
            entries: BTreeMap<&'a str, StableFingerprint<'a>>,
            minimum_score: f64,
        }

        let entries = self
            .entries
            .iter()
            .map(|(name, fingerprint)| {
                let attrs = fingerprint
                    .attrs
                    .iter()
                    .map(|(attr_name, value)| (attr_name.as_str(), value.as_str()))
                    .collect();
                (
                    name.as_str(),
                    StableFingerprint {
                        tag: &fingerprint.tag,
                        text_snippet: &fingerprint.text_snippet,
                        attrs,
                    },
                )
            })
            .collect();

        Ok(serde_json::to_string(&StableMemory {
            entries,
            minimum_score: self.minimum_score,
        })?)
    }

    /// Deserialize selector memory from JSON.
    pub fn from_json(input: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(input)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_div_chain(depth: usize) -> String {
        let mut html = String::from("<html><body>");
        for index in 0..depth {
            html.push_str(&format!("<div id=\"d{index}\">"));
        }
        html.push_str("<span id=\"target\">leaf</span>");
        for _ in 0..depth {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");
        html
    }

    fn sibling_divs(width: usize) -> String {
        let mut html = String::from("<html><body>");
        for index in 0..width {
            html.push_str(&format!(
                "<div id=\"d{index}\"><span id=\"s{index}\">leaf</span></div>"
            ));
        }
        html.push_str("</body></html>");
        html
    }

    #[test]
    fn descendant_axis_prunes_covered_nested_contexts_before_traversal() {
        let depth = 64;
        let document = Html::parse_document(&nested_div_chain(depth));

        reset_candidate_visits();
        let matches = Sel::parse("//div//span")
            .unwrap()
            .select(&document)
            .unwrap();
        let visits = candidate_visits();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].value().attr("id"), Some("target"));
        assert!(
            visits <= depth * 4,
            "descendant-axis evaluation visited {visits} candidates for depth {depth}"
        );
    }

    #[test]
    fn context_pruning_coverage_work_is_linear_for_nested_and_disjoint_contexts() {
        let depth = 128;
        let nested_document = Html::parse_document(&nested_div_chain(depth));

        reset_selector_counters();
        let nested_matches = Sel::parse("//div//span")
            .unwrap()
            .select(&nested_document)
            .unwrap();
        let nested_work = coverage_work();

        assert_eq!(nested_matches.len(), 1);
        assert!(
            nested_work <= depth * 6,
            "nested context pruning did {nested_work} coverage units for depth {depth}"
        );

        let width = 2_048;
        let disjoint_document = Html::parse_document(&sibling_divs(width));

        reset_selector_counters();
        let disjoint_matches = Sel::parse("//div/*")
            .unwrap()
            .select(&disjoint_document)
            .unwrap();
        let disjoint_work = coverage_work();

        assert_eq!(disjoint_matches.len(), width);
        assert!(
            disjoint_work <= width * 6,
            "disjoint context pruning did {disjoint_work} coverage units for width {width}"
        );
    }
}
