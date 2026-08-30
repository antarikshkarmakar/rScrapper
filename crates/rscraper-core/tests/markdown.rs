use rscraper_core::markdown::{html_to_markdown, html_to_markdown_with_options, MarkdownOptions};
use rscraper_core::Error;
use url::Url;

fn parse_events(markdown: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Options, Parser};
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    Parser::new_ext(markdown, options)
        .map(|event| match event {
            Event::Start(tag) => format!("start:{tag:?}"),
            Event::End(tag) => format!("end:{tag:?}"),
            Event::Text(text) => format!("text:{text}"),
            Event::Code(text) => format!("code:{text}"),
            other => format!("{other:?}"),
        })
        .collect()
}

fn compact_events(markdown: &str) -> Vec<String> {
    parse_events(markdown)
        .into_iter()
        .map(|event| {
            for (prefix, compact) in [
                ("start:Link {", "start:Link"),
                ("start:Image {", "start:Image"),
                ("start:Table(", "start:Table"),
                ("start:List(", "start:List"),
                ("start:BlockQuote(", "start:BlockQuote"),
            ] {
                if event.starts_with(prefix) {
                    return compact.to_owned();
                }
            }
            event
        })
        .collect()
}

fn parsed_visible_text(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser};
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut visible = String::new();
    for event in Parser::new_ext(markdown, options) {
        if let Event::Text(text) | Event::Code(text) = event {
            visible.push_str(&text);
        }
    }
    visible
}

fn parsed_code_block_texts(markdown: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut blocks = Vec::new();
    let mut current = None;
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => current = Some(String::new()),
            Event::Text(text) if current.is_some() => {
                current
                    .as_mut()
                    .expect("code-block state checked")
                    .push_str(&text);
            }
            Event::End(TagEnd::CodeBlock) => {
                blocks.push(current.take().expect("code block has a matching start"));
            }
            _ => {}
        }
    }
    blocks
}

fn parsed_inline_html(markdown: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Options, Parser};

    Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
    .filter_map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Some(html.into_string()),
        _ => None,
    })
    .collect()
}

#[derive(Debug, Eq, PartialEq)]
enum ParsedRawEvent {
    HtmlBlockStart,
    Html(String),
    HtmlBlockEnd,
    InlineHtml(String),
}

fn parsed_raw_events(markdown: &str) -> Vec<ParsedRawEvent> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
    .filter_map(|event| match event {
        Event::Start(Tag::HtmlBlock) => Some(ParsedRawEvent::HtmlBlockStart),
        Event::Html(html) => Some(ParsedRawEvent::Html(html.into_string())),
        Event::End(TagEnd::HtmlBlock) => Some(ParsedRawEvent::HtmlBlockEnd),
        Event::InlineHtml(html) => Some(ParsedRawEvent::InlineHtml(html.into_string())),
        _ => None,
    })
    .collect()
}

fn assert_only_inert_inline_boundaries(markdown: &str, expected: usize) {
    assert_eq!(
        parsed_raw_events(markdown),
        (0..expected)
            .map(|_| ParsedRawEvent::InlineHtml("<!---->".to_owned()))
            .collect::<Vec<_>>(),
        "unsafe raw event in {markdown:?}",
    );
}

fn assert_all_raw_events_are_inert_inline_boundaries(markdown: &str) {
    for event in parsed_raw_events(markdown) {
        assert_eq!(
            event,
            ParsedRawEvent::InlineHtml("<!---->".to_owned()),
            "unsafe raw event in {markdown:?}",
        );
    }
}

fn assert_downstream_html_has_no_active_source_markup(markdown: &str) {
    use scraper::{Html, Selector};

    let downstream = rendered_html(markdown);
    let document = Html::parse_document(&downstream);
    let active = Selector::parse("img, svg, [onerror], [onload]").unwrap();
    assert!(
        document.select(&active).next().is_none(),
        "source text became an active element or attribute: {downstream:?}",
    );
    let links = Selector::parse("a[href]").unwrap();
    for link in document.select(&links) {
        let href = link.value().attr("href").unwrap_or_default();
        assert!(
            !href
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("javascript:"),
            "source text became an active javascript link: {downstream:?}",
        );
    }
}

fn parsed_destinations(markdown: &str) -> (Vec<String>, Vec<String>) {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    let mut links = Vec::new();
    let mut images = Vec::new();
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => links.push(dest_url.into_string()),
            Event::Start(Tag::Image { dest_url, .. }) => images.push(dest_url.into_string()),
            _ => {}
        }
    }
    (links, images)
}

fn rendered_html(markdown: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut output = String::new();
    html::push_html(
        &mut output,
        Parser::new_ext(
            markdown,
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
        ),
    );
    output
}

fn parsed_scalar_stacks(markdown: &str) -> Vec<(char, Vec<&'static str>)> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let mut active = Vec::new();
    let mut scalars = Vec::new();
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        match event {
            Event::Start(Tag::Strong) => active.push("strong"),
            Event::Start(Tag::Emphasis) => active.push("emphasis"),
            Event::Start(Tag::Strikethrough) => active.push("deletion"),
            Event::Start(Tag::Link { .. }) => active.push("link"),
            Event::Start(Tag::Image { .. }) => active.push("image"),
            Event::End(end) => {
                let expected = match end {
                    TagEnd::Strong => Some("strong"),
                    TagEnd::Emphasis => Some("emphasis"),
                    TagEnd::Strikethrough => Some("deletion"),
                    TagEnd::Link => Some("link"),
                    TagEnd::Image => Some("image"),
                    _ => None,
                };
                if let Some(expected) = expected {
                    assert_eq!(active.pop(), Some(expected), "unbalanced {markdown:?}");
                }
            }
            Event::Text(text) => {
                scalars.extend(text.chars().map(|ch| (ch, active.clone())));
            }
            Event::Code(text) => {
                active.push("code");
                scalars.extend(text.chars().map(|ch| (ch, active.clone())));
                assert_eq!(active.pop(), Some("code"));
            }
            _ => {}
        }
    }
    assert!(active.is_empty(), "unclosed parser stack for {markdown:?}");
    scalars
}

#[derive(Clone, Copy, Debug)]
enum WrapperTransitionPayload {
    Plain,
    LeadingEmphasis,
    PartialEmphasis,
    Deletion,
    Link,
    Image,
    Code,
    Transparent,
    Empty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WrapperOuterFamily {
    Strong,
    Emphasis,
    Deletion,
}

impl WrapperOuterFamily {
    fn tag(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Emphasis => "em",
            Self::Deletion => "del",
        }
    }

