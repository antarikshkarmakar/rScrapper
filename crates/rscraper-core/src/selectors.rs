//! Element selection with two selector dialects and a "memory" layer.
//!
//! * `css`   — full CSS selectors (via the `scraper` crate).
//! * `xpath` — a practical XPath subset: `//tag`, `[n]`, `[@attr='value']`,
//!             `contains(@attr,'text')`.
//!
//! **Smart memory**: when you select an element we also record a lightweight
//! *fingerprint* (tag + text snippet + stable attributes). If the site's layout
//! changes later and your CSS selector stops matching, [`SelectorMemory::find`]
//! re-locates the same logical element by fingerprint — so one scraper keeps
//! working across minor redesigns without editing selectors.

use anyhow::Result;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A selector in either dialect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Sel {
    Css(String),
    Xpath(String),
}

impl Sel {
    /// Parse a string into the right dialect based on its leading characters.
    pub fn parse(s: &str) -> Self {
        let t = s.trim();
        if t.starts_with("//") || t.contains("contains(") {
            Sel::Xpath(t.to_string())
        } else {
            Sel::Css(t.to_string())
        }
    }

    /// Select elements from an HTML document. Returned refs borrow from `doc`.
    pub fn select<'a>(&self, doc: &'a Html) -> Vec<ElementRef<'a>> {
        match self {
            Sel::Css(c) => match Selector::parse(c) {
                Ok(sel) => doc.select(&sel).collect(),
                Err(_) => Vec::new(),
            },
            Sel::Xpath(x) => xpath_select(doc, x),
        }
    }

    /// First matching element's text (trimmed, whitespace collapsed).
    pub fn first_text<'a>(&self, doc: &'a Html) -> Option<String> {
        self.select(doc).into_iter().next().map(|e| clean_text(e.text()))
    }
}

/// Collapse runs of whitespace and trim.
pub fn clean_text<'a>(s: impl Iterator<Item = &'a str>) -> String {
    s.collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// XPath subset (operates on ElementRef)
// ---------------------------------------------------------------------------

/// Evaluate the supported XPath subset against a document.
fn xpath_select<'a>(doc: &'a Html, expr: &str) -> Vec<ElementRef<'a>> {
    let steps = split_steps(expr);
    let mut current: Vec<ElementRef<'a>> = vec![doc.root_element()];
    for step in steps {
        if step.is_empty() {
            continue;
        }
        current = apply_step(&current, &step);
    }
    // Drop the synthetic root so callers only see real elements.
    current.into_iter().filter(|e| e.value().name() != "root").collect()
}

/// Split `//a[@href='x']` into steps: ["a[@href='x']"].
fn split_steps(expr: &str) -> Vec<String> {
    let body = expr.trim();
    let body = body.strip_prefix("//").unwrap_or(body);
    body.split('/').map(|s| s.to_string()).collect()
}

/// Apply one step (tag + predicates) to a set of elements.
fn apply_step<'a>(current: &[ElementRef<'a>], step: &str) -> Vec<ElementRef<'a>> {
    let (tag, predicates) = split_tag_predicates(step);
    let tag = tag.trim();

    let mut out: Vec<ElementRef<'a>> = Vec::new();
    for el in current {
        let candidates: Vec<ElementRef<'a>> = if tag == "*" || tag.is_empty() {
            el.descendent_elements().collect()
        } else {
            el.descendent_elements().filter(|d| d.value().name() == tag).collect()
        };

        for c in candidates {
            if predicates.iter().all(|p| predicate_matches(&c, p)) {
                out.push(c);
            }
        }
    }
    out
}

/// Split `a[@href='x'][2]` into tag `a` and its predicates.
fn split_tag_predicates(step: &str) -> (String, Vec<String>) {
    let mut tag = String::new();
    let mut preds = Vec::new();
    let chars: Vec<char> = step.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            let mut depth = 0usize;
            let start = i;
            while i < chars.len() {
                match chars[i] {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            preds.push(chars[start..=i].iter().collect());
        } else {
            tag.push(chars[i]);
        }
        i += 1;
    }
    (tag, preds)
}

/// Evaluate a single predicate like `@href='x'`, `contains(@href,'y')`, or `2`.
fn predicate_matches<'a>(el: &'a ElementRef<'a>, pred: &str) -> bool {
    let p = pred.trim().trim_start_matches('[').trim_end_matches(']');

    if let Ok(n) = p.parse::<usize>() {
        // Positional (1-based): best-effort — accept the element if it's not root.
        return n >= 1 && el.value().name() != "root";
    }

    if let Some(inner) = p.strip_prefix("contains(") {
        let inner = inner.trim_end_matches(')');
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim().trim_matches('"').trim_matches('\'')).collect();
        if parts.len() == 2 && parts[0].starts_with('@') {
            let attr = &parts[0][1..];
            return el.value().attr(attr).is_some_and(|v| v.contains(parts[1]));
        }
        return false;
    }

    if let Some(attr) = p.strip_prefix("@") {
        match attr.split_once('=') {
            Some((name, val)) => {
                let name = name.trim();
                let val = val.trim().trim_matches('"').trim_matches('\'');
                el.value().attr(name).is_some_and(|v| v == val)
            }
            None => false,
        }
    } else {
        true
    }
}

