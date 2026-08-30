use super::inline::{
    self, text_has_visible_scalar, write_escaped_text, InlineContext, InlineState,
};
use super::output::FinalWriter;
use super::root::is_excluded_element as skip;
use super::MarkdownOptions;
use crate::{Error, Result};
use ego_tree::NodeRef;
use scraper::{ElementRef, Node};

const MAX_ORDERED_MARKER: i64 = 999_999_999;

pub(super) fn render(
    parent: ElementRef<'_>,
    options: &MarkdownOptions,
    writer: &mut FinalWriter,
) -> Result<bool> {
    Renderer { options }.render_blocks(parent, &LineContext::root(), writer)
}

#[derive(Clone)]
struct LineContext {
    prefix: String,
    blank_prefix: String,
}

impl LineContext {
    fn root() -> Self {
        Self {
            prefix: String::new(),
            blank_prefix: String::new(),
        }
    }

    fn indented(&self, width: usize) -> Self {
        let indentation = " ".repeat(width);
        Self {
            prefix: format!("{}{indentation}", self.prefix),
            blank_prefix: format!("{}{indentation}", self.prefix),
        }
    }

    fn quoted(&self) -> Self {
        Self {
            prefix: format!("{}> ", self.prefix),
            blank_prefix: format!("{}>", self.prefix),
        }
    }
}

struct Renderer<'a> {
    options: &'a MarkdownOptions,
}

struct BlockFrame<'a> {
    next_child: Option<NodeRef<'a, Node>>,
    context: LineContext,
    have_block: bool,
    inline_open: bool,
    prelude_open: bool,
    prelude_has_inline_content: bool,
    inline_state: InlineState,
}

impl<'a> BlockFrame<'a> {
    fn new(parent: ElementRef<'a>, context: LineContext) -> Self {
        Self {
            next_child: parent.first_child(),
            context,
            have_block: false,
            inline_open: false,
            prelude_open: false,
            prelude_has_inline_content: false,
            inline_state: InlineState::new(false),
        }
    }