    fn stack(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Emphasis => "emphasis",
            Self::Deletion => "deletion",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum WrapperTransitionSeparator {
    Adjacent,
    DirectEmpty,
    ConservativeBarrier,
}

impl WrapperTransitionSeparator {
    fn html(self) -> &'static str {
        match self {
            Self::Adjacent => "",
            Self::DirectEmpty => "<!-- split --><span hidden>secret</span><wbr><span></span>",
            Self::ConservativeBarrier => "<span><wbr></span>",
        }
    }
}

fn transition_payload_html(payload: WrapperTransitionPayload, ch: char) -> String {
    match payload {
        WrapperTransitionPayload::Plain => ch.to_string(),
        WrapperTransitionPayload::LeadingEmphasis => format!("<em>{ch}</em>"),
        WrapperTransitionPayload::PartialEmphasis => {
            format!("<em>{ch}</em>{}", ch.to_ascii_lowercase())
        }
        WrapperTransitionPayload::Deletion => format!("<del>{ch}</del>"),
        WrapperTransitionPayload::Link => format!("<a href=\"/{ch}\">{ch}</a>"),
        WrapperTransitionPayload::Image => {
            format!("<img src=\"/{ch}\" alt=\"{ch}\">")
        }
        WrapperTransitionPayload::Code => format!("<code>{ch}</code>"),
        WrapperTransitionPayload::Transparent => format!("<span>{ch}</span>"),
        WrapperTransitionPayload::Empty => "<span></span>".to_owned(),
    }
}

fn transition_payload_stacks(
    payload: WrapperTransitionPayload,
    ch: char,
) -> Vec<(char, Vec<&'static str>)> {
    transition_payload_stacks_for_outer(payload, ch, WrapperOuterFamily::Strong)
}

fn transition_payload_stacks_for_outer(
    payload: WrapperTransitionPayload,
    ch: char,
    outer: WrapperOuterFamily,
) -> Vec<(char, Vec<&'static str>)> {
    let outer_stack = vec![outer.stack()];
    match payload {
        WrapperTransitionPayload::Plain | WrapperTransitionPayload::Transparent => {
            vec![(ch, outer_stack)]
        }
        WrapperTransitionPayload::LeadingEmphasis => {
            let mut stack = outer_stack;
            if outer != WrapperOuterFamily::Emphasis {
                stack.push("emphasis");
            }
            vec![(ch, stack)]
        }
        WrapperTransitionPayload::PartialEmphasis => {
            let mut nested_stack = outer_stack.clone();
            if outer != WrapperOuterFamily::Emphasis {
                nested_stack.push("emphasis");
            }
            vec![(ch, nested_stack), (ch.to_ascii_lowercase(), outer_stack)]
        }
        WrapperTransitionPayload::Deletion => {
            let mut stack = outer_stack;
            if outer != WrapperOuterFamily::Deletion {
                stack.push("deletion");
            }
            vec![(ch, stack)]
        }
        WrapperTransitionPayload::Link => {
            let mut stack = outer_stack;
            stack.push("link");
            vec![(ch, stack)]
        }
        WrapperTransitionPayload::Image => {
            let mut stack = outer_stack;
            stack.push("image");
            vec![(ch, stack)]
        }
        WrapperTransitionPayload::Code => {
            let mut stack = outer_stack;
            stack.push("code");
            vec![(ch, stack)]
        }
        WrapperTransitionPayload::Empty => Vec::new(),
    }
}

fn wrapped_transition_payload_html(
    outer: WrapperOuterFamily,
    payload: WrapperTransitionPayload,
    ch: char,
) -> String {
    let tag = outer.tag();
    format!("<{tag}>{}</{tag}>", transition_payload_html(payload, ch))
}

fn wrapped_transition_payload_rendered_html(
    outer: WrapperOuterFamily,
    payload: WrapperTransitionPayload,
    ch: char,
) -> String {
    let content = match payload {
        WrapperTransitionPayload::Plain | WrapperTransitionPayload::Transparent => ch.to_string(),
        WrapperTransitionPayload::LeadingEmphasis => {
            if outer == WrapperOuterFamily::Emphasis {
                ch.to_string()
            } else {
                format!("<em>{ch}</em>")
            }
        }
        WrapperTransitionPayload::PartialEmphasis => {
            if outer == WrapperOuterFamily::Emphasis {
                format!("{ch}{}", ch.to_ascii_lowercase())
            } else {
                format!("<em>{ch}</em>{}", ch.to_ascii_lowercase())
            }
        }
        WrapperTransitionPayload::Deletion => {
            if outer == WrapperOuterFamily::Deletion {
                ch.to_string()
            } else {
                format!("<del>{ch}</del>")
            }
        }
        WrapperTransitionPayload::Link => format!("<a href=\"/{ch}\">{ch}</a>"),
        WrapperTransitionPayload::Image => {
            format!("<img src=\"/{ch}\" alt=\"{ch}\" />")
        }
        WrapperTransitionPayload::Code => format!("<code>{ch}</code>"),
        WrapperTransitionPayload::Empty => return String::new(),
    };
    let tag = outer.tag();
    format!("<{tag}>{content}</{tag}>")
}

fn options(base_url: &str) -> MarkdownOptions {
    MarkdownOptions {
        base_url: Some(Url::parse(base_url).unwrap()),
        max_chars: 10_000,
    }
}

fn canonical_http_destination(raw: &str, base_url: Option<&str>) -> String {
    let canonical = match base_url {
        Some(base_url) => Url::parse(base_url).unwrap().join(raw).unwrap(),
        None => Url::parse(raw).unwrap(),
    };
    assert!(matches!(canonical.scheme(), "http" | "https"));
    assert!(canonical.username().is_empty());
    assert!(canonical.password().is_none());
    canonical.as_str().replace('\\', "%5C")
}

fn exact_link_markdown(destination: &str) -> String {
    format!("[x]({})", destination.replace('&', "&amp;"))
}

#[test]
fn generated_paragraph_is_recognized_by_the_markdown_parser() {
    let markdown = html_to_markdown("<p>hello</p>");

    assert_eq!(
        parse_events(&markdown),
        ["start:Paragraph", "text:hello", "end:Paragraph",]
    );
}

#[test]
fn final_limit_is_checked_after_whitespace_normalization() {
    let one = MarkdownOptions {
        base_url: None,
        max_chars: 1,
    };
    assert_eq!(
        html_to_markdown_with_options("<p>x </p>", &one).unwrap(),
        "x"
    );
    assert_eq!(
        html_to_markdown_with_options("<p>     x</p>", &one).unwrap(),
        "x"
    );

    let three = MarkdownOptions {
        base_url: None,
        max_chars: 3,
    };
    assert_eq!(
        html_to_markdown_with_options("<p>x          y</p>", &three).unwrap(),
        "x y"
    );
}

#[test]
fn huge_code_and_pre_fail_at_tiny_limits() {
    let body = "a".repeat(2_000_000);
    let options = MarkdownOptions {
        base_url: None,
        max_chars: 16,
    };
    assert!(matches!(
        html_to_markdown_with_options(&format!("<p><code>{body}</code></p>"), &options),
        Err(Error::BodyLimit { limit: 16 })
    ));
    assert!(matches!(
        html_to_markdown_with_options(&format!("<pre>{body}</pre>"), &options),
        Err(Error::BodyLimit { limit: 16 })
    ));
}

#[test]
fn excessive_dom_depth_returns_a_parse_error() {
    let html = format!("{}x{}", "<div>".repeat(300), "</div>".repeat(300));
    assert!(matches!(
        html_to_markdown_with_options(&html, &MarkdownOptions::default()),
        Err(Error::Parse { kind: "html", .. })
    ));
}

fn nested_body(tag: &str, descendants: usize) -> String {
    format!(
        "<body>{}x{}</body>",
        format!("<{tag}>").repeat(descendants),
        format!("</{tag}>").repeat(descendants)
    )
}

#[test]
fn dom_depth_boundary_accepts_256_inline_element_levels() {
    // The parser creates <html> at depth 0 and <body> at depth 1, so 255
    // descendants place the deepest element exactly at MAX_DOM_DEPTH (256).
    assert_eq!(
        html_to_markdown_with_options(&nested_body("span", 255), &MarkdownOptions::default())
            .unwrap(),
        "x"
    );
}

#[test]
fn dom_depth_boundary_accepts_256_block_element_levels() {
    assert_eq!(
        html_to_markdown_with_options(&nested_body("div", 255), &MarkdownOptions::default())
            .unwrap(),
        "x"
    );
}

#[test]
fn dom_depth_boundary_rejects_the_first_deeper_inline_tree() {
    assert!(matches!(
        html_to_markdown_with_options(&nested_body("span", 256), &MarkdownOptions::default()),
        Err(Error::Parse { kind: "html", .. })
    ));
}

#[test]
fn dom_depth_boundary_rejects_the_first_deeper_block_tree() {
    assert!(matches!(
        html_to_markdown_with_options(&nested_body("div", 256), &MarkdownOptions::default()),
        Err(Error::Parse { kind: "html", .. })
    ));
}

fn nested_list_body(pairs: usize, inner: &str) -> String {
    format!(
        "<body>{}{inner}{}</body>",
        "<ul><li>".repeat(pairs),
        "</li></ul>".repeat(pairs)
    )
}

#[test]
fn accepted_recursive_blocks_are_stack_safe_on_a_default_thread() {
    const CHILD_ENV: &str = "RSCRAPER_MARKDOWN_RECURSIVE_DEPTH_CHILD";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env(CHILD_ENV, "1")
        .arg("--exact")
        .arg("accepted_recursive_blocks_child")
        .arg("--nocapture")
        .output()
        .unwrap();

    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("recursive-depth-child-ok"),
        "default-stack child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn accepted_recursive_blocks_child() {
    if std::env::var_os("RSCRAPER_MARKDOWN_RECURSIVE_DEPTH_CHILD").is_none() {
        return;
    }

    std::thread::spawn(|| {
        let blockquotes = nested_body("blockquote", 255);
        let blockquote_markdown =
            html_to_markdown_with_options(&blockquotes, &MarkdownOptions::default()).unwrap();
        assert!(blockquote_markdown.ends_with('x'));

        // body=1; every ul/li pair adds two element levels, and the span is
        // therefore exactly at depth 256 after 127 pairs.
        let lists = nested_list_body(127, "<span>x</span>");
        let list_markdown =
            html_to_markdown_with_options(&lists, &MarkdownOptions::default()).unwrap();
        assert!(list_markdown.ends_with('x'));
    })
    .join()
    .unwrap();
    println!("recursive-depth-child-ok");
}

#[test]
fn recursive_block_depth_257_returns_a_typed_parse_error() {
    assert!(matches!(
        html_to_markdown_with_options(&nested_body("blockquote", 256), &MarkdownOptions::default()),
        Err(Error::Parse { kind: "html", .. })
    ));
    assert!(matches!(
        html_to_markdown_with_options(
            &nested_list_body(127, "<span><span>x</span></span>"),
            &MarkdownOptions::default()
        ),
        Err(Error::Parse { kind: "html", .. })
    ));
}

#[test]
fn image_only_content_survives_block_and_inline_renderability_checks() {
    assert_eq!(
        html_to_markdown("<p><img src=\"/x.png\" alt=\"logo\"></p>"),
        "![logo](/x.png)"
    );
    assert_eq!(
        html_to_markdown("<figure><img src=\"/x.png\" alt=\"logo\"></figure>"),
        "![logo](/x.png)"
    );
    assert_eq!(
        html_to_markdown("<p><strong><img src=\"/x.png\" alt=\"logo\"></strong></p>"),
        "**![logo](/x.png)**"
    );
    assert_eq!(
        html_to_markdown("<p><a href=\"/target\"><img src=\"/x.png\" alt=\"logo\"></a></p>"),
        "[![logo](/x.png)](/target)"
    );
}

#[test]
fn empty_permitted_links_survive_block_and_wrapper_renderability_checks() {
    assert_eq!(
        html_to_markdown("<p><a href=\"/target\"></a></p>"),
        "[](/target)"
    );
    assert_eq!(
        html_to_markdown("<p><strong><a href=\"/target\"></a></strong></p>"),
        "**[](/target)**"
    );
    assert_eq!(
        html_to_markdown("<ul><li><a href=\"/target\"></a></li></ul>"),
        "- [](/target)"
    );
}

#[test]
fn empty_rejected_link_destination_remains_non_renderable() {
    assert_eq!(
        html_to_markdown("<p><a href=\"javascript:alert(1)\"></a></p>"),
        ""
    );
}

#[test]
fn nested_structural_output_survives_container_renderability_checks() {
    assert_eq!(html_to_markdown("<div><hr></div>"), "---");
    assert_eq!(html_to_markdown("<div><pre></pre></div>"), "```\n\n```");
    assert_eq!(html_to_markdown("<blockquote><hr></blockquote>"), "> ---");
    assert_eq!(html_to_markdown("<ul><li><hr></li></ul>"), "- ---");
}

#[test]
fn inline_nested_block_only_descendants_emit_exactly_nothing() {
    let fixtures = [
        ("thematic break", "<span><hr></span>"),
        ("empty pre", "<span><pre></pre></span>"),
        (
            "empty-cell table",
            "<span><table><tr><td></td></tr></table></span>",
        ),
        (
            "hidden-only pre",
            "<span><pre><i hidden>secret</i></pre></span>",
        ),
        (
            "hidden-only table",
            "<span><table><tr><td><i hidden>secret</i></td></tr></table></span>",
        ),
    ];

    for (name, nested) in fixtures {
        for (caller, html) in [
            ("unordered list", format!("<ul><li>{nested}</li></ul>")),
            ("ordered list", format!("<ol><li>{nested}</li></ol>")),
            ("heading", format!("<h1>{nested}</h1>")),
        ] {
            assert_eq!(
                html_to_markdown(&html),
                "",
                "{name} beneath {caller} emitted marker syntax"
            );
        }
    }
}

#[test]
fn inline_nested_block_only_descendants_create_no_list_or_heading_ast() {
    let fixtures = [
        ("thematic break", "<span><hr></span>"),
        ("empty pre", "<span><pre></pre></span>"),
        (
            "empty-cell table",
            "<span><table><tr><td></td></tr></table></span>",
        ),
        (
            "hidden-only pre",
            "<span><pre><i hidden>secret</i></pre></span>",
        ),
        (
            "hidden-only table",
            "<span><table><tr><td><i hidden>secret</i></td></tr></table></span>",
        ),
    ];

    for (name, nested) in fixtures {
        for (caller, html) in [
            ("unordered list", format!("<ul><li>{nested}</li></ul>")),
            ("ordered list", format!("<ol><li>{nested}</li></ol>")),
            ("heading", format!("<h1>{nested}</h1>")),
        ] {
            let markdown = html_to_markdown(&html);
            let events = parse_events(&markdown);
            assert!(
                events.iter().all(|event| {
                    !event.starts_with("start:List(") && !event.starts_with("start:Heading")
                }),
                "{name} beneath {caller} fabricated AST from {markdown:?}: {events:?}"
            );
        }
    }
}

#[test]
fn inline_nested_pre_and_table_text_remains_flattened() {
    let pre = html_to_markdown("<ul><li><span><pre>A\n B</pre></span></li></ul>");
    assert_eq!(pre, "- A B");
    assert!(parse_events(&pre).iter().any(|event| event == "text:A B"));

    let table =
        html_to_markdown("<h2><span><table><tr><td>A</td><td>B</td></tr></table></span></h2>");
    assert_eq!(table, "## AB");
    let table_events = parse_events(&table);
    assert!(table_events
        .iter()
        .any(|event| event.starts_with("start:Heading")));
    assert!(table_events.iter().any(|event| event == "text:AB"));
}

#[test]
fn intrinsic_inline_emitters_remain_renderable_beneath_wrappers() {
    let fixtures = [
        (
            "permitted empty link",
            "<span><pre><a href=\"/target\"></a></pre></span>",
            "- [](/target)",
        ),
        (
            "image",
            "<span><table><tr><td><img src=\"/logo\" alt=\"logo\"></td></tr></table></span>",
            "- ![logo](/logo)",
        ),
        ("code", "<span><pre><code>x</code></pre></span>", "- `x`"),
        (
            "hard break",
            "<span><table><tr><td><br></td></tr></table></span>",
            "-   \n",
        ),
    ];

    for (name, nested, expected) in fixtures {
        let markdown = html_to_markdown(&format!("<ul><li>{nested}</li></ul>"));
        assert_eq!(markdown, expected, "{name} lost inline output");
        assert!(
            parse_events(&markdown)
                .iter()
                .any(|event| event.starts_with("start:List(")),
            "{name} lost its marker-bearing caller: {markdown:?}"
        );
    }
}

#[test]
fn preserves_mixed_inline_order_and_escapes_link_text() {
    let html = "<p>Read <a href=\"../guide?a=1&b=2\">the *guide*</a> now.</p>";
    let markdown = html_to_markdown_with_options(
        html,
        &MarkdownOptions {
            base_url: Some(Url::parse("https://example.com/docs/page").unwrap()),
            max_chars: 10_000,
        },
    )
    .unwrap();

    assert_eq!(
        markdown,
        "Read [the \\*guide\\*](https://example.com/guide?a=1&amp;b=2) now."
    );
}

#[test]
fn renders_text_around_inline_formatting_in_document_order() {
    let markdown = html_to_markdown(
        "<p>Start <strong>bold</strong> and <em>em</em> with <code>code</code> end.</p>",
    );

    assert_eq!(markdown, "Start **bold** and *em* with `code` end.");
}

#[test]
fn renders_image_alt_text_and_resolves_relative_destinations() {
    let markdown = html_to_markdown_with_options(
        "<p>Logo <img src=\"../assets/logo.png\" alt=\"A *logo*\"></p><a href=\"guide\">Guide</a>",
        &options("https://example.com/docs/page/"),
    )
    .unwrap();

    assert_eq!(
        markdown,
        "Logo ![A \\*logo\\*](https://example.com/docs/assets/logo.png)\n\n[Guide](https://example.com/docs/page/guide)"
    );
}

#[test]
fn rejects_dangerous_and_control_character_destinations() {
    let markdown = html_to_markdown_with_options(
        "<p><a href=\"javascript:alert(1)\">bad</a><a href=\"https://example.com/a&#10;b\">control</a><a href=\"/safe\">good</a></p>",
        &options("https://example.com/base/"),
    )
    .unwrap();

    assert_eq!(markdown, "badcontrol[good](https://example.com/safe)");
}

#[test]
fn renders_nested_ordered_and_unordered_lists() {
    let markdown = html_to_markdown(
        "<ul><li>One<ul><li>Nested</li></ul></li><li>Two<ol><li>First</li></ol></li></ul>",
    );

    assert_eq!(markdown, "- One\n    - Nested\n- Two\n    1. First");
}

#[test]
fn renders_blockquote_paragraphs() {
    let markdown = html_to_markdown(
        "<blockquote><p>First paragraph.</p><p>Second <em>paragraph</em>.</p></blockquote>",
    );

    assert_eq!(markdown, "> First paragraph.\n>\n> Second *paragraph*.");
}

#[test]
fn chooses_a_fence_longer_than_source_backtick_runs() {
    let markdown = html_to_markdown("<pre>before\n```\nafter</pre>");

    assert_eq!(markdown, "````\nbefore\n```\nafter\n````");
}

#[test]
fn renders_tables_with_escaped_cells_and_padded_rows() {
    let markdown = html_to_markdown(
        "<table><thead><tr><th>Name|Type</th><th>Notes</th></tr></thead><tbody><tr><td>One</td><td>line\nbreak</td></tr><tr><td>Only</td></tr></tbody></table>",
    );

    assert_eq!(
        markdown,
        "| Name\\|Type | Notes |\n| --- | --- |\n| One | line break |\n| Only |  |"
    );
}

#[test]
fn renders_description_lists_and_line_breaks() {
    let markdown = html_to_markdown(
        "<dl><dt>Term</dt><dd>Definition</dd><dt>Next</dt><dd>Value</dd></dl><p>A<br>B</p><hr><p>C</p>",
    );

    assert_eq!(
        markdown,
        "Term\n: Definition\n\nNext\n: Value\n\nA  \nB\n\n---\n\nC"
    );
}

#[test]
fn description_lists_skip_hidden_direct_terms() {
    let markdown = html_to_markdown(
        "<dl><dt hidden>Hidden term</dt><dd>orphan</dd><dt>Visible</dt><dd>x</dd></dl>",
    );

    assert_eq!(markdown, "Visible\n: x");
}

#[test]
fn description_lists_skip_hidden_direct_definitions() {
    let markdown = html_to_markdown(
        "<dl><dt>Hidden definition</dt><dd hidden>secret</dd><dt>Visible</dt><dd>x</dd></dl>",
    );

    assert_eq!(markdown, "Visible\n: x");
}

#[test]
fn description_lists_skip_classified_direct_terms_and_definitions() {
    let markdown = html_to_markdown(
        "<dl><dt class=\"ad\">Advertisement</dt><dd>orphan</dd><dt>Promo</dt><dd class=\"promo\">secret</dd><dt>Visible</dt><dd>x</dd></dl>",
    );

    assert_eq!(markdown, "Visible\n: x");
}

#[test]
fn orphan_description_nodes_do_not_fabricate_list_items() {
    for description in ["<dl><dt>T</dt></dl>", "<dl><dd>x</dd></dl>"] {
        let markdown = html_to_markdown(&format!("<ul><li>{description}</li></ul>"));

        assert_eq!(markdown, "");
        assert!(
            parse_events(&markdown)
                .iter()
                .all(|event| !event.starts_with("start:List(")),
            "orphan description produced a parsed list: {markdown:?}"
        );
    }
}

#[test]
fn empty_description_term_still_establishes_a_visible_definition() {
    assert_eq!(html_to_markdown("<dl><dt></dt><dd>x</dd></dl>"), ": x");
}

#[test]
fn mixed_structured_blocks_keep_metadata_and_emission_aligned() {
    let markdown = html_to_markdown(
        "<blockquote><ul><li><dl><dt hidden>Hidden</dt><dd>orphan</dd></dl></li><li><dl><dt></dt><dd>x</dd></dl></li><li><dl><dd>orphan</dd></dl></li></ul></blockquote>",
    );
    let events = parse_events(&markdown);

    assert!(
        markdown.contains(": x"),
        "missing visible definition: {markdown:?}"
    );
    assert!(!markdown.contains("Hidden"));
    assert!(!markdown.contains("orphan"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("start:List("))
            .count(),
        1,
        "expected only the real mixed-structure list: {events:?}"
    );
}

#[test]
fn selects_the_first_meaningful_content_root() {
    let markdown = html_to_markdown(
        "<html><head><title>Ignored title.</title></head><body><main><p> \n </p></main><article><p>Article wins.</p></article><p>Body fallback.</p></body></html>",
    );

    assert_eq!(markdown, "Article wins.");
}

#[test]
fn reading_and_shadow_classes_are_not_ads() {
    let markdown =
        html_to_markdown("<main><p class=\"reading shadow\">Keep this article.</p></main>");
    assert_eq!(markdown, "Keep this article.");
}

#[test]
fn skips_exact_non_content_classes_without_substring_false_positives() {
    let markdown = html_to_markdown(
        "<main><p class=\"adventure\">Adventure remains.</p><p class=\"ad\">Advertisement disappears.</p></main>",
    );

    assert_eq!(markdown, "Adventure remains.");
}

#[test]
fn skips_hidden_nodes() {
    let markdown = html_to_markdown(
        "<main><p hidden>Hidden attribute.</p><p aria-hidden=\"true\">Hidden aria.</p><p>Visible.</p></main>",
    );

    assert_eq!(markdown, "Visible.");
}

#[test]
fn enforces_unicode_character_limits_without_partial_output() {
    let error = html_to_markdown_with_options(
        "<p>a🦀b</p>",
        &MarkdownOptions {
            base_url: None,
            max_chars: 2,
        },
    )
    .unwrap_err();

    assert!(matches!(error, Error::BodyLimit { limit: 2 }));
}

#[test]
fn compatibility_wrapper_handles_empty_and_malformed_html() {
    assert_eq!(html_to_markdown(""), "");
    assert_eq!(html_to_markdown("<main><p>Open"), "Open");
}

#[test]
fn renders_a_body_that_contains_only_a_text_node() {
    assert_eq!(
        html_to_markdown("<html><body>Only text.</body></html>"),
        "Only text."
    );
}

#[test]
fn compatibility_wrapper_keeps_headings_and_paragraphs() {
    let markdown = html_to_markdown("<html><body><h1>Title</h1><p>Hello world.</p></body></html>");

    assert!(markdown.contains("# Title"));
    assert!(markdown.contains("Hello world."));
}

#[test]
fn compatibility_wrapper_strips_navigation_and_scripts() {
    let markdown = html_to_markdown(
        "<nav>Menu</nav><script>window.bad = true;</script><main><p>Real content here.</p></main>",
    );

    assert!(markdown.contains("Real content"));
    assert!(!markdown.to_lowercase().contains("menu"));
}

#[test]
fn compatibility_wrapper_keeps_relative_links_and_lists() {
    let markdown = html_to_markdown("<ul><li><a href=\"/a\">A</a></li><li>B</li></ul>");

    assert!(markdown.contains("[A](/a)"));
    assert!(markdown.contains("- B"));
}

#[test]
fn compatibility_wrapper_renders_code_blocks() {
    let markdown = html_to_markdown("<pre><code>let x = 1;</code></pre>");

    assert!(markdown.contains("```"));
    assert!(markdown.contains("let x = 1;"));
}

#[test]
fn escapes_commonmark_block_starters_in_text_labels_alt_text_and_after_breaks() {
    let markdown = html_to_markdown_with_options(
        "<p># title</p><p>- item</p><p>+ item</p><p>1. numbered</p><p>line<br># after</p><p><a href=\"/guide\"># label</a><img src=\"/chart\" alt=\"# alt\"></p>",
        &options("https://example.com/docs/"),
    )
    .unwrap();

    assert_eq!(
        markdown,
        "\\# title\n\n\\- item\n\n\\+ item\n\n1\\. numbered\n\nline  \n\\# after\n\n[\\# label](https://example.com/guide)![\\# alt](https://example.com/chart)"
    );
}

#[test]
fn preserves_block_sequences_inside_list_items_and_description_definitions() {
    let list = html_to_markdown(
        "<ol><li><p>First</p><p>Second</p><pre>x</pre><blockquote><p>Quote</p></blockquote></li><li>Line<br>continued</li></ol>",
    );
    assert_eq!(
        list,
        "1. First\n   \n   Second\n   \n   ```\n   x\n   ```\n   \n   > Quote\n2. Line  \n   continued"
    );

    let descriptions = html_to_markdown(
        "<dl><dt>T</dt><dd><p>One.</p><p>Two.</p></dd><dd><blockquote><p>Also.</p></blockquote></dd></dl>",
    );
    assert_eq!(descriptions, "T\n: One.\n  \n  Two.\n\n: > Also.");
}

#[test]
fn renders_valid_padded_inline_code_spans_for_backtick_edges() {
    let markdown = html_to_markdown(
        "<p><code>&#96;</code> <code>&#96;x</code> <code>x&#96;</code> <code>&#96;&#96;</code></p>",
    );

    assert_eq!(markdown, "`` ` `` `` `x `` `` x` `` ``` `` ```");
    assert!(!markdown.contains('\n'));
}

#[test]
fn rejects_pathological_nesting_and_escaping_expansion_with_typed_errors() {
    let deeply_nested = format!(
        "<main>{}x{}</main>",
        "<div>".repeat(600),
        "</div>".repeat(600)
    );
    let depth_error = html_to_markdown_with_options(
        &deeply_nested,
        &MarkdownOptions {
            base_url: None,
            max_chars: 10_000,
        },
    )
    .unwrap_err();
    assert!(matches!(depth_error, Error::Parse { kind: "html", .. }));

    let expanded = format!("<p>{}</p>", "*".repeat(1_000_000));
    let limit_error = html_to_markdown_with_options(
        &expanded,
        &MarkdownOptions {
            base_url: None,
            max_chars: 1,
        },
    )
    .unwrap_err();
    assert!(matches!(limit_error, Error::BodyLimit { limit: 1 }));
}

#[test]
fn preserves_checked_ordered_list_start_and_value_and_ignores_reversed() {
    assert_eq!(
        html_to_markdown(
            "<ol start=\"3\"><li>Three</li><li value=\"7\">Seven</li><li>Eight</li></ol>"
        ),
        "3. Three\n7. Seven\n8. Eight"
    );
    assert_eq!(
        html_to_markdown("<ol start=\"-2\"><li>One</li><li>Two</li></ol>"),
        "- -2. One\n- -1. Two"
    );
    assert_eq!(
        html_to_markdown("<ol reversed start=\"3\"><li>Three</li><li>Four</li></ol>"),
        "3. Three\n4. Four"
    );
}

#[test]
fn percent_encodes_unsafe_relative_destinations_and_handles_scheme_relative_urls() {
    assert_eq!(
        html_to_markdown("<a href='docs/<draft>.md'>read</a><a href='docs/\"quote\".md'>quote</a><a href='docs\\draft.md'>slash</a><a href='docs(a).md'>paren</a><a href='//cdn.example/a'>cdn</a>"),
        "[read](docs/%3Cdraft%3E.md)[quote](docs/%22quote%22.md)[slash](docs%5Cdraft.md)[paren](docs%28a%29.md)cdn"
    );
    assert_eq!(
        html_to_markdown_with_options("<a href=\"//cdn.example/a\">cdn</a><a href=\"https://user:pass@example.com/\">creds</a>", &options("https://example.com/base/"))
            .unwrap(),
        "[cdn](https://cdn.example/a)creds"
    );
}

#[test]
fn encodes_code_pipes_in_tables_without_double_escaping_text_pipes() {
    let markdown = html_to_markdown(
        "<table><tr><th>Expr</th><th>Plain</th></tr><tr><td><code>a|b</code></td><td>x|y</td></tr></table>",
    );

    assert_eq!(
        markdown,
        "| Expr | Plain |\n| --- | --- |\n| `a\\|b` | x\\|y |"
    );
}

#[test]
fn keeps_valid_code_languages_and_rejects_fence_info_injection() {
    assert_eq!(
        html_to_markdown("<pre><code class=\"foo language-rust bar\">fn main() {}</code></pre>"),
        "``` rust\nfn main() {}\n```"
    );
    assert_eq!(
        html_to_markdown("<pre><code class=\"language-rust;evil\">x</code></pre>"),
        "```\nx\n```"
    );
}

#[test]
fn image_content_roots_are_meaningful_but_decorative_images_are_not() {
    assert_eq!(
        html_to_markdown("<body><main><img alt=\"Chart\" src=\"https://example.com/chart.png\"></main><article><p>Secondary.</p></article></body>"),
        "![Chart](https://example.com/chart.png)"
    );
    assert_eq!(
        html_to_markdown("<body><main><img alt=\"\" src=\"https://example.com/chart.png\"></main><article><p>Secondary.</p></article></body>"),
        "Secondary."
    );
}

#[test]
fn applies_the_character_limit_after_whitespace_normalization() {
    assert_eq!(
        html_to_markdown_with_options(
            "<p>     x</p>",
            &MarkdownOptions {
                base_url: None,
                max_chars: 1
            }
        )
        .unwrap(),
        "x"
    );
}

#[test]
fn excluded_ancestors_cannot_supply_a_preferred_content_root() {
    for wrapper in ["<div hidden>", "<nav>", "<template>"] {
        let closing = if wrapper == "<nav>" {
            "</nav>"
        } else if wrapper == "<template>" {
            "</template>"
        } else {
            "</div>"
        };
        assert_eq!(html_to_markdown(&format!("<body>{wrapper}<main><p>Hidden.</p></main>{closing}<article><p>Visible.</p></article></body>")), "Visible.");
    }
}

#[test]
fn preserves_whitespace_across_inline_boundaries_and_code_span_edges() {
    assert_eq!(
        html_to_markdown("<p>A<strong> B </strong>C</p>"),
        "A **B** C"
    );
    assert_eq!(html_to_markdown("<p><code> a </code></p>"), "`  a  `");
}

#[test]
fn list_depth_is_logical_and_signed_markers_are_safe_text() {
    assert_eq!(
        html_to_markdown("<div><ul><li>A<ul><li>B</li></ul></li></ul></div>"),
        "- A\n    - B"
    );
    assert_eq!(
        html_to_markdown("<ol start=\"-2\"><li>A</li><li value=\"0\">B</li><li>C</li></ol>"),
        "- -2. A\n- 0\\. B\n- 1\\. C"
    );
}

#[test]
fn escapes_parenthesized_ordered_starters() {
    assert_eq!(
        html_to_markdown("<p>1) ordinary</p><p>x<br>2) after</p>"),
        "1\\) ordinary\n\nx  \n2\\) after"
    );
}

#[test]
fn allows_colons_after_the_first_relative_path_segment() {
    assert_eq!(
        html_to_markdown("<a href=\"docs/a:b?x=1#y\">read</a>"),
        "[read](docs/a:b?x=1#y)"
    );
    assert_eq!(
        html_to_markdown("<a href=\"mailto:x@example.com\">bad</a>"),
        "bad"
    );
}

#[test]
fn tables_keep_only_owned_rows_and_do_not_promote_data_to_headers() {
    assert_eq!(
        html_to_markdown("<table><tbody><tr><td>A</td></tr><tr><td>B</td></tr></tbody></table>"),
        "|  |\n| --- |\n| A |\n| B |"
    );
    assert_eq!(html_to_markdown("<table><tr><th>H</th></tr><tr><td>Outer<table><tr><td>Inner</td></tr></table></td></tr></table>"), "| H |\n| --- |\n| OuterInner |");
}

#[test]
fn inline_boundaries_preserve_text_and_ast() {
    let markdown = html_to_markdown(
        "<p>A<a href=\"/x\"> B </a>C</p>\
         <p><strong>A</strong><strong>B</strong></p>\
         <p><em>A</em><em>B</em></p>\
         <p><del>A</del><del>B</del></p>\
         <p><code>A</code><code>B</code></p>\
         <p>A<code> </code>B</p>",
    );
    let events = parse_events(&markdown);

    assert_eq!(
        markdown,
        "A [B](/x) C\n\n**AB**\n\n*AB*\n\n~~AB~~\n\n`AB`\n\nA` `B"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "text:AB")
            .count(),
        3
    );
    assert!(events.iter().any(|event| event == "code:AB"));
    assert!(events.iter().any(|event| event == "code: "));
}

