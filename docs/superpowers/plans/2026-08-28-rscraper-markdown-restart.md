# rScrapper Task 4 Markdown Renderer Restart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the stopped Task 4 renderer with a bounded streaming HTML-to-Markdown subsystem whose CommonMark/GFM structure is parser-verified.

**Architecture:** Preserve the public `MarkdownOptions` and conversion functions, but split private responsibilities into a final-output writer, root and URL policy, inline rendering, and block rendering. Every character reaches one O(1)-accounted writer; DOM metadata may be pre-scanned, but rendered node content is never accumulated in independent unbounded strings.

**Tech Stack:** Rust 2021/MSRV 1.88; `scraper`; `url`; `pulldown-cmark 0.13.4` as a dev-only CommonMark/GFM verifier; existing `thiserror`-based core errors.

**Spec:** `docs/superpowers/specs/2026-08-28-rscraper-markdown-restart-design.md`

## Global Constraints

- Preserve `MarkdownOptions`, `html_to_markdown`, and `html_to_markdown_with_options` public signatures.
- Preserve documented CLI commands, HTTP routes, MCP tool names, and success-response field names.
- Keep Rust edition 2021 and `rust-version = "1.88"` through workspace inheritance.
- Never return partial Markdown from the fallible API.
- Finalized output must not exceed `max_chars`; discarded whitespace does not consume the limit.
- Renderer-owned content buffers must remain bounded by the requested output plus fixed structural state.
- Do not collect arbitrary descendant text into a `String` before applying the output budget.
- Reject DOM element depth beyond 256 with a typed parse error; do not recurse until stack overflow.
- Only HTTP(S) or structurally safe relative destinations may become Markdown links/images.
- Use parser-backed assertions for CommonMark/GFM structure, not only exact string comparisons.
- Default tests use fixtures only and perform no network access.
- Follow RED-GREEN-REFACTOR for every behavior and record exact failing/passing commands in reports.
- The three uncommitted lines from the stopped attempt are removed before tests and reintroduced only after their tests fail.

## Target File Map

```text
Cargo.toml
  pulldown-cmark 0.13.4 workspace dependency used only by tests

crates/rscraper-core/Cargo.toml
  pulldown-cmark dev dependency

crates/rscraper-core/src/markdown.rs
  public facade, options, orchestration, compatibility wrapper

crates/rscraper-core/src/markdown/output.rs
  FinalWriter, Unicode budget, pending whitespace, line context, reservations

crates/rscraper-core/src/markdown/root.rs
  meaningful root selection and excluded-ancestor traversal

crates/rscraper-core/src/markdown/url.rs
  destination validation, resolution, and encoding

crates/rscraper-core/src/markdown/inline.rs
  inline state machine, escaping, formatting, links/images, code spans

crates/rscraper-core/src/markdown/block.rs
  blocks, lists, descriptions, quotes, fences, and tables

crates/rscraper-core/tests/markdown.rs
  exact compatibility and parser-backed integration fixtures
```

---

### Task 1: Build the streaming writer and bounded traversal foundation

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/rscraper-core/Cargo.toml`
- Replace: `crates/rscraper-core/src/markdown.rs`
- Create: `crates/rscraper-core/src/markdown/output.rs`
- Test: `crates/rscraper-core/src/markdown/output.rs`
- Test: `crates/rscraper-core/tests/markdown.rs`

**Interfaces:**
- Consumes: `crate::{Error, OperationLimits, Result}` and the existing public Task 4 signatures.
- Produces privately:

```rust
pub(super) const MAX_DOM_DEPTH: usize = 256;

pub(super) struct FinalWriter {
    output: String,
    used_chars: usize,
    max_chars: usize,
    reserved_chars: usize,
    pending_space: bool,
    line_start: bool,
}

