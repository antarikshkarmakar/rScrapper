# rScrapper Task 4 Markdown Renderer Restart Design

**Status:** Approved design, awaiting implementation plan

**Date:** 2026-08-28

**Parent specification:** `docs/superpowers/specs/2026-08-27-rscraper-platform-rewrite-design.md`

## Purpose

Restart Task 4 as a bounded, parser-validated HTML-to-Markdown subsystem. The
existing renderer accumulated independent buffers and context-local whitespace
rules, so exact fixture patches could not guarantee output limits or valid
CommonMark structure. This restart replaces that architecture while preserving
the approved public API and the behavior required by the parent specification.

## Public contract

The public interface remains unchanged:

```rust
pub struct MarkdownOptions {
    pub base_url: Option<url::Url>,
    pub max_chars: usize,
}

pub fn html_to_markdown(html: &str) -> String;

pub fn html_to_markdown_with_options(
    html: &str,
    options: &MarkdownOptions,
) -> rscraper_core::Result<String>;
```

`html_to_markdown_with_options` returns `Error::BodyLimit` when finalized
Markdown would exceed `max_chars`. It returns a typed parse error when the HTML
tree exceeds the supported depth or when an ordered-list counter cannot be
represented safely and the documented fallback cannot be constructed.

The compatibility wrapper retains its non-fallible signature and returns an
empty string only when the fallible renderer cannot represent a result.

## Architecture

The subsystem is split by responsibility:

```text
crates/rscraper-core/src/markdown.rs
  public facade, HTML parsing, orchestration, compatibility wrapper

crates/rscraper-core/src/markdown/output.rs
  single final-output writer, Unicode budget, pending whitespace, line state

crates/rscraper-core/src/markdown/root.rs
  document-order meaningful-root selection and excluded-ancestor propagation

crates/rscraper-core/src/markdown/url.rs
  HTTP(S) and relative-reference validation plus destination encoding

crates/rscraper-core/src/markdown/inline.rs
  inline DOM state, escaping, links/images, emphasis/deletion, code spans

crates/rscraper-core/src/markdown/block.rs
  block traversal, lists, tables, blockquotes, code fences, descriptions
```

Private modules communicate through explicit renderer state rather than
returning independently rendered `String` values. The DOM input may be large,
but emitted Markdown and renderer-owned intermediates remain bounded.

## Final-output writer

All Markdown bytes flow through one `FinalWriter`:

- It tracks finalized Unicode characters in O(1); it never rescans completed
  output to calculate remaining capacity.
- Collapsible whitespace is held as a small pending state and is charged only
  when it is actually emitted. Leading and trailing collapsible whitespace does
  not cause a false `BodyLimit`.
- It tracks logical line starts so CommonMark-active text is escaped in the
  correct context, including ATX headings, unordered and ordered list markers,
  thematic breaks, and Setext underline runs.
- Structural emitters reserve their closing syntax before writing an opener.
  An error drops the writer, so the API never returns partial Markdown.
- The output `String` never grows beyond `max_chars`. Temporary syntax state is
  fixed-size; bounded inline coalescing may use at most the writer's remaining
  budget.
- Large code, preformatted text, table cells, and ordinary text are streamed
  from DOM nodes. They are not collected with `ElementRef::text().collect()`.
- Every loop that writes text checks the writer before retaining the next
  character. A large input with a tiny limit fails after bounded work and
  allocation relative to the requested output.

The renderer uses an explicit traversal stack and rejects element depth beyond
256 with `Error::Parse`. Meaningfulness discovery is iterative as well.

## Inline semantics

Inline rendering maintains one whitespace and delimiter state across all child
nodes, including nested links and code:

- Direct text and elements preserve DOM order.
- Leading/trailing whitespace inside an inline element is normalized at the
  surrounding boundary rather than trimmed in an isolated buffer.
- Adjacent equivalent emphasis, strong, deletion, or code nodes are coalesced
  when this preserves their visible text and semantic node type. Other adjacent
  delimiters are emitted with a parser-proven non-ambiguous form.
- CommonMark punctuation is escaped contextually. Ordinary HTML text must not
  become a heading, list, thematic break, blockquote, Setext heading, link, or
  other active construct.
- Code spans use a delimiter one backtick longer than the longest source run.
  Their padding follows CommonMark whitespace rules, including all-space,
  leading/trailing-space, newline, and all-backtick content.
- Image alt text is escaped as label text. An image is renderable content only
  when it has meaningful alt text and a permitted destination.

## Block semantics

Supported blocks remain those required by the parent specification: headings,
paragraphs, hard breaks, thematic breaks, blockquotes, fenced code, ordered and
unordered lists, description lists, and GFM tables.

### Lists

List depth is independent of unrelated DOM nesting. Continuation indentation
is calculated from the exact emitted marker width.

Ordered counters are pre-scanned without rendering item content:

- Values from `0` through `999_999_999` use CommonMark ordered markers.
- `ol[start]` and `li[value]` update subsequent values with checked decimal
  arithmetic.
