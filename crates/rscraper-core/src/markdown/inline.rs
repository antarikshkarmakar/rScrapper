use super::output::{FinalWriter, MAX_DOM_DEPTH};
use super::root::is_excluded_element;
use super::url::{bounded_destination, destination_is_allowed_with_budget, BoundedDestination};
use super::MarkdownOptions;
use crate::{Error, Result};
use ego_tree::NodeRef;
use scraper::{ElementRef, Node};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InlineContext {
    Normal,
    LinkLabel,
    ImageAlt,
    TableCell,
}

#[derive(Clone, Copy)]
pub(super) struct InlineState {
    pub(super) trim_leading: bool,
    pub(super) table: bool,
    pub(super) force_start: bool,
    pub(super) pending_space: bool,
    leading_whitespace: bool,
    line_phase: LineStartPhase,
    after_hard_break: bool,
    emitted_boundary: EmittedBoundary,
    wrapper_close_needs_punctuation: bool,
    last_closed_wrapper: Option<ClosedWrapper>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LineStartPhase {
    Start,
    Digits,
    Content,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EmittedBoundary {
    None,
    Word(char),
    Unknown(char),
    Punctuation,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UpcomingBoundary {
    WordLike,
    Punctuation,
    Unknown,
}

impl InlineState {
    pub(super) fn new(table: bool) -> Self {
        Self {
            trim_leading: true,
            table,
            force_start: false,
            pending_space: false,
            leading_whitespace: false,
            line_phase: LineStartPhase::Start,
            after_hard_break: false,
            emitted_boundary: EmittedBoundary::None,
            wrapper_close_needs_punctuation: false,
            last_closed_wrapper: None,
        }
    }

    fn for_context(context: InlineContext) -> Self {
        Self {
            trim_leading: true,
            table: context == InlineContext::TableCell,
            force_start: matches!(context, InlineContext::LinkLabel | InlineContext::ImageAlt),
            pending_space: false,
            leading_whitespace: false,
            line_phase: LineStartPhase::Start,
            after_hard_break: false,
            emitted_boundary: EmittedBoundary::None,
            wrapper_close_needs_punctuation: false,
            last_closed_wrapper: None,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FormatFamily {
    Strong,
    Emphasis,
    Deletion,
}

const DELETION_WRAPPER_BOUNDARY: &str = "<!---->";

#[derive(Clone, Copy)]
struct ClosedWrapper {
    family: FormatFamily,
    marker: &'static str,
}

enum Completion {
    Root,
    Transparent,
    Wrapped {
        opening_boundary: &'static str,
        marker: &'static str,
        marker_chars: usize,
        family: FormatFamily,
        started: bool,
        emitted: bool,
    },
    Link {
        destination: BoundedDestination,
        closing_chars: usize,
        started: bool,
    },
}

struct InlineFrame<'a> {
    next_child: Option<NodeRef<'a, Node>>,
    follow_siblings: bool,
    node_depth: usize,
    state: InlineState,
    context: InlineContext,
    completion: Completion,
    wrote: bool,
}

pub(super) fn render_inline_children(
    element: ElementRef<'_>,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    context: InlineContext,
    depth: usize,
) -> Result<()> {
    let mut state = InlineState::for_context(context);
    render_nodes(
        element.first_child(),
        true,
        writer,
        options,
        context,
        depth + 1,
        "",
        &mut state,
    )?;
    Ok(())
}

pub(super) fn render_inline_children_with_state(
    element: ElementRef<'_>,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    context: InlineContext,
    depth: usize,
    line_prefix: &str,
    state: &mut InlineState,
) -> Result<bool> {
    render_nodes(
        element.first_child(),
        true,
        writer,
        options,
        context,
        depth + 1,
        line_prefix,
        state,
    )
}

pub(super) fn render_inline_element_with_state(
    element: ElementRef<'_>,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    context: InlineContext,
    depth: usize,
    line_prefix: &str,
    state: &mut InlineState,
) -> Result<bool> {
    render_nodes(
        Some(*element),
        false,
        writer,
        options,
        context,
        depth,
        line_prefix,
        state,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_nodes<'a>(
    first_node: Option<NodeRef<'a, Node>>,
    follow_siblings: bool,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    context: InlineContext,
    depth: usize,
    line_prefix: &str,
    state: &mut InlineState,
) -> Result<bool> {
    let mut frames = vec![InlineFrame {
        next_child: first_node,
        follow_siblings,
        node_depth: depth,
        state: *state,
        context,
        completion: Completion::Root,
        wrote: false,
    }];

    loop {
        let empty_link_needs_syntax = frames.last().is_some_and(|frame| {
            frame.next_child.is_none()
                && matches!(frame.completion, Completion::Link { started: false, .. })
        });
        if empty_link_needs_syntax {
            start_pending_frames(
                writer,
                line_prefix,
                &mut frames,
                UpcomingBoundary::Punctuation,
                false,
            )?;
        }

        if let Some(completed) = frames.pop_if(|frame| frame.next_child.is_none()) {
            let completed_boundary = completed.state.emitted_boundary;
            match completed.completion {
                Completion::Root => {
                    *state = completed.state;
                    return Ok(completed.wrote);
                }
                Completion::Transparent => {
                    let parent = frames.last_mut().expect("transparent frame has a parent");
                    parent.state = completed.state;
                    parent.wrote |= completed.wrote;
                }
                Completion::Wrapped {
                    marker,
                    marker_chars,
                    family,
                    started,
                    emitted,
                    ..
                } => {
                    if !started {
                        if emitted {
                            let parent = frames.last_mut().expect("wrapped frame has a parent");
                            mark_content_after_wrapper_break(writer, &mut parent.state);
                            parent.wrote = true;
                        } else {
                            propagate_empty_boundary(writer, completed.state, frames.last_mut());
                        }
                        continue;
                    }
                    let trailing = completed.state.pending_space;
                    writer.discard_pending_space();
                    writer.release(marker_chars);
                    writer.write_literal(marker)?;
                    let parent = frames.last_mut().expect("wrapped frame has a parent");
                    mark_wrapper_close(&mut parent.state, completed_boundary, family, marker);
                    parent.wrote = true;
                    if trailing {
                        writer.request_space();
                        parent.state.pending_space = !writer.is_line_start();
                        parent.state.wrapper_close_needs_punctuation = false;
                        parent.state.last_closed_wrapper = None;
                    }
                }
                Completion::Link {
                    destination,
                    closing_chars,
                    started,
                } => {
                    debug_assert!(started, "empty links are started before completion");
                    let trailing = completed.state.pending_space;
                    writer.discard_pending_space();
                    writer.release(closing_chars);
                    writer.write_literal("](")?;
                    writer.write_literal(&destination.text)?;
                    writer.write_char(')')?;
                    let parent = frames.last_mut().expect("link frame has a parent");
                    mark_syntax_content(&mut parent.state);
                    parent.wrote = true;
                    if trailing {
                        writer.request_space();
                        parent.state.pending_space = !writer.is_line_start();
                    }
                }
            }
            continue;
        }

        let (node, node_depth) = {
            let frame = frames.last_mut().expect("inline frame exists");
            let node = frame.next_child.expect("inline child checked above");
            frame.next_child = if frame.follow_siblings {
                node.next_sibling()
            } else {
                None
            };
            (node, frame.node_depth)
        };

        if let Some(text) = node.value().as_text() {
            write_inline_text(writer, line_prefix, text, &mut frames)?;
            continue;
        }

        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };
        check_depth(node_depth)?;
        if is_excluded_element(&element) {
            continue;
        }

        match element.value().name() {
            "a" => open_link(
                element,
                node_depth,
                writer,
                options,
                line_prefix,
                &mut frames,
            )?,
            "img" => render_image(element, writer, options, line_prefix, &mut frames)?,
            "strong" | "b" => open_wrapped(
                element,
                node_depth,
                FormatFamily::Strong,
                "**",
                writer,
                options,
                line_prefix,
                &mut frames,
            )?,
            "em" | "i" => open_wrapped(
                element,
                node_depth,
                FormatFamily::Emphasis,
                "*",
                writer,
                options,
                line_prefix,
                &mut frames,
            )?,
            "del" | "s" | "strike" => open_wrapped(
                element,
                node_depth,
                FormatFamily::Deletion,
                "~~",
                writer,
                options,
                line_prefix,
                &mut frames,
            )?,
            "code" => render_code_run(element, writer, options, line_prefix, &mut frames)?,
            "br" => render_break(writer, line_prefix, &mut frames)?,
            _ => {
                let parent = frames.last().expect("transparent parent exists");
                frames.push(InlineFrame {
                    next_child: element.first_child(),
                    follow_siblings: true,
                    node_depth: node_depth + 1,
                    state: parent.state,
                    context: parent.context,
                    completion: Completion::Transparent,
                    wrote: false,
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn open_wrapped<'a>(
    element: ElementRef<'a>,
    depth: usize,
    family: FormatFamily,
    marker: &'static str,
    writer: &mut FinalWriter,
    _options: &MarkdownOptions,
    line_prefix: &str,
    frames: &mut Vec<InlineFrame<'a>>,
) -> Result<()> {
    if frames.iter().any(|frame| {
        matches!(
            frame.completion,
            Completion::Wrapped {
                family: active,
                ..
            } if active == family
        )
    }) {
        let parent = frames.last().expect("wrapped parent exists");
        frames.push(InlineFrame {
            next_child: element.first_child(),
            follow_siblings: true,
            node_depth: depth + 1,
            state: parent.state,
            context: parent.context,
            completion: Completion::Transparent,
            wrote: false,
        });
        return Ok(());
    }

    let opening = wrapper_opening(family, marker, frames);
    if simple_text_child(element).is_some() {
        return render_simple_wrapped_run(
            element,
            depth,
            family,
            opening,
            writer,
            line_prefix,
            frames,
        );
    }

    let marker_chars = opening.marker.chars().count();
    let context = {
        let parent = frames.last().expect("wrapped parent exists");
        parent.context
    };
    frames.push(InlineFrame {
        next_child: element.first_child(),
        follow_siblings: true,
        node_depth: depth + 1,
        state: InlineState::for_context(context),
        context,
        completion: Completion::Wrapped {
            opening_boundary: opening.boundary,
            marker: opening.marker,
            marker_chars,
            family,
            started: false,
            emitted: false,
        },
        wrote: false,
    });
    Ok(())
}

#[derive(Clone, Copy)]
struct WrapperOpening {
    boundary: &'static str,
    marker: &'static str,
}

fn wrapper_opening(
    family: FormatFamily,
    mut marker: &'static str,
    frames: &[InlineFrame<'_>],
) -> WrapperOpening {
    let previous = frames
        .last()
        .expect("wrapped parent exists")
        .state
        .last_closed_wrapper;
    if let Some(closed) = previous {
        marker = match family {
            FormatFamily::Strong if closed.marker.contains('*') => "__",
            FormatFamily::Strong if closed.marker.contains('_') => "**",
            FormatFamily::Emphasis if closed.marker.contains('*') => "_",
            FormatFamily::Emphasis if closed.marker.contains('_') => "*",
            FormatFamily::Deletion => marker,
            _ => marker,
        };
    }

    if family == FormatFamily::Emphasis {
        if let Some(strong_index) = leading_strong_frame(frames) {
            let outer_marker = match frames[strong_index].completion {
                Completion::Wrapped { marker, .. } => marker,
                _ => unreachable!("the parent was just matched as a strong wrapper"),
            };
            marker = if outer_marker == "**" { "_" } else { "*" };
        }
    }
    let boundary = if family == FormatFamily::Deletion
        && previous.is_some_and(|closed| closed.family == FormatFamily::Deletion)
    {
        DELETION_WRAPPER_BOUNDARY
    } else {
        ""
    };
    WrapperOpening { boundary, marker }
}

#[allow(clippy::too_many_arguments)]
fn render_simple_wrapped_run<'a>(
    element: ElementRef<'a>,
    depth: usize,
    family: FormatFamily,
    opening: WrapperOpening,
    writer: &mut FinalWriter,
    line_prefix: &str,
    frames: &mut Vec<InlineFrame<'a>>,
) -> Result<()> {
    let parent_index = frames.len() - 1;
    let context = frames[parent_index].context;
    let marker_chars = opening.marker.chars().count();
    frames.push(InlineFrame {
        next_child: None,
        follow_siblings: false,
        node_depth: depth + 1,
        state: InlineState::for_context(context),
        context,
        completion: Completion::Wrapped {
            opening_boundary: opening.boundary,
            marker: opening.marker,
            marker_chars,
            family,
            started: false,
            emitted: false,
        },
        wrote: false,
    });

    let mut member = element;
    let mut cursor = if frames[parent_index].follow_siblings {
        element.next_sibling()
    } else {
        None
    };
    loop {
        let text_node = simple_text_child(member).expect("simple member was checked");
        let text = text_node
            .value()
            .as_text()
            .expect("simple member owns one direct text child");
        write_inline_text(writer, line_prefix, text, frames)?;

        match next_simple_format_member(cursor, family) {
            SimpleFormatNext::Member { element, after } => {
                frames[parent_index].next_child = after;
                member = element;
                cursor = after;
            }
            SimpleFormatNext::End { resume } => {
                frames[parent_index].next_child = resume;
                break;
            }
        }
    }
    Ok(())
}

fn simple_text_child<'a>(element: ElementRef<'a>) -> Option<NodeRef<'a, Node>> {
    let child = element.first_child()?;
    if child.next_sibling().is_some() || child.value().as_text().is_none() {
        return None;
    }
    Some(child)
}

enum SimpleFormatNext<'a> {
    Member {
        element: ElementRef<'a>,
        after: Option<NodeRef<'a, Node>>,
    },
    End {
        resume: Option<NodeRef<'a, Node>>,
    },
}

fn next_simple_format_member<'a>(
    mut cursor: Option<NodeRef<'a, Node>>,
    family: FormatFamily,
) -> SimpleFormatNext<'a> {
    while let Some(node) = cursor {
        cursor = node.next_sibling();
        if let Some(text) = node.value().as_text() {
            if text.is_empty() {
                continue;
            }
            return SimpleFormatNext::End { resume: Some(node) };
        }
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };
        if is_excluded_element(&element) {
            continue;
        }
        if element_matches_family(element, family) && simple_text_child(element).is_some() {
            return SimpleFormatNext::Member {
                element,
                after: cursor,
            };
        }
        if is_structurally_empty_separator(element) {
            continue;
        }
        return SimpleFormatNext::End { resume: Some(node) };
    }
    SimpleFormatNext::End { resume: None }
}

fn is_structurally_empty_separator(element: ElementRef<'_>) -> bool {
    element.value().name() == "wbr"
        || (element.first_child().is_none()
            && !matches!(element.value().name(), "a" | "img" | "br"))
}

fn leading_strong_frame(frames: &[InlineFrame<'_>]) -> Option<usize> {
    for (index, frame) in frames.iter().enumerate().rev() {
        match frame.completion {
            Completion::Transparent if !frame.wrote => continue,
            Completion::Wrapped {
                family: FormatFamily::Strong,
                ..
            } if !frame.wrote => return Some(index),
            _ => return None,
        }
    }
    None
}

fn open_link<'a>(
    element: ElementRef<'a>,
    depth: usize,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    _line_prefix: &str,
    frames: &mut Vec<InlineFrame<'a>>,
) -> Result<()> {
    let destination = element
        .value()
        .attr("href")
        .map(|raw| {
            bounded_destination(
                raw,
                options.base_url.as_ref(),
                writer.remaining(),
                options.max_chars,
            )
        })
        .transpose()?
        .flatten();
    let Some(destination) = destination else {
        let parent = frames.last().expect("link parent exists");
        frames.push(InlineFrame {
            next_child: element.first_child(),
            follow_siblings: true,
            node_depth: depth + 1,
            state: parent.state,
            context: parent.context,
            completion: Completion::Transparent,
            wrote: false,
        });
        return Ok(());
    };

    let closing_chars = destination.chars + 3;
    let table = frames.last().expect("link parent exists").state.table;
    frames.push(InlineFrame {
        next_child: element.first_child(),
        follow_siblings: true,
        node_depth: depth + 1,
        state: InlineState {
            table,
            ..InlineState::for_context(InlineContext::LinkLabel)
        },
        context: InlineContext::LinkLabel,
        completion: Completion::Link {
            destination,
            closing_chars,
            started: false,
        },
        wrote: false,
    });
    Ok(())
}

fn render_image(
    element: ElementRef<'_>,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    line_prefix: &str,
    frames: &mut Vec<InlineFrame<'_>>,
) -> Result<()> {
    let alt = element.value().attr("alt").unwrap_or_default();
    let first_visible_alt = alt.chars().find(|ch| !ch.is_whitespace());
    let alt_is_visible = first_visible_alt.is_some();
    let destination = element
        .value()
        .attr("src")
        .map(|raw| {
            bounded_destination(
                raw,
                options.base_url.as_ref(),
                writer.remaining(),
                options.max_chars,
            )
        })
        .transpose()?
        .flatten();

    let Some(destination) = destination else {
        if alt_is_visible {
            start_pending_frames(
                writer,
                line_prefix,
                frames,
                upcoming_scalar_boundary(first_visible_alt.expect("visible alt checked above")),
                false,
            )?;
            let frame = frames.last_mut().expect("image parent exists");
            write_escaped_text(
                writer,
                line_prefix,
                alt,
                InlineContext::ImageAlt,
                true,
                &mut frame.state,
            )?;
            frame.wrote = true;
        }
        return Ok(());
    };
    if !alt_is_visible {
        return Ok(());
    }

    start_pending_frames(
        writer,
        line_prefix,
        frames,
        UpcomingBoundary::Punctuation,
        false,
    )?;
    let closing_chars = destination.chars + 3;
    writer.reserve(closing_chars)?;
    write_prefixed_literal(writer, line_prefix, "![")?;
    let mut alt_state = InlineState::for_context(InlineContext::ImageAlt);
    write_escaped_text(
        writer,
        line_prefix,
        alt,
        InlineContext::ImageAlt,
        true,
        &mut alt_state,
    )?;
    writer.discard_pending_space();
    writer.release(closing_chars);
    writer.write_literal("](")?;
    writer.write_literal(&destination.text)?;
    writer.write_char(')')?;
    let frame = frames.last_mut().expect("image parent exists");
    mark_syntax_content(&mut frame.state);
    frame.wrote = true;
    Ok(())
}

fn render_break(
    writer: &mut FinalWriter,
    line_prefix: &str,
    frames: &mut Vec<InlineFrame<'_>>,
) -> Result<()> {
    let context = frames.last().expect("break parent exists").context;
    if context != InlineContext::Normal {
        let frame = frames.last_mut().expect("break parent exists");
        writer.request_space();
        frame.state.trim_leading = false;
        frame.state.pending_space = !writer.is_line_start();
        frame.state.line_phase = LineStartPhase::Content;
        frame.state.after_hard_break = false;
        frame.state.wrapper_close_needs_punctuation = false;
        frame.state.last_closed_wrapper = None;
        frame.wrote = true;
        return Ok(());
    }

    let consecutive = frames
        .last()
        .expect("break parent exists")
        .state
        .after_hard_break;
    writer.discard_pending_space();
    close_active_wrappers_before_break(writer, frames)?;
    frames
        .last_mut()
        .expect("break parent exists")
        .state
        .pending_space = false;
    if consecutive {
        write_prefixed_literal(writer, line_prefix, "\\")?;
        writer.newline()?;
    } else {
        writer.write_literal("  ")?;
        writer.newline()?;
    }
    for frame in frames.iter_mut() {
        reset_after_break(&mut frame.state);
    }
    frames.last_mut().expect("break parent exists").wrote = true;
    Ok(())
}

fn close_active_wrappers_before_break(
    writer: &mut FinalWriter,
    frames: &mut [InlineFrame<'_>],
) -> Result<()> {
    for frame in frames.iter_mut().rev() {
        let Completion::Wrapped {
            marker,
            marker_chars,
            started,
            emitted,
            ..
        } = &mut frame.completion
        else {
            continue;
        };
        if *started {
            writer.release(*marker_chars);
            writer.write_literal(marker)?;
            *started = false;
            *emitted = true;
        }
    }
    Ok(())
}

fn reset_after_break(state: &mut InlineState) {
    state.trim_leading = true;
    state.force_start = false;
    state.pending_space = false;
    state.leading_whitespace = false;
    state.line_phase = LineStartPhase::Start;
    state.after_hard_break = true;
    state.emitted_boundary = EmittedBoundary::None;
    state.wrapper_close_needs_punctuation = false;
    state.last_closed_wrapper = None;
}

fn render_code_run<'a>(
    element: ElementRef<'a>,
    writer: &mut FinalWriter,
    options: &MarkdownOptions,
    line_prefix: &str,
    frames: &mut Vec<InlineFrame<'a>>,
) -> Result<()> {
    {
        let parent = frames.last_mut().expect("code parent exists");
        detach_code_siblings(&mut parent.next_child, options)?;
    }
    let table = frames.last().expect("code parent exists").state.table;
    let scan = scan_code_run(
        element,
        table,
        writer.remaining(),
        options.max_chars,
        options,
    )?;
    if scan.empty {
        return Ok(());
    }
    start_pending_frames(
        writer,
        line_prefix,
        frames,
        UpcomingBoundary::Punctuation,
        false,
    )?;
    writer.reserve(scan.delimiter)?;
    write_prefixed_repeated(writer, line_prefix, '`', scan.delimiter)?;
    if scan.pad {
        writer.write_char(' ')?;
    }
    for_each_normalized_code_char(element, options, |ch| {
        if table && ch == '|' {
            writer.write_literal("\\|")
        } else {
            writer.write_char(ch)
        }
    })?;
    if scan.pad {
        writer.write_char(' ')?;
    }
    writer.release(scan.delimiter);
    write_repeated(writer, '`', scan.delimiter)?;
    let parent = frames.last_mut().expect("code parent exists");
    mark_syntax_content(&mut parent.state);
    parent.wrote = true;
    Ok(())
}

struct CodeScan {
    delimiter: usize,
    pad: bool,
    empty: bool,
}

fn scan_code_run(
    element: ElementRef<'_>,
    table: bool,
    available: usize,
    limit: usize,
    options: &MarkdownOptions,
) -> Result<CodeScan> {
    let mut longest_ticks = 0usize;
    let mut current_ticks = 0usize;
    let mut content_chars = 0usize;
    let mut first = None;
    let mut last = None;
    let mut all_spaces = true;

    for_each_normalized_code_char(element, options, |ch| {
        content_chars = content_chars
            .checked_add(if table && ch == '|' { 2 } else { 1 })
            .ok_or(Error::BodyLimit { limit })?;
        first.get_or_insert(ch);
        last = Some(ch);
        all_spaces &= ch == ' ';
        if ch == '`' {
            current_ticks += 1;
            longest_ticks = longest_ticks.max(current_ticks);
        } else {
            current_ticks = 0;
        }
        ensure_code_fits(
            content_chars,
            longest_ticks + 1,
            code_needs_padding(first, last, all_spaces),
            available,
            limit,
        )
    })?;

    if content_chars == 0 {
        return Ok(CodeScan {
            delimiter: 0,
            pad: false,
            empty: true,
        });
    }
    let delimiter = longest_ticks + 1;
    let pad = code_needs_padding(first, last, all_spaces);
    ensure_code_fits(content_chars, delimiter, pad, available, limit)?;
    Ok(CodeScan {
        delimiter,
        pad,
        empty: false,
    })
}

fn code_needs_padding(first: Option<char>, last: Option<char>, all_spaces: bool) -> bool {
    first == Some('`')
        || last == Some('`')
        || (!all_spaces && first == Some(' ') && last == Some(' '))
}

fn ensure_code_fits(
    content_chars: usize,
    delimiter: usize,
    pad: bool,
    available: usize,
    limit: usize,
) -> Result<()> {
    let required = content_chars
        .checked_add(delimiter.saturating_mul(2))
        .and_then(|chars| chars.checked_add(usize::from(pad) * 2))
        .ok_or(Error::BodyLimit { limit })?;
    if required > available {
        Err(Error::BodyLimit { limit })
    } else {
        Ok(())
    }
}

fn for_each_normalized_code_char(
    first: ElementRef<'_>,
    options: &MarkdownOptions,
    mut visit: impl FnMut(char) -> Result<()>,
) -> Result<()> {
    let mut group = Some(first);
    let mut previous_was_carriage_return = false;
    while let Some(element) = group {
        if is_excluded_element(&element) {
            group = next_logical_code(element.next_sibling(), options)?;
            continue;
        }
        let mut pending = Vec::new();
        if let Some(child) = element.first_child() {
            pending.push(child);
        }
        while let Some(node) = pending.pop() {
            if let Some(sibling) = node.next_sibling() {
                pending.push(sibling);
            }
            if let Some(text) = node.value().as_text() {
                for ch in text.chars() {
                    if ch == '\n' && previous_was_carriage_return {
                        previous_was_carriage_return = false;
                        continue;
                    }
                    if ch == '\r' {
                        visit(' ')?;
                        previous_was_carriage_return = true;
                    } else if ch == '\n' {
                        visit(' ')?;
                        previous_was_carriage_return = false;
                    } else {
                        visit(ch)?;
                        previous_was_carriage_return = false;
                    }
                }
            } else if let Some(child) = ElementRef::wrap(node) {
                if !is_excluded_element(&child) {
                    if let Some(grandchild) = child.first_child() {
                        pending.push(grandchild);
                    }
                }
            }
        }
        group = next_logical_code(element.next_sibling(), options)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum InlineContentKind {
    Empty,
    Whitespace,
    SyntaxOnly,
    FallbackText,
    VisibleText,
    MeaningfulImage,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum InlineMetadataPurpose {
    Emission,
    PreferredRoot,
}

pub(super) fn inline_content_kind(
    element: ElementRef<'_>,
    options: &MarkdownOptions,
) -> Result<InlineContentKind> {
    inline_content_kind_for(element, options, InlineMetadataPurpose::Emission)
}

pub(super) fn inline_content_kind_for(
    element: ElementRef<'_>,
    options: &MarkdownOptions,
    purpose: InlineMetadataPurpose,
) -> Result<InlineContentKind> {
    let mut pending = vec![(*element, false)];
    let mut presence = InlineContentKind::Empty;
    while let Some((node, follow_siblings)) = pending.pop() {
        if follow_siblings {
            if let Some(sibling) = node.next_sibling() {
                pending.push((sibling, true));
            }
        }
        if let Some(text) = node.value().as_text() {
            for ch in text.chars() {
                #[cfg(test)]
                super::record_text_scalar_visit();
                if !ch.is_whitespace() {
                    presence = presence.max(InlineContentKind::VisibleText);
                    break;
                }
                presence = presence.max(InlineContentKind::Whitespace);
            }
            continue;
        }
        let Some(child) = ElementRef::wrap(node) else {
            continue;
        };
        if is_excluded_element(&child) {
            continue;
        }
        match child.value().name() {
            "code" => {
                presence = presence.max(code_content_kind(child));
                continue;
            }
            "br" => presence = presence.max(InlineContentKind::SyntaxOnly),
            "a" => {
                if purpose == InlineMetadataPurpose::Emission {
                    if let Some(raw) = child.value().attr("href") {
                        if destination_is_allowed_with_budget(
                            raw,
                            options.base_url.as_ref(),
                            options.max_chars,
                            options.max_chars,
                        )? {
                            presence = presence.max(InlineContentKind::SyntaxOnly);
                        }
                    }
                }
            }
            "img" => {
                let alt_is_visible = child
                    .value()
                    .attr("alt")
                    .is_some_and(|alt| alt.chars().any(|ch| !ch.is_whitespace()));
                if alt_is_visible {
                    if let Some(raw) = child.value().attr("src") {
                        if destination_is_allowed_with_budget(
                            raw,
                            options.base_url.as_ref(),
                            options.max_chars,
                            options.max_chars,
                        )? {
                            presence = presence.max(InlineContentKind::MeaningfulImage);
                        } else {
                            presence = presence.max(InlineContentKind::FallbackText);
                        }
                    } else {
                        presence = presence.max(InlineContentKind::FallbackText);
                    }
                }
            }
            _ => {}
        }
        if let Some(grandchild) = child.first_child() {
            pending.push((grandchild, true));
        }
    }
    Ok(presence)
}

fn code_content_kind(element: ElementRef<'_>) -> InlineContentKind {
    let mut pending = element.first_child().into_iter().collect::<Vec<_>>();
    let mut kind = InlineContentKind::Empty;
    while let Some(node) = pending.pop() {
        if let Some(sibling) = node.next_sibling() {
            pending.push(sibling);
        }
        if let Some(text) = node.value().as_text() {
            for ch in text.chars() {
                if ch.is_whitespace() {
                    kind = kind.max(InlineContentKind::SyntaxOnly);
                } else {
                    return InlineContentKind::VisibleText;
                }
            }
        } else if let Some(child) = ElementRef::wrap(node) {
            if !is_excluded_element(&child) {
                if let Some(grandchild) = child.first_child() {
                    pending.push(grandchild);
                }
            }
        }
    }
    kind
}

fn next_logical_code<'a>(
    mut cursor: Option<NodeRef<'a, Node>>,
    options: &MarkdownOptions,
) -> Result<Option<ElementRef<'a>>> {
    while let Some(node) = cursor {
        cursor = node.next_sibling();
        if let Some(text) = node.value().as_text() {
            if text.is_empty() {
                continue;
            }
            return Ok(None);
        }
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };
        if is_excluded_element(&element) {
            continue;
        }
        if element.value().name() == "code" {
            return Ok(Some(element));
        }
        if inline_content_kind(element, options)? != InlineContentKind::Empty {
            return Ok(None);
        }
    }
    Ok(None)
}

fn element_matches_family(element: ElementRef<'_>, family: FormatFamily) -> bool {
    match family {
        FormatFamily::Strong => matches!(element.value().name(), "strong" | "b"),
        FormatFamily::Emphasis => matches!(element.value().name(), "em" | "i"),
        FormatFamily::Deletion => matches!(element.value().name(), "del" | "s" | "strike"),
    }
}

fn detach_code_siblings(
    cursor: &mut Option<NodeRef<'_, Node>>,
    options: &MarkdownOptions,
) -> Result<()> {
    let mut search = *cursor;
    while let Some(element) = next_logical_code(search, options)? {
        search = element.next_sibling();
        *cursor = search;
    }
    Ok(())
}

fn first_word_may_need_boundary_entity(frames: &[InlineFrame<'_>]) -> bool {
    // A one-scalar wrapper prefix can itself become an encoded boundary before a
    // later nested opener. Stay conservative across non-intrinsic nodes so that
    // an arbitrary nesting chain cannot invalidate an opener already emitted.
    let mut next = None;
    for frame in frames.iter().rev() {
        if frame.next_child.is_some() {
            next = frame.next_child;
            break;
        }
        if !matches!(frame.completion, Completion::Transparent | Completion::Root) {
            break;
        }
    }
    let Some(node) = next else {
        return false;
    };
    if let Some(text) = node.value().as_text() {
        return text.is_empty();
    }
    let Some(element) = ElementRef::wrap(node) else {
        return true;
    };
    if is_excluded_element(&element) {
        return true;
    }
    !matches!(element.value().name(), "img" | "code" | "br")
}

fn start_pending_frames(
    writer: &mut FinalWriter,
    line_prefix: &str,
    frames: &mut [InlineFrame<'_>],
    upcoming_boundary: UpcomingBoundary,
    current_text_has_more: bool,
) -> Result<()> {
    let first_pending = frames.iter().position(|frame| {
        matches!(
            frame.completion,
            Completion::Wrapped { started: false, .. } | Completion::Link { started: false, .. }
        )
    });
    let Some(first_pending) = first_pending else {
        return Ok(());
    };

    let leading_whitespace = frames[first_pending..]
        .iter()
        .any(|frame| frame.state.leading_whitespace);
    if leading_whitespace
        && first_pending > 0
        && !frames[first_pending - 1].state.trim_leading
        && !writer.is_line_start()
    {
        writer.request_space();
        frames[first_pending - 1].state.pending_space = true;
        frames[first_pending - 1]
            .state
            .wrapper_close_needs_punctuation = false;
        frames[first_pending - 1].state.last_closed_wrapper = None;
    }
    for frame in &mut frames[first_pending..] {
        frame.state.leading_whitespace = false;
    }

    if first_pending > 0 {
        let pending_syntax = frames[first_pending..]
            .iter()
            .filter(|frame| {
                matches!(
                    frame.completion,
                    Completion::Wrapped { started: false, .. }
                        | Completion::Link { started: false, .. }
                )
            })
            .count();
        let wrapper_is_first_syntax = matches!(
            frames[first_pending].completion,
            Completion::Wrapped { started: false, .. }
        );
        let upcoming_word_may_be_rewritten = upcoming_boundary == UpcomingBoundary::WordLike
            && !current_text_has_more
            && first_word_may_need_boundary_entity(&frames[first_pending..]);
        let wrapper_is_followed_by_punctuation = pending_syntax > 1
            || upcoming_boundary != UpcomingBoundary::WordLike
            || upcoming_word_may_be_rewritten;
        let preceding = &mut frames[first_pending - 1].state;
        preceding.wrapper_close_needs_punctuation = false;
        if wrapper_is_first_syntax && wrapper_is_followed_by_punctuation && !preceding.pending_space
        {
            if let EmittedBoundary::Word(ch) | EmittedBoundary::Unknown(ch) =
                preceding.emitted_boundary
            {
                writer.rewrite_last_scalar_as_numeric_entity(ch)?;
                preceding.emitted_boundary = EmittedBoundary::Punctuation;
            }
        }
    }

    for frame in &mut frames[first_pending..] {
        match &mut frame.completion {
            Completion::Wrapped {
                opening_boundary,
                marker,
                marker_chars,
                started,
                ..
            } if !*started => {
                writer.reserve(*marker_chars)?;
                if writer.is_line_start() {
                    *opening_boundary = "";
                }
                write_prefixed_literal(writer, line_prefix, opening_boundary)?;
                *opening_boundary = "";
                write_prefixed_literal(writer, line_prefix, marker)?;
                *started = true;
            }
            Completion::Link {
                closing_chars,
                started,
                ..
            } if !*started => {
                writer.reserve(*closing_chars)?;
                write_prefixed_literal(writer, line_prefix, "[")?;
                *started = true;
            }
            _ => {}
        }
    }
    Ok(())
}

fn propagate_empty_boundary(
    writer: &mut FinalWriter,
    child: InlineState,
    parent: Option<&mut InlineFrame<'_>>,
) {
    let Some(parent) = parent else {
        return;
    };
    if !child.leading_whitespace && !child.pending_space {
        return;
    }
    if parent.state.trim_leading {
        parent.state.leading_whitespace = true;
    } else {
        writer.request_space();
        parent.state.pending_space = !writer.is_line_start();
        parent.state.wrapper_close_needs_punctuation = false;
        parent.state.last_closed_wrapper = None;
    }
}

fn mark_syntax_content(state: &mut InlineState) {
    state.trim_leading = false;
    state.force_start = false;
    state.pending_space = false;
    state.leading_whitespace = false;
    state.line_phase = LineStartPhase::Content;
    state.after_hard_break = false;
    state.emitted_boundary = EmittedBoundary::Punctuation;
    state.wrapper_close_needs_punctuation = false;
    state.last_closed_wrapper = None;
}

fn mark_wrapper_close(
    state: &mut InlineState,
    inner_boundary: EmittedBoundary,
    family: FormatFamily,
    marker: &'static str,
) {
    mark_syntax_content(state);
    state.wrapper_close_needs_punctuation = marker.contains('_')
        || matches!(
            inner_boundary,
            EmittedBoundary::Punctuation | EmittedBoundary::Unknown(_)
        );
    state.last_closed_wrapper = Some(ClosedWrapper { family, marker });
}

fn mark_content_after_wrapper_break(writer: &FinalWriter, state: &mut InlineState) {
    if writer.is_line_start() {
        reset_after_break(state);
    } else {
        mark_syntax_content(state);
    }
}

fn write_inline_text(
    writer: &mut FinalWriter,
    line_prefix: &str,
    text: &str,
    frames: &mut Vec<InlineFrame<'_>>,
) -> Result<()> {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        #[cfg(test)]
        super::record_text_scalar_visit();
        if ch.is_whitespace() {
            let frame = frames.last_mut().expect("text frame exists");
            if frame.state.trim_leading {
                frame.state.leading_whitespace = true;
            } else {
                let mut encoded = [0; 4];
                writer.write_normalized_text(ch.encode_utf8(&mut encoded))?;
                frame.state.pending_space = !writer.is_line_start();
                frame.state.line_phase = LineStartPhase::Content;
                frame.state.wrapper_close_needs_punctuation = false;
                frame.state.last_closed_wrapper = None;
            }
            continue;
        }

        start_pending_frames(
            writer,
            line_prefix,
            frames,
            upcoming_scalar_boundary(ch),
            chars.peek().is_some(),
        )?;
        let frame = frames.last_mut().expect("text frame exists");
        write_escaped_scalar(writer, line_prefix, ch, frame.context, &mut frame.state)?;
        frame.wrote = true;
    }
    Ok(())
}

fn write_escaped_scalar(
    writer: &mut FinalWriter,
    line_prefix: &str,
    ch: char,
    _context: InlineContext,
    state: &mut InlineState,
) -> Result<()> {
    if state.force_start {
        state.line_phase = LineStartPhase::Start;
    }
    let phase = state.line_phase;
    if writer.is_line_start() && !line_prefix.is_empty() {
        writer.write_literal(line_prefix)?;
    }
    let escape = matches!(
        ch,
        '\\' | '*' | '_' | '[' | ']' | '`' | '~' | '|' | '<' | '>' | '&'
    ) || (phase == LineStartPhase::Start && matches!(ch, '#' | '-' | '+' | '!' | '='))
        || (phase == LineStartPhase::Digits && matches!(ch, '.' | ')'));
    let encode_boundary = state.wrapper_close_needs_punctuation
        && upcoming_scalar_boundary(ch) != UpcomingBoundary::Punctuation
        && !state.pending_space;
    state.wrapper_close_needs_punctuation = false;
    state.last_closed_wrapper = None;
    if escape {
        writer.write_char('\\')?;
    }
    let mut encoded = [0; 4];
    writer.write_normalized_text(ch.encode_utf8(&mut encoded))?;
    if encode_boundary {
        writer.rewrite_last_scalar_as_numeric_entity(ch)?;
    }
    state.trim_leading = false;
    state.force_start = false;
    state.pending_space = false;
    state.leading_whitespace = false;
    state.line_phase = match phase {
        LineStartPhase::Start | LineStartPhase::Digits if ch.is_ascii_digit() => {
            LineStartPhase::Digits
        }
        _ => LineStartPhase::Content,
    };
    state.emitted_boundary = if encode_boundary || escape || ch.is_ascii_punctuation() {
        EmittedBoundary::Punctuation
    } else if ch.is_ascii() || ch.is_alphanumeric() {
        EmittedBoundary::Word(ch)
    } else {
        EmittedBoundary::Unknown(ch)
    };
    Ok(())
}

fn upcoming_scalar_boundary(ch: char) -> UpcomingBoundary {
    if ch.is_ascii_punctuation() {
        UpcomingBoundary::Punctuation
    } else if ch.is_ascii() || ch.is_alphanumeric() {
        UpcomingBoundary::WordLike
    } else {
        UpcomingBoundary::Unknown
    }
}

pub(super) fn write_escaped_text(
    writer: &mut FinalWriter,
    line_prefix: &str,
    text: &str,
    context: InlineContext,
    force: bool,
    state: &mut InlineState,
) -> Result<()> {
    if force
        || state.force_start
        || writer.is_line_start()
        || matches!(context, InlineContext::LinkLabel | InlineContext::ImageAlt)
    {
        state.line_phase = LineStartPhase::Start;
    }
    for ch in text.chars() {
        #[cfg(test)]
        super::record_text_scalar_visit();
        if ch.is_whitespace() {
            if state.trim_leading {
                state.leading_whitespace = true;
            } else {
                let mut encoded = [0; 4];
                writer.write_normalized_text(ch.encode_utf8(&mut encoded))?;
                state.pending_space = !writer.is_line_start();
                state.line_phase = LineStartPhase::Content;
                state.wrapper_close_needs_punctuation = false;
                state.last_closed_wrapper = None;
            }
            continue;
        }
        write_escaped_scalar(writer, line_prefix, ch, context, state)?;
    }
    Ok(())
}

pub(super) fn text_has_visible_scalar(text: &str) -> bool {
    for ch in text.chars() {
        #[cfg(test)]
        super::record_text_scalar_visit();
        if !ch.is_whitespace() {
            return true;
        }
    }
    false
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

fn write_prefixed_literal(writer: &mut FinalWriter, line_prefix: &str, text: &str) -> Result<()> {
    for ch in text.chars() {
        write_prefixed_char(writer, line_prefix, ch)?;
    }
    Ok(())
}

fn write_prefixed_repeated(
    writer: &mut FinalWriter,
    line_prefix: &str,
    ch: char,
    count: usize,
) -> Result<()> {
    for _ in 0..count {
        write_prefixed_char(writer, line_prefix, ch)?;
    }
    Ok(())
}

fn write_prefixed_char(writer: &mut FinalWriter, line_prefix: &str, ch: char) -> Result<()> {
    if writer.is_line_start() && ch != '\n' && ch != '\r' && !line_prefix.is_empty() {
        writer.write_literal(line_prefix)?;
    }
    writer.write_char(ch)
}

fn write_repeated(writer: &mut FinalWriter, ch: char, count: usize) -> Result<()> {
    for _ in 0..count {
        writer.write_char(ch)?;
    }
    Ok(())
}