impl FinalWriter {
    pub(super) fn new(max_chars: usize) -> Self;
    pub(super) fn remaining(&self) -> usize;
    pub(super) fn is_line_start(&self) -> bool;
    pub(super) fn request_space(&mut self);
    pub(super) fn discard_pending_space(&mut self);
    pub(super) fn write_literal(&mut self, text: &str) -> Result<()>;
    pub(super) fn write_char(&mut self, ch: char) -> Result<()>;
    pub(super) fn write_normalized_text(&mut self, text: &str) -> Result<()>;
    pub(super) fn newline(&mut self) -> Result<()>;
    pub(super) fn blank_line(&mut self) -> Result<()>;
    pub(super) fn reserve(&mut self, chars: usize) -> Result<()>;
    pub(super) fn release(&mut self, chars: usize);
    pub(super) fn finish(self) -> Result<String>;
}
```

- `used_chars + reserved_chars` never exceeds `max_chars`.
- `write_normalized_text` retains only a boolean pending-space state; leading/trailing whitespace is not emitted.
- `finish` discards pending collapsible whitespace and returns exactly the accumulated finalized output.

- [ ] **Step 1: Remove the stopped untested production edits**

Use `apply_patch` to restore the three behaviors in `markdown.rs` to commit `b70fe18`: remove `=` from the line-start escape set, restore the old first-segment expression, and restore the prior child push order. Do not use Git checkout/reset. This makes every restart behavior earn a new failing test.

- [ ] **Step 2: Add writer and pathological-input tests before production modules**

Add focused tests with these exact behaviors:

```rust
#[test]
fn final_limit_is_checked_after_whitespace_normalization() {
    let one = MarkdownOptions { base_url: None, max_chars: 1 };
    assert_eq!(html_to_markdown_with_options("<p>x </p>", &one).unwrap(), "x");
    assert_eq!(html_to_markdown_with_options("<p>     x</p>", &one).unwrap(), "x");

    let three = MarkdownOptions { base_url: None, max_chars: 3 };
    assert_eq!(html_to_markdown_with_options("<p>x          y</p>", &three).unwrap(), "x y");
}