- If any item needs a negative, more-than-nine-digit, out-of-range, or
  overflowing counter, the whole ordered list uses a safe unordered fallback.
  Each bullet begins with the escaped visible decimal counter followed by the
  item content. This preserves list structure and visible numbering without
  emitting invalid ordered-list syntax or silently resetting values.
- `reversed` remains an explicitly unsupported presentation hint in 0.2; item
  order is preserved and ordinary forward counter rules apply.

Paragraphs, code blocks, blockquotes, nested lists, and hard-break continuation
lines inside an item are indented relative to that item's actual marker.

### Tables

The renderer performs a metadata-only pre-scan of rows owned by the current
table. It stores section ownership, maximum column count, and a normalized
alignment vector, but never rendered cell strings.

- Rows from nested tables are not adopted by their ancestors.
- An owned `thead` row supplies the GFM column header.
- A direct row containing genuine column-header cells may supply a header only
  when it is not a `tbody` row-header (`scope="row"`) case.
- Data-only tables receive a synthetic empty header; their first data row is not
  promoted.
- `left`, `center`, and `right` alignment is emitted using GFM delimiter colons
  when declarations agree. Conflicting declarations fall back to unaligned.
- Cells are streamed through the shared writer. Pipes and newlines are encoded
  in a table-aware context, including pipes inside code spans.
- Nested tables inside cells contribute bounded flattened cell content but do
  not create duplicate outer rows.

### Code fences

Preformatted content is scanned twice without collecting it: once to find the
longest backtick run, then again to stream content. A direct `code` child's
conservative `language-*` class becomes the opening info token. Raw contents and
the closing fence remain within the shared output budget.

## Content-root selection

Root selection preserves the priority order `main`, `[role=main]`, `article`,
then `body`/document root. Within each priority, the first meaningful candidate
in document order wins.

Traversal propagates excluded state from hidden elements and semantic
non-content ancestors such as `nav`, `template`, `script`, and `style`. A
preferred descendant cannot escape an excluded ancestor. Meaningfulness is
based on renderable text or a non-decorative permitted image, not text alone.

Class filtering uses exact boundary-aware tokens from the named exclusion set;
substrings such as `reading`, `shadow`, and `adventure` remain content.

## URL and destination safety

Absolute destinations must be HTTP(S), contain no credentials, and contain no
control characters. With a base URL, relative references are resolved through
`Url::join` and revalidated.

Without a base URL:

- scheme-relative references are rejected;
- a colon is rejected only when it appears in the first path segment before
  any query or fragment, where it would be scheme-like;
- colons in later path segments, query strings, and fragments are permitted;
- Unicode and unsafe Markdown destination delimiters are percent-encoded;
- malformed percent escapes, control characters, credentials, and dangerous
  absolute schemes are rejected.

Destinations are emitted in a parser-safe parenthesized form with backslashes,
parentheses, angle brackets, quotes, whitespace, and controls encoded.

## Testing strategy

`pulldown-cmark` is added only as a dev dependency. Tests enable its GFM table
and strikethrough options and assert both event structure and recovered visible
text. Exact string assertions remain where formatting is part of the contract.

The restart has three independently reviewed checkpoints:

1. **Writer foundation:** parser-backed test helpers, `FinalWriter`, O(1)
   accounting, pending whitespace, depth bounds, streaming text/code/pre, and
   pathological allocation/performance fixtures.
2. **Inline and selection:** cross-node whitespace, delimiter coalescing, code
   spans, escaping, links/images, URL policy, document-order roots, and hidden
   ancestry.
3. **Block integration:** marker-aware lists and fallback counters, descriptions,
   blockquotes, code fences, table section/alignment ownership, full regression
   suite, and final Task 4 gate.

Each checkpoint follows RED-GREEN-REFACTOR, commits separately, and receives a
fresh scoped review before the next checkpoint begins.

Required final gates:

```text
cargo test -p rscraper-core --test markdown
cargo test -p rscraper-core
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Performance tests use deterministic operation/allocation proxies rather than
fragile wall-clock thresholds where possible. Local tests perform no network
access.

## Migration and compatibility

No public Rust signature changes in this restart. Existing accepted Markdown
may change where the old output parsed incorrectly—for example adjacent inline
delimiters, signed ordered counters, data-only tables, alignment, and hidden
root selection. These corrections are documented later in `MIGRATION.md` under
Task 12.

The three uncommitted safeguard edits left by the stopped attempt are not
trusted as implementation. They are removed before production work and
reintroduced only after their corresponding RED tests fail.

## Acceptance criteria

- All parent Task 4 fixtures and restart fixtures pass.
- Parser-backed assertions prove expected paragraph, inline, list, code, link,
  image, and table structure plus visible text.
- Final output never exceeds `max_chars`, false limits from discarded
  whitespace do not occur, and large inputs with tiny limits retain only
  bounded renderer-owned state.
- Traversal cannot overflow the call stack on pathological depth.
- No hidden/skipped ancestor leaks a preferred root.
- All destinations are safe and syntactically valid.
- Strict formatting, Clippy, core, and workspace gates pass.
- A fresh final reviewer approves Task 4 before Task 5 begins.