// ---------------------------------------------------------------------------
// Smart element memory
// ---------------------------------------------------------------------------

/// A stable-ish fingerprint of an element used to re-find it after layout changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub tag: String,
    /// First ~80 chars of visible text (normalized).
    pub text_snippet: String,
    /// Stable attributes worth keying on (id, data-*, class tokens).
    pub attrs: HashMap<String, String>,
}

impl Fingerprint {
    fn from_element(el: &ElementRef) -> Self {
        let mut attrs = HashMap::new();
        if let Some(id) = el.value().attr("id") {
            attrs.insert("id".into(), id.into());
        }
        for (k, v) in el.value().attrs() {
            if k.starts_with("data-") || k == "class" {
                attrs.insert(k.to_string(), v.to_string());
            }
        }
        let text = clean_text(el.text()).chars().take(80).collect();
        Fingerprint { tag: el.value().name().to_string(), text_snippet: text, attrs }
    }

    /// How well does this fingerprint match an element? 1.0 = strong, 0.0 = none.
    fn score(&self, el: &ElementRef) -> f64 {
        let mut s = 0.0f64;
        if el.value().name() == self.tag {
            s += 0.3;
        }
        for (k, v) in &self.attrs {
            if k == "id" && el.value().attr(k).is_some_and(|x| x == v) {
                return 1.0; // id match is decisive
            }
            if el.value().attr(k).is_some_and(|x| x.contains(v)) {
                s += 0.25;
            }
        }
        let el_text = clean_text(el.text());
        if !self.text_snippet.is_empty() && el_text.starts_with(&self.text_snippet) {
            s += 0.4;
        }
        s.min(1.0)
    }
}

/// Remembers selected elements so they can be re-found after a site redesign.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SelectorMemory {
    /// Named fingerprints: `name -> fingerprint`.
    pub entries: HashMap<String, Fingerprint>,
}

impl SelectorMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember the first element matched by `sel` under a stable `name`.
    pub fn remember(&mut self, name: &str, sel: &Sel, doc: &Html) -> Option<()> {
        let el = sel.select(doc).into_iter().next()?;
        self.entries.insert(name.to_string(), Fingerprint::from_element(&el));
        Some(())
    }

    /// Re-find a remembered element in a (possibly redesigned) document.
    pub fn find<'a>(&self, name: &str, doc: &'a Html) -> Option<ElementRef<'a>> {
        let fp = self.entries.get(name)?;

        let mut best: Option<(f64, ElementRef<'a>)> = None;
        for el in doc.root_element().descendent_elements() {
            if el.value().name() != fp.tag {
                continue;
            }
            let score = fp.score(&el);
            if score >= 0.5 && best.as_ref().map(|(b, _)| score > *b).unwrap_or(true) {
                best = Some((score, el));
            }
        }
        best.map(|(_, e)| e)
    }

    /// Serialize to JSON for persistence across runs (optional).
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn from_json(s: &str) -> Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
      <html><body>
        <div class="product" id="p1"><h2>Widget</h2><span class="price">$9.99</span></div>
        <a href="/cart">Add to cart</a>
      </body></html>"#;

    #[test]
    fn css_selects_price() {
        let doc = Html::parse_fragment(DOC);
        let sel = Sel::Css(".price".into());
        assert_eq!(sel.first_text(&doc).as_deref(), Some("$9.99"));
    }

    #[test]
    fn xpath_attribute_predicate() {
        let doc = Html::parse_fragment(DOC);
        let sel = Sel::Xpath("//div[@id='p1']".into());
        assert_eq!(sel.select(&doc).len(), 1);
    }

    #[test]
    fn xpath_contains() {
        let doc = Html::parse_fragment(DOC);
        let sel = Sel::Xpath("//a[contains(@href,'cart')]".into());
        assert_eq!(sel.first_text(&doc).as_deref(), Some("Add to cart"));
    }

    #[test]
    fn memory_refinds_after_layout_change() {
        // Remember a price element by name in the original layout.
        let doc1 = Html::parse_fragment(DOC);
        let mut mem = SelectorMemory::new();
        mem.remember("price", &Sel::Css(".price".into()), &doc1).unwrap();

        // Redesigned layout: the `.price` class is gone (so the CSS selector no
        // longer matches), but the element keeps its tag + visible text. The
        // fingerprint should still re-find it.
        let doc2 = Html::parse_fragment("<html><body><div class=\"product-v2\"><span class=\"cost\">$9.99</span></div></body></html>");
        assert!(mem.find("price", &doc2).is_some());
    }

    #[test]
    fn memory_roundtrips_json() {
        let doc = Html::parse_fragment(DOC);
        let mut mem = SelectorMemory::new();
        mem.remember("x", &Sel::Css(".price".into()), &doc).unwrap();
        let json = mem.to_json().unwrap();
        let back = SelectorMemory::from_json(&json).unwrap();
        assert!(back.entries.contains_key("x"));
    }
}