#[test]
fn huge_code_and_pre_fail_at_tiny_limits() {
    let body = "a".repeat(2_000_000);
    let options = MarkdownOptions { base_url: None, max_chars: 16 };
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
```

Add `FinalWriter` unit tests proving `used_chars` stays O(1)-tracked, reservations prevent opening syntax without space for closing syntax, pending trailing space is free, a Unicode crab counts as one character, and `output.chars().count() <= max_chars` on every successful finish.

Add a parser helper in the integration test:

```rust
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
```

- [ ] **Step 3: Verify RED**

Run:

```text
cargo test -p rscraper-core --test markdown final_limit_is_checked_after_whitespace_normalization -- --nocapture
cargo test -p rscraper-core --test markdown huge_code_and_pre_fail_at_tiny_limits -- --nocapture
```

Expected: the final-limit case fails on trailing/internal whitespace and the large code/pre path demonstrates the old independent allocation path. The parser helper initially fails to compile because `pulldown-cmark` is absent.

- [ ] **Step 4: Add the demonstrated dev dependency**

After RED, add:

```toml
# Cargo.toml [workspace.dependencies]
pulldown-cmark = "0.13.4"

# crates/rscraper-core/Cargo.toml [dev-dependencies]
pulldown-cmark = { workspace = true }
```

- [ ] **Step 5: Implement the facade and FinalWriter**

Replace the old `Buf`, `Vec<String>` block accumulation, `used/left`, and final `join` accounting with `FinalWriter`. Keep the public signatures and defaults unchanged. Integrate whitespace normalization into the writer; do not normalize a completed independent string afterward.

Implement an explicit traversal stack/depth guard used by the facade. Code/pre traversal must iterate descendant text nodes twice when needed and check the writer before retaining the next character. A syntax reservation is released only when its matching closer is emitted.

Task 1 need not finish all inline/list/table semantics, but every pre-existing Markdown test must remain green. Temporary private adapters may remain only when they write through the shared `FinalWriter` and cannot retain arbitrary rendered content.

- [ ] **Step 6: Verify GREEN and bounded architecture**

Run:

```text
cargo test -p rscraper-core markdown::output::tests -- --nocapture
cargo test -p rscraper-core --test markdown
cargo test -p rscraper-core
cargo clippy -p rscraper-core --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Inspect the diff and confirm there is no `ElementRef::text().collect::<String>()`, no function that recomputes completed output length, and no per-cell rendered `Vec<String>`.

- [ ] **Step 7: Checkpoint**

```text
git add Cargo.toml Cargo.lock crates/rscraper-core
git commit -m "refactor: add bounded markdown writer"
```

---

### Task 2: Rebuild inline rendering, URL policy, and root selection

**Files:**
- Modify: `crates/rscraper-core/src/markdown.rs`
- Modify: `crates/rscraper-core/src/markdown/output.rs`
- Create: `crates/rscraper-core/src/markdown/root.rs`
- Create: `crates/rscraper-core/src/markdown/url.rs`
- Create: `crates/rscraper-core/src/markdown/inline.rs`
- Test: `crates/rscraper-core/tests/markdown.rs`

**Interfaces:**
- Consumes: `FinalWriter`, `MAX_DOM_DEPTH`, `MarkdownOptions`.
- Produces privately:

```rust
pub(super) enum InlineContext {
    Normal,
    LinkLabel,
    ImageAlt,
    TableCell,
}

pub(super) fn render_inline_children(
    element: scraper::ElementRef<'_>,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    context: InlineContext,
    depth: usize,
) -> Result<()>;

pub(super) fn safe_destination(raw: &str, base: Option<&url::Url>) -> Option<String>;

pub(super) fn select_content_root<'a>(
    document: &'a scraper::Html,
    options: &MarkdownOptions,
) -> Result<scraper::ElementRef<'a>>;
```

- [ ] **Step 1: Add failing parser-backed inline, URL, root, and escape tests**

Add fixtures covering:

```rust
#[test]
fn inline_boundaries_preserve_text_and_ast() {
    let markdown = html_to_markdown(
        "<p>A<a href=\"/x\"> B </a>C</p>\
         <p><strong>A</strong><strong>B</strong></p>\
         <p><code>A</code><code>B</code></p>\
         <p>A<code> </code>B</p>",
    );
    assert!(markdown.contains("A [B](/x) C"));
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "text:AB"));
    assert!(events.iter().any(|event| event == "code:AB"));
    assert!(events.iter().any(|event| event == "code: "));
}

#[test]
fn ordinary_text_never_becomes_setext_or_list_markdown() {
    let markdown = html_to_markdown("<p>x<br>===</p><p>1) ordinary</p>");
    let events = parse_events(&markdown);
    assert!(!events.iter().any(|event| event.contains("Heading")));
    assert!(!events.iter().any(|event| event.contains("List(")));
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
```

Add parser-backed cases for leading/trailing/all-space code, newline code,
backtick-only code, adjacent `em`/`del`, link-label and image-alt block starters,
Unicode/percent/parenthesis destinations, credentials, scheme-relative URLs,
hidden `nav`/`template` ancestors, image-only roots, and empty-candidate fallback.

- [ ] **Step 2: Verify RED**

Run:

```text
cargo test -p rscraper-core --test markdown inline_boundaries_preserve_text_and_ast -- --nocapture
cargo test -p rscraper-core --test markdown relative_query_and_fragment_colons_are_safe -- --nocapture
cargo test -p rscraper-core --test markdown first_visible_preferred_root_wins_in_document_order -- --nocapture
```

Expected: link whitespace/adjacent delimiters, query/fragment colons, and root order fail against the stopped renderer.

- [ ] **Step 3: Implement document-order root and URL modules**

Root traversal propagates excluded state and performs separate document-order passes for `main`, `[role=main]`, and `article`, then body/root. Meaningfulness uses streamed renderable text or an image with nonempty alt and a permitted destination. No descendant under an excluded ancestor is eligible.

URL validation splits the raw reference before `?`/`#` to inspect only the first path segment for a scheme-like colon. Resolve against a base when present; reject scheme-relative no-base references, credentials, controls, invalid percent escapes, and non-HTTP(S) absolute schemes. Percent-encode Markdown-forbidden destination bytes without double-encoding valid escapes.

- [ ] **Step 4: Implement one inline state machine**

All child nodes share the writer's whitespace and line state. Coalesce immediately adjacent equivalent strong/emphasis/deletion/code elements before emitting delimiters. Code spans scan source runs and apply CommonMark padding exactly; all-space content remains the same visible text. Text escaping consults logical line state and protects both `1.` and `1)` markers plus Setext `=` runs.

Links/images reserve their closing syntax, render labels/alts in the appropriate context, and release the reservation only when the closer is written. Invalid destinations render safe visible text without link syntax.

- [ ] **Step 5: Verify GREEN and regression**

Run:

```text
cargo test -p rscraper-core --test markdown
cargo test -p rscraper-core
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 6: Checkpoint**

```text
git add crates/rscraper-core
git commit -m "refactor: rebuild markdown inline rendering"
```

---

### Task 3: Rebuild block, list, and table rendering and close Task 4

**Files:**
- Modify: `crates/rscraper-core/src/markdown.rs`
- Modify: `crates/rscraper-core/src/markdown/output.rs`
- Modify: `crates/rscraper-core/src/markdown/inline.rs`
- Create: `crates/rscraper-core/src/markdown/block.rs`
- Test: `crates/rscraper-core/tests/markdown.rs`

**Interfaces:**
- Consumes: the Task 1 writer and Task 2 inline/root/URL modules.
- Produces the final behavior behind the unchanged public Task 4 API.

- [ ] **Step 1: Add failing parser-backed list and table tests**

Add exact list fixtures:

```rust
#[test]
fn ordered_lists_parse_with_marker_relative_nesting() {
    let markdown = html_to_markdown(
        "<div><ol start=\"123\"><li>A<ul><li>B</li></ul></li>\
         <li value=\"999999999\">C</li></ol></div>",
    );
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "start:List(Some(123))"));
    assert!(events.iter().filter(|event| event.as_str() == "start:Item").count() >= 3);
    assert!(events.iter().any(|event| event == "text:B"));
}

