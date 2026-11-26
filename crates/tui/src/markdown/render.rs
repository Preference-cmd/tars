//! GFM Markdown renderer producing wrapped [`Line<'static>`] output for
//! `tars-tui` transcript rows and detail panels.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::highlight::{highlight_code, sanitize_single_line, sanitize_text};

const ACCENT: Color = Color::Rgb(215, 147, 114);
const MUTED: Color = Color::DarkGray;

#[derive(Clone)]
enum PrefixFrame {
    BlockQuote,
    ListItem {
        is_ordered: bool,
        marker_spans: Vec<Span<'static>>,
        indent_spaces: usize,
        marker_pending: bool,
    },
}

#[derive(Clone)]
struct ListContext {
    next_index: Option<u64>,
    depth: usize,
}

struct LinkState {
    dest_url: String,
    spans: Vec<Span<'static>>,
    plain_text: String,
}

struct TableState {
    alignments: Vec<Alignment>,
    headers: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    current_row: Vec<Vec<Span<'static>>>,
    current_cell: Vec<Span<'static>>,
    in_head: bool,
    in_cell: bool,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    inline_spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    prefix_stack: Vec<PrefixFrame>,
    list_stack: Vec<ListContext>,
    link: Option<LinkState>,
    image_dest: Option<String>,
    image_alt: String,
    in_code_block: bool,
    code_lang: String,
    code_buf: String,
    table: Option<TableState>,
    need_blank_line: bool,
}

impl Renderer {
    fn new(width: u16) -> Self {
        Self {
            width: usize::from(width.max(1)),
            out: Vec::new(),
            inline_spans: Vec::new(),
            style_stack: vec![Style::default()],
            prefix_stack: Vec::new(),
            list_stack: Vec::new(),
            link: None,
            image_dest: None,
            image_alt: String::new(),
            in_code_block: false,
            code_lang: String::new(),
            code_buf: String::new(),
            table: None,
            need_blank_line: false,
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn push_span(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(table) = &mut self.table
            && table.in_cell
            && self.link.is_none()
        {
            push_merged_span(&mut table.current_cell, text, style);
            return;
        }
        if let Some(link) = &mut self.link {
            link.plain_text.push_str(text);
            push_merged_span(&mut link.spans, text, style);
            return;
        }
        push_merged_span(&mut self.inline_spans, text, style);
    }

    fn active_blockquote_prefix(&self) -> Vec<Span<'static>> {
        let mut prefix = Vec::new();
        for frame in &self.prefix_stack {
            match frame {
                PrefixFrame::BlockQuote => {
                    prefix.push(Span::styled("│ ", Style::default().fg(MUTED)));
                }
                PrefixFrame::ListItem { indent_spaces, .. } => {
                    if *indent_spaces > 0 {
                        prefix.push(Span::raw(" ".repeat(*indent_spaces)));
                    }
                }
            }
        }
        prefix
    }

    fn emit_blank_separator_if_needed(&mut self) {
        if !self.need_blank_line || self.out.is_empty() {
            self.need_blank_line = false;
            return;
        }
        self.need_blank_line = false;
        let prefix = self.active_blockquote_prefix();
        if prefix.is_empty() {
            self.out.push(Line::default());
        } else {
            self.out.push(Line::from(prefix));
        }
    }

    fn take_line_prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut first = Vec::new();
        let mut cont = Vec::new();
        for frame in &mut self.prefix_stack {
            match frame {
                PrefixFrame::BlockQuote => {
                    let bar = Span::styled("│ ", Style::default().fg(MUTED));
                    first.push(bar.clone());
                    cont.push(bar);
                }
                PrefixFrame::ListItem {
                    marker_spans,
                    indent_spaces,
                    marker_pending,
                    ..
                } => {
                    if *marker_pending {
                        first.extend(marker_spans.iter().cloned());
                        *marker_pending = false;
                    } else if *indent_spaces > 0 {
                        first.push(Span::raw(" ".repeat(*indent_spaces)));
                    }
                    if *indent_spaces > 0 {
                        cont.push(Span::raw(" ".repeat(*indent_spaces)));
                    }
                }
            }
        }
        (first, cont)
    }

