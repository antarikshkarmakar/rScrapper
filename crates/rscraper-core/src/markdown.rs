//! Bounded DOM-aware HTML to Markdown conversion.
mod block;
mod inline;
mod output;
mod root;
mod url;

use self::output::{FinalWriter, MAX_DOM_DEPTH};
use crate::{Error, OperationLimits, Result};
use ::url::Url;
use scraper::{ElementRef, Html};

#[cfg(test)]
thread_local! {
    static VISITED_TEXT_SCALARS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BLOCK_CURSOR_ADVANCES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VISITED_URL_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static URL_NORMALIZATION_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LIST_METADATA_ITEM_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TABLE_METADATA_ROW_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TABLE_METADATA_CELL_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TABLE_METADATA_ROW_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TABLE_ALIGNMENT_STATE_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_text_scalar_visit() {
    VISITED_TEXT_SCALARS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn reset_text_scalar_visits() {
    VISITED_TEXT_SCALARS.with(|count| count.set(0));
}

#[cfg(test)]
fn text_scalar_visits() -> usize {
    VISITED_TEXT_SCALARS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_block_cursor_advances(advances: usize) {
    BLOCK_CURSOR_ADVANCES.with(|count| count.set(count.get() + advances));
}

#[cfg(test)]
fn reset_block_cursor_advances() {
    BLOCK_CURSOR_ADVANCES.with(|count| count.set(0));
}

#[cfg(test)]
fn block_cursor_advances() -> usize {
    BLOCK_CURSOR_ADVANCES.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_url_byte_visit() {
    VISITED_URL_BYTES.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn reset_url_byte_visits() {
    VISITED_URL_BYTES.with(|count| count.set(0));
}

#[cfg(test)]
fn url_byte_visits() -> usize {
    VISITED_URL_BYTES.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_url_normalization_size(size: usize) {
    URL_NORMALIZATION_PEAK.with(|peak| peak.set(peak.get().max(size)));
}

#[cfg(test)]
fn reset_url_normalization_peak() {
    URL_NORMALIZATION_PEAK.with(|peak| peak.set(0));
}

#[cfg(test)]
fn url_normalization_peak() -> usize {
    URL_NORMALIZATION_PEAK.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_list_metadata_item_visit() {
    LIST_METADATA_ITEM_VISITS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn record_table_metadata_row_visit() {
    TABLE_METADATA_ROW_VISITS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn record_table_metadata_cell_visit() {
    TABLE_METADATA_CELL_VISITS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn record_table_metadata_row_size(size: usize) {
    TABLE_METADATA_ROW_PEAK.with(|peak| peak.set(peak.get().max(size)));
}

#[cfg(test)]
fn record_table_alignment_state_size(size: usize) {
    TABLE_ALIGNMENT_STATE_PEAK.with(|peak| peak.set(peak.get().max(size)));
}

#[cfg(test)]
fn reset_structured_metadata_proxies() {
    LIST_METADATA_ITEM_VISITS.with(|count| count.set(0));
    TABLE_METADATA_ROW_VISITS.with(|count| count.set(0));
    TABLE_METADATA_CELL_VISITS.with(|count| count.set(0));
    TABLE_METADATA_ROW_PEAK.with(|peak| peak.set(0));
    TABLE_ALIGNMENT_STATE_PEAK.with(|peak| peak.set(0));
}

#[cfg(test)]
fn structured_metadata_proxies() -> (usize, usize, usize, usize, usize) {
    (
        LIST_METADATA_ITEM_VISITS.with(std::cell::Cell::get),
        TABLE_METADATA_ROW_VISITS.with(std::cell::Cell::get),
        TABLE_METADATA_CELL_VISITS.with(std::cell::Cell::get),
        TABLE_METADATA_ROW_PEAK.with(std::cell::Cell::get),
        TABLE_ALIGNMENT_STATE_PEAK.with(std::cell::Cell::get),
    )
}

/// Options for fallible bounded HTML-to-Markdown conversion.
#[derive(Debug, Clone)]
pub struct MarkdownOptions {
    /// Base URL used to resolve safe relative links.
    pub base_url: Option<Url>,
    /// Maximum Unicode scalar values in the final Markdown.
    pub max_chars: usize,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            max_chars: OperationLimits::default().max_output_chars,
        }
    }
}

/// Convert with default limits, returning an empty string on conversion failure.
///
/// Call [`html_to_markdown_with_options`] when failure must be distinguished
/// from a legitimately empty document.
pub fn html_to_markdown(html: &str) -> String {
    html_to_markdown_with_options(html, &MarkdownOptions::default()).unwrap_or_default()
}

/// Convert HTML to Markdown while enforcing depth, link, and output bounds.
pub fn html_to_markdown_with_options(html: &str, options: &MarkdownOptions) -> Result<String> {
    let document = Html::parse_document(html);
    validate_dom_depth(document.root_element())?;
    let root = root::select_content_root(&document, options)?;
    let mut writer = FinalWriter::new(options.max_chars);
    block::render(root, options, &mut writer)?;
    writer.finish()
}

fn validate_dom_depth(root: ElementRef<'_>) -> Result<()> {
    let mut pending = vec![(root, 0usize, false)];
    while let Some((element, depth, visit_previous)) = pending.pop() {
        check_depth(depth)?;
        if visit_previous {
            if let Some(sibling) = previous_element_sibling(element) {
                pending.push((sibling, depth, true));
            }
        }
        if let Some(child) = element.child_elements().last() {
            pending.push((child, depth + 1, true));
        }
    }
    Ok(())
}

fn previous_element_sibling<'a>(element: ElementRef<'a>) -> Option<ElementRef<'a>> {
    let mut sibling = element.prev_sibling();
    while let Some(node) = sibling {
        if let Some(element) = ElementRef::wrap(node) {
            return Some(element);
        }
        sibling = node.prev_sibling();
    }
    None
}

fn check_depth(depth: usize) -> Result<()> {
    if depth > MAX_DOM_DEPTH {
        Err(Error::Parse {
            kind: "html",
            message: "document nesting exceeds 256 levels".into(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod traversal_tests {
    use super::{
        block, block_cursor_advances, html_to_markdown_with_options, reset_block_cursor_advances,
        reset_structured_metadata_proxies, reset_text_scalar_visits, reset_url_byte_visits,
        reset_url_normalization_peak, structured_metadata_proxies, text_scalar_visits,
        url_byte_visits, url_normalization_peak, MarkdownOptions,
    };
    use crate::Error;
    use scraper::{Html, Selector};

    #[test]
    fn nested_wrappers_and_links_stop_visiting_text_when_the_budget_is_exhausted() {
        let body = "x".repeat(100_000);
        let html = format!("<p><strong><em><a href=\"/target\">{body}</a></em></strong></p>");
        reset_text_scalar_visits();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 20,
            },
        );
        let visits = text_scalar_visits();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 20 })));
        assert!(
            visits < 64,
            "tiny output budget visited {visits} text scalars from a 100,000-scalar payload"
        );
    }

    #[test]
    fn simple_wrapper_eligibility_does_not_prescan_later_text() {
        let body = "x".repeat(100_000);
        let html = format!("<p><strong>A</strong><strong>{body}</strong></p>");
        reset_text_scalar_visits();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 12,
            },
        );
        let visits = text_scalar_visits();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 12 })));
        assert!(
            visits < 64,
            "simple-run eligibility visited {visits} text scalars before the tiny budget failed"
        );
    }

    #[test]
    fn nested_long_whitespace_prefix_has_depth_independent_linear_work() {
        const SPACES: usize = 10_000;
        let html = format!(
            "<p><strong><em><del><span>{}x</span></del></em></strong></p>",
            " ".repeat(SPACES)
        );
        reset_text_scalar_visits();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 10,
            },
        );
        let visits = text_scalar_visits();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 10 })));
        assert!(
            visits <= SPACES * 2 + 64,
            "nested wrappers revisited a {SPACES}-scalar whitespace prefix: {visits} visits"
        );
    }

    #[test]
    fn huge_destinations_have_exact_linear_work_and_bounded_normalization_storage() {
        let absolute = format!("https://example.com/{}", "x".repeat(100_000));
        reset_url_byte_visits();
        reset_url_normalization_peak();
        let absolute_result = html_to_markdown_with_options(
            &format!("<a href=\"{absolute}\">x</a>"),
            &MarkdownOptions {
                base_url: None,
                max_chars: 8,
            },
        );
        let absolute_visits = url_byte_visits();
        let absolute_peak = url_normalization_peak();
        assert!(matches!(
            absolute_result,
            Err(Error::BodyLimit { limit: 8 })
        ));
        assert!(
            (absolute.len()..=absolute.len() * 2 + 64).contains(&absolute_visits),
            "absolute URL normalization visited {absolute_visits} bytes"
        );
        assert!(
            absolute_peak <= 24,
            "absolute URL normalization owned {absolute_peak} chars"
        );

        let forbidden = format!("javascript:{}", "x".repeat(100_000));
        reset_url_byte_visits();
        reset_url_normalization_peak();
        let forbidden_result = html_to_markdown_with_options(
            &format!("<a href=\"{forbidden}\">x</a>"),
            &MarkdownOptions {
                base_url: None,
                max_chars: 1,
            },
        )
        .unwrap();
        let forbidden_visits = url_byte_visits();
        assert!(
            forbidden_visits < 64,
            "forbidden URL validation visited {forbidden_visits} bytes"
        );
        assert_eq!(url_normalization_peak(), 0);
        assert_eq!(forbidden_result, "x");

        let collapsing = "a/../".repeat(20_000);
        reset_url_byte_visits();
        reset_url_normalization_peak();
        let collapsed = html_to_markdown_with_options(
            &format!("<a href=\"{collapsing}\">x</a>"),
            &MarkdownOptions {
                base_url: Some(url::Url::parse("https://example.com/base/").unwrap()),
                max_chars: 30,
            },
        )
        .unwrap();
        let collapsing_visits = url_byte_visits();
        assert_eq!(collapsed, "[x](https://example.com/base/)");
        assert!(
            collapsing_visits <= collapsing.len() * 4 + 128,
            "collapsing URL normalization visited {collapsing_visits} bytes"
        );
        assert!(url_normalization_peak() <= 30);

        let backslash_collapsing = r"a\..\".repeat(20_000);
        reset_url_byte_visits();
        reset_url_normalization_peak();
        let backslash_collapsed = html_to_markdown_with_options(
            &format!(r#"<a href="{backslash_collapsing}">x</a>"#),
            &MarkdownOptions {
                base_url: Some(url::Url::parse("https://example.com/base/").unwrap()),
                max_chars: 30,
            },
        )
        .unwrap();
        let backslash_visits = url_byte_visits();
        assert_eq!(backslash_collapsed, "[x](https://example.com/base/)");
        assert!(
            backslash_visits <= backslash_collapsing.len() * 4 + 128,
            "backslash URL normalization visited {backslash_visits} bytes"
        );
        assert!(url_normalization_peak() <= 30);
    }

    #[test]
    fn large_base_with_short_root_reference_uses_bounded_normalization_storage() {
        let base = format!("https://example.com/{}/", "segment/".repeat(20_000));
        reset_url_normalization_peak();

        let markdown = html_to_markdown_with_options(
            "<a href=\"/x\">x</a>",
            &MarkdownOptions {
                base_url: Some(url::Url::parse(&base).unwrap()),
                max_chars: 26,
            },
        )
        .unwrap();
        let peak = url_normalization_peak();

        assert_eq!(markdown, "[x](https://example.com/x)");
        assert!(
            (1..=26).contains(&peak),
            "normalization owned {peak} chars for a 26-character output"
        );
    }

    #[test]
    fn wide_transparent_container_advances_the_block_cursor_linearly() {
        const SIBLINGS: usize = 1_000;
        let html = format!(
            "<body><div>{}</div></body>",
            "<span>x</span>".repeat(SIBLINGS)
        );
        reset_block_cursor_advances();

        let markdown = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: SIBLINGS,
            },
        )
        .unwrap();
        let advances = block_cursor_advances();

        assert_eq!(markdown, "x".repeat(SIBLINGS));
        assert!(
            advances <= SIBLINGS + 8,
            "{SIBLINGS} siblings required {advances} block iterator advances"
        );
    }

    #[test]
    fn wide_table_metadata_does_not_render_cell_payloads_before_a_tiny_limit_fails() {
        const CELLS: usize = 1_000;
        let html = format!(
            "<table><tr>{}</tr></table>",
            "<td>payload</td>".repeat(CELLS)
        );
        reset_text_scalar_visits();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 4,
            },
        );
        let visits = text_scalar_visits();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 4 })));
        assert!(
            visits < 64,
            "table metadata rendered cell payloads before the tiny limit failed: {visits} visits"
        );
    }

    #[test]
    fn tiny_budget_bounds_ordered_list_metadata_item_visits() {
        const ITEMS: usize = 25_000;
        let html = format!("<ol>{}</ol>", "<li>x</li>".repeat(ITEMS));
        reset_structured_metadata_proxies();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 1,
            },
        );
        let (item_visits, _, _, _, _) = structured_metadata_proxies();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 1 })));
        assert!(
            item_visits < 64,
            "one output character visited {item_visits} of {ITEMS} ordered-list items"
        );

        reset_structured_metadata_proxies();
        let moderate_result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 64,
            },
        );
        let (moderate_visits, _, _, _, _) = structured_metadata_proxies();
        assert!(matches!(
            moderate_result,
            Err(Error::BodyLimit { limit: 64 })
        ));
        assert!(
            moderate_visits < 32,
            "64 output characters visited {moderate_visits} of {ITEMS} ordered-list items"
        );
    }

    #[test]
    fn tiny_budget_bounds_table_row_metadata_and_owned_storage() {
        const ROWS: usize = 25_000;
        let html = format!(
            "<table><tbody>{}</tbody></table>",
            "<tr><td>x</td></tr>".repeat(ROWS)
        );
        reset_structured_metadata_proxies();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 4,
            },
        );
        let (_, row_visits, cell_visits, row_peak, alignment_peak) = structured_metadata_proxies();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 4 })));
        assert!(row_visits < 64, "visited {row_visits} of {ROWS} rows");
        assert!(cell_visits < 64, "visited {cell_visits} of {ROWS} cells");
        assert!(row_peak < 64, "retained {row_peak} row metadata entries");
        assert!(
            alignment_peak < 64,
            "retained {alignment_peak} alignment states"
        );

        reset_structured_metadata_proxies();
        let moderate_result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 64,
            },
        );
        let (_, moderate_rows, moderate_cells, moderate_row_peak, moderate_alignment_peak) =
            structured_metadata_proxies();
        assert!(matches!(
            moderate_result,
            Err(Error::BodyLimit { limit: 64 })
        ));
        assert!(moderate_rows < 32, "visited {moderate_rows} of {ROWS} rows");
        assert!(
            moderate_cells < 64,
            "visited {moderate_cells} of {ROWS} cells"
        );
        assert!(
            moderate_row_peak < 32,
            "retained {moderate_row_peak} row metadata entries"
        );
        assert!(
            moderate_alignment_peak < 32,
            "retained {moderate_alignment_peak} alignment states"
        );
    }

    #[test]
    fn tiny_budget_bounds_wide_table_cell_metadata_and_alignment_storage() {
        const CELLS: usize = 25_000;
        let html = format!("<table><tr>{}</tr></table>", "<td>x</td>".repeat(CELLS));
        reset_structured_metadata_proxies();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 4,
            },
        );
        let (_, row_visits, cell_visits, row_peak, alignment_peak) = structured_metadata_proxies();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 4 })));
        assert!(row_visits < 64, "visited {row_visits} rows");
        assert!(cell_visits < 64, "visited {cell_visits} of {CELLS} cells");
        assert!(row_peak < 64, "retained {row_peak} row metadata entries");
        assert!(
            alignment_peak < 64,
            "retained {alignment_peak} of {CELLS} alignment states"
        );
    }

    #[test]
    fn owned_table_walk_does_not_visit_nested_table_rows() {
        const NESTED_ROWS: usize = 10_000;
        let html = format!(
            "<table><tr><td><table>{}</table></td></tr></table>",
            "<tr><td>x</td></tr>".repeat(NESTED_ROWS)
        );
        reset_structured_metadata_proxies();

        let result = html_to_markdown_with_options(
            &html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 4,
            },
        );
        let (_, row_visits, cell_visits, _, _) = structured_metadata_proxies();

        assert!(matches!(result, Err(Error::BodyLimit { limit: 4 })));
        assert!(
            row_visits < 8,
            "owned-row discovery visited {row_visits} nested rows"
        );
        assert!(
            cell_visits < 8,
            "owned-row discovery visited {cell_visits} cells"
        );
    }

    #[test]
    fn preferred_table_root_stops_without_emission_metadata() {
        const LATER_ROWS: usize = 25_000;
        let document = Html::parse_fragment(&format!(
            "<table><tr><td>meaningful</td></tr>{}</table>",
            "<tr><td>later</td></tr>".repeat(LATER_ROWS)
        ));
        let selector = Selector::parse("table").unwrap();
        let table = document.select(&selector).next().unwrap();
        reset_structured_metadata_proxies();

        assert!(block::preferred_root_is_meaningful(table, &MarkdownOptions::default()).unwrap());
        let (_, row_visits, cell_visits, row_peak, alignment_peak) = structured_metadata_proxies();

        assert!(
            row_visits < 8,
            "preferred-root check visited {row_visits} rows"
        );
        assert!(
            cell_visits < 8,
            "preferred-root check visited {cell_visits} cells"
        );
        assert_eq!(row_peak, 0, "preferred-root check retained row metadata");
        assert_eq!(
            alignment_peak, 0,
            "preferred-root check retained alignment metadata"
        );
    }
}