#[test]
fn unsupported_ordered_counters_use_one_safe_fallback_list() {
    let markdown = html_to_markdown(
        "<ol start=\"-2\"><li>A</li><li value=\"9223372036854775807\">B</li></ol>",
    );
    let events = parse_events(&markdown);
    assert!(events.iter().any(|event| event == "start:List(None)"));
    assert_eq!(events.iter().filter(|event| event.as_str() == "start:Item").count(), 2);
    assert!(events.iter().any(|event| event == "text:-2. A"));
    assert!(events.iter().any(|event| event == "text:9223372036854775807. B"));
}
```

Add table fixtures that parse with `ENABLE_TABLES` and assert:

- a `tbody` `th scope="row"` row remains a body row behind a synthetic header;
- explicit `thead` becomes the header;
- `align="right"`, `align="center"`, and `align="left"` produce the expected `Tag::Table` alignment vector;
- conflicting column alignment becomes `None`;
- nested table rows are not adopted by the outer table;
- data-only and uneven-row tables keep every data row;
- pipes/newlines/code pipes preserve column count and visible cell text.

Add block fixtures for multiple paragraphs/pre/blockquote/nested list inside an item, repeated `dd`, raw code fence language, hard breaks, thematic breaks, and a large table under a tiny output limit.

- [ ] **Step 2: Verify RED**

Run:

```text
cargo test -p rscraper-core --test markdown ordered_lists_parse_with_marker_relative_nesting -- --nocapture
cargo test -p rscraper-core --test markdown unsupported_ordered_counters_use_one_safe_fallback_list -- --nocapture
cargo test -p rscraper-core --test markdown table -- --nocapture
```

Expected: marker-relative nesting, unsupported-counter fallback, row-header ownership, and alignment fail before the block rewrite.

- [ ] **Step 3: Implement streaming block traversal**

Use an explicit block traversal stack and write completed blocks directly to the shared writer. Blank-line normalization belongs to `FinalWriter`; do not accumulate `Vec<String>` blocks.

Paragraphs, headings, blockquotes, descriptions, hard breaks, and thematic breaks reserve their required syntax and stream descendants. Code fences perform a metadata/text-node scan for maximum backticks and language, then stream the same nodes into the writer.

- [ ] **Step 4: Implement marker-aware lists**

Pre-scan only direct `li` counter attributes to decide ordered versus fallback mode. For representable ordered lists, calculate continuation indentation from the emitted marker length. For fallback mode, emit a single unordered-list AST and begin each item with its escaped visible decimal counter. Checked decimal state never silently resets or saturates. Nested lists increment only logical list depth.

- [ ] **Step 5: Implement owned-row streaming tables**

Pre-scan the current table for owned rows, section, column count, and normalized alignment. Do not render cell bodies during the pre-scan and do not retain rendered cells. Emit the header/synthetic header and delimiter row, then stream each owned data row. A nested table may contribute bounded flattened text to its cell but never contributes an outer row.

- [ ] **Step 6: Verify Task 4 GREEN**

Run:

```text
cargo test -p rscraper-core --test markdown
cargo test -p rscraper-core
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Run source invariants:

```text
rg -n 'text\(\)\.collect|Vec<String>|fn used\(|fn left\(' crates/rscraper-core/src/markdown.rs crates/rscraper-core/src/markdown
```

Expected: no renderer content collection, block-vector accumulation, or output-length rescanning remains. Any `Vec` used for bounded table metadata must contain only node references, widths, sections, or alignment values.

- [ ] **Step 7: Checkpoint**

```text
git add crates/rscraper-core
git commit -m "refactor: complete structured markdown rendering"
```

- [ ] **Step 8: Final Task 4 acceptance**

Freeze the complete restart diff from the pre-restart base and dispatch a fresh capable reviewer. Approval requires both spec compliance and code quality with no open Critical/Important findings. Only then mark parent Task 4 complete and proceed to Task 5.

---

## Plan self-review checklist

- The three checkpoints preserve the exact public interface used by Tasks 6 and 8.
- Every known open gate finding maps to an explicit failing test and implementation step.
- Output bounds, whitespace finalization, depth, URLs, roots, inline delimiters, lists, tables, descriptions, and fences have named ownership.
- Tests use the current official `pulldown-cmark 0.13.4` parser with table and strikethrough options.
- No task depends on a type or file introduced only by a later checkpoint.
- No live network or browser execution is introduced by the restart.