#[test]
fn code_spans_preserve_parser_visible_content() {
    let fixtures = [
        ("leading space", "<p><code> leading</code></p>", " leading"),
        (
            "trailing space",
            "<p><code>trailing </code></p>",
            "trailing ",
        ),
        ("interior spaces", "<p><code>a  b</code></p>", "a  b"),
        ("all spaces", "<p><code>   </code></p>", "   "),
        ("embedded newline", "<p><code>a\nb</code></p>", "a b"),
        ("backticks only", "<p><code>&#96;&#96;</code></p>", "``"),
        (
            "mixed longest run",
            "<p><code>&#96;a&#96;&#96;b&#96;</code></p>",
            "`a``b`",
        ),
    ];

    for (name, html, expected) in fixtures {
        let markdown = html_to_markdown(html);
        let code_events: Vec<_> = parse_events(&markdown)
            .into_iter()
            .filter_map(|event| event.strip_prefix("code:").map(str::to_owned))
            .collect();
        assert_eq!(code_events, [expected], "{name}: {markdown:?}");
    }
}

#[test]
fn ordinary_text_never_becomes_setext_or_list_markdown() {
    let markdown = html_to_markdown(
        "<p># heading</p><p>&gt; quote</p><p>- item</p><p>+ item</p>\
         <p>* item</p><p>1. item</p><p>1) item</p>\
         <p>x<br>===</p><p>y<br>---</p>\
         <p>[not](a-link)</p>",
    );
    let events = parse_events(&markdown);

    assert!(markdown.contains("\\# heading"));
    assert!(markdown.contains("\\> quote"));
    assert!(markdown.contains("\\- item"));
    assert!(markdown.contains("\\+ item"));
    assert!(markdown.contains("\\* item"));
    assert!(markdown.contains("1\\. item"));
    assert!(markdown.contains("1\\) item"));
    assert!(markdown.contains("x  \n\\==="));
    assert!(markdown.contains("y  \n\\---"));
    assert!(markdown.contains("\\[not\\](a-link)"));
    assert!(events.iter().all(|event| {
        !event.starts_with("start:Heading")
            && !event.starts_with("start:List(")
            && !event.starts_with("start:BlockQuote")
            && !event.starts_with("start:Link")
            && event != "Rule"
    }));
}

#[test]
fn label_and_alt_text_escape_block_starters() {
    let markdown = html_to_markdown(
        "<p><a href=\"/x\">&gt; label</a> \
         <img src=\"/x.png\" alt=\"1) alt\"></p>",
    );

    assert_eq!(markdown, "[\\> label](/x) ![1\\) alt](/x.png)");
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event.starts_with("start:Link")));
    assert!(events.iter().any(|event| event.starts_with("start:Image")));
    assert!(events
        .iter()
        .all(|event| !event.starts_with("start:BlockQuote") && !event.starts_with("start:List(")));
}

#[test]
fn relative_query_and_fragment_colons_are_safe() {
    assert_eq!(
        html_to_markdown(
            "<p><a href=\"?next=a:b\">query</a> \
             <a href=\"#sec:a\">fragment</a> \
             <a href=\"docs/a:b\">path</a> \
             <a href=\"a:b/c\">scheme-like</a></p>",
        ),
        "[query](?next=a:b) [fragment](#sec:a) [path](docs/a:b) scheme-like"
    );
}

#[test]
fn unsafe_destination_forms_fall_back_to_visible_text() {
    let fixtures = [
        ("credentials", "https://user:pass@example.com/x"),
        ("control", "docs/a&#10;b"),
        ("short percent escape", "docs/a%2"),
        ("invalid percent escape", "docs/a%GG"),
        ("trailing percent escape", "docs/a%"),
        ("javascript scheme", "javascript:alert(1)"),
        ("mail scheme", "mailto:x@example.com"),
        ("ftp scheme", "ftp://example.com/x"),
        ("data scheme", "data:text/plain,x"),
        ("scheme relative", "//cdn.example.com/x"),
    ];

    for (label, destination) in fixtures {
        let html = format!("<p><a href=\"{destination}\">{label}</a></p>");
        assert_eq!(html_to_markdown(&html), label, "accepted {destination:?}");
    }
}

#[test]
fn destination_encoding_is_unicode_safe_and_preserves_valid_escapes() {
    assert_eq!(
        html_to_markdown("<p><a href=\"docs/é(a)&lt;b&gt;&quot;&#32;q\\c%2F.md\">read</a></p>"),
        "[read](docs/%C3%A9%28a%29%3Cb%3E%22%20q%5Cc%2F.md)"
    );
}

#[test]
fn base_resolution_revalidates_scheme_and_credentials() {
    assert_eq!(
        html_to_markdown_with_options(
            "<p><a href=\"../a:b?q=x:y#f:z\">relative</a> \
             <a href=\"https://user:pass@example.com/x\">credentials</a></p>",
            &options("https://example.com/docs/page/")
        )
        .unwrap(),
        "[relative](https://example.com/docs/a:b?q=x:y#f:z) credentials"
    );

    let ftp_options = MarkdownOptions {
        base_url: Some(Url::parse("ftp://example.com/base/").unwrap()),
        max_chars: 10_000,
    };
    assert_eq!(
        html_to_markdown_with_options("<a href=\"child\">relative</a>", &ftp_options).unwrap(),
        "relative"
    );
}

#[test]
fn first_visible_preferred_root_wins_in_document_order() {
    assert_eq!(
        html_to_markdown(
            "<body><div hidden><main>Hidden</main></div>\
             <main><p>First.</p></main><main><p>Second.</p></main></body>",
        ),
        "First."
    );
}

#[test]
fn root_priority_uses_separate_document_order_passes() {
    assert_eq!(
        html_to_markdown(
            "<body><article>Article.</article><section role=\"main\">Role.</section>\
             <main>First main.</main><main>Second main.</main></body>"
        ),
        "First main."
    );
    assert_eq!(
        html_to_markdown(
            "<body><article>Article.</article><section role=\"main\">First role.</section>\
             <section role=\"main\">Second role.</section></body>"
        ),
        "First role."
    );
}

#[test]
fn excluded_state_propagates_through_non_content_ancestors() {
    let wrappers = [
        ("hidden", "<div hidden>", "</div>"),
        ("aria hidden", "<div aria-hidden=\"true\">", "</div>"),
        ("navigation", "<nav>", "</nav>"),
        ("template", "<template>", "</template>"),
        ("script", "<script>", "</script>"),
        ("style", "<style>", "</style>"),
    ];

    for (name, open, close) in wrappers {
        let html = format!(
            "<body>{open}<main><p>Hidden.</p></main>{close}<article>Visible.</article></body>"
        );
        assert_eq!(html_to_markdown(&html), "Visible.", "{name}");
    }
}

#[test]
fn exclusion_classes_match_exact_tokens_only() {
    assert_eq!(
        html_to_markdown(
            "<main><p class=\"reading shadow adventure\">Keep.</p>\
             <p class=\"ad\">Drop ad.</p><p class=\"promo\">Drop promo.</p></main>"
        ),
        "Keep."
    );
}

#[test]
fn image_only_roots_require_visible_alt_and_safe_destination() {
    let fallbacks = [
        "<img alt=\"Chart\" src=\"javascript:bad\">",
        "<img alt=\"\" src=\"/chart.png\">",
        "<img alt=\"   \" src=\"/chart.png\">",
        "<img alt=\"Chart\">",
    ];
    for image in fallbacks {
        let html = format!("<body><main>{image}</main><article><p>Fallback.</p></article></body>");
        assert_eq!(html_to_markdown(&html), "Fallback.", "{image}");
    }

    assert_eq!(
        html_to_markdown(
            "<body><main><img alt=\"Chart\" src=\"/chart.png\"></main>\
             <article>Fallback.</article></body>"
        ),
        "![Chart](/chart.png)"
    );
}

#[test]
fn empty_preferred_candidates_fall_through_without_descendant_leaks() {
    assert_eq!(
        html_to_markdown(
            "<body><main><nav><article>Leaked.</article></nav></main>\
             <main><p> </p></main><article>Visible.</article></body>"
        ),
        "Visible."
    );
    assert_eq!(
        html_to_markdown("<body><main> </main><p>Body fallback.</p></body>"),
        "Body fallback."
    );
}

#[test]
fn empty_formatting_does_not_adopt_following_sibling_output() {
    assert_eq!(
        html_to_markdown("<p><strong></strong>X</p><p><em></em>Y</p><p><del></del>Z</p>"),
        "X\n\nY\n\nZ"
    );
}

#[test]
fn coalesced_inline_runs_never_leak_hidden_siblings() {
    let markdown = html_to_markdown(
        "<p><strong>A</strong><strong hidden>SECRET</strong><strong>B</strong></p>\
         <p><code>A</code><code hidden>SECRET</code><code>B</code></p>",
    );

    assert!(!markdown.contains("SECRET"), "{markdown:?}");
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "text:AB"), "{events:?}");
    assert!(events.iter().any(|event| event == "code:AB"), "{events:?}");
}

#[test]
fn destination_entities_remain_literal_in_link_and_image_targets() {
    let fixtures = [
        (
            "javascript&amp;colon;alert(1)",
            "javascript&amp;colon;alert%281%29",
            "javascript&colon;alert%281%29",
            "javascript&amp;colon;alert%281%29",
        ),
        (
            "&amp;sol;&amp;sol;evil.example/x",
            "&amp;sol;&amp;sol;evil.example/x",
            "&sol;&sol;evil.example/x",
            "&amp;sol;&amp;sol;evil.example/x",
        ),
        (
            "https&amp;colon;&amp;sol;&amp;sol;user&amp;commat;example.com/x",
            "https&amp;colon;&amp;sol;&amp;sol;user&amp;commat;example.com/x",
            "https&colon;&sol;&sol;user&commat;example.com/x",
            "https&amp;colon;&amp;sol;&amp;sol;user&amp;commat;example.com/x",
        ),
        (
            "javascript&amp;#58;alert(1)",
            "javascript&amp;#58;alert%281%29",
            "javascript&#58;alert%281%29",
            "javascript&amp;#58;alert%281%29",
        ),
        (
            "javascript&amp;#x3A;alert(1)",
            "javascript&amp;#x3A;alert%281%29",
            "javascript&#x3A;alert%281%29",
            "javascript&amp;#x3A;alert%281%29",
        ),
        (
            "docs/&amp;#10;x",
            "docs/&amp;#10;x",
            "docs/&#10;x",
            "docs/&amp;#10;x",
        ),
        (
            "docs/&amp;#x0A;x",
            "docs/&amp;#x0A;x",
            "docs/&#x0A;x",
            "docs/&amp;#x0A;x",
        ),
    ];

    for (source, markdown_destination, parsed_destination, html_destination) in fixtures {
        let markdown = html_to_markdown(&format!(
            "<p><a href=\"{source}\">link</a><img src=\"{source}\" alt=\"alt\"></p>"
        ));
        assert_eq!(
            markdown,
            format!("[link]({markdown_destination})![alt]({markdown_destination})")
        );
        assert_eq!(
            parsed_destinations(&markdown),
            (
                vec![parsed_destination.to_owned()],
                vec![parsed_destination.to_owned()]
            )
        );
        assert_eq!(
            rendered_html(&markdown),
            format!(
                "<p><a href=\"{html_destination}\">link</a><img src=\"{html_destination}\" alt=\"alt\" /></p>\n"
            )
        );
    }
}

#[test]
fn destination_ampersand_spelling_is_fully_charged_to_writer_budget() {
    let link = "<a href=\"a&amp;b\">x</a>";
    assert!(matches!(
        html_to_markdown_with_options(
            link,
            &MarkdownOptions {
                base_url: None,
                max_chars: 11,
            }
        ),
        Err(Error::BodyLimit { limit: 11 })
    ));
    assert_eq!(
        html_to_markdown_with_options(
            link,
            &MarkdownOptions {
                base_url: None,
                max_chars: 12,
            }
        )
        .unwrap(),
        "[x](a&amp;b)"
    );

    let image = "<img src=\"a&amp;b\" alt=\"x\">";
    assert!(matches!(
        html_to_markdown_with_options(
            image,
            &MarkdownOptions {
                base_url: None,
                max_chars: 12,
            }
        ),
        Err(Error::BodyLimit { limit: 12 })
    ));
    assert_eq!(
        html_to_markdown_with_options(
            image,
            &MarkdownOptions {
                base_url: None,
                max_chars: 13,
            }
        )
        .unwrap(),
        "![x](a&amp;b)"
    );
}

#[test]
fn logically_empty_nodes_emit_nothing_without_descendant_coalescing_scans() {
    let markdown = html_to_markdown(
        "<p><strong>A</strong><span hidden>X</span><strong>B</strong></p>\
         <p><strong>A</strong><span><wbr></span><strong>B</strong></p>\
         <p><code>A</code><span hidden>X</span><code>B</code></p>\
         <p><code>A</code><span><wbr></span><code>B</code></p>",
    );

    assert_eq!(markdown, "**AB**\n\n**A**__B__\n\n`AB`\n\n`AB`");
    let events = parse_events(&markdown);
    assert_eq!(events.iter().filter(|event| *event == "text:AB").count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "start:Strong")
            .count(),
        3
    );
    assert_eq!(events.iter().filter(|event| *event == "code:AB").count(), 2);
    assert_eq!(parsed_visible_text(&markdown), "ABABABAB");
}

#[test]
fn whitespace_only_formatting_returns_one_pending_boundary_space() {
    assert_eq!(
        html_to_markdown("<p>A<strong> </strong>B</p><p>A<em> </em>B</p><p>A<del> </del>B</p>"),
        "A B\n\nA B\n\nA B"
    );
}

#[test]
fn intrinsic_first_wrapper_content_does_not_invent_a_leading_space() {
    let markdown = html_to_markdown(
        "<p>A<strong><img alt=\"X\" src=\"/x\"> B</strong>C</p>\
         <p>A<strong><code>X</code> B</strong>C</p>\
         <p>A<strong><a href=\"/x\"></a> B</strong>C</p>",
    );
    assert_eq!(
        markdown,
        "&#65;**![X](/x) B**C\n\n&#65;**`X` B**C\n\n&#65;**[](/x) B**C"
    );
    assert_eq!(
        compact_events(&markdown)
            .iter()
            .filter(|event| event.as_str() == "start:Strong")
            .count(),
        3
    );
    assert_eq!(parsed_visible_text(&markdown), "AX BCAX BCA BC");
}

#[test]
fn strong_and_emphasis_nesting_orders_have_distinct_ast() {
    let strong_em = html_to_markdown("<p><strong><em>A</em></strong></p>");
    assert_eq!(strong_em, "**_A_**");
    assert_eq!(
        parse_events(&strong_em),
        [
            "start:Paragraph",
            "start:Strong",
            "start:Emphasis",
            "text:A",
            "end:Emphasis",
            "end:Strong",
            "end:Paragraph",
        ]
    );

    let em_strong = html_to_markdown("<p><em><strong>B</strong></em></p>");
    assert_eq!(em_strong, "***B***");
    assert_eq!(
        parse_events(&em_strong),
        [
            "start:Paragraph",
            "start:Emphasis",
            "start:Strong",
            "text:B",
            "end:Strong",
            "end:Emphasis",
            "end:Paragraph",
        ]
    );
}

#[test]
fn ordered_marker_escape_phase_survives_inline_node_boundaries() {
    let markdown = html_to_markdown(
        "<p>1<span>.</span> item</p>\
         <p>2<!-- split -->) item</p>\
         <p>3<span><span>.</span></span> item</p>\
         <p>x<br>4<span>)</span> item</p>\
         <blockquote><p>5<span>.</span> item</p></blockquote>",
    );

    assert_eq!(
        markdown,
        "1\\. item\n\n2\\) item\n\n3\\. item\n\nx  \n4\\) item\n\n> 5\\. item"
    );
    let events = parse_events(&markdown);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("start:List("))
            .count(),
        0,
        "{events:?}"
    );
}

#[test]
fn preferred_roots_require_structurally_renderable_content() {
    let empty_candidates = [
        "<dl><dt>Ghost</dt></dl>",
        "<dl><dd>Ghost</dd></dl>",
        "<table><caption>Ghost</caption></table>",
        "<ul><div>Ghost</div></ul>",
        "<ol><span>Ghost</span></ol>",
    ];
    for candidate in empty_candidates {
        let html =
            format!("<body><main>{candidate}</main><article><p>Fallback.</p></article></body>");
        assert_eq!(html_to_markdown(&html), "Fallback.", "{candidate}");
    }

    assert_eq!(
        html_to_markdown(
            "<body><main><dl><dt>Term</dt><dd>Definition</dd></dl></main>\
             <article>Fallback.</article></body>"
        ),
        "Term\n: Definition"
    );
    assert_eq!(
        html_to_markdown(
            "<body><main><table><tr><td>Cell</td></tr></table></main>\
             <article>Fallback.</article></body>"
        ),
        "|  |\n| --- |\n| Cell |"
    );
    assert_eq!(
        html_to_markdown(
            "<body><main><ul><li>Item</li></ul></main><article>Fallback.</article></body>"
        ),
        "- Item"
    );
}

#[test]
fn table_code_pipe_preserves_one_cell_and_exact_code_event() {
    let markdown =
        html_to_markdown("<table><tr><th>Expr</th></tr><tr><td><code>a|b</code></td></tr></table>");
    assert_eq!(markdown, "| Expr |\n| --- |\n| `a\\|b` |");
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "code:a|b"), "{events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("start:TableCell"))
            .count(),
        2,
        "{events:?}"
    );
}

#[test]
fn empty_code_is_non_renderable_in_output_and_metadata() {
    assert_eq!(html_to_markdown("<p>A<code></code>B</p>"), "AB");
    assert_eq!(html_to_markdown("<p><code></code></p>"), "");
    assert_eq!(html_to_markdown("<ul><li><code></code></li></ul>"), "");
    assert!(parse_events(&html_to_markdown("<code></code>"))
        .iter()
        .all(|event| event != "text:``" && !event.starts_with("code:")));
}

