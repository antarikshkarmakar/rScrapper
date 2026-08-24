//! Convert HTML into clean, LLM-ready Markdown.
//!
//! We drop chrome (nav, footers, ads, scripts) and emit a compact document with
//! headings, lists, links, code blocks, tables, and blockquotes — exactly what an
//! agent wants to read.

use scraper::{ElementRef, Html};

/// Tags we never render (chrome / non-content).
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "iframe", "canvas",
    "nav", "footer", "header", "aside", "form", "button", "select", "option",
];

/// Class hints that mark an element as non-content (ads, widgets).
const SKIP_CLASS_HINTS: &[&str] = &[
    "ad", "ads", "advert", "advertisement", "banner", "promo", "cookie",
    "newsletter", "sidebar", "related-posts", "share", "social-share",
];

/// Convert an HTML document to clean Markdown.
pub fn html_to_markdown(html: &str) -> String {
    let doc = Html::parse_fragment(html);
    // Prefer the main content region if present; otherwise use <body> or root.
    let root_el = find_content_root(&doc).unwrap_or_else(|| doc.root_element());

    let mut out = String::new();
    render_children(root_el, &mut out);
    collapse_blank_lines(&out)
}

/// Find the best content container: <main>, [role=main], article, or body.
fn find_content_root<'a>(doc: &'a Html) -> Option<ElementRef<'a>> {
    for sel in ["main", "[role='main']", "article"] {
        if let Ok(s) = scraper::Selector::parse(sel) {
            if let Some(el) = doc.select(&s).next() {
                return Some(el);
            }
        }
    }
    None
}

/// Render the child elements of `el` into markdown.
fn render_children(el: ElementRef, out: &mut String) {
    for child in el.child_elements() {
        if is_skipped(&child) {
            continue;
        }
        render_element(child, out);
    }
}

/// Render a single element (dispatch by tag).
fn render_element(el: ElementRef, out: &mut String) {
    let tag = el.value().name();

    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = (tag.as_bytes()[1] - b'0') as usize;
            out.push('\n');
            for _ in 0..level {
                out.push('#');
            }
            out.push(' ');
            out.push_str(&inline_text(el));
            out.push_str("\n\n");
        }
        "p" => {
            let text = inline_text(el);
            if !text.is_empty() {
                out.push('\n');
                out.push_str(&text);
                out.push_str("\n\n");
            }
        }
        "ul" | "ol" => render_list(el, out),
        "pre" => {
            let code = el.text().collect::<String>();
            out.push_str("\n```\n");
            out.push_str(code.trim());
            out.push_str("\n```\n\n");
        }
        "blockquote" => {
            for line in inline_text(el).lines() {
                if !line.is_empty() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        "table" => render_table(el, out),
        // Block containers: recurse into their children.
        "div" | "section" | "article" | "figure" | "figcaption" | "li" | "td" | "th" | "tr" => {
            render_children(el, out);
        }
        // Inline elements: emit their inline text (links become [text](href)).
        _ => {
            push_inline(&el, out);
        }
    }
}

/// Render the inline content of an element (text + links) without block breaks.
fn push_inline(el: &ElementRef, out: &mut String) {
    // If this is a link, render it as [text](href).
    if el.value().name() == "a" {
        let text = inline_text(*el);
        if !text.is_empty() {
            if let Some(href) = el.value().attr("href") {
                out.push_str(&format!("[{}]({})", text, href));
            } else {
                out.push_str(&text);
            }
        }
        return;
    }

    // Otherwise emit this element's full inline text. `.text()` already includes
    // nested descendant text (e.g. <b>Bold</b> → "Bold"), so nothing is lost; we
    // just don't preserve emphasis markup, which is fine for LLM-ready output.
    let own = el.text().collect::<String>();
    if !own.trim().is_empty() {
        out.push_str(&collapse_ws(&own));
    }
}

/// Inline text of an element (no markdown), whitespace collapsed.
fn inline_text(el: ElementRef) -> String {
    let raw: String = el.text().collect();
    collapse_ws(&raw).trim().to_string()
}

fn render_list(el: ElementRef, out: &mut String) {
    let ordered = el.value().name() == "ol";
    let mut idx = 1usize;
    for child in el.child_elements() {
        if child.value().name() != "li" {
            continue;
        }
        out.push('\n');
        if ordered {
            out.push_str(&format!("{}. ", idx));
            idx += 1;
        } else {
            out.push_str("- ");
        }
        render_inline_children(child, out);
    }
}

/// Render an element's inline content: its direct text nodes plus child elements
/// (nested lists are rendered as lists). This is what makes `<li>B</li>` work —
/// the "B" lives in a text node, not a child element.
fn render_inline_children(el: ElementRef, out: &mut String) {
    for child in el.children() {
        if let Some(text_node) = child.value().as_text() {
            let t = collapse_ws(&text_node.to_string());
            if !t.is_empty() {
                out.push_str(&t);
            }
        } else if let Some(elem) = ElementRef::wrap(child) {
            match elem.value().name() {
                "ul" | "ol" => render_list(elem, out),
                _ => push_inline(&elem, out),
            }
        }
    }
}

fn render_table(el: ElementRef, out: &mut String) {
    let rows: Vec<ElementRef> = el.child_elements().collect();
    if rows.is_empty() {
        return;
    }
    out.push('\n');
    for (i, row) in rows.iter().enumerate() {
        let cells: Vec<String> = row.child_elements().map(|c| inline_text(c)).collect();
        if cells.is_empty() {
            continue;
        }
        out.push_str(&format!("| {} |", cells.join(" | ")));
        out.push('\n');
        if i == 0 {
            for _ in 0..cells.len() {
                out.push_str("| --- ");
            }
            out.push('|');
            out.push('\n');
        }
    }
    out.push('\n');
}

/// Collapse internal whitespace runs to single spaces.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reduce 3+ newlines to exactly two (blank line between blocks).
fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for ch in s.chars() {
        if ch == '\n' {
            blank_run += 1;
            if blank_run <= 2 {
                out.push(ch);
            }
        } else {
            blank_run = 0;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn is_skipped(el: &ElementRef) -> bool {
    if SKIP_TAGS.contains(&el.value().name()) {
        return true;
    }
    let class = el.value().attr("class").unwrap_or("");
    for token in class.split_whitespace() {
        if SKIP_CLASS_HINTS.iter().any(|h| token.contains(h)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_headings_and_paragraphs() {
        let md = html_to_markdown("<html><body><h1>Title</h1><p>Hello world.</p></body></html>");
        assert!(md.contains("# Title"));
        assert!(md.contains("Hello world."));
    }

    #[test]
    fn strips_nav_and_scripts() {
        let html = "<nav>Menu</nav><script>x=1;</script><main><p>Real content here.</p></main>";
        let md = html_to_markdown(html);
        assert!(md.contains("Real content"));
        assert!(!md.to_lowercase().contains("menu"));
    }

    #[test]
    fn renders_links_and_lists() {
        let html = "<ul><li><a href='/a'>A</a></li><li>B</li></ul>";
        let md = html_to_markdown(html);
        assert!(md.contains("[A](/a)"));
        assert!(md.contains("- B"));
    }

    #[test]
    fn renders_code_block() {
        let md = html_to_markdown("<pre><code>let x = 1;</code></pre>");
        assert!(md.contains("```"));
        assert!(md.contains("let x = 1;"));
    }
}