    fn with_inline_prelude(parent: ElementRef<'a>, context: LineContext) -> Self {
        Self {
            next_child: parent.first_child(),
            context,
            have_block: true,
            inline_open: true,
            prelude_open: true,
            prelude_has_inline_content: false,
            inline_state: InlineState::new(false),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ListMode {
    Unordered,
    Ordered,
    OrderedFallback,
}

struct ListFrame<'a> {
    next_child: Option<NodeRef<'a, Node>>,
    context: LineContext,
    mode: ListMode,
    next_number: Option<i64>,
    wrote_item: bool,
}

impl<'a> ListFrame<'a> {
    fn new(
        list: ElementRef<'a>,
        context: LineContext,
        options: &MarkdownOptions,
        available: usize,
    ) -> Result<Self> {
        let mode = list_mode(list, options, available)?;
        let next_number = if mode == ListMode::Unordered {
            None
        } else {
            Some(counter_attribute(list, "start")?.unwrap_or(1))
        };
        Ok(Self {
            next_child: list.first_child(),
            context,
            mode,
            next_number,
            wrote_item: false,
        })
    }

    fn counter_for(&mut self, item: ElementRef<'_>) -> Result<Option<i64>> {
        if self.mode == ListMode::Unordered {
            return Ok(None);
        }
        if let Some(value) = counter_attribute(item, "value")? {
            self.next_number = Some(value);
        }
        let value = self.next_number.ok_or_else(counter_overflow_error)?;
        self.next_number = value.checked_add(1);
        Ok(Some(value))
    }
}

struct DescriptionFrame<'a> {
    next_child: Option<NodeRef<'a, Node>>,
    context: LineContext,
    term: Option<ElementRef<'a>>,
    rendered_for_term: bool,
    rendered_any: bool,
}

impl<'a> DescriptionFrame<'a> {
    fn new(list: ElementRef<'a>, context: LineContext) -> Self {
        Self {
            next_child: list.first_child(),
            context,
            term: None,
            rendered_for_term: false,
            rendered_any: false,
        }
    }
}

enum WorkFrame<'a> {
    Blocks(BlockFrame<'a>),
    List(ListFrame<'a>),
    Description(DescriptionFrame<'a>),
}

enum BlockMetadataFrame<'a> {
    Children {
        next_child: Option<NodeRef<'a, Node>>,
    },
    List {
        next_child: Option<NodeRef<'a, Node>>,
    },
    Description {
        next_child: Option<NodeRef<'a, Node>>,
        have_term: bool,
    },
}

enum BlockMetadataStep<'a> {
    Content(StructuralContentKind),
    Empty,
    Frame(BlockMetadataFrame<'a>),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StructuralPurpose {
    Emission,
    PreferredRoot,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum StructuralContentKind {
    None,
    SyntaxOnly,
    FallbackText,
    VisibleText,
    MeaningfulImage,
}

impl StructuralContentKind {
    fn accepted(self, purpose: StructuralPurpose) -> bool {
        match purpose {
            StructuralPurpose::Emission => self >= Self::SyntaxOnly,
            StructuralPurpose::PreferredRoot => {
                matches!(self, Self::VisibleText | Self::MeaningfulImage)
            }
        }
    }
}

impl From<inline::InlineContentKind> for StructuralContentKind {
    fn from(kind: inline::InlineContentKind) -> Self {
        match kind {
            inline::InlineContentKind::Empty | inline::InlineContentKind::Whitespace => Self::None,
            inline::InlineContentKind::SyntaxOnly => Self::SyntaxOnly,
            inline::InlineContentKind::FallbackText => Self::FallbackText,
            inline::InlineContentKind::VisibleText => Self::VisibleText,
            inline::InlineContentKind::MeaningfulImage => Self::MeaningfulImage,
        }
    }
}

pub(super) fn structurally_renderable(
    element: ElementRef<'_>,
    options: &MarkdownOptions,
) -> Result<bool> {
    Ok(
        structural_content_kind(element, options, StructuralPurpose::Emission)?
            .accepted(StructuralPurpose::Emission),
    )
}

pub(super) fn preferred_root_is_meaningful(
    element: ElementRef<'_>,
    options: &MarkdownOptions,
) -> Result<bool> {
    Ok(
        structural_content_kind(element, options, StructuralPurpose::PreferredRoot)?
            .accepted(StructuralPurpose::PreferredRoot),
    )
}

fn structural_content_kind(
    element: ElementRef<'_>,
    options: &MarkdownOptions,
    purpose: StructuralPurpose,
) -> Result<StructuralContentKind> {
    if skip(&element) {
        return Ok(StructuralContentKind::None);
    }
    let mut frames = Vec::new();
    let mut best = StructuralContentKind::None;
    match structural_metadata_step(element, options, purpose)? {
        BlockMetadataStep::Content(kind) if kind.accepted(purpose) => return Ok(kind),
        BlockMetadataStep::Content(kind) => best = best.max(kind),
        BlockMetadataStep::Empty => return Ok(best),
        BlockMetadataStep::Frame(frame) => frames.push(frame),
    }

    while let Some(frame) = frames.last_mut() {
        match frame {
            BlockMetadataFrame::Children { next_child } => {
                let Some(node) = take_next_metadata_node(next_child) else {
                    frames.pop();
                    continue;
                };
                if let Some(text) = node.value().as_text() {
                    if text_has_visible_scalar(text) {
                        return Ok(StructuralContentKind::VisibleText);
                    }
                    continue;
                }
                let Some(child) = ElementRef::wrap(node) else {
                    continue;
                };
                if skip(&child) {
                    continue;
                }
                if !is_block(child.value().name()) {
                    let kind = StructuralContentKind::from(inline::inline_content_kind_for(
                        child,
                        options,
                        inline_metadata_purpose(purpose),
                    )?);
                    if kind.accepted(purpose) {
                        return Ok(kind);
                    }
                    best = best.max(kind);
                    continue;
                }
                match structural_metadata_step(child, options, purpose)? {
                    BlockMetadataStep::Content(kind) if kind.accepted(purpose) => return Ok(kind),
                    BlockMetadataStep::Content(kind) => best = best.max(kind),
                    BlockMetadataStep::Empty => {}
                    BlockMetadataStep::Frame(frame) => frames.push(frame),
                }
            }
            BlockMetadataFrame::List { next_child } => {
                let Some(node) = take_next_metadata_node(next_child) else {
                    frames.pop();
                    continue;
                };
                let Some(item) = ElementRef::wrap(node) else {
                    continue;
                };
                if item.value().name() == "li" && !skip(&item) {
                    frames.push(BlockMetadataFrame::Children {
                        next_child: item.first_child(),
                    });
                }
            }
            BlockMetadataFrame::Description {
                next_child,
                have_term,
            } => {
                let Some(node) = take_next_metadata_node(next_child) else {
                    frames.pop();
                    continue;
                };
                let Some(child) = ElementRef::wrap(node) else {
                    continue;
                };
                if skip(&child) {
                    if child.value().name() == "dt" {
                        *have_term = false;
                    }
                    continue;
                }
                match child.value().name() {
                    "dt" => *have_term = true,
                    "dd" if *have_term => {
                        frames.push(BlockMetadataFrame::Children {
                            next_child: child.first_child(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(best)
}

fn structural_metadata_step<'a>(
    element: ElementRef<'a>,
    options: &MarkdownOptions,
    purpose: StructuralPurpose,
) -> Result<BlockMetadataStep<'a>> {
    let step = match element.value().name() {
        "pre" if purpose == StructuralPurpose::PreferredRoot => {
            BlockMetadataStep::Content(pre_content_kind(element))
        }
        "pre" | "hr" => BlockMetadataStep::Content(StructuralContentKind::SyntaxOnly),
        "table" if purpose == StructuralPurpose::PreferredRoot => {
            BlockMetadataStep::Content(table_preferred_content_kind(element, options)?)
        }
        "table" if table_has_owned_cell(element) => {
            BlockMetadataStep::Content(StructuralContentKind::SyntaxOnly)
        }
        "table" => BlockMetadataStep::Empty,
        "ul" | "ol" => BlockMetadataStep::Frame(BlockMetadataFrame::List {
            next_child: element.first_child(),
        }),
        "dl" => BlockMetadataStep::Frame(BlockMetadataFrame::Description {
            next_child: element.first_child(),
            have_term: false,
        }),
        name if is_transparent_block(name) || matches!(name, "blockquote" | "li" | "dd") => {
            BlockMetadataStep::Frame(BlockMetadataFrame::Children {
                next_child: element.first_child(),
            })
        }
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let kind = StructuralContentKind::from(inline::inline_content_kind_for(
                element,
                options,
                inline_metadata_purpose(purpose),
            )?);
            if kind == StructuralContentKind::None {
                BlockMetadataStep::Empty
            } else {
                BlockMetadataStep::Content(kind)
            }
        }
        _ => BlockMetadataStep::Empty,
    };
    Ok(step)
}

fn pre_content_kind(pre: ElementRef<'_>) -> StructuralContentKind {
    let mut pending = pre.first_child().into_iter().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Some(sibling) = node.next_sibling() {
            pending.push(sibling);
        }
        if let Some(text) = node.value().as_text() {
            if text.chars().any(|ch| !ch.is_whitespace()) {
                return StructuralContentKind::VisibleText;
            }
        } else if let Some(element) = ElementRef::wrap(node) {
            if !skip(&element) {
                if let Some(child) = element.first_child() {
                    pending.push(child);
                }
            }
        }
    }
    StructuralContentKind::SyntaxOnly
}

fn table_preferred_content_kind(
    table: ElementRef<'_>,
    options: &MarkdownOptions,
) -> Result<StructuralContentKind> {
    let mut kind = StructuralContentKind::None;
    let mut have_cell = false;
    let mut rows = OwnedRowCursor::new(table);
    while let Some(row) = rows.next() {
        for cell in table_cells(row.row) {
            #[cfg(test)]
            super::record_table_metadata_cell_visit();
            have_cell = true;
            let cell_kind = StructuralContentKind::from(inline::inline_content_kind_for(
                cell,
                options,
                inline::InlineMetadataPurpose::PreferredRoot,
            )?);
            kind = kind.max(cell_kind);
            if kind.accepted(StructuralPurpose::PreferredRoot) {
                return Ok(kind);
            }
        }
    }
    if have_cell {
        Ok(kind.max(StructuralContentKind::SyntaxOnly))
    } else {
        Ok(StructuralContentKind::None)
    }
}

fn inline_metadata_purpose(purpose: StructuralPurpose) -> inline::InlineMetadataPurpose {
    match purpose {
        StructuralPurpose::Emission => inline::InlineMetadataPurpose::Emission,
        StructuralPurpose::PreferredRoot => inline::InlineMetadataPurpose::PreferredRoot,
    }
}

fn take_next_node<'a>(next_child: &mut Option<NodeRef<'a, Node>>) -> Option<NodeRef<'a, Node>> {
    let node = *next_child;
    if let Some(node) = node {
        *next_child = node.next_sibling();
        #[cfg(test)]
        super::record_block_cursor_advances(1);
    }
    node
}

fn take_next_metadata_node<'a>(
    next_child: &mut Option<NodeRef<'a, Node>>,
) -> Option<NodeRef<'a, Node>> {
    let node = *next_child;
    if let Some(node) = node {
        *next_child = node.next_sibling();
    }
    node
}

impl Renderer<'_> {
    fn render_blocks(
        &self,
        parent: ElementRef<'_>,
        context: &LineContext,
        writer: &mut FinalWriter,
    ) -> Result<bool> {
        let mut frames = vec![WorkFrame::Blocks(BlockFrame::new(parent, context.clone()))];
        loop {
            let exhausted = match frames.last().expect("root work frame remains active") {
                WorkFrame::Blocks(frame) => frame.next_child.is_none(),
                WorkFrame::List(frame) => frame.next_child.is_none(),
                WorkFrame::Description(frame) => frame.next_child.is_none(),
            };
            if exhausted {
                let completed = frames.pop().expect("completed work frame exists");
                if let WorkFrame::Blocks(completed) = completed {
                    writer.discard_pending_space();
                    if let Some(WorkFrame::Blocks(parent_frame)) = frames.last_mut() {
                        if completed.have_block {
                            parent_frame.have_block = true;
                            parent_frame.inline_open = false;
                            parent_frame.inline_state = InlineState::new(false);
                        }
                    } else if frames.is_empty() {
                        return Ok(completed.have_block);
                    }
                }
                continue;
            }

            match frames.last_mut().expect("active work frame exists") {
                WorkFrame::Blocks(frame) => {
                    let node = take_next_node(&mut frame.next_child)
                        .expect("non-exhausted block frame has a child");
                    if let Some(text) = node.value().as_text() {
                        if !text_has_visible_scalar(text) {
                            if frame.inline_open {
                                writer.request_space();
                                frame.inline_state.pending_space = !writer.is_line_start();
                            }
                            continue;
                        }
                        if frame.have_block && !frame.inline_open {
                            contextual_blank_line(writer, &frame.context)?;
                            frame.inline_state = InlineState::new(false);
                        }
                        write_escaped_text(
                            writer,
                            &frame.context.prefix,
                            text,
                            InlineContext::Normal,
                            false,
                            &mut frame.inline_state,
                        )?;
                        frame.have_block = true;
                        frame.inline_open = true;
                        if frame.prelude_open {
                            frame.prelude_has_inline_content = true;
                        }
                        continue;
                    }

                    let Some(element) = ElementRef::wrap(node) else {
                        continue;
                    };
                    if skip(&element) {
                        continue;
                    }

                    if is_block(element.value().name()) {
                        let table = if element.value().name() == "table" {
                            table_metadata(element, writer.remaining(), self.options.max_chars)?
                        } else {
                            None
                        };
                        if element.value().name() == "table" && table.is_none() {
                            continue;
                        }
                        if table.is_none() && !self.block_has_output(element)? {
                            continue;
                        }
                        let paragraph_joins_prelude = frame.prelude_open
                            && !frame.prelude_has_inline_content
                            && element.value().name() == "p";
                        if frame.prelude_open && !paragraph_joins_prelude {
                            writer.discard_pending_space();
                            contextual_blank_line(writer, &frame.context)?;
                            frame.have_block = false;
                            frame.inline_open = false;
                            frame.prelude_open = false;
                            frame.prelude_has_inline_content = false;
                            frame.inline_state = InlineState::new(false);
                        }
                        if frame.have_block && !paragraph_joins_prelude {
                            if matches!(element.value().name(), "ul" | "ol") && frame.inline_open {
                                writer.discard_pending_space();
                                writer.newline()?;
                            } else {
                                contextual_blank_line(writer, &frame.context)?;
                            }
                        }

                        let nested = match element.value().name() {
                            name if is_transparent_block(name) => Some(WorkFrame::Blocks(
                                BlockFrame::new(element, frame.context.clone()),
                            )),
                            "blockquote" => {
                                if !writer.is_line_start() {
                                    writer.write_literal("> ")?;
                                }
                                Some(WorkFrame::Blocks(BlockFrame::new(
                                    element,
                                    frame.context.quoted(),
                                )))
                            }
                            "ul" | "ol" => Some(WorkFrame::List(ListFrame::new(
                                element,
                                frame.context.clone(),
                                self.options,
                                writer.remaining(),
                            )?)),
                            "dl" => Some(WorkFrame::Description(DescriptionFrame::new(
                                element,
                                frame.context.clone(),
                            ))),
                            _ => None,
                        };
                        if let Some(table) = table {
                            self.render_table(table, &frame.context, writer)?;
                        } else if nested.is_none() {
                            self.render_block(element, &frame.context, writer)?;
                        }
                        if !is_transparent_block(element.value().name()) {
                            writer.discard_pending_space();
                            frame.have_block = true;
                            frame.inline_open = false;
                            frame.prelude_open = false;
                            frame.prelude_has_inline_content = false;
                            frame.inline_state = InlineState::new(false);
                        }
                        if let Some(nested) = nested {
                            frames.push(nested);
                        }
                    } else {
                        if frame.have_block && !frame.inline_open {
                            contextual_blank_line(writer, &frame.context)?;
                            frame.inline_state = InlineState::new(false);
                        }
                        if self.render_inline_element(
                            element,
                            &frame.context,
                            writer,
                            &mut frame.inline_state,
                        )? {
                            frame.have_block = true;
                            frame.inline_open = true;
                            if frame.prelude_open {
                                frame.prelude_has_inline_content = true;
                            }
                        }
                    }
                }
                WorkFrame::List(frame) => {
                    let node = take_next_node(&mut frame.next_child)
                        .expect("non-exhausted list frame has a child");
                    let Some(item) = ElementRef::wrap(node) else {
                        continue;
                    };
                    if item.value().name() != "li" || skip(&item) {
                        continue;
                    }
                    let counter = frame.counter_for(item)?;
                    if !self.block_has_output(item)? {
                        continue;
                    }
                    if frame.wrote_item {
                        writer.discard_pending_space();
                        writer.newline()?;
                    }
                    let continuation_width = match frame.mode {
                        ListMode::Unordered => {
                            write_prefixed_literal(writer, &frame.context, "- ")?;
                            4
                        }
                        ListMode::Ordered => {
                            let marker = format!(
                                "{}. ",
                                counter.expect("ordered list frames always have counters")
                            );
                            write_prefixed_literal(writer, &frame.context, &marker)?;
                            marker.chars().count()
                        }
                        ListMode::OrderedFallback => {
                            write_prefixed_literal(writer, &frame.context, "- ")?;
                            write_visible_counter(
                                writer,
                                counter.expect("fallback list frames always have counters"),
                            )?;
                            writer.request_space();
                            4
                        }
                    };
                    let continuation = frame.context.indented(continuation_width);
                    frame.wrote_item = true;
                    let item_frame = if frame.mode == ListMode::OrderedFallback {
                        BlockFrame::with_inline_prelude(item, continuation)
                    } else {
                        BlockFrame::new(item, continuation)
                    };
                    frames.push(WorkFrame::Blocks(item_frame));
                }
                WorkFrame::Description(frame) => {
                    let node = take_next_node(&mut frame.next_child)
                        .expect("non-exhausted description frame has a child");
                    let Some(child) = ElementRef::wrap(node) else {
                        continue;
                    };
                    if skip(&child) {
                        if child.value().name() == "dt" {
                            frame.term = None;
                            frame.rendered_for_term = false;
                        }
                        continue;
                    }
                    match child.value().name() {
                        "dt" => {
                            frame.term = Some(child);
                            frame.rendered_for_term = false;
                        }
                        "dd" if frame.term.is_some() && self.block_has_output(child)? => {
                            if !frame.rendered_for_term {
                                if frame.rendered_any {
                                    contextual_blank_line(writer, &frame.context)?;
                                }
                                self.render_inline_children(
                                    frame.term.expect("description term checked above"),
                                    &frame.context,
                                    writer,
                                    &mut InlineState::new(false),
                                )?;
                                writer.discard_pending_space();
                                writer.newline()?;
                            } else {
                                contextual_blank_line(writer, &frame.context)?;
                            }
                            write_prefixed_literal(writer, &frame.context, ": ")?;
                            let continuation = frame.context.indented(2);
                            frame.rendered_for_term = true;
                            frame.rendered_any = true;
                            frames.push(WorkFrame::Blocks(BlockFrame::new(child, continuation)));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn render_block(
        &self,
        element: ElementRef<'_>,
        context: &LineContext,
        writer: &mut FinalWriter,
    ) -> Result<()> {
        match element.value().name() {
            "p" => {
                self.render_inline_children(
                    element,
                    context,
                    writer,
                    &mut InlineState::new(false),
                )?;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(element.value().name().as_bytes()[1] - b'0');
                write_prefixed_repeated(writer, context, '#', level)?;
                writer.write_char(' ')?;
                self.render_inline_children(
                    element,
                    context,
                    writer,
                    &mut InlineState::new(false),
                )?;
            }
            "pre" => self.render_fence(element, context, writer)?,
            "hr" => write_prefixed_literal(writer, context, "---")?,
            _ => {}
        }
        Ok(())
    }

    fn render_inline_children(
        &self,
        parent: ElementRef<'_>,
        context: &LineContext,
        writer: &mut FinalWriter,
        state: &mut InlineState,
    ) -> Result<bool> {
        let inline_context = if state.table {
            InlineContext::TableCell
        } else {
            InlineContext::Normal
        };
        if context.prefix.is_empty() && state.trim_leading && !state.pending_space {
            inline::render_inline_children(parent, writer, self.options, inline_context, 0)?;
            state.trim_leading = false;
            return Ok(true);
        }
        inline::render_inline_children_with_state(
            parent,
            writer,
            self.options,
            inline_context,
            0,
            &context.prefix,
            state,
        )
    }

    fn render_inline_element(
        &self,
        element: ElementRef<'_>,
        context: &LineContext,
        writer: &mut FinalWriter,
        state: &mut InlineState,
    ) -> Result<bool> {
        let inline_context = if state.table {
            InlineContext::TableCell
        } else {
            InlineContext::Normal
        };
        inline::render_inline_element_with_state(
            element,
            writer,
            self.options,
            inline_context,
            0,
            &context.prefix,
            state,
        )
    }

    fn render_table(
        &self,
        metadata: TableMetadata<'_>,
        context: &LineContext,
        writer: &mut FinalWriter,
    ) -> Result<()> {
        if let Some(header_index) = metadata.header_index {
            self.render_table_row(
                metadata.rows[header_index].row,
                metadata.width,
                context,
                writer,
            )?;
            writer.newline()?;
            render_table_delimiter(&metadata.alignments, context, writer)?;
            for (index, row) in metadata.rows.iter().enumerate() {
                if index == header_index {
                    continue;
                }
                writer.newline()?;
                self.render_table_row(row.row, metadata.width, context, writer)?;
            }
        } else {
            render_empty_table_row(metadata.width, context, writer)?;
            writer.newline()?;
            render_table_delimiter(&metadata.alignments, context, writer)?;
            for row in metadata.rows {
                writer.newline()?;
                self.render_table_row(row.row, metadata.width, context, writer)?;
            }
        }
        Ok(())
    }

    fn render_table_row(
        &self,
        row: ElementRef<'_>,
        width: usize,
        context: &LineContext,
        writer: &mut FinalWriter,
    ) -> Result<()> {
        write_prefixed_literal(writer, context, "| ")?;
        let mut cells = 0usize;
        for cell in table_cells(row) {
            if cells > 0 {
                writer.write_literal(" | ")?;
            }
            if !skip(&cell) {
                self.render_inline_children(cell, context, writer, &mut InlineState::new(true))?;
            }
            writer.discard_pending_space();
            cells += 1;
        }
        while cells < width {
            if cells > 0 {
                writer.write_literal(" | ")?;
            }
            cells += 1;
        }
        writer.write_literal(" |")
    }

    fn render_fence(
        &self,
        pre: ElementRef<'_>,
        context: &LineContext,
        writer: &mut FinalWriter,
    ) -> Result<()> {
        let code = pre
            .child_elements()
            .find(|element| element.value().name() == "code" && !skip(element));
        let language = code.and_then(code_language);
        let scan = scan_fence(
            pre,
            language,
            context,
            writer.is_line_start(),
            writer.remaining(),
            self.options.max_chars,
        )?;

        writer.reserve(scan.delimiter)?;
        write_prefixed_repeated(writer, context, '`', scan.delimiter)?;
        if let Some(language) = language {
            writer.write_char(' ')?;
            writer.write_literal(language)?;
        }
        writer.write_char('\n')?;

        let mut saw_raw = false;
        let mut raw_ends_with_newline = false;
        for_each_fence_char(pre, |ch| {
            saw_raw = true;
            raw_ends_with_newline = ch == '\n';
            write_raw_fence_char(writer, context, ch)
        })?;
        if !saw_raw || !raw_ends_with_newline {
            write_raw_fence_char(writer, context, '\n')?;
        }
        writer.release(scan.delimiter);
        write_prefixed_repeated(writer, context, '`', scan.delimiter)
    }

    fn block_has_output(&self, element: ElementRef<'_>) -> Result<bool> {
        structurally_renderable(element, self.options)
    }
}

fn list_mode(
    list: ElementRef<'_>,
    options: &MarkdownOptions,
    available: usize,
) -> Result<ListMode> {
    if list.value().name() != "ol" {
        return Ok(ListMode::Unordered);
    }
    if available < 3 {
        return Err(Error::BodyLimit {
            limit: options.max_chars,
        });
    }
    let mut mode = ListMode::Ordered;
    let mut next_number = Some(counter_attribute(list, "start")?.unwrap_or(1));
    let mut renderable_items = 0usize;
    for node in list.children() {
        let Some(item) = ElementRef::wrap(node) else {
            continue;
        };
        if item.value().name() != "li" || skip(&item) {
            continue;
        }
        #[cfg(test)]
        super::record_list_metadata_item_visit();
        if let Some(value) = counter_attribute(item, "value")? {
            next_number = Some(value);
        }
        let value = next_number.ok_or_else(counter_overflow_error)?;
        if !(0..=MAX_ORDERED_MARKER).contains(&value) {
            mode = ListMode::OrderedFallback;
        }
        next_number = value.checked_add(1);
        if structurally_renderable(item, options)? {
            renderable_items += 1;
            let minimum_output = renderable_items.saturating_mul(4).saturating_sub(1);
            if minimum_output > available {
                return Err(Error::BodyLimit {
                    limit: options.max_chars,
                });
            }
        }
    }
    Ok(mode)
}

fn counter_attribute(element: ElementRef<'_>, attribute: &str) -> Result<Option<i64>> {
    let Some(raw) = element.value().attr(attribute) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    match trimmed.parse::<i64>() {
        Ok(value) => Ok(Some(value)),
        Err(_) if is_decimal_integer(trimmed) => Err(Error::Parse {
            kind: "html",
            message: "ordered-list counter is outside the supported decimal range".to_owned(),
        }),
        Err(_) => Ok(None),
    }
}

fn is_decimal_integer(value: &str) -> bool {
    let digits = value
        .strip_prefix('-')
        .or_else(|| value.strip_prefix('+'))
        .unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn counter_overflow_error() -> Error {
    Error::Parse {
        kind: "html",
        message: "ordered-list counter overflowed its checked decimal state".to_owned(),
    }
}

fn write_visible_counter(writer: &mut FinalWriter, counter: i64) -> Result<()> {
    writer.write_literal(&counter.to_string())?;
    if (0..=MAX_ORDERED_MARKER).contains(&counter) {
        writer.write_literal("\\.")
    } else {
        writer.write_char('.')
    }
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "pre"
            | "blockquote"
            | "table"
            | "dl"
            | "hr"
            | "div"
            | "section"
            | "article"
            | "main"
            | "body"
            | "figure"
            | "figcaption"
    )
}

fn is_transparent_block(name: &str) -> bool {
    matches!(
        name,
        "div" | "section" | "article" | "main" | "body" | "figure" | "figcaption"
    )
}

fn contextual_blank_line(writer: &mut FinalWriter, context: &LineContext) -> Result<()> {
    writer.discard_pending_space();
    if context.blank_prefix.is_empty() {
        return writer.blank_line();
    }
    writer.newline()?;
    writer.write_literal(&context.blank_prefix)?;
    writer.write_char('\n')
}

fn write_prefixed_literal(
    writer: &mut FinalWriter,
    context: &LineContext,
    text: &str,
) -> Result<()> {
    for ch in text.chars() {
        write_prefixed_char(writer, context, ch)?;
    }
    Ok(())
}

fn write_prefixed_repeated(
    writer: &mut FinalWriter,
    context: &LineContext,
    ch: char,
    count: usize,
) -> Result<()> {
    for _ in 0..count {
        write_prefixed_char(writer, context, ch)?;
    }
    Ok(())
}

fn write_prefixed_char(writer: &mut FinalWriter, context: &LineContext, ch: char) -> Result<()> {
    if writer.is_line_start() && ch != '\n' && ch != '\r' && !context.prefix.is_empty() {
        writer.write_literal(&context.prefix)?;
    }
    writer.write_char(ch)
}

fn write_raw_fence_char(writer: &mut FinalWriter, context: &LineContext, ch: char) -> Result<()> {
    if ch == '\n' && writer.is_line_start() && !context.blank_prefix.is_empty() {
        writer.write_literal(&context.blank_prefix)?;
    }
    write_prefixed_char(writer, context, ch)
}

struct FenceScan {
    delimiter: usize,
}

fn scan_fence(
    source: ElementRef<'_>,
    language: Option<&str>,
    context: &LineContext,
    opening_at_line_start: bool,
    available: usize,
    limit: usize,
) -> Result<FenceScan> {
    let language_chars = language.map_or(0, |value| value.chars().count() + 1);
    let prefix_chars = context.prefix.chars().count();
    let blank_prefix_chars = context.blank_prefix.chars().count();
    let opening_prefix_chars = if opening_at_line_start {
        prefix_chars
    } else {
        0
    };
    let mut raw_output_chars = 0usize;
    let mut raw_line_start = true;
    let mut saw_raw = false;
    let mut raw_ends_with_newline = false;
    let mut longest_ticks = 0usize;
    let mut current_ticks = 0usize;

    for_each_fence_char(source, |ch| {
        saw_raw = true;
        raw_ends_with_newline = ch == '\n';
        if ch == '\n' {
            if raw_line_start {
                raw_output_chars += blank_prefix_chars;
            }
            raw_output_chars += 1;
            raw_line_start = true;
            current_ticks = 0;
        } else {
            if raw_line_start {
                raw_output_chars += prefix_chars;
            }
            raw_output_chars += 1;
            raw_line_start = false;
            if ch == '`' {
                current_ticks += 1;
                longest_ticks = longest_ticks.max(current_ticks);
            } else {
                current_ticks = 0;
            }
        }
        let delimiter = (longest_ticks + 1).max(3);
        let lower_bound = opening_prefix_chars
            + delimiter * 2
            + language_chars
            + 1
            + raw_output_chars
            + prefix_chars;
        if lower_bound > available {
            return Err(Error::BodyLimit { limit });
        }
        Ok(())
    })?;
    let delimiter = (longest_ticks + 1).max(3);
    let synthetic_newline_chars = if !saw_raw || !raw_ends_with_newline {
        usize::from(raw_line_start) * blank_prefix_chars + 1
    } else {
        0
    };
    let exact_output_chars = opening_prefix_chars
        + delimiter * 2
        + language_chars
        + 1
        + raw_output_chars
        + synthetic_newline_chars
        + prefix_chars;
    if exact_output_chars > available {
        return Err(Error::BodyLimit { limit });
    }
    Ok(FenceScan { delimiter })
}

fn for_each_fence_char(
    source: ElementRef<'_>,
    mut visitor: impl FnMut(char) -> Result<()>,
) -> Result<()> {
    let mut frames = vec![source.first_child()];
    while let Some(next_child) = frames.last_mut() {
        let Some(node) = take_next_metadata_node(next_child) else {
            frames.pop();
            continue;
        };
        if let Some(text) = node.value().as_text() {
            for ch in text.chars() {
                #[cfg(test)]
                super::record_text_scalar_visit();
                visitor(ch)?;
            }
            continue;
        }
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };
        if !skip(&element) {
            frames.push(element.first_child());
        }
    }
    Ok(())
}

fn code_language(code: ElementRef<'_>) -> Option<&str> {
    code.value().attr("class").and_then(|classes| {
        classes.split_ascii_whitespace().find_map(|class| {
            class.strip_prefix("language-").filter(|language| {
                !language.is_empty()
                    && language.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'_' | b'.')
                    })
            })
        })
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RowSection {
    Direct,
    Head,
    Body,
    Foot,
}

#[derive(Clone, Copy)]
struct RowMetadata<'a> {
    row: ElementRef<'a>,
    section: RowSection,
}

struct OwnedRowCursor<'a> {
    next_table_child: Option<NodeRef<'a, Node>>,
    next_section_child: Option<NodeRef<'a, Node>>,
    section: RowSection,
}

impl<'a> OwnedRowCursor<'a> {
    fn new(table: ElementRef<'a>) -> Self {
        Self {
            next_table_child: table.first_child(),
            next_section_child: None,
            section: RowSection::Direct,
        }
    }

    fn next(&mut self) -> Option<RowMetadata<'a>> {
        loop {
            if let Some(node) = take_next_metadata_node(&mut self.next_section_child) {
                let Some(row) = ElementRef::wrap(node) else {
                    continue;
                };
                if row.value().name() != "tr" {
                    continue;
                }
                #[cfg(test)]
                super::record_table_metadata_row_visit();
                if !skip(&row) {
                    return Some(RowMetadata {
                        row,
                        section: self.section,
                    });
                }
                continue;
            }

            let node = take_next_metadata_node(&mut self.next_table_child)?;
            let Some(child) = ElementRef::wrap(node) else {
                continue;
            };
            match child.value().name() {
                "tr" => {
                    #[cfg(test)]
                    super::record_table_metadata_row_visit();
                    if !skip(&child) {
                        return Some(RowMetadata {
                            row: child,
                            section: RowSection::Direct,
                        });
                    }
                }
                "thead" | "tbody" | "tfoot" if !skip(&child) => {
                    self.section = match child.value().name() {
                        "thead" => RowSection::Head,
                        "tbody" => RowSection::Body,
                        "tfoot" => RowSection::Foot,
                        _ => unreachable!("matched table section"),
                    };
                    self.next_section_child = child.first_child();
                }
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TableAlignment {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Default)]
struct AlignmentState {
    value: Option<TableAlignment>,
    conflicted: bool,
}

impl AlignmentState {
    fn observe(&mut self, value: TableAlignment) {
        if self.conflicted {
            return;
        }
        match self.value {
            None => self.value = Some(value),
            Some(existing) if existing == value => {}
            Some(_) => {
                self.value = None;
                self.conflicted = true;
            }
        }
    }

    fn resolved(self) -> Option<TableAlignment> {
        (!self.conflicted).then_some(self.value).flatten()
    }
}

struct TableMetadata<'a> {
    rows: Vec<RowMetadata<'a>>,
    width: usize,
    header_index: Option<usize>,
    alignments: Vec<Option<TableAlignment>>,
}

fn table_metadata(
    table: ElementRef<'_>,
    available: usize,
    limit: usize,
) -> Result<Option<TableMetadata<'_>>> {
    let mut rows = Vec::new();
    let mut width = 0usize;
    let mut minimum_row_chars = 0usize;
    let mut owned_rows = OwnedRowCursor::new(table);
    while let Some(row) = owned_rows.next() {
        let mut cells = 0usize;
        for _ in table_cells(row.row) {
            #[cfg(test)]
            super::record_table_metadata_cell_visit();
            cells += 1;
            let candidate_width = width.max(cells);
            let candidate_row_chars =
                minimum_row_chars.saturating_add(cells.saturating_mul(3).saturating_add(1));
            if minimum_table_output(candidate_row_chars, rows.len() + 1, candidate_width)
                > available
            {
                return Err(Error::BodyLimit { limit });
            }
        }
        if cells == 0 {
            continue;
        }
        width = width.max(cells);
        minimum_row_chars =
            minimum_row_chars.saturating_add(cells.saturating_mul(3).saturating_add(1));
        if minimum_table_output(minimum_row_chars, rows.len() + 1, width) > available {
            return Err(Error::BodyLimit { limit });
        }
        rows.push(RowMetadata {
            row: row.row,
            section: row.section,
        });
        #[cfg(test)]
        super::record_table_metadata_row_size(rows.capacity());
    }
    if rows.is_empty() {
        return Ok(None);
    }

    let header_index = rows
        .iter()
        .position(|row| row.section == RowSection::Head)
        .or_else(|| {
            rows.first()
                .filter(|row| row_has_column_headers(row.row))
                .map(|_| 0)
        });

    let mut states = vec![AlignmentState::default(); width];
    #[cfg(test)]
    super::record_table_alignment_state_size(states.capacity());
    collect_column_alignments(table, &mut states);
    for row in &rows {
        collect_row_alignments(row.row, &mut states);
    }
    let alignments = states.into_iter().map(AlignmentState::resolved).collect();
    Ok(Some(TableMetadata {
        rows,
        width,
        header_index,
        alignments,
    }))
}

fn minimum_table_output(row_chars: usize, row_count: usize, width: usize) -> usize {
    let delimiter_chars = width.saturating_mul(6).saturating_add(1);
    row_chars
        .saturating_add(delimiter_chars)
        .saturating_add(row_count)
}

fn table_has_owned_cell(table: ElementRef<'_>) -> bool {
    let mut rows = OwnedRowCursor::new(table);
    while let Some(row) = rows.next() {
        if table_cells(row.row).next().is_some() {
            #[cfg(test)]
            super::record_table_metadata_cell_visit();
            return true;
        }
    }
    false
}

fn table_cells(row: ElementRef<'_>) -> impl Iterator<Item = ElementRef<'_>> {
    row.child_elements()
        .filter(|cell| matches!(cell.value().name(), "th" | "td"))
}

fn row_has_column_headers(row: ElementRef<'_>) -> bool {
    let mut have_column_header = false;
    for cell in table_cells(row) {
        #[cfg(test)]
        super::record_table_metadata_cell_visit();
        if cell.value().name() != "th" || skip(&cell) {
            continue;
        }
        if cell
            .value()
            .attr("scope")
            .is_some_and(|scope| scope.trim().eq_ignore_ascii_case("row"))
        {
            return false;
        }
        have_column_header = true;
    }
    have_column_header
}

fn collect_column_alignments(table: ElementRef<'_>, states: &mut [AlignmentState]) {
    let mut column = 0usize;
    for child in table.child_elements() {
        if column >= states.len() {
            break;
        }
        match child.value().name() {
            "col" => {
                let span = column_span(child, states.len() - column);
                if !skip(&child) {
                    observe_range(states, column, span, child.value().attr("align"));
                }
                column += span;
            }
            "colgroup" => {
                let group_hidden = skip(&child);
                let group_alignment = (!group_hidden)
                    .then(|| child.value().attr("align"))
                    .flatten();
                let mut have_column = false;
                for col in child
                    .child_elements()
                    .filter(|element| element.value().name() == "col")
                {
                    if column >= states.len() {
                        break;
                    }
                    have_column = true;
                    let span = column_span(col, states.len() - column);
                    observe_range(states, column, span, group_alignment);
                    if !group_hidden && !skip(&col) {
                        observe_range(states, column, span, col.value().attr("align"));
                    }
                    column += span;
                }
                if !have_column && column < states.len() {
                    let span = column_span(child, states.len() - column);
                    observe_range(states, column, span, group_alignment);
                    column += span;
                }
            }
            _ => {}
        }
    }
}

fn collect_row_alignments(row: ElementRef<'_>, states: &mut [AlignmentState]) {
    let row_alignment = row.value().attr("align");
    for (column, cell) in table_cells(row).enumerate() {
        #[cfg(test)]
        super::record_table_metadata_cell_visit();
        if column >= states.len() {
            break;
        }
        if skip(&cell) {
            continue;
        }
        observe_range(states, column, 1, row_alignment);
        observe_range(states, column, 1, cell.value().attr("align"));
    }
}

fn column_span(element: ElementRef<'_>, remaining: usize) -> usize {
    element
        .value()
        .attr("span")
        .and_then(|span| span.parse::<usize>().ok())
        .filter(|span| *span > 0)
        .unwrap_or(1)
        .min(remaining)
}

fn observe_range(states: &mut [AlignmentState], start: usize, span: usize, raw: Option<&str>) {
    let Some(alignment) = raw.and_then(parse_alignment) else {
        return;
    };
    for state in states.iter_mut().skip(start).take(span) {
        state.observe(alignment);
    }
}

fn parse_alignment(raw: &str) -> Option<TableAlignment> {
    let value = raw.trim();
    if value.eq_ignore_ascii_case("left") {
        Some(TableAlignment::Left)
    } else if value.eq_ignore_ascii_case("center") {
        Some(TableAlignment::Center)
    } else if value.eq_ignore_ascii_case("right") {
        Some(TableAlignment::Right)
    } else {
        None
    }
}

fn render_empty_table_row(
    width: usize,
    context: &LineContext,
    writer: &mut FinalWriter,
) -> Result<()> {
    write_prefixed_literal(writer, context, "| ")?;
    for column in 0..width {
        if column > 0 {
            writer.write_literal(" | ")?;
        }
    }
    writer.write_literal(" |")
}

fn render_table_delimiter(
    alignments: &[Option<TableAlignment>],
    context: &LineContext,
    writer: &mut FinalWriter,
) -> Result<()> {
    write_prefixed_literal(writer, context, "| ")?;
    for (column, alignment) in alignments.iter().enumerate() {
        if column > 0 {
            writer.write_literal(" | ")?;
        }
        writer.write_literal(match alignment {
            Some(TableAlignment::Left) => ":---",
            Some(TableAlignment::Center) => ":---:",
            Some(TableAlignment::Right) => "---:",
            None => "---",
        })?;
    }
    writer.write_literal(" |")
}