#[test]
fn entity_like_text_is_preserved_in_all_inline_contexts() {
    let markdown = html_to_markdown(
        "<p>&amp;copy; &amp;#35; &amp;#x3C;</p>\
         <p><a href=\"/x\">&amp;copy;</a><img src=\"/x\" alt=\"&amp;#35;\"></p>\
         <p><a href=\"javascript:bad\">&amp;lt;</a>\
         <img src=\"javascript:bad\" alt=\"&amp;gt;\"></p>",
    );
    assert_eq!(
        markdown,
        "\\&copy; \\&#35; \\&#x3C;\n\n[\\&copy;](/x)![\\&#35;](/x)\n\n\\&lt;\\&gt;"
    );
    let events = parse_events(&markdown);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| event.strip_prefix("text:"))
            .collect::<Vec<_>>(),
        ["&copy; ", "&#35; ", "&#x3C;", "&copy;", "&#35;", "&lt;", "&gt;"]
    );
}

#[test]
fn destination_limits_use_exact_normalized_output_not_raw_length() {
    let forbidden = format!("javascript:{}", "x".repeat(100_000));
    let invalid = format!("https://example.com/{}%ZZ", "x".repeat(100_000));
    for destination in [forbidden, invalid] {
        assert_eq!(
            html_to_markdown_with_options(
                &format!("<a href=\"{destination}\">x</a>"),
                &MarkdownOptions {
                    base_url: None,
                    max_chars: 1,
                },
            )
            .unwrap(),
            "x"
        );
    }

    let relative = "a/../".repeat(20);
    let relative_html = format!("<a href=\"{relative}\">x</a>");
    assert_eq!(
        html_to_markdown_with_options(
            &relative_html,
            &MarkdownOptions {
                base_url: Some(Url::parse("https://example.com/base/").unwrap()),
                max_chars: 30,
            },
        )
        .unwrap(),
        "[x](https://example.com/base/)"
    );
    assert!(matches!(
        html_to_markdown_with_options(
            &relative_html,
            &MarkdownOptions {
                base_url: Some(Url::parse("https://example.com/base/").unwrap()),
                max_chars: 29,
            },
        ),
        Err(Error::BodyLimit { limit: 29 })
    ));

    let absolute = format!("https://example.com/{}", "a/../".repeat(20));
    let absolute_html = format!("<a href=\"{absolute}\">x</a>");
    assert_eq!(
        html_to_markdown_with_options(
            &absolute_html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 25,
            },
        )
        .unwrap(),
        "[x](https://example.com/)"
    );
    assert!(matches!(
        html_to_markdown_with_options(
            &absolute_html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 24,
            },
        ),
        Err(Error::BodyLimit { limit: 24 })
    ));
}