    fn flush_inline(&mut self) {
        if self.inline_spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline_spans);
        let (mut line_spans, cont_prefix) = self.take_line_prefixes();
        for span in spans {
            push_merged_span(&mut line_spans, &span.content, span.style);
        }
        let wrapped = wrap_styled_line_impl(Line::from(line_spans), self.width, &cont_prefix, true);
        self.out.extend(wrapped);
    }

    fn push_inline_text(&mut self, raw: &str, style: Style) {
        let clean = sanitize_text(raw);
        if !clean.contains('\n') {
            self.push_span(&clean, style);
            return;
        }
        let mut segments = clean.split('\n').peekable();
        while let Some(seg) = segments.next() {
            self.push_span(seg, style);
            if segments.peek().is_some() {
                if self.table.as_ref().is_some_and(|t| t.in_cell) {
                    self.push_span(" ", style);
                } else {
                    self.flush_inline();
                }
            }
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_buf.push_str(&text);
                } else if self.image_dest.is_some() {
                    self.image_alt.push_str(&text);
                } else {
                    let style = self.current_style();
                    self.push_inline_text(&text, style);
                }
            }
            Event::Code(code) => {
                let clean = sanitize_single_line(&code);
                if self.image_dest.is_some() {
                    self.image_alt.push_str(&clean);
                } else {
                    let style = self.current_style().fg(Color::Cyan);
                    self.push_span(&clean, style);
                }
            }
            Event::Html(html) => {
                if self.in_code_block {
                    self.code_buf.push_str(&html);
                } else {
                    self.emit_blank_separator_if_needed();
                    let trimmed = html.strip_suffix('\n').unwrap_or(&html);
                    let clean = sanitize_text(trimmed);
                    for line in clean.split('\n') {
                        self.push_span(line, Style::default().fg(MUTED));
                        self.flush_inline();
                    }
                    self.need_blank_line = true;
                }
            }
            Event::InlineHtml(html) => {
                let clean = sanitize_single_line(&html);
                if self.image_dest.is_some() {
                    self.image_alt.push_str(&clean);
                } else {
                    self.push_span(&clean, Style::default().fg(MUTED));
                }
            }
            Event::SoftBreak => {
                if self.in_code_block {
                    self.code_buf.push('\n');
                } else if self.image_dest.is_some() {
                    self.image_alt.push(' ');
                } else {
                    let style = self.current_style();
                    self.push_span(" ", style);
                }
            }
            Event::HardBreak => {
                if self.in_code_block {
                    self.code_buf.push('\n');
                } else if self.image_dest.is_some() {
                    self.image_alt.push(' ');
                } else if self.table.as_ref().is_some_and(|t| t.in_cell) {
                    let style = self.current_style();
                    self.push_span(" ", style);
                } else {
                    self.flush_inline();
                    if self.inline_spans.is_empty()
                        && let Some(PrefixFrame::ListItem {
                            marker_pending: true,
                            ..
                        }) = self.prefix_stack.last()
                    {
                        let (prefix, _) = self.take_line_prefixes();
                        self.out.push(Line::from(prefix));
                    }
                }
            }
            Event::Rule => {
                self.flush_inline();
                self.emit_blank_separator_if_needed();
                let (prefix, _) = self.take_line_prefixes();
                let prefix_w: usize = prefix.iter().map(Span::width).sum();
                let avail = self.width.saturating_sub(prefix_w);
                let rule_len = if avail >= 3 { avail } else { avail.max(1) };
                let mut spans = prefix;
                spans.push(Span::styled(
                    "─".repeat(rule_len),
                    Style::default().fg(MUTED),
                ));
                self.out.push(Line::from(spans));
                self.need_blank_line = true;
            }
            Event::TaskListMarker(checked) => {
                let (label, color) = if checked {
                    ("[x] ", Color::Green)
                } else {
                    ("[ ] ", MUTED)
                };
                if let Some(PrefixFrame::ListItem {
                    is_ordered,
                    marker_spans,
                    indent_spaces,
                    marker_pending: true,
                }) = self.prefix_stack.last_mut()
                {
                    if *is_ordered {
                        marker_spans.push(Span::styled(label, Style::default().fg(color)));
                        *indent_spaces += label.width();
                    } else {
                        *marker_spans = vec![Span::styled(label, Style::default().fg(color))];
                        *indent_spaces = label.width();
                    }
                } else {
                    self.push_span(label, Style::default().fg(color));
                }
            }
            Event::FootnoteReference(name) => {
                let clean = sanitize_single_line(&name);
                self.push_span(&format!("[^{clean}]"), Style::default().fg(MUTED));
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                let clean = sanitize_single_line(&math);
                self.push_span(&clean, Style::default().fg(Color::Cyan));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_inline();
                self.emit_blank_separator_if_needed();
            }
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.emit_blank_separator_if_needed();
                self.style_stack.push(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.emit_blank_separator_if_needed();
                self.prefix_stack.push(PrefixFrame::BlockQuote);
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                self.emit_blank_separator_if_needed();
                self.in_code_block = true;
                self.code_buf.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) => sanitize_single_line(&info),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(start) => {
                self.flush_inline();
                if self.list_stack.is_empty() {
                    self.emit_blank_separator_if_needed();
                } else {
                    self.need_blank_line = false;
                }
                let depth = self.list_stack.len();
                self.list_stack.push(ListContext {
                    next_index: start,
                    depth,
                });
            }
            Tag::Item => {
                self.flush_inline();
                if self.need_blank_line {
                    self.emit_blank_separator_if_needed();
                }
                let (is_ordered, marker_spans, indent_spaces) =
                    if let Some(ctx) = self.list_stack.last_mut() {
                        if let Some(idx) = &mut ctx.next_index {
                            let label = format!("{idx}. ");
                            *idx += 1;
                            let w = label.width();
                            (
                                true,
                                vec![Span::styled(label, Style::default().fg(ACCENT))],
                                w,
                            )
                        } else if ctx.depth == 0 {
                            (
                                false,
                                vec![Span::styled("• ", Style::default().fg(ACCENT))],
                                2,
                            )
                        } else {
                            (
                                false,
                                vec![Span::styled("◦ ", Style::default().fg(MUTED))],
                                2,
                            )
                        }
                    } else {
                        (
                            false,
                            vec![Span::styled("• ", Style::default().fg(ACCENT))],
                            2,
                        )
                    };
                self.prefix_stack.push(PrefixFrame::ListItem {
                    is_ordered,
                    marker_spans,
                    indent_spaces,
                    marker_pending: true,
                });
            }
            Tag::Table(alignments) => {
                self.flush_inline();
                self.emit_blank_separator_if_needed();
                self.table = Some(TableState {
                    alignments,
                    headers: Vec::new(),
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    current_cell: Vec::new(),
                    in_head: false,
                    in_cell: false,
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                    table.current_row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.in_cell = true;
                    table.current_cell.clear();
                }
            }
            Tag::Emphasis => {
                let s = self.current_style().add_modifier(Modifier::ITALIC);
                self.style_stack.push(s);
            }
            Tag::Strong => {
                let s = self.current_style().add_modifier(Modifier::BOLD);
                self.style_stack.push(s);
            }
            Tag::Strikethrough => {
                let s = self.current_style().add_modifier(Modifier::CROSSED_OUT);
                self.style_stack.push(s);
            }
            Tag::Link { dest_url, .. } => {
                let s = self
                    .current_style()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::UNDERLINED);
                self.style_stack.push(s);
                self.link = Some(LinkState {
                    dest_url: sanitize_single_line(&dest_url),
                    spans: Vec::new(),
                    plain_text: String::new(),
                });
            }
            Tag::Image { dest_url, .. } => {
                self.image_dest = Some(sanitize_single_line(&dest_url));
                self.image_alt.clear();
            }
            Tag::FootnoteDefinition(name) => {
                self.flush_inline();
                self.emit_blank_separator_if_needed();
                let clean = sanitize_single_line(&name);
                self.push_span(&format!("[^{clean}]: "), Style::default().fg(MUTED));
            }
            Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline();
                self.need_blank_line = true;
            }
            TagEnd::Heading(_) => {
                self.flush_inline();
                self.pop_style();
                self.need_blank_line = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                if matches!(self.prefix_stack.last(), Some(PrefixFrame::BlockQuote)) {
                    self.prefix_stack.pop();
                }
                self.need_blank_line = true;
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.render_code_block();
                self.need_blank_line = true;
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.need_blank_line = true;
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if let Some(PrefixFrame::ListItem {
                    marker_pending: true,
                    ..
                }) = self.prefix_stack.last()
                {
                    let (prefix, _) = self.take_line_prefixes();
                    if !prefix.is_empty() {
                        self.out.push(Line::from(prefix));
                    }
                }
                if matches!(self.prefix_stack.last(), Some(PrefixFrame::ListItem { .. })) {
                    self.prefix_stack.pop();
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                    self.need_blank_line = true;
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = false;
                    if !table.current_row.is_empty() {
                        table.headers = std::mem::take(&mut table.current_row);
                    }
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.current_row);
                    if table.in_head {
                        table.headers = row;
                    } else {
                        table.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.in_cell = false;
                    let cell = std::mem::take(&mut table.current_cell);
                    table.current_row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.pop_style();
            }
            TagEnd::Link => {
                self.pop_style();
                if let Some(link) = self.link.take() {
                    let url = link.dest_url.trim().to_owned();
                    let label = link.plain_text.trim().to_owned();
                    for span in link.spans {
                        self.push_span(&span.content, span.style);
                    }
                    if !url.is_empty() && label != url {
                        self.push_span(&format!(" ({url})"), Style::default().fg(MUTED));
                    }
                }
            }
            TagEnd::Image => {
                let url = sanitize_single_line(self.image_dest.take().as_deref().unwrap_or(""));
                let alt = sanitize_single_line(&std::mem::take(&mut self.image_alt));
                let formatted = match (alt.is_empty(), url.is_empty()) {
                    (false, false) if alt != url => format!("[img: {alt} ({url})]"),
                    (false, _) => format!("[img: {alt}]"),
                    (true, false) => format!("[img: {url}]"),
                    (true, true) => "[img]".to_owned(),
                };
                self.push_span(&formatted, Style::default().fg(MUTED));
            }
            TagEnd::FootnoteDefinition => {
                self.flush_inline();
                self.need_blank_line = true;
            }
            TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn render_code_block(&mut self) {
        let code = std::mem::take(&mut self.code_buf);
        let lang = std::mem::take(&mut self.code_lang);
        let (first_prefix, cont_prefix) = self.take_line_prefixes();
        let prefix_w: usize = cont_prefix.iter().map(Span::width).sum();
        let avail = self.width.saturating_sub(prefix_w).max(1);
        let use_guide = avail > 2;

        let lang_label = lang.split_whitespace().next().unwrap_or("");
        let mut used_first_prefix = false;

        if !lang_label.is_empty() {
            let prefix = if !used_first_prefix {
                used_first_prefix = true;
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            let mut header_spans = prefix;
            let header_text = if use_guide {
                format!("╭─ {lang_label}")
            } else {
                lang_label.to_owned()
            };
            header_spans.push(Span::styled(header_text, Style::default().fg(MUTED)));
            let wrapped =
                wrap_styled_line_impl(Line::from(header_spans), self.width, &cont_prefix, false);
            self.out.extend(wrapped);
        }

        let highlighted = highlight_code(&lang, &code);
        for code_line in highlighted {
            let line_prefix = if !used_first_prefix {
                used_first_prefix = true;
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            let mut full_prefix = line_prefix;
            let mut wrap_cont = cont_prefix.clone();
            if use_guide {
                let guide = Span::styled("│ ", Style::default().fg(MUTED));
                full_prefix.push(guide.clone());
                wrap_cont.push(guide);
            }
            let mut spans = full_prefix;
            if code_line.spans.is_empty() {
                self.out.push(Line::from(spans));
            } else {
                for span in code_line.spans {
                    push_merged_span(&mut spans, &span.content, span.style);
                }
                let wrapped =
                    wrap_styled_line_impl(Line::from(spans), self.width, &wrap_cont, false);
                self.out.extend(wrapped);
            }
        }

        if !lang_label.is_empty() && use_guide {
            let mut footer_spans = if !used_first_prefix {
                first_prefix
            } else {
                cont_prefix
            };
            let footer_text = if avail >= 3 { "╰──" } else { "╰─" };
            footer_spans.push(Span::styled(footer_text, Style::default().fg(MUTED)));
            self.out.push(Line::from(footer_spans));
        }
    }

    fn render_table(&mut self, table: TableState) {
        let num_cols = table
            .alignments
            .len()
            .max(table.headers.len())
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if num_cols == 0 {
            return;
        }

        let (first_prefix, cont_prefix) = self.take_line_prefixes();
        let prefix_w: usize = cont_prefix.iter().map(Span::width).sum();
        let avail = self.width.saturating_sub(prefix_w).max(1);

        let overhead = num_cols * 3 + 1;
        let min_box_width = overhead + num_cols;

        if avail >= 20 && avail >= min_box_width {
            self.render_boxed_table(table, num_cols, avail, &first_prefix, &cont_prefix);
        } else {
            self.render_stacked_table(table, num_cols, avail, &first_prefix, &cont_prefix);
        }
    }

    fn render_boxed_table(
        &mut self,
        table: TableState,
        num_cols: usize,
        avail: usize,
        first_prefix: &[Span<'static>],
        cont_prefix: &[Span<'static>],
    ) {
        let mut natural = vec![1usize; num_cols];
        for (c, cell) in table.headers.iter().enumerate() {
            let w: usize = cell.iter().map(Span::width).sum();
            natural[c] = natural[c].max(w);
        }
        for row in &table.rows {
            for (c, cell) in row.iter().enumerate() {
                let w: usize = cell.iter().map(Span::width).sum();
                natural[c] = natural[c].max(w);
            }
        }

        let avail_content = avail.saturating_sub(num_cols * 3 + 1);
        let total_natural: usize = natural.iter().sum();
        let col_widths = if total_natural <= avail_content {
            natural
        } else {
            let mut widths = vec![1usize; num_cols];
            let mut remaining = avail_content.saturating_sub(num_cols);
            for (c, &nat) in natural.iter().enumerate() {
                let target = if total_natural == 0 {
                    1
                } else {
                    ((nat as u128 * avail_content as u128) / total_natural as u128) as usize
                };
                let extra = target
                    .saturating_sub(1)
                    .min(nat.saturating_sub(1))
                    .min(remaining);
                widths[c] += extra;
                remaining -= extra;
            }
            while remaining > 0 {
                let mut best: Option<(usize, usize)> = None;
                for (c, &nat) in natural.iter().enumerate() {
                    let deficit = nat.saturating_sub(widths[c]);
                    if deficit > 0 && best.is_none_or(|(_, d)| deficit > d) {
                        best = Some((c, deficit));
                    }
                }
                let Some((c, _)) = best else { break };
                widths[c] += 1;
                remaining -= 1;
            }
            widths
        };

        let mut used_first = false;
        let mut next_prefix = || -> Vec<Span<'static>> {
            if !used_first {
                used_first = true;
                first_prefix.to_vec()
            } else {
                cont_prefix.to_vec()
            }
        };

        // Top border
        self.out.push(table_border_line(
            &next_prefix(),
            '┌',
            '┬',
            '┐',
            &col_widths,
        ));

        // Header row
        if !table.headers.is_empty() {
            let header_cells: Vec<Vec<Span<'static>>> = (0..num_cols)
                .map(|c| {
                    table
                        .headers
                        .get(c)
                        .map(|spans| {
                            spans
                                .iter()
                                .map(|s| {
                                    Span::styled(
                                        s.content.clone(),
                                        s.style.add_modifier(Modifier::BOLD),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect();
            self.push_boxed_table_row(
                &header_cells,
                &col_widths,
                &table.alignments,
                &mut next_prefix,
            );
            if !table.rows.is_empty() {
                self.out.push(table_border_line(
                    &next_prefix(),
                    '├',
                    '┼',
                    '┤',
                    &col_widths,
                ));
            }
        }

        // Body rows
        for row in &table.rows {
            self.push_boxed_table_row(row, &col_widths, &table.alignments, &mut next_prefix);
        }

        // Bottom border
        self.out.push(table_border_line(
            &next_prefix(),
            '└',
            '┴',
            '┘',
            &col_widths,
        ));
    }

    fn push_boxed_table_row<F>(
        &mut self,
        row: &[Vec<Span<'static>>],
        col_widths: &[usize],
        alignments: &[Alignment],
        next_prefix: &mut F,
    ) where
        F: FnMut() -> Vec<Span<'static>>,
    {
        let wrapped_cols: Vec<Vec<Line<'static>>> = col_widths
            .iter()
            .enumerate()
            .map(|(c, &w)| {
                let cell_spans = row.get(c).cloned().unwrap_or_default();
                if cell_spans.is_empty() {
                    vec![Line::default()]
                } else {
                    wrap_styled_line_impl(Line::from(cell_spans), w, &[], true)
                }
            })
            .collect();
        let row_height = wrapped_cols.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let border_style = Style::default().fg(MUTED);

        for line_idx in 0..row_height {
            let mut spans = next_prefix();
            spans.push(Span::styled("│", border_style));
            for (c, &col_w) in col_widths.iter().enumerate() {
                spans.push(Span::raw(" "));
                let cell_line = wrapped_cols[c].get(line_idx);
                let cell_w = cell_line.map_or(0, Line::width);
                let pad = col_w.saturating_sub(cell_w);
                let align = alignments.get(c).copied().unwrap_or(Alignment::None);
                let (left_pad, right_pad) = match align {
                    Alignment::Right => (pad, 0),
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    Alignment::None | Alignment::Left => (0, pad),
                };
                if left_pad > 0 {
                    spans.push(Span::raw(" ".repeat(left_pad)));
                }
                if let Some(cl) = cell_line {
                    for s in &cl.spans {
                        push_merged_span(&mut spans, &s.content, s.style);
                    }
                }
                if right_pad > 0 {
                    spans.push(Span::raw(" ".repeat(right_pad)));
                }
                spans.push(Span::raw(" "));
                spans.push(Span::styled("│", border_style));
            }
            self.out.push(Line::from(spans));
        }
    }

    fn render_stacked_table(
        &mut self,
        table: TableState,
        num_cols: usize,
        avail: usize,
        first_prefix: &[Span<'static>],
        cont_prefix: &[Span<'static>],
    ) {
        let mut used_first = false;
        let emit_wrapped =
            |line: Line<'static>, out: &mut Vec<Line<'static>>, used_first: &mut bool| {
                let prefix = if !*used_first {
                    *used_first = true;
                    first_prefix
                } else {
                    cont_prefix
                };
                let mut combined = prefix.to_vec();
                for s in line.spans {
                    push_merged_span(&mut combined, &s.content, s.style);
                }
                out.extend(wrap_styled_line_impl(
                    Line::from(combined),
                    self.width,
                    cont_prefix,
                    true,
                ));
            };

        if table.rows.is_empty() {
            for c in 0..num_cols {
                if let Some(cell) = table.headers.get(c)
                    && !cell.is_empty()
                {
                    let bold_spans: Vec<Span<'static>> = cell
                        .iter()
                        .map(|s| {
                            Span::styled(s.content.clone(), s.style.add_modifier(Modifier::BOLD))
                        })
                        .collect();
                    emit_wrapped(Line::from(bold_spans), &mut self.out, &mut used_first);
                }
            }
            return;
        }

        for (row_idx, row) in table.rows.iter().enumerate() {
            if row_idx > 0 {
                let div_len = avail.clamp(1, 3);
                let mut div = if !used_first {
                    used_first = true;
                    first_prefix.to_vec()
                } else {
                    cont_prefix.to_vec()
                };
                div.push(Span::styled(
                    "─".repeat(div_len),
                    Style::default().fg(MUTED),
                ));
                self.out.push(Line::from(div));
            }
            for c in 0..num_cols {
                let mut line_spans = Vec::new();
                if let Some(header) = table.headers.get(c)
                    && !header.is_empty()
                {
                    for s in header {
                        push_merged_span(
                            &mut line_spans,
                            &s.content,
                            s.style.fg(MUTED).add_modifier(Modifier::BOLD),
                        );
                    }
                    push_merged_span(&mut line_spans, ": ", Style::default().fg(MUTED));
                }
                if let Some(cell) = row.get(c) {
                    for s in cell {
                        push_merged_span(&mut line_spans, &s.content, s.style);
                    }
                }
                if !line_spans.is_empty() {
                    emit_wrapped(Line::from(line_spans), &mut self.out, &mut used_first);
                }
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_inline();
        while let Some(last) = self.out.last() {
            if last.spans.is_empty() || last.spans.iter().all(|s| s.content.is_empty()) {
                self.out.pop();
            } else {
                break;
            }
        }
        self.out
    }
}

fn table_border_line(
    prefix: &[Span<'static>],
    left: char,
    mid: char,
    right: char,
    col_widths: &[usize],
) -> Line<'static> {
    let mut border = String::new();
    border.push(left);
    for (idx, &w) in col_widths.iter().enumerate() {
        if idx > 0 {
            border.push(mid);
        }
        for _ in 0..(w + 2) {
            border.push('─');
        }
    }
    border.push(right);
    let mut spans = prefix.to_vec();
    spans.push(Span::styled(border, Style::default().fg(MUTED)));
    Line::from(spans)
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H4 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H5 | HeadingLevel::H6 => {
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
        }
    }
}

fn push_merged_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
        return;
    }
    spans.push(Span::styled(text.to_owned(), style));
}

/// Render GFM Markdown into wrapped [`Line<'static>`] values bounded by `width`
/// display columns.
///
/// Untrusted input is sanitized before parsing and every derived fragment is
/// re-sanitized so raw escape sequences, control characters, BiDi overrides, and
/// Unicode line/paragraph separators never enter any [`Span`].
pub(crate) fn render_markdown(input: &str, width: u16) -> Vec<Line<'static>> {
    if input.trim().is_empty() {
        return Vec::new();
    }
    let sanitized = sanitize_text(input);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(&sanitized, options);
    let mut renderer = Renderer::new(width);
    for event in parser {
        renderer.handle_event(event);
    }
    renderer.finish()
}

/// Wrap a styled [`Line<'static>`] by grapheme display width, prepending
/// `continuation_prefix` to wrapped continuation lines.
pub(crate) fn wrap_styled_line(
    line: Line<'static>,
    width: usize,
    continuation_prefix: &[Span<'static>],
) -> Vec<Line<'static>> {
    wrap_styled_line_impl(line, width, continuation_prefix, true)
}

struct WrapUnit {
    pieces: Vec<(String, Style)>,
    width: usize,
    is_whitespace: bool,
}

fn wrap_styled_line_impl(
    line: Line<'static>,
    width: usize,
    continuation_prefix: &[Span<'static>],
    word_wrap: bool,
) -> Vec<Line<'static>> {
    let max_width = width.max(1);
    if line.spans.is_empty() || line.spans.iter().all(|s| s.content.is_empty()) {
        return vec![line];
    }

    let prefix_width: usize = continuation_prefix.iter().map(Span::width).sum();
    let apply_prefix = prefix_width > 0 && prefix_width < max_width;

    if !word_wrap {
        return wrap_by_grapheme(
            line.spans,
            max_width,
            continuation_prefix,
            prefix_width,
            apply_prefix,
        );
    }

    // Group contiguous non-whitespace graphemes into word units across style runs
    // so styled inline spans like `**hello**world` stay intact on a single wrap unit.
    let mut units: Vec<WrapUnit> = Vec::new();
    let mut current_word: Vec<(String, Style)> = Vec::new();
    let mut current_word_w = 0usize;

    for span in line.spans {
        let style = span.style;
        for grapheme in span.content.graphemes(true) {
            let gw = grapheme.width();
            if grapheme.chars().all(char::is_whitespace) {
                if !current_word.is_empty() {
                    units.push(WrapUnit {
                        pieces: std::mem::take(&mut current_word),
                        width: current_word_w,
                        is_whitespace: false,
                    });
                    current_word_w = 0;
                }
                units.push(WrapUnit {
                    pieces: vec![(grapheme.to_owned(), style)],
                    width: gw,
                    is_whitespace: true,
                });
            } else {
                push_piece(&mut current_word, grapheme, style);
                current_word_w += gw;
            }
        }
    }
    if !current_word.is_empty() {
        units.push(WrapUnit {
            pieces: current_word,
            width: current_word_w,
            is_whitespace: false,
        });
    }

    let mut out = Vec::new();
    let mut cur_spans: Vec<Span<'static>> = Vec::new();
    let mut cur_col = 0usize;
    let mut has_content = false;

    for unit in units {
        if !has_content || cur_col + unit.width <= max_width {
            if !has_content && unit.width > max_width {
                // Single unit exceeds max_width even on an empty line: split by grapheme.
                wrap_oversized_pieces(
                    &unit.pieces,
                    max_width,
                    continuation_prefix,
                    prefix_width,
                    apply_prefix,
                    &mut out,
                    &mut cur_spans,
                    &mut cur_col,
                    &mut has_content,
                );
            } else {
                for (text, style) in &unit.pieces {
                    push_merged_span(&mut cur_spans, text, *style);
                }
                cur_col += unit.width;
                has_content = true;
            }
            continue;
        }

        // Current line has content and unit would overflow.
        out.push(Line::from(std::mem::take(&mut cur_spans)));
        start_continuation_line(
            continuation_prefix,
            prefix_width,
            apply_prefix,
            &mut cur_spans,
            &mut cur_col,
            &mut has_content,
        );

        if unit.is_whitespace {
            // Drop the single breaking whitespace at the wrap boundary.
            continue;
        }

        if cur_col + unit.width <= max_width {
            for (text, style) in &unit.pieces {
                push_merged_span(&mut cur_spans, text, *style);
            }
            cur_col += unit.width;
            has_content = true;
        } else {
            wrap_oversized_pieces(
                &unit.pieces,
                max_width,
                continuation_prefix,
                prefix_width,
                apply_prefix,
                &mut out,
                &mut cur_spans,
                &mut cur_col,
                &mut has_content,
            );
        }
    }

    if has_content || out.is_empty() {
        out.push(Line::from(cur_spans));
    }
    out
}

fn wrap_by_grapheme(
    spans: Vec<Span<'static>>,
    max_width: usize,
    continuation_prefix: &[Span<'static>],
    prefix_width: usize,
    apply_prefix: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut cur_spans: Vec<Span<'static>> = Vec::new();
    let mut cur_col = 0usize;
    let mut has_content = false;

    for span in spans {
        let style = span.style;
        for grapheme in span.content.graphemes(true) {
            let gw = grapheme.width();
            if has_content && cur_col + gw > max_width {
                out.push(Line::from(std::mem::take(&mut cur_spans)));
                start_continuation_line(
                    continuation_prefix,
                    prefix_width,
                    apply_prefix,
                    &mut cur_spans,
                    &mut cur_col,
                    &mut has_content,
                );
            }
            push_merged_span(&mut cur_spans, grapheme, style);
            cur_col += gw;
            has_content = true;
        }
    }

    if has_content || out.is_empty() {
        out.push(Line::from(cur_spans));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn wrap_oversized_pieces(
    pieces: &[(String, Style)],
    max_width: usize,
    continuation_prefix: &[Span<'static>],
    prefix_width: usize,
    apply_prefix: bool,
    out: &mut Vec<Line<'static>>,
    cur_spans: &mut Vec<Span<'static>>,
    cur_col: &mut usize,
    has_content: &mut bool,
) {
    for (text, style) in pieces {
        for grapheme in text.graphemes(true) {
            let gw = grapheme.width();
            if *has_content && *cur_col + gw > max_width {
                out.push(Line::from(std::mem::take(cur_spans)));
                start_continuation_line(
                    continuation_prefix,
                    prefix_width,
                    apply_prefix,
                    cur_spans,
                    cur_col,
                    has_content,
                );
            }
            push_merged_span(cur_spans, grapheme, *style);
            *cur_col += gw;
            *has_content = true;
        }
    }
}

fn start_continuation_line(
    continuation_prefix: &[Span<'static>],
    prefix_width: usize,
    apply_prefix: bool,
    cur_spans: &mut Vec<Span<'static>>,
    cur_col: &mut usize,
    has_content: &mut bool,
) {
    if apply_prefix {
        cur_spans.extend(continuation_prefix.iter().cloned());
        *cur_col = prefix_width;
    } else {
        *cur_col = 0;
    }
    *has_content = false;
}

fn push_piece(pieces: &mut Vec<(String, Style)>, text: &str, style: Style) {
    if let Some((last_text, last_style)) = pieces.last_mut()
        && *last_style == style
    {
        last_text.push_str(text);
        return;
    }
    pieces.push((text.to_owned(), style));
}