#[test]
fn special_http_references_match_url_crate_targets() {
    let base = "https://base.test/p/q";
    let fixtures = [
        (None, r"https://example.com\a\b"),
        (Some(base), r"\\evil.test\x"),
        (None, r"https://example.com\@evil.example/x"),
        (None, "http:example.com"),
        (Some(base), "https:foo"),
        (None, r"https:\\example.com/a\b/..\c\%2e\d"),
        (Some(base), r"a\b/..\c/%2e%2e\d"),
        (None, r"https://EXAMPLE.com:443\a"),
        (None, r"https://[2001:db8::1]:443\a"),
        (None, r"https://example.com\a?q=one\two#f\g"),
    ];

    for (base_url, raw) in fixtures {
        let expected_destination = canonical_http_destination(raw, base_url);
        let expected_markdown = exact_link_markdown(&expected_destination);
        let exact_limit = expected_markdown.chars().count();
        let options = MarkdownOptions {
            base_url: base_url.map(|base_url| Url::parse(base_url).unwrap()),
            max_chars: exact_limit,
        };
        let markdown =
            html_to_markdown_with_options(&format!(r#"<a href="{raw}">x</a>"#), &options).unwrap();

        assert_eq!(
            markdown, expected_markdown,
            "raw={raw:?}, base={base_url:?}"
        );
        assert_eq!(
            parsed_destinations(&markdown).0,
            [expected_destination],
            "raw={raw:?}, base={base_url:?}"
        );
        assert!(
            matches!(
                html_to_markdown_with_options(
                    &format!(r#"<a href="{raw}">x</a>"#),
                    &MarkdownOptions {
                        base_url: base_url.map(|base_url| Url::parse(base_url).unwrap()),
                        max_chars: exact_limit - 1,
                    },
                ),
                Err(Error::BodyLimit { limit }) if limit == exact_limit - 1
            ),
            "one-short URL rendered: raw={raw:?}, base={base_url:?}"
        );
    }

    for raw in [
        r"https://user:pass@example.com\a",
        r"https:\\user:pass@example.com\a",
    ] {
        assert_eq!(
            html_to_markdown_with_options(
                &format!(r#"<a href="{raw}">x</a>"#),
                &MarkdownOptions {
                    base_url: None,
                    max_chars: 1,
                },
            )
            .unwrap(),
            "x",
            "accepted credentials in {raw:?}"
        );
    }
    assert_eq!(
        html_to_markdown_with_options(
            r#"<a href="\\user:pass@evil.test\x">x</a>"#,
            &MarkdownOptions {
                base_url: Some(Url::parse(base).unwrap()),
                max_chars: 1,
            },
        )
        .unwrap(),
        "x"
    );
}

#[test]
fn backslash_dot_collapse_uses_exact_final_limit() {
    let relative = r"a\..\".repeat(20);
    let relative_destination =
        canonical_http_destination(&relative, Some("https://example.com/base/"));
    assert_eq!(relative_destination, "https://example.com/base/");
    let relative_expected = exact_link_markdown(&relative_destination);
    assert_eq!(relative_expected.chars().count(), 30);
    let relative_html = format!(r#"<a href="{relative}">x</a>"#);
    assert_eq!(
        html_to_markdown_with_options(
            &relative_html,
            &MarkdownOptions {
                base_url: Some(Url::parse("https://example.com/base/").unwrap()),
                max_chars: 30,
            },
        )
        .unwrap(),
        relative_expected
    );
    assert!(matches!(
        html_to_markdown_with_options(
            &relative_html,
            &MarkdownOptions {
                base_url: Some(Url::parse("https://example.com/base/").unwrap()),
                max_chars: 29,
            },
        ),
        Err(Error::BodyLimit { limit: 29 })
    ));

    let absolute = format!(r"https://example.com\{}", r"a\..\".repeat(20));
    let absolute_destination = canonical_http_destination(&absolute, None);
    assert_eq!(absolute_destination, "https://example.com/");
    let absolute_expected = exact_link_markdown(&absolute_destination);
    assert_eq!(absolute_expected.chars().count(), 25);
    let absolute_html = format!(r#"<a href="{absolute}">x</a>"#);
    assert_eq!(
        html_to_markdown_with_options(
            &absolute_html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 25,
            },
        )
        .unwrap(),
        absolute_expected
    );
    assert!(matches!(
        html_to_markdown_with_options(
            &absolute_html,
            &MarkdownOptions {
                base_url: None,
                max_chars: 24,
            },
        ),
        Err(Error::BodyLimit { limit: 24 })
    ));
}

#[test]
fn invalid_images_preserve_alt_fallback_and_split_inline_runs() {
    for image in [
        "<img src=\"javascript:bad\" alt=\"Fallback\">",
        "<img alt=\"Fallback\">",
    ] {
        let markdown = html_to_markdown(&format!("<p>{image}</p>"));
        assert_eq!(markdown, "Fallback");
        assert_eq!(
            parse_events(&markdown),
            ["start:Paragraph", "text:Fallback", "end:Paragraph"]
        );
    }

    let strong = html_to_markdown(
        "<p><strong>A</strong><img src=\"javascript:bad\" alt=\"X\"><strong>B</strong></p>",
    );
    assert_eq!(strong, "**A**X**B**");
    assert_eq!(
        parse_events(&strong),
        [
            "start:Paragraph",
            "start:Strong",
            "text:A",
            "end:Strong",
            "text:X",
            "start:Strong",
            "text:B",
            "end:Strong",
            "end:Paragraph",
        ]
    );

    let code = html_to_markdown(
        "<p><code>A</code><img src=\"javascript:bad\" alt=\"X\"><code>B</code></p>",
    );
    assert_eq!(code, "`A`X`B`");
    assert_eq!(
        parse_events(&code),
        [
            "start:Paragraph",
            "code:A",
            "text:X",
            "code:B",
            "end:Paragraph",
        ]
    );
}

#[test]
fn nested_same_family_wrappers_flatten_to_one_parser_semantic_run() {
    let fixtures = [
        ("strong", "**XAY**", "Strong"),
        ("em", "*XAY*", "Emphasis"),
        ("del", "~~XAY~~", "Strikethrough"),
    ];

    for (tag, expected, event) in fixtures {
        for body in [
            format!("<{tag}><{tag}>XAY</{tag}></{tag}>"),
            format!("<{tag}>X<{tag}>A</{tag}>Y</{tag}>"),
        ] {
            let markdown = html_to_markdown(&body);
            assert_eq!(markdown, expected, "{tag}: {body}");
            assert_eq!(
                parse_events(&markdown),
                [
                    "start:Paragraph".to_owned(),
                    format!("start:{event}"),
                    "text:XAY".to_owned(),
                    format!("end:{event}"),
                    "end:Paragraph".to_owned(),
                ],
                "{tag}: {body}"
            );
        }
    }
}

#[test]
fn mixed_wrapper_partial_nesting_preserves_exact_ast() {
    let fixtures = [
        (
            "<p><strong>X<em>Y</em>Z</strong></p>",
            "**X*Y*Z**",
            vec![
                "start:Paragraph",
                "start:Strong",
                "text:X",
                "start:Emphasis",
                "text:Y",
                "end:Emphasis",
                "text:Z",
                "end:Strong",
                "end:Paragraph",
            ],
        ),
        (
            "<p><strong><em>A</em>B</strong></p>",
            "**_A_&#66;**",
            vec![
                "start:Paragraph",
                "start:Strong",
                "start:Emphasis",
                "text:A",
                "end:Emphasis",
                "text:B",
                "end:Strong",
                "end:Paragraph",
            ],
        ),
        (
            "<p><em>X<strong>Y</strong>Z</em></p>",
            "*X**Y**Z*",
            vec![
                "start:Paragraph",
                "start:Emphasis",
                "text:X",
                "start:Strong",
                "text:Y",
                "end:Strong",
                "text:Z",
                "end:Emphasis",
                "end:Paragraph",
            ],
        ),
        (
            "<p><em><strong>A</strong>B</em></p>",
            "***A**B*",
            vec![
                "start:Paragraph",
                "start:Emphasis",
                "start:Strong",
                "text:A",
                "end:Strong",
                "text:B",
                "end:Emphasis",
                "end:Paragraph",
            ],
        ),
        (
            "<p><del>X<strong>Y<em>Z</em>Q</strong>R</del></p>",
            "~~&#88;**Y*Z*Q**R~~",
            vec![
                "start:Paragraph",
                "start:Strikethrough",
                "text:X",
                "start:Strong",
                "text:Y",
                "start:Emphasis",
                "text:Z",
                "end:Emphasis",
                "text:Q",
                "end:Strong",
                "text:R",
                "end:Strikethrough",
                "end:Paragraph",
            ],
        ),
    ];

    for (html, expected_markdown, expected_events) in fixtures {
        assert_eq!(
            compact_events(expected_markdown),
            expected_events,
            "bad fixture"
        );
        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected_markdown, "{html}");
        assert_eq!(compact_events(&markdown), expected_events, "{html}");
    }
}

#[test]
fn wrapper_delimiters_use_actual_emitted_punctuation_boundaries() {
    for (tag, marker, event) in [
        ("strong", "**", "Strong"),
        ("em", "*", "Emphasis"),
        ("del", "~~", "Strikethrough"),
    ] {
        let leading_html = format!(
            "<p>A<{tag}><span hidden>secret</span><!-- split --><wbr><span></span>!B</{tag}></p>"
        );
        let leading_expected = format!("&#65;{marker}\\!B{marker}");
        let leading_events = vec![
            "start:Paragraph".to_owned(),
            "text:A".to_owned(),
            format!("start:{event}"),
            "text:!B".to_owned(),
            format!("end:{event}"),
            "end:Paragraph".to_owned(),
        ];
        assert_eq!(
            compact_events(&leading_expected),
            leading_events,
            "bad leading-{tag} parser fixture"
        );
        let leading = html_to_markdown(&leading_html);
        assert_eq!(leading, leading_expected, "leading {tag}");
        assert_eq!(compact_events(&leading), leading_events, "leading {tag}");
        assert_eq!(parsed_visible_text(&leading), "A!B", "leading {tag}");

        let trailing_html = format!(
            "<p><{tag}>A!</{tag}><!-- split --><span hidden>secret</span><wbr><span></span>C</p>"
        );
        let trailing_expected = format!("{marker}A!{marker}&#67;");
        let trailing_events = vec![
            "start:Paragraph".to_owned(),
            format!("start:{event}"),
            "text:A!".to_owned(),
            format!("end:{event}"),
            "text:C".to_owned(),
            "end:Paragraph".to_owned(),
        ];
        assert_eq!(
            compact_events(&trailing_expected),
            trailing_events,
            "bad trailing-{tag} parser fixture"
        );
        let trailing = html_to_markdown(&trailing_html);
        assert_eq!(trailing, trailing_expected, "trailing {tag}");
        assert_eq!(compact_events(&trailing), trailing_events, "trailing {tag}");
        assert_eq!(parsed_visible_text(&trailing), "A!C", "trailing {tag}");
    }
}

#[test]
fn wrappers_preserve_terminal_inline_emitters_and_logically_empty_tails() {
    let fixtures = [
        (
            "hidden/comment/wbr tail",
            "<p><strong><a href=\"/x\">A</a><!-- split --><span hidden>secret</span><wbr><span></span></strong>C</p>",
            "**[A](/x)**&#67;",
            vec![
                "start:Paragraph",
                "start:Strong",
                "start:Link",
                "text:A",
                "end:Link",
                "end:Strong",
                "text:C",
                "end:Paragraph",
            ],
            "AC",
        ),
        (
            "empty tail",
            "<p><strong><a href=\"/x\">A</a><span></span></strong>C</p>",
            "**[A](/x)**&#67;",
            vec![
                "start:Paragraph",
                "start:Strong",
                "start:Link",
                "text:A",
                "end:Link",
                "end:Strong",
                "text:C",
                "end:Paragraph",
            ],
            "AC",
        ),
        (
            "partial text and terminal link",
            "<p><em>X<a href=\"/x\">A</a></em>C</p>",
            "*X[A](/x)*&#67;",
            vec![
                "start:Paragraph",
                "start:Emphasis",
                "text:X",
                "start:Link",
                "text:A",
                "end:Link",
                "end:Emphasis",
                "text:C",
                "end:Paragraph",
            ],
            "XAC",
        ),
        (
            "terminal image",
            "<p><strong><img alt=\"A\" src=\"/x\"></strong>C</p>",
            "**![A](/x)**&#67;",
            vec![
                "start:Paragraph",
                "start:Strong",
                "start:Image",
                "text:A",
                "end:Image",
                "end:Strong",
                "text:C",
                "end:Paragraph",
            ],
            "AC",
        ),
        (
            "terminal code",
            "<p><em><code>A</code></em>C</p>",
            "*`A`*&#67;",
            vec![
                "start:Paragraph",
                "start:Emphasis",
                "code:A",
                "end:Emphasis",
                "text:C",
                "end:Paragraph",
            ],
            "AC",
        ),
    ];

    for (name, html, expected_markdown, expected_events, expected_visible) in fixtures {
        assert_eq!(
            compact_events(expected_markdown),
            expected_events,
            "bad {name} parser fixture"
        );
        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected_markdown, "{name}");
        assert_eq!(compact_events(&markdown), expected_events, "{name}");
        assert_eq!(parsed_visible_text(&markdown), expected_visible, "{name}");
    }
}

#[test]
fn independent_mixed_and_table_wrappers_keep_exact_parser_event_order() {
    let independent_html = "<p><strong><a href=\"/x\">A</a></strong><span hidden>secret</span><strong><a href=\"/y\">B</a></strong>C</p>";
    let independent_expected = "**[A](/x)**__[B](/y)__&#67;";
    let independent_events = [
        "start:Paragraph",
        "start:Strong",
        "start:Link",
        "text:A",
        "end:Link",
        "end:Strong",
        "start:Strong",
        "start:Link",
        "text:B",
        "end:Link",
        "end:Strong",
        "text:C",
        "end:Paragraph",
    ];
    assert_eq!(compact_events(independent_expected), independent_events);

    let mixed_html = "<p>A<strong><em>!B</em></strong>C</p>";
    let mixed_expected = "&#65;**_\\!B_**&#67;";
    let mixed_events = [
        "start:Paragraph",
        "text:A",
        "start:Strong",
        "start:Emphasis",
        "text:!B",
        "end:Emphasis",
        "end:Strong",
        "text:C",
        "end:Paragraph",
    ];
    assert_eq!(
        compact_events(mixed_expected),
        mixed_events,
        "bad mixed-wrapper parser fixture"
    );

    let table_html = "<table><tr><td>A<strong>!B</strong>C</td><td><em>X<a href=\"/x\">A</a></em>C</td></tr></table>";
    let table_expected = "|  |  |\n| --- | --- |\n| &#65;**\\!B**C | *X[A](/x)*&#67; |";
    let table_events = [
        "start:Table",
        "start:TableHead",
        "start:TableCell",
        "end:TableCell",
        "start:TableCell",
        "end:TableCell",
        "end:TableHead",
        "start:TableRow",
        "start:TableCell",
        "text:A",
        "start:Strong",
        "text:!B",
        "end:Strong",
        "text:C",
        "end:TableCell",
        "start:TableCell",
        "start:Emphasis",
        "text:X",
        "start:Link",
        "text:A",
        "end:Link",
        "end:Emphasis",
        "text:C",
        "end:TableCell",
        "end:TableRow",
        "end:Table",
    ];
    assert_eq!(
        compact_events(table_expected),
        table_events,
        "bad table-wrapper parser fixture"
    );

    let independent = html_to_markdown(independent_html);
    assert_eq!(independent, independent_expected);
    assert_eq!(compact_events(&independent), independent_events);
    assert_eq!(parsed_visible_text(&independent), "ABC");

    let mixed = html_to_markdown(mixed_html);
    assert_eq!(mixed, mixed_expected);
    assert_eq!(compact_events(&mixed), mixed_events);
    assert_eq!(parsed_visible_text(&mixed), "A!BC");

    let partial_mixed_html = "<p>A<strong>X<em>!B</em>Y</strong>C</p>";
    let partial_mixed_expected = "&#65;**&#88;*\\!B*Y**C";
    let partial_mixed_events = [
        "start:Paragraph",
        "text:A",
        "start:Strong",
        "text:X",
        "start:Emphasis",
        "text:!B",
        "end:Emphasis",
        "text:Y",
        "end:Strong",
        "text:C",
        "end:Paragraph",
    ];
    assert_eq!(
        compact_events(partial_mixed_expected),
        partial_mixed_events,
        "bad partial mixed-wrapper parser fixture"
    );
    let partial_mixed = html_to_markdown(partial_mixed_html);
    assert_eq!(partial_mixed, partial_mixed_expected);
    assert_eq!(compact_events(&partial_mixed), partial_mixed_events);
    assert_eq!(parsed_visible_text(&partial_mixed), "AX!BYC");

    let cascading_mixed_html = "<p>A<strong>X<em>Y<del>!B</del>Q</em>R</strong>C</p>";
    let cascading_mixed_expected = "&#65;**&#88;*&#89;~~\\!B~~Q*R**C";
    let cascading_mixed_events = [
        "start:Paragraph",
        "text:A",
        "start:Strong",
        "text:X",
        "start:Emphasis",
        "text:Y",
        "start:Strikethrough",
        "text:!B",
        "end:Strikethrough",
        "text:Q",
        "end:Emphasis",
        "text:R",
        "end:Strong",
        "text:C",
        "end:Paragraph",
    ];
    assert_eq!(
        compact_events(cascading_mixed_expected),
        cascading_mixed_events,
        "bad cascading mixed-wrapper parser fixture"
    );
    let cascading_mixed = html_to_markdown(cascading_mixed_html);
    assert_eq!(cascading_mixed, cascading_mixed_expected);
    assert_eq!(compact_events(&cascading_mixed), cascading_mixed_events);
    assert_eq!(parsed_visible_text(&cascading_mixed), "AXY!BQRC");

    let table = html_to_markdown(table_html);
    assert_eq!(table, table_expected);
    assert_eq!(compact_events(&table), table_events);
    assert_eq!(parsed_visible_text(&table), "A!BCXAC");
}

#[test]
fn parser_safe_wrapper_boundaries_obey_exact_character_limits() {
    let fixtures = [
        ("<p>A<strong>!B</strong></p>", "&#65;**\\!B**"),
        ("<p><strong>A!</strong>C</p>", "**A!**&#67;"),
        (
            "<p><strong><a href=\"/x\">A</a><span hidden>secret</span></strong>C</p>",
            "**[A](/x)**&#67;",
        ),
        (
            "<p><strong><img alt=\"A\" src=\"/x\"></strong>C</p>",
            "**![A](/x)**&#67;",
        ),
        ("<p><em><code>A</code></em>C</p>", "*`A`*&#67;"),
        (
            "<p>A<strong>X<em>Y<del>!B</del>Q</em>R</strong>C</p>",
            "&#65;**&#88;*&#89;~~\\!B~~Q*R**C",
        ),
    ];

    for (html, expected) in fixtures {
        let exact_limit = expected.chars().count();
        assert_eq!(
            html_to_markdown_with_options(
                html,
                &MarkdownOptions {
                    base_url: None,
                    max_chars: exact_limit,
                }
            )
            .unwrap(),
            expected,
            "exact limit for {html}"
        );
        assert!(
            matches!(
                html_to_markdown_with_options(
                    html,
                    &MarkdownOptions {
                        base_url: None,
                        max_chars: exact_limit - 1,
                    }
                ),
                Err(Error::BodyLimit { limit }) if limit == exact_limit - 1
            ),
            "one-short limit for {html}"
        );
    }
}

#[test]
fn logical_empty_strong_tails_keep_strong_outside_emphasis() {
    let expected = "**_A_**";
    let expected_events = [
        "start:Paragraph",
        "start:Strong",
        "start:Emphasis",
        "text:A",
        "end:Emphasis",
        "end:Strong",
        "end:Paragraph",
    ];
    assert_eq!(compact_events(expected), expected_events);
    assert_eq!(
        rendered_html(expected),
        "<p><strong><em>A</em></strong></p>\n"
    );

    for (name, html) in [
        ("whitespace", "<p><strong> <em>A</em> </strong></p>"),
        (
            "empty span",
            "<p><strong><em>A</em><span></span></strong></p>",
        ),
        ("comment", "<p><strong><em>A</em><!-- tail --></strong></p>"),
        (
            "hidden and wbr",
            "<p><strong><em>A</em><span hidden>secret</span><wbr></strong></p>",
        ),
        (
            "transparent payload and tail",
            "<p><strong><span><em>A</em></span><span></span></strong></p>",
        ),
    ] {
        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(compact_events(&markdown), expected_events, "{name}");
        assert_eq!(
            rendered_html(&markdown),
            "<p><strong><em>A</em></strong></p>\n",
            "{name}"
        );
        assert_eq!(parsed_visible_text(&markdown), "A", "{name}");
    }
}

#[test]
fn adjacent_strong_emphasis_groups_use_unambiguous_independent_runs() {
    let fixtures = [
        (
            "word tail",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong>C</p>",
            "**_A_**__*B*__&#67;",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong>C</p>\n",
            "ABC",
        ),
        (
            "punctuation tail",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong>!</p>",
            "**_A_**__*B*__!",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong>!</p>\n",
            "AB!",
        ),
        (
            "transparent payloads",
            "<p><strong><span><em>A</em></span></strong><strong><span><em>B</em></span></strong>C</p>",
            "**_A_**__*B*__&#67;",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong>C</p>\n",
            "ABC",
        ),
        (
            "logically absent separators",
            "<p><strong><em>A</em></strong><!-- split --><span hidden>secret</span><wbr><span></span><strong><em>B</em></strong>C</p>",
            "**_A_**__*B*__&#67;",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong>C</p>\n",
            "ABC",
        ),
        (
            "three groups",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong><strong><em>C</em></strong>D</p>",
            "**_A_**__*B*__**_C_**&#68;",
            "<p><strong><em>A</em></strong><strong><em>B</em></strong><strong><em>C</em></strong>D</p>\n",
            "ABCD",
        ),
    ];
    let expected_events = [
        "start:Paragraph",
        "start:Strong",
        "start:Emphasis",
        "text:A",
        "end:Emphasis",
        "end:Strong",
        "start:Strong",
        "start:Emphasis",
        "text:B",
        "end:Emphasis",
        "end:Strong",
        "text:C",
        "end:Paragraph",
    ];

    for (name, html, expected, expected_html, visible) in fixtures {
        if name == "word tail" {
            assert_eq!(compact_events(expected), expected_events, "bad fixture");
        }
        assert_eq!(rendered_html(expected), expected_html, "bad {name} fixture");
        assert_eq!(parsed_visible_text(expected), visible, "bad {name} fixture");

        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(
            compact_events(&markdown),
            compact_events(expected),
            "{name}"
        );
        assert_eq!(rendered_html(&markdown), expected_html, "{name}");
        assert_eq!(parsed_visible_text(&markdown), visible, "{name}");
    }
}

#[test]
fn mixed_wrapper_group_payloads_preserve_parser_html_and_visible_text() {
    let intrinsic_html = "<p><strong><em>L<a href=\"/x\">A</a></em></strong><strong><em>I<img alt=\"B\" src=\"/y\"></em></strong><strong><em>K<code>C</code></em></strong>D</p>";
    let intrinsic_expected = "**_L[A](/x)_**__*I![B](/y)*__**_K`C`_**&#68;";
    let intrinsic_events = [
        "start:Paragraph",
        "start:Strong",
        "start:Emphasis",
        "text:L",
        "start:Link",
        "text:A",
        "end:Link",
        "end:Emphasis",
        "end:Strong",
        "start:Strong",
        "start:Emphasis",
        "text:I",
        "start:Image",
        "text:B",
        "end:Image",
        "end:Emphasis",
        "end:Strong",
        "start:Strong",
        "start:Emphasis",
        "text:K",
        "code:C",
        "end:Emphasis",
        "end:Strong",
        "text:D",
        "end:Paragraph",
    ];
    let intrinsic_rendered = "<p><strong><em>L<a href=\"/x\">A</a></em></strong><strong><em>I<img src=\"/y\" alt=\"B\" /></em></strong><strong><em>K<code>C</code></em></strong>D</p>\n";
    assert_eq!(compact_events(intrinsic_expected), intrinsic_events);
    assert_eq!(rendered_html(intrinsic_expected), intrinsic_rendered);

    let intrinsic = html_to_markdown(intrinsic_html);
    assert_eq!(intrinsic, intrinsic_expected);
    assert_eq!(compact_events(&intrinsic), intrinsic_events);
    assert_eq!(rendered_html(&intrinsic), intrinsic_rendered);
    assert_eq!(parsed_visible_text(&intrinsic), "LAIBKCD");

    let pure_intrinsic_html = "<p><strong><em><a href=\"/x\">A</a></em></strong><strong><em><img alt=\"B\" src=\"/y\"></em></strong><strong><em><code>C</code></em></strong>D</p>";
    let pure_intrinsic_expected = "**_[A](/x)_**__*![B](/y)*__**_`C`_**&#68;";
    let pure_intrinsic_events = [
        "start:Paragraph",
        "start:Strong",
        "start:Emphasis",
        "start:Link",
        "text:A",
        "end:Link",
        "end:Emphasis",
        "end:Strong",
        "start:Strong",
        "start:Emphasis",
        "start:Image",
        "text:B",
        "end:Image",
        "end:Emphasis",
        "end:Strong",
        "start:Strong",
        "start:Emphasis",
        "code:C",
        "end:Emphasis",
        "end:Strong",
        "text:D",
        "end:Paragraph",
    ];
    assert_eq!(
        compact_events(pure_intrinsic_expected),
        pure_intrinsic_events
    );
    let pure_intrinsic = html_to_markdown(pure_intrinsic_html);
    assert_eq!(pure_intrinsic, pure_intrinsic_expected);
    assert_eq!(compact_events(&pure_intrinsic), pure_intrinsic_events);
    assert_eq!(
        rendered_html(&pure_intrinsic),
        "<p><strong><em><a href=\"/x\">A</a></em></strong><strong><em><img src=\"/y\" alt=\"B\" /></em></strong><strong><em><code>C</code></em></strong>D</p>\n"
    );
    assert_eq!(parsed_visible_text(&pure_intrinsic), "ABCD");

    let mixed_html =
        "<p><strong><em><del>A</del></em></strong><strong><em><del>B</del></em></strong>C</p>";
    let mixed_expected = "**_~~A~~_**__*~~B~~*__&#67;";
    let mixed_rendered =
        "<p><strong><em><del>A</del></em></strong><strong><em><del>B</del></em></strong>C</p>\n";
    assert_eq!(rendered_html(mixed_expected), mixed_rendered);
    let mixed = html_to_markdown(mixed_html);
    assert_eq!(mixed, mixed_expected);
    assert_eq!(compact_events(&mixed), compact_events(mixed_expected));
    assert_eq!(rendered_html(&mixed), mixed_rendered);
    assert_eq!(parsed_visible_text(&mixed), "ABC");

    let partial_html = "<p><strong><em>A</em>B</strong></p>";
    let partial_expected = "**_A_&#66;**";
    let partial_rendered = "<p><strong><em>A</em>B</strong></p>\n";
    assert_eq!(rendered_html(partial_expected), partial_rendered);
    let partial = html_to_markdown(partial_html);
    assert_eq!(partial, partial_expected);
    assert_eq!(compact_events(&partial), compact_events(partial_expected));
    assert_eq!(rendered_html(&partial), partial_rendered);
    assert_eq!(parsed_visible_text(&partial), "AB");

    let table_html =
        "<table><tr><td><strong><em>A</em></strong><strong><em>B</em></strong>C</td></tr></table>";
    let table_expected = "|  |\n| --- |\n| **_A_**__*B*__&#67; |";
    let table_events = [
        "start:Table",
        "start:TableHead",
        "start:TableCell",
        "end:TableCell",
        "end:TableHead",
        "start:TableRow",
        "start:TableCell",
        "start:Strong",
        "start:Emphasis",
        "text:A",
        "end:Emphasis",
        "end:Strong",
        "start:Strong",
        "start:Emphasis",
        "text:B",
        "end:Emphasis",
        "end:Strong",
        "text:C",
        "end:TableCell",
        "end:TableRow",
        "end:Table",
    ];
    assert_eq!(compact_events(table_expected), table_events);
    assert!(rendered_html(table_expected)
        .contains("<td><strong><em>A</em></strong><strong><em>B</em></strong>C</td>"));
    let table = html_to_markdown(table_html);
    assert_eq!(table, table_expected);
    assert_eq!(compact_events(&table), table_events);
    assert_eq!(rendered_html(&table), rendered_html(table_expected));
    assert_eq!(parsed_visible_text(&table), "ABC");
}

#[test]
fn logical_wrapper_group_spelling_obeys_exact_character_limits() {
    let fixtures = [
        ("<p><strong> <em>A</em> </strong></p>", "**_A_**"),
        (
            "<p><strong><em>A</em></strong><strong><em>B</em></strong>C</p>",
            "**_A_**__*B*__&#67;",
        ),
        (
            "<p><strong><em>L<a href=\"/x\">A</a></em></strong><strong><em>I<img alt=\"B\" src=\"/y\"></em></strong><strong><em>K<code>C</code></em></strong>D</p>",
            "**_L[A](/x)_**__*I![B](/y)*__**_K`C`_**&#68;",
        ),
        (
            "<p><strong><em><a href=\"/x\">A</a></em></strong><strong><em><img alt=\"B\" src=\"/y\"></em></strong><strong><em><code>C</code></em></strong>D</p>",
            "**_[A](/x)_**__*![B](/y)*__**_`C`_**&#68;",
        ),
        (
            "<p><strong><em><del>A</del></em></strong><strong><em><del>B</del></em></strong>C</p>",
            "**_~~A~~_**__*~~B~~*__&#67;",
        ),
        (
            "<table><tr><td><strong><em>A</em></strong><strong><em>B</em></strong>C</td></tr></table>",
            "|  |\n| --- |\n| **_A_**__*B*__&#67; |",
        ),
    ];

    for (html, expected) in fixtures {
        let exact = expected.chars().count();
        assert_eq!(
            html_to_markdown_with_options(
                html,
                &MarkdownOptions {
                    base_url: None,
                    max_chars: exact,
                },
            )
            .unwrap(),
            expected,
            "exact limit for {html}",
        );
        assert!(
            matches!(
                html_to_markdown_with_options(
                    html,
                    &MarkdownOptions {
                        base_url: None,
                        max_chars: exact - 1,
                    },
                ),
                Err(Error::BodyLimit { limit }) if limit == exact - 1
            ),
            "one-short limit for {html}",
        );
    }
}

#[test]
fn independent_complex_wrapper_groups_close_the_round_five_repros() {
    let fixtures = [
        (
            "ordinary then two emphasis groups",
            "<p><strong>A</strong><strong><em>B</em></strong><strong><em>C</em></strong>Z</p>",
            "**A**__*B*__**_C_**&#90;",
            "<p><strong>A</strong><strong><em>B</em></strong><strong><em>C</em></strong>Z</p>\n",
            "ABCZ",
        ),
        (
            "ordinary then two deletion groups",
            "<p><strong>A</strong><strong><del>B</del></strong><strong><del>C</del></strong>Z</p>",
            "**A**__~~B~~__**~~C~~**&#90;",
            "<p><strong>A</strong><strong><del>B</del></strong><strong><del>C</del></strong>Z</p>\n",
            "ABCZ",
        ),
        (
            "ordinary then emphasis and link",
            "<p><strong>A</strong><strong><em><a href=\"/b\">B</a></em></strong>Z</p>",
            "**A**__*[B](/b)*__&#90;",
            "<p><strong>A</strong><strong><em><a href=\"/b\">B</a></em></strong>Z</p>\n",
            "ABZ",
        ),
        (
            "logical empty separators",
            "<p><strong>A</strong><!-- split --><span hidden>secret</span><wbr><span></span><strong><em>B</em></strong><!-- split --><span hidden>secret</span><wbr><span></span><strong><em>C</em></strong>Z</p>",
            "**A**__*B*__**_C_**&#90;",
            "<p><strong>A</strong><strong><em>B</em></strong><strong><em>C</em></strong>Z</p>\n",
            "ABCZ",
        ),
    ];

    for (name, html, expected, expected_html, expected_visible) in fixtures {
        assert_eq!(rendered_html(expected), expected_html, "bad {name} fixture");
        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(
            compact_events(&markdown),
            compact_events(expected),
            "{name}"
        );
        assert_eq!(rendered_html(&markdown), expected_html, "{name}");
        assert_eq!(parsed_visible_text(&markdown), expected_visible, "{name}");
    }
}

#[test]
fn independent_complex_wrapper_groups_preserve_table_cell_event_order() {
    let html = "<table><tr>\
        <td><strong>A</strong><strong><em>B</em></strong><strong><em>C</em></strong>Z</td>\
        <td><strong>A</strong><strong><del>B</del></strong><strong><del>C</del></strong>Z</td>\
        <td><strong>A</strong><strong><em><a href=\"/b\">B</a></em></strong>Z</td>\
        </tr></table>";
    let expected = "|  |  |  |\n| --- | --- | --- |\n| **A**__*B*__**_C_**&#90; | **A**__~~B~~__**~~C~~**&#90; | **A**__*[B](/b)*__&#90; |";
    let expected_html = rendered_html(expected);
    assert!(expected_html.contains(
        "<td><strong>A</strong><strong><em>B</em></strong><strong><em>C</em></strong>Z</td>"
    ));
    assert!(expected_html.contains(
        "<td><strong>A</strong><strong><del>B</del></strong><strong><del>C</del></strong>Z</td>"
    ));
    assert!(expected_html
        .contains("<td><strong>A</strong><strong><em><a href=\"/b\">B</a></em></strong>Z</td>"));

    let markdown = html_to_markdown(html);
    assert_eq!(markdown, expected);
    assert_eq!(compact_events(&markdown), compact_events(expected));
    assert_eq!(rendered_html(&markdown), expected_html);
    assert_eq!(parsed_visible_text(&markdown), "ABCZABCZABZ");
}

#[test]
fn wrapper_transition_matrix_preserves_every_scalar_stack() {
    let payloads = [
        WrapperTransitionPayload::Plain,
        WrapperTransitionPayload::LeadingEmphasis,
        WrapperTransitionPayload::PartialEmphasis,
        WrapperTransitionPayload::Deletion,
        WrapperTransitionPayload::Link,
        WrapperTransitionPayload::Image,
        WrapperTransitionPayload::Code,
        WrapperTransitionPayload::Transparent,
        WrapperTransitionPayload::Empty,
    ];

    for first in payloads {
        for second in payloads {
            let html = format!(
                "<p><strong>A</strong><strong>{}</strong><strong>{}</strong>Z</p>",
                transition_payload_html(first, 'B'),
                transition_payload_html(second, 'C'),
            );
            let markdown = html_to_markdown(&html);
            let mut expected = vec![('A', vec!["strong"])];
            expected.extend(transition_payload_stacks(first, 'B'));
            expected.extend(transition_payload_stacks(second, 'C'));
            expected.push(('Z', Vec::new()));

            assert_eq!(
                parsed_scalar_stacks(&markdown),
                expected,
                "{first:?} -> {second:?}: {markdown:?}",
            );
            assert_eq!(
                parsed_visible_text(&markdown),
                expected.iter().map(|(ch, _)| *ch).collect::<String>(),
                "{first:?} -> {second:?}: {markdown:?}",
            );
        }
    }
}

#[test]
fn independent_emphasis_groups_alternate_parser_safe_markers() {
    let html = "<p><em>A</em><em><strong>B</strong></em><em><strong>C</strong></em>Z</p>";
    let expected = "*A*_**B**_***C***&#90;";
    let expected_html =
        "<p><em>A</em><em><strong>B</strong></em><em><strong>C</strong></em>Z</p>\n";
    assert_eq!(rendered_html(expected), expected_html, "bad fixture");

    let markdown = html_to_markdown(html);
    assert_eq!(markdown, expected);
    assert_eq!(compact_events(&markdown), compact_events(expected));
    assert_eq!(rendered_html(&markdown), expected_html);
    assert_eq!(parsed_visible_text(&markdown), "ABCZ");
}

#[test]
fn mixed_outer_wrapper_boundaries_preserve_minimal_review_cases() {
    let fixtures = [
        (
            "strong to emphasis link",
            "<p><strong>A</strong><em><a href=\"/b\">B</a></em>Z</p>",
            "**A**_[B](/b)_&#90;",
            "<p><strong>A</strong><em><a href=\"/b\">B</a></em>Z</p>\n",
            "ABZ",
        ),
        (
            "emphasis to strong link",
            "<p><em>A</em><strong><a href=\"/b\">B</a></strong>Z</p>",
            "*A*__[B](/b)__&#90;",
            "<p><em>A</em><strong><a href=\"/b\">B</a></strong>Z</p>\n",
            "ABZ",
        ),
        (
            "strong to emphasis punctuation",
            "<p><strong>A</strong><em>!B</em>Z</p>",
            "**A**_\\!B_&#90;",
            "<p><strong>A</strong><em>!B</em>Z</p>\n",
            "A!BZ",
        ),
        (
            "emphasis to strong punctuation",
            "<p><em>A</em><strong>!B</strong>Z</p>",
            "*A*__\\!B__&#90;",
            "<p><em>A</em><strong>!B</strong>Z</p>\n",
            "A!BZ",
        ),
        (
            "strong to emphasis Unicode code",
            "<p><strong>é</strong><em><code>界</code></em>!</p>",
            "**é**_`界`_!",
            "<p><strong>é</strong><em><code>界</code></em>!</p>\n",
            "é界!",
        ),
        (
            "emphasis to strong Unicode code",
            "<p><em>é</em><strong><code>界</code></strong>!</p>",
            "*é*__`界`__!",
            "<p><em>é</em><strong><code>界</code></strong>!</p>\n",
            "é界!",
        ),
    ];

    for (name, html, expected, expected_html, expected_visible) in fixtures {
        assert_eq!(rendered_html(expected), expected_html, "bad {name} fixture");
        assert!(
            parsed_inline_html(expected).is_empty(),
            "bad {name} fixture"
        );

        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(
            compact_events(&markdown),
            compact_events(expected),
            "{name}"
        );
        assert_eq!(rendered_html(&markdown), expected_html, "{name}");
        assert_eq!(parsed_visible_text(&markdown), expected_visible, "{name}");
        assert!(parsed_inline_html(&markdown).is_empty(), "{name}");
    }
}

#[test]
fn independent_deletion_boundaries_use_only_inert_zero_visible_comments() {
    const BOUNDARY: &str = "<!---->";
    let fixtures = [
        (
            "independent deletion emphasis groups",
            "<p><del><em>A</em></del><del><em>B</em></del>Z</p>",
            "~~*A*~~<!---->~~*B*~~&#90;",
            "<p><del><em>A</em></del><!----><del><em>B</em></del>Z</p>\n",
            "ABZ",
        ),
        (
            "conservative transparent barrier",
            "<p><del>A</del><span><wbr></span><del>B</del>Z</p>",
            "~~A~~<!---->~~B~~Z",
            "<p><del>A</del><!----><del>B</del>Z</p>\n",
            "ABZ",
        ),
    ];

    for (name, html, expected, expected_html, expected_visible) in fixtures {
        assert_eq!(
            parsed_inline_html(expected),
            [BOUNDARY],
            "bad {name} fixture"
        );
        assert_eq!(rendered_html(expected), expected_html, "bad {name} fixture");

        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(
            compact_events(&markdown),
            compact_events(expected),
            "{name}"
        );
        assert_eq!(rendered_html(&markdown), expected_html, "{name}");
        assert_eq!(parsed_visible_text(&markdown), expected_visible, "{name}");
        assert_eq!(parsed_inline_html(&markdown), [BOUNDARY], "{name}");
        assert!(!expected_html.contains("<script"), "{name}");
        assert!(!expected_html.contains("<style"), "{name}");
        assert!(!expected_html.contains("<iframe"), "{name}");
        assert!(!expected_html.contains("onerror="), "{name}");
    }
}

#[test]
fn mixed_and_deletion_boundaries_preserve_table_cell_event_order() {
    let html = "<table><tr>\
        <td><strong>A</strong><em><a href=\"/b\">B</a></em>Z</td>\
        <td><del><em>C</em></del><del><em>D</em></del>!</td>\
        </tr></table>";
    let expected = "|  |  |\n| --- | --- |\n| **A**_[B](/b)_&#90; | ~~*C*~~<!---->~~*D*~~! |";
    let expected_html = rendered_html(expected);
    assert!(expected_html.contains("<td><strong>A</strong><em><a href=\"/b\">B</a></em>Z</td>"));
    assert!(expected_html.contains("<td><del><em>C</em></del><!----><del><em>D</em></del>!</td>"));
    assert_eq!(parsed_inline_html(expected), ["<!---->"]);

    let markdown = html_to_markdown(html);
    assert_eq!(markdown, expected);
    assert_eq!(compact_events(&markdown), compact_events(expected));
    assert_eq!(rendered_html(&markdown), expected_html);
    assert_eq!(parsed_visible_text(&markdown), "ABZCD!");
    assert_eq!(parsed_inline_html(&markdown), ["<!---->"]);
}

#[test]
fn four_group_mixed_outer_family_matrix_preserves_every_scalar_stack() {
    const FAMILIES: [WrapperOuterFamily; 3] = [
        WrapperOuterFamily::Strong,
        WrapperOuterFamily::Emphasis,
        WrapperOuterFamily::Deletion,
    ];
    const PAYLOADS: [WrapperTransitionPayload; 4] = [
        WrapperTransitionPayload::Transparent,
        WrapperTransitionPayload::Link,
        WrapperTransitionPayload::Code,
        WrapperTransitionPayload::LeadingEmphasis,
    ];
    const CHARS: [char; 4] = ['A', 'B', 'C', 'D'];

    for first in FAMILIES {
        for second in FAMILIES {
            for third in FAMILIES {
                for fourth in FAMILIES {
                    let families = [first, second, third, fourth];
                    let mut html = String::from("<p>");
                    let mut expected_html = String::from("<p>");
                    let mut expected_stacks = Vec::new();
                    let mut expected_comments = 0usize;

                    for index in 0..families.len() {
                        if index > 0
                            && families[index - 1] == WrapperOuterFamily::Deletion
                            && families[index] == WrapperOuterFamily::Deletion
                        {
                            expected_html.push_str("<!---->");
                            expected_comments += 1;
                        }
                        html.push_str(&wrapped_transition_payload_html(
                            families[index],
                            PAYLOADS[index],
                            CHARS[index],
                        ));
                        expected_html.push_str(&wrapped_transition_payload_rendered_html(
                            families[index],
                            PAYLOADS[index],
                            CHARS[index],
                        ));
                        expected_stacks.extend(transition_payload_stacks_for_outer(
                            PAYLOADS[index],
                            CHARS[index],
                            families[index],
                        ));
                    }
                    html.push_str("Z</p>");
                    expected_html.push_str("Z</p>\n");
                    expected_stacks.push(('Z', Vec::new()));

                    let markdown = html_to_markdown(&html);
                    assert_eq!(
                        parsed_scalar_stacks(&markdown),
                        expected_stacks,
                        "{families:?}: {markdown:?}",
                    );
                    assert_eq!(
                        parsed_visible_text(&markdown),
                        expected_stacks
                            .iter()
                            .map(|(ch, _)| *ch)
                            .collect::<String>(),
                        "{families:?}: {markdown:?}",
                    );
                    assert_eq!(
                        rendered_html(&markdown),
                        expected_html,
                        "{families:?}: {markdown:?}",
                    );
                    assert_eq!(
                        parsed_inline_html(&markdown),
                        vec!["<!---->".to_owned(); expected_comments],
                        "{families:?}: {markdown:?}",
                    );
                }
            }
        }
    }
}

#[test]
fn fixed_deletion_four_group_transition_matrix_has_zero_marker_leaks() {
    const PAYLOADS: [WrapperTransitionPayload; 9] = [
        WrapperTransitionPayload::Plain,
        WrapperTransitionPayload::LeadingEmphasis,
        WrapperTransitionPayload::PartialEmphasis,
        WrapperTransitionPayload::Deletion,
        WrapperTransitionPayload::Link,
        WrapperTransitionPayload::Image,
        WrapperTransitionPayload::Code,
        WrapperTransitionPayload::Transparent,
        WrapperTransitionPayload::Empty,
    ];
    const SEPARATORS: [WrapperTransitionSeparator; 3] = [
        WrapperTransitionSeparator::Adjacent,
        WrapperTransitionSeparator::DirectEmpty,
        WrapperTransitionSeparator::ConservativeBarrier,
    ];

    for first in PAYLOADS {
        for second in PAYLOADS {
            for third in PAYLOADS {
                for first_separator in SEPARATORS {
                    for second_separator in SEPARATORS {
                        for third_separator in SEPARATORS {
                            let html = format!(
                                "<p><del>A</del>{}<del>{}</del>{}<del>{}</del>{}<del>{}</del>Z</p>",
                                first_separator.html(),
                                transition_payload_html(first, 'B'),
                                second_separator.html(),
                                transition_payload_html(second, 'C'),
                                third_separator.html(),
                                transition_payload_html(third, 'D'),
                            );
                            let markdown = html_to_markdown(&html);
                            let mut expected =
                                vec![('A', vec![WrapperOuterFamily::Deletion.stack()])];
                            expected.extend(transition_payload_stacks_for_outer(
                                first,
                                'B',
                                WrapperOuterFamily::Deletion,
                            ));
                            expected.extend(transition_payload_stacks_for_outer(
                                second,
                                'C',
                                WrapperOuterFamily::Deletion,
                            ));
                            expected.extend(transition_payload_stacks_for_outer(
                                third,
                                'D',
                                WrapperOuterFamily::Deletion,
                            ));
                            expected.push(('Z', Vec::new()));

                            assert_eq!(
                                parsed_scalar_stacks(&markdown),
                                expected,
                                "{first:?}/{first_separator:?} -> {second:?}/{second_separator:?} -> {third:?}/{third_separator:?}: {markdown:?}",
                            );
                            assert_eq!(
                                parsed_visible_text(&markdown),
                                expected.iter().map(|(ch, _)| *ch).collect::<String>(),
                                "{first:?}/{first_separator:?} -> {second:?}/{second_separator:?} -> {third:?}/{third_separator:?}: {markdown:?}",
                            );
                            assert!(
                                parsed_inline_html(&markdown)
                                    .iter()
                                    .all(|html| html == "<!---->"),
                                "{first:?}/{first_separator:?} -> {second:?}/{second_separator:?} -> {third:?}/{third_separator:?}: {markdown:?}",
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn mixed_and_deletion_boundaries_obey_exact_character_limits() {
    let fixtures = [
        (
            "<p><strong>A</strong><em><a href=\"/b\">B</a></em>Z</p>",
            "**A**_[B](/b)_&#90;",
        ),
        (
            "<p><em>A</em><strong><a href=\"/b\">B</a></strong>Z</p>",
            "*A*__[B](/b)__&#90;",
        ),
        (
            "<p><strong>é</strong><em><code>界</code></em>!</p>",
            "**é**_`界`_!",
        ),
        (
            "<p><del><em>A</em></del><del><em>B</em></del>Z</p>",
            "~~*A*~~<!---->~~*B*~~&#90;",
        ),
        (
            "<p><del>A</del><span><wbr></span><del>B</del>Z</p>",
            "~~A~~<!---->~~B~~Z",
        ),
        (
            "<table><tr><td><strong>A</strong><em><a href=\"/b\">B</a></em>Z</td><td><del><em>C</em></del><del><em>D</em></del>!</td></tr></table>",
            "|  |  |\n| --- | --- |\n| **A**_[B](/b)_&#90; | ~~*C*~~<!---->~~*D*~~! |",
        ),
    ];

    for (html, expected) in fixtures {
        let exact = expected.chars().count();
        assert_eq!(
            html_to_markdown_with_options(
                html,
                &MarkdownOptions {
                    base_url: None,
                    max_chars: exact,
                },
            )
            .unwrap(),
            expected,
            "exact limit for {html}",
        );
        assert!(
            matches!(
                html_to_markdown_with_options(
                    html,
                    &MarkdownOptions {
                        base_url: None,
                        max_chars: exact - 1,
                    },
                ),
                Err(Error::BodyLimit { limit }) if limit == exact - 1
            ),
            "one-short limit for {html}",
        );
    }
}

#[test]
fn pending_deletion_boundaries_are_absent_after_leading_and_consecutive_breaks() {
    let fixtures = [
        (
            "root leading break",
            "<p><del>A</del><del><br>B</del>Z</p>",
            "~~A~~  \n~~B~~Z",
            1,
        ),
        (
            "root consecutive breaks",
            "<p><del>A</del><del><br><br>B</del>Z</p>",
            "~~A~~  \n\\\n~~B~~Z",
            2,
        ),
        (
            "blockquote leading break",
            "<blockquote><p><del>A</del><del><br>B</del>Z</p></blockquote>",
            "> ~~A~~  \n> ~~B~~Z",
            1,
        ),
        (
            "blockquote consecutive breaks",
            "<blockquote><p><del>A</del><del><br><br>B</del>Z</p></blockquote>",
            "> ~~A~~  \n> \\\n> ~~B~~Z",
            2,
        ),
        (
            "list leading break",
            "<ul><li><del>A</del><del><br>B</del>Z</li></ul>",
            "- ~~A~~  \n    ~~B~~Z",
            1,
        ),
        (
            "list consecutive breaks",
            "<ul><li><del>A</del><del><br><br>B</del>Z</li></ul>",
            "- ~~A~~  \n    \\\n    ~~B~~Z",
            2,
        ),
    ];

    for (name, html, expected, expected_breaks) in fixtures {
        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{name}");
        assert_only_inert_inline_boundaries(&markdown, 0);
        assert_eq!(
            compact_events(&markdown)
                .iter()
                .filter(|event| event.as_str() == "HardBreak")
                .count(),
            expected_breaks,
            "{name}: {markdown:?}",
        );
        assert_eq!(
            parsed_scalar_stacks(&markdown),
            [
                ('A', vec!["deletion"]),
                ('B', vec!["deletion"]),
                ('Z', Vec::new()),
            ],
            "{name}: {markdown:?}",
        );
        assert_eq!(parsed_visible_text(&markdown), "ABZ", "{name}");
    }
}

#[test]
fn entity_encoded_hostile_text_after_breaks_never_becomes_raw_html() {
    const IMG_ENTITY: &str = "&lt;img src=x onerror=alert(1) data-pad=x&gt;";
    const IMG_TEXT: &str = "<img src=x onerror=alert(1) data-pad=x>";
    const SVG_ENTITY: &str = "&lt;svg onload=alert(2) data-pad=x&gt;&lt;/svg&gt;";
    const SVG_TEXT: &str = "<svg onload=alert(2) data-pad=x></svg>";
    const LINK_ENTITY: &str =
        "&lt;a href=&quot;javascript:alert(3)&quot; data-pad=x&gt;click&lt;/a&gt;";
    const LINK_TEXT: &str = "<a href=\"javascript:alert(3)\" data-pad=x>click</a>";

    let payloads = [
        ("image", IMG_ENTITY, IMG_TEXT),
        ("svg", SVG_ENTITY, SVG_TEXT),
        ("javascript link", LINK_ENTITY, LINK_TEXT),
    ];
    let containers = [
        ("root leading", "<p>", "</p>", "<br>", 1usize),
        (
            "blockquote consecutive",
            "<blockquote><p>",
            "</p></blockquote>",
            "<br><br>",
            2,
        ),
        ("list leading", "<ul><li>", "</li></ul>", "<br>", 1),
    ];

    for (payload_name, entity, decoded) in payloads {
        for (context_name, open, close, breaks, expected_breaks) in containers {
            let html = format!("{open}<del>A</del><del>{breaks}{entity}</del>Z{close}");
            let markdown = html_to_markdown(&html);
            assert_only_inert_inline_boundaries(&markdown, 0);
            assert_downstream_html_has_no_active_source_markup(&markdown);
            assert_eq!(
                compact_events(&markdown)
                    .iter()
                    .filter(|event| event.as_str() == "HardBreak")
                    .count(),
                expected_breaks,
                "{context_name}/{payload_name}: {markdown:?}",
            );

            let mut expected_stacks = vec![('A', vec!["deletion"])];
            expected_stacks.extend(decoded.chars().map(|ch| (ch, vec!["deletion"])));
            expected_stacks.push(('Z', Vec::new()));
            assert_eq!(
                parsed_scalar_stacks(&markdown),
                expected_stacks,
                "{context_name}/{payload_name}: {markdown:?}",
            );
            assert_eq!(
                parsed_visible_text(&markdown),
                format!("A{decoded}Z"),
                "{context_name}/{payload_name}: {markdown:?}",
            );
        }
    }
}

#[test]
fn line_start_link_and_code_payloads_keep_source_text_in_parser_events() {
    const IMG_ENTITY: &str = "&lt;img src=x onerror=alert(1) data-pad=x&gt;";
    const IMG_TEXT: &str = "<img src=x onerror=alert(1) data-pad=x>";
    const SVG_ENTITY: &str = "&lt;svg onload=alert(2) data-pad=x&gt;&lt;/svg&gt;";
    const SVG_TEXT: &str = "<svg onload=alert(2) data-pad=x></svg>";

    let link_html =
        format!("<p><del>A</del><del><br><a href=\"/safe\">{SVG_ENTITY}</a></del>Z</p>");
    let link = html_to_markdown(&link_html);
    assert_only_inert_inline_boundaries(&link, 0);
    assert_downstream_html_has_no_active_source_markup(&link);
    let mut link_stacks = vec![('A', vec!["deletion"])];
    link_stacks.extend(SVG_TEXT.chars().map(|ch| (ch, vec!["deletion", "link"])));
    link_stacks.push(('Z', Vec::new()));
    assert_eq!(parsed_scalar_stacks(&link), link_stacks, "{link:?}");
    assert_eq!(
        parsed_destinations(&link),
        (vec!["/safe".to_owned()], Vec::new())
    );

    let code_html = format!(
        "<blockquote><p><del>A</del><del><br><br><code>{IMG_ENTITY}</code></del>Z</p></blockquote>"
    );
    let code = html_to_markdown(&code_html);
    assert_only_inert_inline_boundaries(&code, 0);
    assert_downstream_html_has_no_active_source_markup(&code);
    let mut code_stacks = vec![('A', vec!["deletion"])];
    code_stacks.extend(IMG_TEXT.chars().map(|ch| (ch, vec!["deletion", "code"])));
    code_stacks.push(('Z', Vec::new()));
    assert_eq!(parsed_scalar_stacks(&code), code_stacks, "{code:?}");
}

#[test]
fn table_boundaries_remain_constant_inline_comments_for_hostile_text() {
    let html = "<table><tr>\
        <td><del>A</del><del><br>&lt;img src=x onerror=alert(1) data-pad=x&gt;</del>Z</td>\
        <td><del>A</del><del><br><a href=\"/safe\">&lt;svg onload=alert(2) data-pad=x&gt;&lt;/svg&gt;</a></del>Z</td>\
        <td><del>A</del><del><br><code>&lt;a href=&quot;javascript:alert(3)&quot; data-pad=x&gt;click&lt;/a&gt;</code></del>Z</td>\
        </tr></table>";
    let markdown = html_to_markdown(html);

    assert_only_inert_inline_boundaries(&markdown, 3);
    assert_downstream_html_has_no_active_source_markup(&markdown);
    assert_eq!(
        parsed_destinations(&markdown),
        (vec!["/safe".to_owned()], Vec::new()),
    );
    assert_eq!(
        compact_events(&markdown)
            .iter()
            .filter(|event| event.as_str() == "start:TableCell")
            .count(),
        6,
        "{markdown:?}",
    );
}

#[test]
fn line_start_and_inline_deletion_boundaries_obey_exact_character_limits() {
    let fixtures = [
        "<p><del>A</del><del><br>B</del>Z</p>",
        "<p><del>A</del><del><br><br>&lt;img src=x onerror=alert(1) data-pad=x&gt;</del>Z</p>",
        "<blockquote><p><del>A</del><del><br>&lt;svg onload=alert(2) data-pad=x&gt;&lt;/svg&gt;</del>Z</p></blockquote>",
        "<ul><li><del>A</del><del><br>&lt;a href=&quot;javascript:alert(3)&quot; data-pad=x&gt;click&lt;/a&gt;</del>Z</li></ul>",
        "<p><del>A</del><del><br><a href=\"/safe\">&lt;svg onload=alert(2)&gt;&lt;/svg&gt;</a></del>Z</p>",
        "<blockquote><p><del>A</del><del><br><code>&lt;img src=x onerror=alert(1)&gt;</code></del>Z</p></blockquote>",
        "<table><tr><td><del>A</del><del><br>&lt;img src=x onerror=alert(1) data-pad=x&gt;</del>Z</td></tr></table>",
    ];

    for html in fixtures {
        let expected = html_to_markdown(html);
        assert_all_raw_events_are_inert_inline_boundaries(&expected);
        assert_downstream_html_has_no_active_source_markup(&expected);
        let exact = expected.chars().count();
        assert_eq!(
            html_to_markdown_with_options(
                html,
                &MarkdownOptions {
                    base_url: None,
                    max_chars: exact,
                },
            )
            .unwrap(),
            expected,
            "exact limit for {html}",
        );
        assert!(
            matches!(
                html_to_markdown_with_options(
                    html,
                    &MarkdownOptions {
                        base_url: None,
                        max_chars: exact - 1,
                    },
                ),
                Err(Error::BodyLimit { limit }) if limit == exact - 1
            ),
            "one-short limit for {html}",
        );
    }
}

#[test]
fn exhaustive_pair_transition_matrix_has_only_inert_inline_raw_events() {
    const FAMILIES: [WrapperOuterFamily; 3] = [
        WrapperOuterFamily::Strong,
        WrapperOuterFamily::Emphasis,
        WrapperOuterFamily::Deletion,
    ];
    const PAYLOADS: [WrapperTransitionPayload; 9] = [
        WrapperTransitionPayload::Plain,
        WrapperTransitionPayload::LeadingEmphasis,
        WrapperTransitionPayload::PartialEmphasis,
        WrapperTransitionPayload::Deletion,
        WrapperTransitionPayload::Link,
        WrapperTransitionPayload::Image,
        WrapperTransitionPayload::Code,
        WrapperTransitionPayload::Transparent,
        WrapperTransitionPayload::Empty,
    ];
    const SEPARATORS: [WrapperTransitionSeparator; 3] = [
        WrapperTransitionSeparator::Adjacent,
        WrapperTransitionSeparator::DirectEmpty,
        WrapperTransitionSeparator::ConservativeBarrier,
    ];

    for first_family in FAMILIES {
        for second_family in FAMILIES {
            for first_payload in PAYLOADS {
                for second_payload in PAYLOADS {
                    for separator in SEPARATORS {
                        let html = format!(
                            "<p>{}{}{}Z</p>",
                            wrapped_transition_payload_html(first_family, first_payload, 'A',),
                            separator.html(),
                            wrapped_transition_payload_html(second_family, second_payload, 'B',),
                        );
                        let markdown = html_to_markdown(&html);
                        let mut expected =
                            transition_payload_stacks_for_outer(first_payload, 'A', first_family);
                        expected.extend(transition_payload_stacks_for_outer(
                            second_payload,
                            'B',
                            second_family,
                        ));
                        expected.push(('Z', Vec::new()));

                        assert_eq!(
                            parsed_scalar_stacks(&markdown),
                            expected,
                            "{first_family:?}/{first_payload:?}/{separator:?} -> {second_family:?}/{second_payload:?}: {markdown:?}",
                        );
                        assert_eq!(
                            parsed_visible_text(&markdown),
                            expected.iter().map(|(ch, _)| *ch).collect::<String>(),
                            "{first_family:?}/{first_payload:?}/{separator:?} -> {second_family:?}/{second_payload:?}: {markdown:?}",
                        );
                        assert_all_raw_events_are_inert_inline_boundaries(&markdown);
                    }
                }
            }
        }
    }
}

#[test]
fn direct_text_wrapper_and_code_runs_keep_their_single_delimiter_pair() {
    let fixtures = [
        (
            "strong",
            "<p><strong>A</strong><!-- split --><span hidden>secret</span><wbr><span></span><strong>B</strong>Z</p>",
            "**AB**Z",
        ),
        (
            "emphasis",
            "<p><em>A</em><!-- split --><span hidden>secret</span><wbr><span></span><em>B</em>Z</p>",
            "*AB*Z",
        ),
        (
            "deletion",
            "<p><del>A</del><!-- split --><span hidden>secret</span><wbr><span></span><del>B</del>Z</p>",
            "~~AB~~Z",
        ),
        (
            "code",
            "<p><code>A</code><!-- split --><span hidden>secret</span><wbr><span></span><code>B</code>Z</p>",
            "`AB`Z",
        ),
        (
            "wrapper whitespace transfer",
            "<p><strong>A </strong><strong> B</strong></p>",
            "**A B**",
        ),
    ];

    for (name, html, expected) in fixtures {
        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(
            compact_events(&markdown),
            compact_events(expected),
            "{name}"
        );
        assert_eq!(rendered_html(&markdown), rendered_html(expected), "{name}");
        assert_eq!(
            parsed_visible_text(&markdown),
            parsed_visible_text(expected),
            "{name}"
        );
    }
}

#[test]
fn simple_and_complex_wrapper_runs_obey_exact_character_limits() {
    let fixtures = [
        ("<p><strong>A</strong><strong>B</strong></p>", "**AB**"),
        ("<p><em>A</em><em>B</em></p>", "*AB*"),
        ("<p><del>A</del><del>B</del></p>", "~~AB~~"),
        ("<p><code>A</code><code>B</code></p>", "`AB`"),
        (
            "<p><strong>A</strong><strong><em>B</em></strong><strong><em>C</em></strong>Z</p>",
            "**A**__*B*__**_C_**&#90;",
        ),
        (
            "<p><strong>A</strong><strong><del>B</del></strong><strong><del>C</del></strong>Z</p>",
            "**A**__~~B~~__**~~C~~**&#90;",
        ),
        (
            "<p><strong>A</strong><strong><em><a href=\"/b\">B</a></em></strong>Z</p>",
            "**A**__*[B](/b)*__&#90;",
        ),
    ];

    for (html, expected) in fixtures {
        let exact = expected.chars().count();
        assert_eq!(
            html_to_markdown_with_options(
                html,
                &MarkdownOptions {
                    base_url: None,
                    max_chars: exact,
                },
            )
            .unwrap(),
            expected,
            "exact limit for {html}",
        );
        assert!(
            matches!(
                html_to_markdown_with_options(
                    html,
                    &MarkdownOptions {
                        base_url: None,
                        max_chars: exact - 1,
                    },
                ),
                Err(Error::BodyLimit { limit }) if limit == exact - 1
            ),
            "one-short limit for {html}",
        );
    }
}

#[test]
fn collapsed_context_breaks_preserve_wrapper_and_link_scopes() {
    let outer = html_to_markdown("<p><strong><a href=\"/x\">A<br>B</a></strong>C</p>");
    let outer_expected = "**[A B](/x)**&#67;";
    assert_eq!(
        compact_events(outer_expected),
        [
            "start:Paragraph",
            "start:Strong",
            "start:Link",
            "text:A B",
            "end:Link",
            "end:Strong",
            "text:C",
            "end:Paragraph",
        ],
        "bad outer-wrapper/link fixture"
    );
    assert_eq!(outer, outer_expected);
    assert_eq!(
        compact_events(&outer),
        [
            "start:Paragraph",
            "start:Strong",
            "start:Link",
            "text:A B",
            "end:Link",
            "end:Strong",
            "text:C",
            "end:Paragraph",
        ]
    );

    let inner = html_to_markdown("<p><a href=\"/x\"><strong>A<br>B</strong></a>C</p>");
    assert_eq!(inner, "[**A B**](/x)C");
    assert_eq!(
        compact_events(&inner),
        [
            "start:Paragraph",
            "start:Link",
            "start:Strong",
            "text:A B",
            "end:Strong",
            "end:Link",
            "text:C",
            "end:Paragraph",
        ]
    );

    let table = html_to_markdown("<table><tr><td><em>A<br><br>B</em></td></tr></table>");
    assert_eq!(table, "|  |\n| --- |\n| *A B* |");
    let table_events = compact_events(&table);
    assert_eq!(
        table_events
            .iter()
            .filter(|event| event.as_str() == "HardBreak")
            .count(),
        0,
        "{table_events:?}"
    );
    assert!(
        table_events
            .windows(3)
            .any(|events| events == ["start:Emphasis", "text:A B", "end:Emphasis"]),
        "{table_events:?}"
    );

    let table_terminal = html_to_markdown("<table><tr><td><em>A<br><br></em>B</td></tr></table>");
    assert_eq!(table_terminal, "|  |\n| --- |\n| *A* B |");
    let terminal_events = compact_events(&table_terminal);
    assert_eq!(
        terminal_events
            .iter()
            .filter(|event| event.as_str() == "HardBreak")
            .count(),
        0,
        "{terminal_events:?}"
    );
    assert!(
        terminal_events
            .windows(4)
            .any(|events| events == ["start:Emphasis", "text:A", "end:Emphasis", "text: B",]),
        "{terminal_events:?}"
    );
}

#[test]
fn consecutive_normal_breaks_preserve_exact_hard_break_ast() {
    let expected_nonterminal = "**A**  \n\\\n**B**C";
    let expected_nonterminal_events = [
        "start:Paragraph",
        "start:Strong",
        "text:A",
        "end:Strong",
        "HardBreak",
        "HardBreak",
        "start:Strong",
        "text:B",
        "end:Strong",
        "text:C",
        "end:Paragraph",
    ];
    assert_eq!(
        compact_events(expected_nonterminal),
        expected_nonterminal_events,
        "bad consecutive-break fixture"
    );
    let ordinary = html_to_markdown("<p><strong>A<br><br>B</strong>C</p>");
    assert_eq!(ordinary, expected_nonterminal);
    assert_eq!(compact_events(&ordinary), expected_nonterminal_events);

    let terminal = html_to_markdown("<p><strong>A<br><br></strong>B</p>");
    assert_eq!(terminal, "**A**  \n\\\nB");
    assert_eq!(
        compact_events(&terminal),
        [
            "start:Paragraph",
            "start:Strong",
            "text:A",
            "end:Strong",
            "HardBreak",
            "HardBreak",
            "text:B",
            "end:Paragraph",
        ]
    );

    let three = html_to_markdown("<p>A<br><br><br>B</p>");
    assert_eq!(three, "A  \n\\\n\\\nB");
    assert_eq!(
        compact_events(&three)
            .iter()
            .filter(|event| event.as_str() == "HardBreak")
            .count(),
        3,
        "{:?}",
        compact_events(&three)
    );

    for (html, expected) in [
        (
            "<blockquote><p><strong>A<br><br>B</strong>C</p></blockquote>",
            "> **A**  \n> \\\n> **B**C",
        ),
        (
            "<blockquote><p><strong>A<br><br></strong>B</p></blockquote>",
            "> **A**  \n> \\\n> B",
        ),
        (
            "<ul><li><strong>A<br><br>B</strong>C</li></ul>",
            "- **A**  \n    \\\n    **B**C",
        ),
        (
            "<ul><li><strong>A<br><br></strong>B</li></ul>",
            "- **A**  \n    \\\n    B",
        ),
    ] {
        let markdown = html_to_markdown(html);
        assert_eq!(markdown, expected, "{html}");
        assert_eq!(
            compact_events(&markdown)
                .iter()
                .filter(|event| event.as_str() == "HardBreak")
                .count(),
            2,
            "{html}: {:?}",
            compact_events(&markdown)
        );
    }
}

#[test]
fn mixed_wrappers_reopen_with_the_same_ast_after_breaks() {
    let expected = "**&#88;*Y~~A~~***  \n***~~B~~Z*Q**";
    let expected_events = [
        "start:Paragraph",
        "start:Strong",
        "text:X",
        "start:Emphasis",
        "text:Y",
        "start:Strikethrough",
        "text:A",
        "end:Strikethrough",
        "end:Emphasis",
        "end:Strong",
        "HardBreak",
        "start:Strong",
        "start:Emphasis",
        "start:Strikethrough",
        "text:B",
        "end:Strikethrough",
        "text:Z",
        "end:Emphasis",
        "text:Q",
        "end:Strong",
        "end:Paragraph",
    ];
    assert_eq!(
        compact_events(expected),
        expected_events,
        "bad mixed fixture"
    );
    let markdown = html_to_markdown("<p><strong>X<em>Y<del>A<br>B</del>Z</em>Q</strong></p>");
    assert_eq!(markdown, expected);
    assert_eq!(compact_events(&markdown), expected_events);
}

#[test]
fn wrappers_close_before_hard_break_and_reopen_only_for_later_content() {
    let ordinary = [
        ("strong", "**", "Strong"),
        ("em", "*", "Emphasis"),
        ("del", "~~", "Strikethrough"),
    ];
    for (tag, marker, event) in ordinary {
        let terminal = html_to_markdown(&format!("<p><{tag}>A<br></{tag}>B</p>"));
        assert_eq!(terminal, format!("{marker}A{marker}  \nB"));
        let terminal_events = parse_events(&terminal);
        assert!(terminal_events
            .iter()
            .any(|item| item == &format!("start:{event}")));
        assert!(terminal_events.iter().any(|item| item == "HardBreak"));
        assert!(!terminal.lines().skip(1).any(|line| line == marker));

        let nonterminal = html_to_markdown(&format!("<p><{tag}>A<br>B</{tag}>C</p>"));
        assert_eq!(
            nonterminal,
            format!("{marker}A{marker}  \n{marker}B{marker}C")
        );
        assert_eq!(
            parse_events(&nonterminal)
                .iter()
                .filter(|item| *item == &format!("start:{event}"))
                .count(),
            2
        );
    }

    let quoted_nonterminal =
        html_to_markdown("<blockquote><p><strong>A<br>B</strong></p></blockquote>");
    assert_eq!(quoted_nonterminal, "> **A**  \n> **B**");
    assert_eq!(
        parse_events(&quoted_nonterminal)
            .iter()
            .filter(|event| *event == "start:Strong")
            .count(),
        2
    );

    let quoted_terminal =
        html_to_markdown("<blockquote><p><strong>A<br></strong></p><p>B</p></blockquote>");
    assert_eq!(quoted_terminal, "> **A**  \n>\n> B");
    assert!(!quoted_terminal.lines().any(|line| line == "**"));

    let list_nonterminal = html_to_markdown("<ul><li><em>A<br>B</em></li></ul>");
    assert_eq!(list_nonterminal, "- *A*  \n    *B*");
    assert_eq!(
        parse_events(&list_nonterminal)
            .iter()
            .filter(|event| *event == "start:Emphasis")
            .count(),
        2
    );

    let list_terminal = html_to_markdown("<ul><li><del>A<br></del>B</li></ul>");
    assert_eq!(list_terminal, "- ~~A~~  \n    B");
    assert!(!list_terminal.lines().any(|line| line == "~~"));
}

#[test]
fn preferred_roots_reject_syntax_only_content() {
    let candidates = [
        "<hr>",
        "<pre></pre>",
        "<p><a href=\"/x\"></a></p>",
        "<p><br></p>",
        "<table><tr><td></td></tr></table>",
    ];
    for candidate in candidates {
        let html =
            format!("<body><main>{candidate}</main><article><p>Fallback</p></article></body>");
        assert_eq!(html_to_markdown(&html), "Fallback", "{candidate}");
    }

    assert_eq!(
        html_to_markdown("<body><main><hr><p>Visible</p></main><article>Fallback</article></body>"),
        "---\n\nVisible"
    );
    assert_eq!(
        html_to_markdown(
            "<body><main><img src=\"/safe.png\" alt=\"Meaningful\"></main>\
             <article>Fallback</article></body>"
        ),
        "![Meaningful](/safe.png)"
    );
    assert_eq!(
        html_to_markdown(
            "<body><main><img src=\"javascript:bad\" alt=\"Fallback alt\"></main>\
             <article>Article fallback</article></body>"
        ),
        "Article fallback"
    );
}

#[test]
fn excluded_table_rows_and_sections_cannot_supply_preferred_root_content() {
    let excluded_rows = [
        "<tr hidden><td>Hidden</td></tr>",
        "<tr aria-hidden=\"true\"><td>Hidden</td></tr>",
        "<tr class=\"ad\"><td>Hidden</td></tr>",
        "<tr hidden><td><img src=\"/safe.png\" alt=\"Safe\"></td></tr>",
        "<tr class=\"promo\"><td><img src=\"javascript:bad\" alt=\"Bad\"></td></tr>",
    ];
    for row in excluded_rows {
        let html =
            format!("<body><main><table>{row}</table></main><article>Fallback</article></body>");
        assert_eq!(html_to_markdown(&html), "Fallback", "{row}");
    }

    for section in ["thead", "tbody", "tfoot"] {
        for attributes in ["hidden", "aria-hidden=\"true\"", "class=\"ad\""] {
            let html = format!(
                "<body><main><table><{section} {attributes}><tr><td>Hidden</td></tr>\
                 </{section}></table></main><article>Fallback</article></body>"
            );
            assert_eq!(
                html_to_markdown(&html),
                "Fallback",
                "{section} {attributes}"
            );
        }
    }

    assert_eq!(
        html_to_markdown(
            "<body><main><table><tr hidden><td>Hidden</td></tr>\
             <tr><td>Visible</td></tr></table></main><article>Fallback</article></body>"
        ),
        "|  |\n| --- |\n| Visible |"
    );
    assert_eq!(
        html_to_markdown(
            "<body><main><table><tbody class=\"advertisement adventure\">\
             <tr><td>Hidden</td></tr></tbody><tbody><tr class=\"adventure\">\
             <td>Visible</td></tr></tbody></table></main><article>Fallback</article></body>"
        ),
        "|  |\n| --- |\n| Visible |"
    );
    assert_eq!(
        html_to_markdown(
            "<body><main><table><tbody><tr><td><img src=\"/safe.png\" alt=\"Safe\">\
             </td></tr></tbody></table></main><article>Fallback</article></body>"
        ),
        "|  |\n| --- |\n| ![Safe](/safe.png) |"
    );
    assert_eq!(
        html_to_markdown(
            "<body><main><table><tr><td><img src=\"javascript:bad\" alt=\"Bad\">\
             </td></tr></table></main><article>Fallback</article></body>"
        ),
        "Fallback"
    );
}

#[test]
fn ordered_lists_parse_with_marker_relative_nesting() {
    let markdown = html_to_markdown(
        "<div><ol start=\"123\"><li>A<ul><li>B</li></ul></li>\
         <li value=\"999999999\">C</li></ol></div>",
    );
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "start:List(Some(123))"));
    assert!(
        events
            .iter()
            .filter(|event| event.as_str() == "start:Item")
            .count()
            >= 3
    );
    assert!(events.iter().any(|event| event == "text:B"));
}

#[test]
fn unsupported_ordered_counters_use_one_safe_fallback_list() {
    let markdown = html_to_markdown(
        "<ol start=\"-2\"><li>A</li><li value=\"9223372036854775807\">B</li></ol>",
    );
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "start:List(None)"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("start:List("))
            .count(),
        1,
        "{markdown:?}: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "start:Item")
            .count(),
        2
    );
    assert!(events.iter().any(|event| event == "text:-2. A"));
    assert!(
        events
            .iter()
            .any(|event| event == "text:9223372036854775807. B"),
        "{markdown:?}: {events:?}"
    );
}

#[test]
fn fallback_counter_preludes_preserve_leading_structural_blocks() {
    let cases = [
        (
            "quote",
            "<ol start=\"-2\"><li><blockquote><p>Quote</p></blockquote></li></ol>",
            "- -2.\n    \n    > Quote",
            "start:BlockQuote",
            1,
        ),
        (
            "nested list",
            "<ol start=\"-2\"><li><ul><li>Nested</li></ul></li></ol>",
            "- -2.\n    \n    - Nested",
            "start:List(None)",
            2,
        ),
        (
            "fence",
            "<ol start=\"-2\"><li><pre>x</pre></li></ol>",
            "- -2.\n    \n    ```\n    x\n    ```",
            "start:CodeBlock(Fenced(Borrowed(\"\")))",
            1,
        ),
        (
            "table",
            "<ol start=\"-2\"><li><table><tr><th>H</th></tr><tr><td>V</td></tr></table></li></ol>",
            "- -2.\n    \n    | H |\n    | --- |\n    | V |",
            "start:Table(",
            1,
        ),
        (
            "heading",
            "<ol start=\"-2\"><li><h2>Heading</h2></li></ol>",
            "- -2.\n    \n    ## Heading",
            "start:Heading",
            1,
        ),
        (
            "rule",
            "<ol start=\"-2\"><li><hr></li></ol>",
            "- -2.\n    \n    ---",
            "Rule",
            1,
        ),
    ];

    for (name, html, expected, structural_event, expected_lists) in cases {
        let markdown = html_to_markdown(html);
        let events = parse_events(&markdown);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("start:List(None)"))
                .count(),
            expected_lists,
            "{name}: {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|event| event.starts_with(structural_event)),
            "{name}: {markdown:?}: {events:?}",
        );
    }

    let inline = html_to_markdown("<ol start=\"-2\"><li><p>Inline</p></li></ol>");
    assert_eq!(inline, "- -2. Inline");
    assert_eq!(
        parse_events(&inline)
            .iter()
            .filter(|event| event.starts_with("start:List(None)"))
            .count(),
        1,
    );

    let formatted =
        html_to_markdown("<ol start=\"-2\"><li><p><strong>Bold</strong> tail</p></li></ol>");
    assert_eq!(formatted, "- -2. **Bold** tail");
    assert!(parse_events(&formatted)
        .iter()
        .any(|event| event == "start:Strong"));

    let paragraph_after_direct =
        html_to_markdown("<ol start=\"-2\"><li>Direct<p>Paragraph</p></li></ol>");
    assert_eq!(paragraph_after_direct, "- -2. Direct\n    \n    Paragraph");
    assert_eq!(
        parse_events(&paragraph_after_direct)
            .iter()
            .filter(|event| event.as_str() == "start:Paragraph")
            .count(),
        2,
    );

    let hard_break = html_to_markdown("<ol start=\"-2\"><li><p>Line<br>continued</p></li></ol>");
    assert_eq!(hard_break, "- -2. Line  \n    continued");
    let hard_break_events = parse_events(&hard_break);
    assert_eq!(
        hard_break_events
            .iter()
            .filter(|event| event.starts_with("start:List(None)"))
            .count(),
        1,
    );
    assert!(hard_break_events.iter().any(|event| event == "HardBreak"));
}

#[test]
fn ordered_counter_boundaries_malformed_values_and_reversed_are_deterministic() {
    let zero = html_to_markdown("<ol start=\"0\"><li>Zero</li></ol>");
    assert!(
        parse_events(&zero)
            .iter()
            .any(|event| event == "start:List(Some(0))"),
        "{zero:?}"
    );

    let nine_digits = html_to_markdown("<ol start=\"999999999\"><li>Nine digits</li></ol>");
    assert!(
        parse_events(&nine_digits)
            .iter()
            .any(|event| event == "start:List(Some(999999999))"),
        "{nine_digits:?}"
    );

    let malformed = html_to_markdown(
        "<ol start=\"not-a-number\" reversed><li>One</li>\
         <li value=\"still-not-a-number\">Two</li><li>Three</li></ol>",
    );
    assert_eq!(malformed, "1. One\n2. Two\n3. Three");

    let ten_digits =
        html_to_markdown("<ol start=\"1000000000\"><li>Ten digits</li><li>Next</li></ol>");
    let ten_digit_events = parse_events(&ten_digits);
    assert_eq!(
        ten_digit_events
            .iter()
            .filter(|event| event.starts_with("start:List("))
            .collect::<Vec<_>>(),
        ["start:List(None)"]
    );
    assert!(ten_digit_events
        .iter()
        .any(|event| event == "text:1000000000. Ten digits"));
    assert!(ten_digit_events
        .iter()
        .any(|event| event == "text:1000000001. Next"));

    let reset_after_max = html_to_markdown(
        "<ol start=\"9223372036854775807\"><li>Max</li>\
         <li value=\"0\">Reset</li></ol>",
    );
    let reset_events = parse_events(&reset_after_max);
    assert!(reset_events
        .iter()
        .any(|event| event == "text:9223372036854775807. Max"));
    assert!(parsed_visible_text(&reset_after_max).contains("0. Reset"));
    assert_eq!(
        reset_events
            .iter()
            .filter(|event| event.starts_with("start:List("))
            .count(),
        1,
        "{reset_after_max:?}: {reset_events:?}"
    );
    assert_eq!(
        reset_events
            .iter()
            .filter(|event| event.as_str() == "start:Item")
            .count(),
        2,
        "{reset_after_max:?}: {reset_events:?}"
    );

    for html in [
        "<ol start=\"9223372036854775807\"><li>A</li><li>B</li></ol>",
        "<ol><li value=\"9223372036854775807\">A</li><li>B</li></ol>",
    ] {
        assert!(matches!(
            html_to_markdown_with_options(html, &MarkdownOptions::default()),
            Err(Error::Parse { kind: "html", .. })
        ));
    }
}

#[test]
fn ordered_list_continuations_match_one_three_and_nine_digit_markers() {
    assert_eq!(
        html_to_markdown("<ol><li>A<br>B<ul><li>N</li></ul></li></ol>"),
        "1. A  \n   B\n   - N"
    );
    assert_eq!(
        html_to_markdown(
            "<ol start=\"123\"><li><p>A</p><p>B</p>\
             <blockquote><p>Q</p></blockquote></li></ol>"
        ),
        "123. A\n     \n     B\n     \n     > Q"
    );
    assert_eq!(
        html_to_markdown(
            "<ol start=\"999999999\"><li><pre>x</pre>\
             <ul><li>N</li></ul></li></ol>"
        ),
        "999999999. ```\n           x\n           ```\n           \n           - N"
    );
}

#[test]
fn hidden_description_terms_break_definition_ownership() {
    assert_eq!(
        html_to_markdown(
            "<dl><dt>A</dt><dd>one</dd><dt hidden>Hidden</dt><dd>orphan</dd>\
             <dt>B</dt><dd hidden>secret</dd><dd>two</dd></dl>"
        ),
        "A\n: one\n\nB\n: two"
    );
}

#[test]
fn repeated_definitions_keep_empty_terms_and_nested_blocks() {
    let markdown = html_to_markdown(
        "<dl><dt></dt><dd><p>Zero</p><ul><li>Nested</li></ul></dd>\
         <dd><blockquote><p>Again</p></blockquote></dd></dl>",
    );

    assert_eq!(markdown, ": Zero\n  \n  - Nested\n\n: > Again");
}

#[test]
fn raw_fences_preserve_ticks_language_newlines_and_exact_budgets() {
    let html = "<pre><code class=\"other language-rust\">line\n````\nend</code></pre>";
    let expected = "````` rust\nline\n````\nend\n`````";
    assert_eq!(html_to_markdown(html), expected);

    let exact = MarkdownOptions {
        base_url: None,
        max_chars: expected.chars().count(),
    };
    assert_eq!(
        html_to_markdown_with_options(html, &exact).unwrap(),
        expected
    );

    let one_short = MarkdownOptions {
        base_url: None,
        max_chars: expected.chars().count() - 1,
    };
    assert!(matches!(
        html_to_markdown_with_options(html, &one_short),
        Err(Error::BodyLimit { .. })
    ));

    let empty = "```\n\n```";
    assert_eq!(html_to_markdown("<pre><code></code></pre>"), empty);
    assert_eq!(
        html_to_markdown_with_options(
            "<pre></pre>",
            &MarkdownOptions {
                base_url: None,
                max_chars: empty.chars().count(),
            }
        )
        .unwrap(),
        empty
    );
    assert!(matches!(
        html_to_markdown_with_options(
            "<pre></pre>",
            &MarkdownOptions {
                base_url: None,
                max_chars: empty.chars().count() - 1,
            }
        ),
        Err(Error::BodyLimit { .. })
    ));
}

#[test]
fn raw_fences_stream_all_pre_descendants_in_dom_order() {
    let html = "<pre>before<code class=\"language-rust\">inside</code><span>middle</span>\
                <code>after</code>end</pre>";
    let expected = "``` rust\nbeforeinsidemiddleafterend\n```";
    assert_eq!(html_to_markdown(html), expected);

    for max_chars in [expected.chars().count(), expected.chars().count() - 1] {
        let result = html_to_markdown_with_options(
            html,
            &MarkdownOptions {
                base_url: None,
                max_chars,
            },
        );
        if max_chars == expected.chars().count() {
            assert_eq!(result.unwrap(), expected);
        } else {
            assert!(matches!(result, Err(Error::BodyLimit { limit }) if limit == max_chars));
        }
    }

    let preferred = "<body><main><pre>outside<code></code></pre></main>\
                     <article>Fallback</article></body>";
    assert_eq!(html_to_markdown(preferred), "```\noutside\n```");
}

#[test]
fn raw_fence_newlines_preserve_context_ast_code_text_and_budgets() {
    let cases = [
        ("root", "<pre>a\n\nb</pre>", "```\na\n\nb\n```", "a\n\nb\n"),
        (
            "quote",
            "<blockquote><pre>a\n\nb</pre></blockquote>",
            "> ```\n> a\n>\n> b\n> ```",
            "a\n\nb\n",
        ),
        (
            "ordered list",
            "<ol><li><pre>a\n\nb</pre></li></ol>",
            "1. ```\n   a\n   \n   b\n   ```",
            "a\n\nb\n",
        ),
        (
            "ordered list plus quote",
            "<ol><li><blockquote><pre>a\n\nb</pre></blockquote></li></ol>",
            "1. > ```\n   > a\n   >\n   > b\n   > ```",
            "a\n\nb\n",
        ),
    ];

    for (name, html, expected, code_text) in cases {
        let markdown = html_to_markdown(html);
        let events = parse_events(&markdown);
        assert_eq!(markdown, expected, "{name}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("start:CodeBlock(Fenced"))
                .count(),
            1,
            "{name}: {events:?}",
        );
        assert_eq!(parsed_code_block_texts(&markdown), [code_text], "{name}");

        let exact = MarkdownOptions {
            base_url: None,
            max_chars: expected.chars().count(),
        };
        assert_eq!(
            html_to_markdown_with_options(html, &exact).unwrap(),
            expected
        );
        let one_short = MarkdownOptions {
            base_url: None,
            max_chars: expected.chars().count() - 1,
        };
        assert!(matches!(
            html_to_markdown_with_options(html, &one_short),
            Err(Error::BodyLimit { .. })
        ));
    }

    let raw_edges = "<pre>\n\nA\n\n</pre>";
    // HTML parsing strips the first newline immediately after a `pre` start tag;
    // the renderer preserves the remaining DOM newline and both trailing ones.
    let expected_edges = "```\n\nA\n\n```";
    assert_eq!(html_to_markdown(raw_edges), expected_edges);
    assert_eq!(parsed_code_block_texts(expected_edges), ["\nA\n\n"]);
    assert_eq!(
        html_to_markdown_with_options(
            raw_edges,
            &MarkdownOptions {
                base_url: None,
                max_chars: expected_edges.chars().count(),
            }
        )
        .unwrap(),
        expected_edges,
    );
    assert!(matches!(
        html_to_markdown_with_options(
            raw_edges,
            &MarkdownOptions {
                base_url: None,
                max_chars: expected_edges.chars().count() - 1,
            }
        ),
        Err(Error::BodyLimit { .. })
    ));
}

#[test]
fn excluded_fence_descendants_never_leak_into_raw_code() {
    assert_eq!(
        html_to_markdown("<pre><code>before<span hidden>secret</span>after\nnext</code></pre>"),
        "```\nbeforeafter\nnext\n```"
    );
}

#[test]
fn table_sections_keep_row_headers_as_data_and_explicit_heads_as_headers() {
    let row_header = html_to_markdown(
        "<table><tbody><tr><th scope=\"row\">Row</th><td>Value</td></tr></tbody></table>",
    );
    assert_eq!(row_header, "|  |  |\n| --- | --- |\n| Row | Value |");
    let row_header_events = parse_events(&row_header);
    assert!(row_header_events
        .iter()
        .any(|event| event == "start:Table([None, None])"));
    assert_eq!(
        row_header_events
            .iter()
            .filter(|event| event.as_str() == "start:TableRow")
            .count(),
        1
    );

    let explicit = html_to_markdown(
        "<table><thead><tr><th>H1</th><th>H2</th></tr></thead>\
         <tbody><tr><td>B1</td><td>B2</td></tr></tbody>\
         <tfoot><tr><td>F1</td><td>F2</td></tr></tfoot></table>",
    );
    assert_eq!(
        explicit,
        "| H1 | H2 |\n| --- | --- |\n| B1 | B2 |\n| F1 | F2 |"
    );
    assert_eq!(parsed_visible_text(&explicit), "H1H2B1B2F1F2");
}

#[test]
fn table_alignment_normalizes_cell_row_and_column_metadata() {
    let columns = html_to_markdown(
        "<table><colgroup><col align=\"right\"><col align=\"center\">\
         <col align=\"left\"></colgroup><thead><tr><th>R</th><th>C</th><th>L</th></tr>\
         </thead><tbody><tr><td align=\"right\">1</td><td align=\"center\">2</td>\
         <td align=\"left\">3</td></tr></tbody></table>",
    );
    assert!(
        parse_events(&columns)
            .iter()
            .any(|event| event == "start:Table([Right, Center, Left])"),
        "{columns:?}"
    );
    assert_eq!(
        columns,
        "| R | C | L |\n| ---: | :---: | :--- |\n| 1 | 2 | 3 |"
    );

    let row = html_to_markdown(
        "<table><thead><tr align=\"center\"><th>A</th><th>B</th></tr></thead>\
         <tbody><tr align=\"center\"><td>C</td><td>D</td></tr></tbody></table>",
    );
    assert!(
        parse_events(&row)
            .iter()
            .any(|event| event == "start:Table([Center, Center])"),
        "{row:?}"
    );
}

#[test]
fn conflicting_table_alignment_falls_back_per_column() {
    let markdown = html_to_markdown(
        "<table><thead><tr><th align=\"right\">A</th><th align=\"center\">B</th></tr>\
         </thead><tbody><tr><td align=\"left\">C</td><td align=\"center\">D</td></tr>\
         </tbody></table>",
    );

    assert!(
        parse_events(&markdown)
            .iter()
            .any(|event| event == "start:Table([None, Center])"),
        "{markdown:?}"
    );
    assert_eq!(markdown, "| A | B |\n| --- | :---: |\n| C | D |");
}

#[test]
fn table_rows_are_owned_and_data_rows_are_never_dropped_or_promoted() {
    let markdown = html_to_markdown(
        "<table><tbody><tr><td>A</td></tr><tr><td>B</td><td>C</td></tr>\
         <tr><td>Outer<table><thead><tr><th>I1</th><th>I2</th><th>I3</th></tr></thead>\
         <tbody><tr><td>J1</td><td>J2</td><td>J3</td></tr></tbody></table></td><td>D</td>\
         </tr></tbody></table>",
    );

    assert_eq!(
        markdown,
        "|  |  |\n| --- | --- |\n| A |  |\n| B | C |\n| OuterI1I2I3J1J2J3 | D |"
    );
    let events = parse_events(&markdown);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("start:Table("))
            .count(),
        1,
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "start:TableRow")
            .count(),
        3,
        "{events:?}"
    );
}

#[test]
fn table_cells_preserve_pipes_newlines_code_links_images_and_column_count() {
    let markdown = html_to_markdown(
        "<table><thead><tr><th>Text</th><th>Lines</th><th>Code</th><th>Rich</th></tr></thead>\
         <tbody><tr><td>A|B</td><td>line\nbreak</td><td><code>x|y</code></td>\
         <td>&amp;copy; <a href=\"/x\">L|K</a><img src=\"/i\" alt=\"I|M\"></td></tr>\
         </tbody></table>",
    );
    let events = parse_events(&markdown);

    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "start:TableCell")
            .count(),
        8,
        "{events:?}"
    );
    assert!(events.iter().any(|event| event == "code:x|y"), "{events:?}");
    assert!(parsed_visible_text(&markdown).contains("A|B"), "{events:?}");
    assert!(
        events.iter().any(|event| event == "text:line break"),
        "{events:?}"
    );
    assert_eq!(parsed_raw_events(&markdown), []);
    assert!(parsed_visible_text(&markdown).contains("&copy; L|KI|M"));
}

#[test]
fn hidden_table_cells_keep_their_column_without_leaking_content_or_alignment() {
    let markdown = html_to_markdown(
        "<table><colgroup><col hidden align=\"right\"><col></colgroup>\
         <thead><tr><th>Hidden</th><th>Visible</th></tr></thead>\
         <tbody><tr><td hidden align=\"right\">secret</td><td>shown</td></tr></tbody></table>",
    );

    assert_eq!(
        markdown,
        "| Hidden | Visible |\n| --- | --- |\n|  | shown |"
    );
    assert!(!parsed_visible_text(&markdown).contains("secret"));
    assert!(
        parse_events(&markdown)
            .iter()
            .any(|event| event == "start:Table([None, None])"),
        "{markdown:?}"
    );
}

#[test]
fn block_sequences_and_tiny_structured_budgets_remain_bounded() {
    let markdown = html_to_markdown(
        "<ol><li><p>One</p><p>Two<br>continued</p><pre>x</pre>\
         <blockquote><p>Quote</p></blockquote><ul><li>Nested</li></ul><hr></li></ol>",
    );
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "start:List(Some(1))"));
    assert!(events.iter().any(|event| event == "text:continued"));
    assert!(parsed_visible_text(&markdown).contains('x'));
    assert!(events.iter().any(|event| event == "text:Quote"));
    assert!(events.iter().any(|event| event == "text:Nested"));
    assert!(events.iter().any(|event| event == "Rule"));

    let large_table = format!(
        "<table><tbody>{}</tbody></table>",
        "<tr><td>payload</td><td>more</td></tr>".repeat(5_000)
    );
    assert!(matches!(
        html_to_markdown_with_options(
            &large_table,
            &MarkdownOptions {
                base_url: None,
                max_chars: 4,
            }
        ),
        Err(Error::BodyLimit { limit: 4 })
    ));

    let deep = format!(
        "<body>{}x{}</body>",
        "<ul><li>".repeat(127),
        "</li></ul>".repeat(127)
    );
    assert!(matches!(
        html_to_markdown_with_options(
            &deep,
            &MarkdownOptions {
                base_url: None,
                max_chars: 2,
            }
        ),
        Err(Error::BodyLimit { limit: 2 })
    ));
}
