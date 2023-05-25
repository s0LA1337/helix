use crate::{
    commands::{self, OnKeyCallback},
    compositor::{Component, Context, Event, EventResult},
    job::{self, Callback},
    key,
    keymap::{KeymapResult, Keymaps},
    ui::{
        document::{render_document, LinePos, TextRenderer, TranslatedPosition},
        Completion, ProgressSpinners,
    },
};

use helix_core::{
    chars::char_is_word,
    diagnostic::NumberOrString,
    graphemes::{
        ensure_grapheme_boundary_next_byte, next_grapheme_boundary, prev_grapheme_boundary,
    },
    movement::Direction,
    syntax::{self, HighlightEvent, RopeProvider},
    text_annotations::TextAnnotations,
    tree_sitter::{QueryCursor, QueryMatch},
    unicode::width::UnicodeWidthStr,
    visual_offset_from_block, Change, Position, Range, Selection, Transaction,
};
use helix_view::{
    document::{Mode, SavePoint, SCRATCH_BUFFER_NAME},
    editor::{CompleteAction, CursorShapeConfig},
    graphics::{Color, CursorKind, Modifier, Rect, Style},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Document, Editor, Theme, View,
};
use std::{collections::HashSet, mem::take, num::NonZeroUsize, path::PathBuf, rc::Rc, sync::Arc};

use tui::{buffer::Buffer as Surface, text::Span};

use super::{completion::CompletionItem, document::render_text, statusline};
use super::{document::LineDecoration, lsp::SignatureHelp};

#[derive(Debug, Clone)]
pub struct StickyNode {
    line: usize,
    visual_line: u16,
    byte_range: std::ops::Range<usize>,
    indicator: Option<String>,
    top_first_byte: usize,
    cursor_byte: usize,
    has_context_end: bool,
}

impl PartialEq for StickyNode {
    fn eq(&self, other: &Self) -> bool {
        self.line == other.line
    }
}

impl Eq for StickyNode {}

impl std::hash::Hash for StickyNode {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.line.hash(state);
    }
}

pub struct EditorView {
    pub keymaps: Keymaps,
    on_next_key: Option<OnKeyCallback>,
    pseudo_pending: Vec<KeyEvent>,
    pub(crate) last_insert: (commands::MappableCommand, Vec<InsertEvent>),
    pub(crate) completion: Option<Completion>,
    spinners: ProgressSpinners,
    sticky_nodes: Option<Vec<StickyNode>>,
}

#[derive(Debug, Clone)]
pub enum InsertEvent {
    Key(KeyEvent),
    CompletionApply {
        trigger_offset: usize,
        changes: Vec<Change>,
    },
    TriggerCompletion,
    RequestCompletion,
}

impl Default for EditorView {
    fn default() -> Self {
        Self::new(Keymaps::default())
    }
}

impl EditorView {
    pub fn new(keymaps: Keymaps) -> Self {
        Self {
            keymaps,
            on_next_key: None,
            pseudo_pending: Vec::new(),
            last_insert: (commands::MappableCommand::normal_mode, Vec::new()),
            completion: None,
            spinners: ProgressSpinners::default(),
            sticky_nodes: None,
        }
    }

    pub fn spinners_mut(&mut self) -> &mut ProgressSpinners {
        &mut self.spinners
    }

    pub fn render_view(
        &mut self,
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        is_focused: bool,
    ) {
        let inner = view.inner_area(doc);
        let area = view.area;
        let theme = &editor.theme;
        let config = editor.config();

        let should_render_rainbow_brackets = doc
            .language_config()
            .and_then(|lang_config| lang_config.rainbow_brackets)
            .unwrap_or(config.rainbow_brackets);

        let text_annotations = view.text_annotations(doc, Some(theme));
        let mut line_decorations: Vec<Box<dyn LineDecoration>> = Vec::new();
        let mut translated_positions: Vec<TranslatedPosition> = Vec::new();

        if is_focused && config.cursorline {
            line_decorations.push(Self::cursorline_decorator(doc, view, theme))
        }

        if is_focused && config.cursorcolumn {
            Self::highlight_cursorcolumn(doc, view, surface, theme, inner, &text_annotations);
        }

        // Set DAP highlights, if needed.
        if let Some(frame) = editor.current_stack_frame() {
            let dap_line = frame.line.saturating_sub(1);
            let style = theme.get("ui.highlight.frameline");
            let line_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                if pos.doc_line != dap_line {
                    return;
                }
                renderer.surface.set_style(
                    Rect::new(inner.x, inner.y + pos.visual_line, inner.width, 1),
                    style,
                );
            };

            line_decorations.push(Box::new(line_decoration));
        }

        let mut highlights =
            Self::doc_syntax_highlights(doc, view.offset.anchor, inner.height, theme);
        if should_render_rainbow_brackets {
            highlights = Box::new(syntax::merge(
                highlights,
                Self::doc_rainbow_highlights(doc, view.offset.anchor, inner.height, theme),
            ));
        }
        let overlay_highlights = Self::overlay_syntax_highlights(
            doc,
            view.offset.anchor,
            inner.height,
            &text_annotations,
        );
        if !overlay_highlights.is_empty() {
            highlights = Box::new(syntax::merge(highlights, overlay_highlights));
        }

        if config.cursor_word {
            if let Some(cursor_word_highlights) =
                Self::collect_cursor_word_highlights(doc, editor, view, viewport, theme)
            {
                highlights = Box::new(syntax::merge(highlights, cursor_word_highlights));
            }
        }

        for diagnostic in Self::doc_diagnostics_highlights(doc, theme) {
            // Most of the `diagnostic` Vecs are empty most of the time. Skipping
            // a merge for any empty Vec saves a significant amount of work.
            if diagnostic.is_empty() {
                continue;
            }
            highlights = Box::new(syntax::merge(highlights, diagnostic));
        }

        let highlights: Box<dyn Iterator<Item = HighlightEvent>> = if is_focused {
            let highlights = syntax::merge(
                highlights,
                Self::doc_selection_highlights(
                    editor.mode(),
                    doc,
                    view,
                    theme,
                    &config.cursor_shape,
                ),
            );
            let focused_view_elements = Self::highlight_focused_view_elements(view, doc, theme);
            if focused_view_elements.is_empty() {
                Box::new(highlights)
            } else {
                Box::new(syntax::merge(highlights, focused_view_elements))
            }
        } else {
            Box::new(highlights)
        };

        if config.sticky_context.enable {
            self.sticky_nodes =
                Self::calculate_sticky_nodes(&self.sticky_nodes, doc, view, &config);
        }

        if is_focused {
            let cursor = doc
                .selection(view.id)
                .primary()
                .cursor(doc.text().slice(..));
            // set the cursor_cache to out of view in case the position is not found
            editor.cursor_cache.set(Some(None));
            let update_cursor_cache =
                |_: &mut TextRenderer, pos| editor.cursor_cache.set(Some(Some(pos)));
            translated_positions.push((cursor, Box::new(update_cursor_cache)));
        }

        Self::render_gutter(
            editor,
            doc,
            &self.sticky_nodes,
            view,
            theme,
            is_focused,
            &mut line_decorations,
        );

        render_document(
            surface,
            inner,
            doc,
            view.offset,
            &text_annotations,
            highlights,
            theme,
            &mut line_decorations,
            &mut translated_positions,
        );

        if config.sticky_context.enable {
            Self::render_sticky_context(
                doc,
                view,
                surface,
                &self.sticky_nodes,
                &text_annotations,
                &mut line_decorations,
                &mut translated_positions,
                theme,
            );
        }

        Self::render_rulers(editor, doc, view, inner, surface, theme);

        // if we're not at the edge of the screen, draw a right border
        if viewport.right() != view.area.right() {
            let x = area.right();
            let border_style = theme.get("ui.window");
            for y in area.top()..area.bottom() {
                surface[(x, y)]
                    .set_symbol(tui::symbols::line::VERTICAL)
                    //.set_symbol(" ")
                    .set_style(border_style);
            }
        }

        Self::render_diagnostics(doc, view, inner, surface, theme);

        let statusline_area = view
            .area
            .clip_top(view.area.height.saturating_sub(1))
            .clip_bottom(1); // -1 from bottom to remove commandline

        let mut context =
            statusline::RenderContext::new(editor, doc, view, is_focused, &self.spinners);

        statusline::render(&mut context, statusline_area, surface);
    }

    pub fn render_rulers(
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        let editor_rulers = &editor.config().rulers;
        let ruler_theme = theme
            .try_get("ui.virtual.ruler")
            .unwrap_or_else(|| Style::default().bg(Color::Red));

        let rulers = doc
            .language_config()
            .and_then(|config| config.rulers.as_ref())
            .unwrap_or(editor_rulers);

        rulers
            .iter()
            // View might be horizontally scrolled, convert from absolute distance
            // from the 1st column to relative distance from left of viewport
            .filter_map(|ruler| ruler.checked_sub(1 + view.offset.horizontal_offset as u16))
            .filter(|ruler| ruler < &viewport.width)
            .map(|ruler| viewport.clip_left(ruler).with_width(1))
            .for_each(|area| surface.set_style(area, ruler_theme))
    }

    /// Gets the word under the cursor
    pub fn cursor_word<'a>(doc: &'a Document, view: &View) -> Option<&'a str> {
        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let char_under_cursor = text.get_char(cursor);
        if !char_under_cursor.map_or(false, char_is_word) {
            return None;
        }

        let chars_at_cursor = text.chars_at(cursor);
        let reversed_chars = chars_at_cursor.clone().reversed();
        let start = cursor.saturating_sub(reversed_chars.take_while(|c| char_is_word(*c)).count());
        let end = cursor + chars_at_cursor.take_while(|c| char_is_word(*c)).count();

        text.slice(start..end).as_str()
    }

    /// Calculates the ranges of the word under the cursor and returns the result
    fn calculate_cursor_word(
        doc: &Document,
        view: &View,
        viewport: Rect,
        scope_index: usize,
    ) -> Vec<(usize, std::ops::Range<usize>)> {
        let text = doc.text().slice(..);
        let mut result = Vec::new();

        let Some(cursor_word) = Self::cursor_word(doc, view) else {
            return result;
        };

        let row = text.char_to_line(view.offset.anchor.min(text.len_chars()));
        let line_range = {
            // Calculate the lines in view
            let last_line = text.len_lines().saturating_sub(1);
            let last_visible_line = (row + viewport.height as usize).min(last_line);
            let first_visible_line = row;

            first_visible_line..last_visible_line
        };

        let relevant_lines = text
            .slice(text.line_to_char(line_range.start)..text.line_to_char(line_range.end))
            .chunks();

        for (line, line_number) in relevant_lines.zip(line_range) {
            result.extend(
                line.match_indices(cursor_word)
                    .map(|(i, _)| i)
                    .filter(|i| line[..*i].chars().next_back().map_or(false, char_is_word))
                    .filter(|i| {
                        !line
                            .chars()
                            .nth(i + cursor_word.len())
                            .map_or(false, char_is_word)
                    })
                    .map(|i| line_number + i)
                    .map(|start| (scope_index, { start..start + cursor_word.len() })),
            );
        }

        result
    }

    /// Apply the decoration for the word to be highlighted
    pub fn collect_cursor_word_highlights(
        doc: &Document,
        editor: &Editor,
        view: &View,
        viewport: Rect,
        theme: &Theme,
    ) -> Option<Vec<(usize, std::ops::Range<usize>)>> {
        let scope_index = theme.find_scope_index("ui.wordmatch")?;
        let mut result: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
        let lsp_supports_highlights = doc
            .language_servers_with_feature(syntax::LanguageServerFeature::DocumentHighlight)
            .count()
            > 0;

        match lsp_supports_highlights {
            true => result.extend(
                editor
                    .cursor_highlights
                    .iter()
                    .map(|range| (scope_index, range.to_owned())),
            ),
            false => result.extend(Self::calculate_cursor_word(
                doc,
                view,
                viewport,
                scope_index,
            )),
        }

        Some(result)
    }

    pub fn overlay_syntax_highlights(
        doc: &Document,
        anchor: usize,
        height: u16,
        text_annotations: &TextAnnotations,
    ) -> Vec<(usize, std::ops::Range<usize>)> {
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));

        let range = {
            // Calculate viewport byte ranges:
            // Saturating subs to make it inclusive zero indexing.
            let last_line = text.len_lines().saturating_sub(1);
            let last_visible_line = (row + height as usize).saturating_sub(1).min(last_line);
            let start = text.line_to_byte(row.min(last_line));
            let end = text.line_to_byte(last_visible_line + 1);

            start..end
        };

        text_annotations.collect_overlay_highlights(range)
    }

    /// Get syntax highlights for a document in a view represented by the first line
    /// and column (`offset`) and the last line. This is done instead of using a view
    /// directly to enable rendering syntax highlighted docs anywhere (eg. picker preview)
    pub fn doc_syntax_highlights<'doc>(
        doc: &'doc Document,
        anchor: usize,
        height: u16,
        _theme: &Theme,
    ) -> Box<dyn Iterator<Item = HighlightEvent> + 'doc> {
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));

        let range = {
            // Calculate viewport byte ranges:
            // Saturating subs to make it inclusive zero indexing.
            let last_line = text.len_lines().saturating_sub(1);
            let last_visible_line = (row + height as usize).saturating_sub(1).min(last_line);
            let start = text.line_to_byte(row.min(last_line));
            let end = text.line_to_byte(last_visible_line + 1);

            start..end
        };

        match doc.syntax() {
            Some(syntax) => {
                let iter = syntax
                    // TODO: range doesn't actually restrict source, just highlight range
                    .highlight_iter(text.slice(..), Some(range), None)
                    .map(|event| event.unwrap())
                    .map(move |event| match event {
                        // TODO: use byte slices directly
                        // convert byte offsets to char offset
                        HighlightEvent::Source { start, end } => {
                            let start =
                                text.byte_to_char(ensure_grapheme_boundary_next_byte(text, start));
                            let end =
                                text.byte_to_char(ensure_grapheme_boundary_next_byte(text, end));
                            HighlightEvent::Source { start, end }
                        }
                        event => event,
                    });

                Box::new(iter)
            }
            None => Box::new(
                [HighlightEvent::Source {
                    start: text.byte_to_char(range.start),
                    end: text.byte_to_char(range.end),
                }]
                .into_iter(),
            ),
        }
    }

    pub fn doc_rainbow_highlights(
        doc: &Document,
        anchor: usize,
        height: u16,
        theme: &Theme,
    ) -> Vec<(usize, std::ops::Range<usize>)> {
        let syntax = match doc.syntax() {
            Some(syntax) => syntax,
            None => return Vec::new(),
        };

        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));

        // calculate viewport byte ranges
        let last_line = doc.text().len_lines().saturating_sub(1);
        let last_visible_line = (row + height as usize).saturating_sub(1).min(last_line);
        let visible_start = text.line_to_byte(row.min(last_line));
        let visible_end = text.line_to_byte(last_visible_line + 1);

        // The calculation for the current nesting level for rainbow highlights
        // depends on where we start the iterator from. For accuracy, we start
        // the iterator further back than the viewport: at the start of the containing
        // non-root syntax-tree node. Any spans that are off-screen are truncated when
        // the spans are merged via [syntax::merge].
        let syntax_node_start =
            syntax::child_for_byte_range(syntax.tree().root_node(), visible_start..visible_start)
                .map_or(visible_start, |node| node.byte_range().start);
        let syntax_node_range = syntax_node_start..visible_end;

        let mut spans = syntax.rainbow_spans(text, Some(syntax_node_range), theme.rainbow_length());

        for (_highlight, range) in spans.iter_mut() {
            let start = text.byte_to_char(ensure_grapheme_boundary_next_byte(text, range.start));
            let end = text.byte_to_char(ensure_grapheme_boundary_next_byte(text, range.end));

            *range = start..end;
        }

        spans
    }

    /// Get highlight spans for document diagnostics
    pub fn doc_diagnostics_highlights(
        doc: &Document,
        theme: &Theme,
    ) -> [Vec<(usize, std::ops::Range<usize>)>; 5] {
        use helix_core::diagnostic::Severity;
        let get_scope_of = |scope| {
            theme
            .find_scope_index_exact(scope)
            // get one of the themes below as fallback values
            .or_else(|| theme.find_scope_index_exact("diagnostic"))
            .or_else(|| theme.find_scope_index_exact("ui.cursor"))
            .or_else(|| theme.find_scope_index_exact("ui.selection"))
            .expect(
                "at least one of the following scopes must be defined in the theme: `diagnostic`, `ui.cursor`, or `ui.selection`",
            )
        };

        // basically just queries the theme color defined in the config
        let hint = get_scope_of("diagnostic.hint");
        let info = get_scope_of("diagnostic.info");
        let warning = get_scope_of("diagnostic.warning");
        let error = get_scope_of("diagnostic.error");
        let r#default = get_scope_of("diagnostic"); // this is a bit redundant but should be fine

        let mut default_vec: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
        let mut info_vec = Vec::new();
        let mut hint_vec = Vec::new();
        let mut warning_vec = Vec::new();
        let mut error_vec = Vec::new();

        for diagnostic in doc.diagnostics() {
            // Separate diagnostics into different Vecs by severity.
            let (vec, scope) = match diagnostic.severity {
                Some(Severity::Info) => (&mut info_vec, info),
                Some(Severity::Hint) => (&mut hint_vec, hint),
                Some(Severity::Warning) => (&mut warning_vec, warning),
                Some(Severity::Error) => (&mut error_vec, error),
                _ => (&mut default_vec, r#default),
            };

            // If any diagnostic overlaps ranges with the prior diagnostic,
            // merge the two together. Otherwise push a new span.
            match vec.last_mut() {
                Some((_, range)) if diagnostic.range.start <= range.end => {
                    // This branch merges overlapping diagnostics, assuming that the current
                    // diagnostic starts on range.start or later. If this assertion fails,
                    // we will discard some part of `diagnostic`. This implies that
                    // `doc.diagnostics()` is not sorted by `diagnostic.range`.
                    debug_assert!(range.start <= diagnostic.range.start);
                    range.end = diagnostic.range.end.max(range.end)
                }
                _ => vec.push((scope, diagnostic.range.start..diagnostic.range.end)),
            }
        }

        [default_vec, info_vec, hint_vec, warning_vec, error_vec]
    }

    /// Get highlight spans for selections in a document view.
    pub fn doc_selection_highlights(
        mode: Mode,
        doc: &Document,
        view: &View,
        theme: &Theme,
        cursor_shape_config: &CursorShapeConfig,
    ) -> Vec<(usize, std::ops::Range<usize>)> {
        let text = doc.text().slice(..);
        let selection = doc.selection(view.id);
        let primary_idx = selection.primary_index();

        let cursorkind = cursor_shape_config.from_mode(mode);
        let cursor_is_block = cursorkind == CursorKind::Block;

        let selection_scope = theme
            .find_scope_index_exact("ui.selection")
            .expect("could not find `ui.selection` scope in the theme!");
        let primary_selection_scope = theme
            .find_scope_index_exact("ui.selection.primary")
            .unwrap_or(selection_scope);

        let base_cursor_scope = theme
            .find_scope_index_exact("ui.cursor")
            .unwrap_or(selection_scope);
        let base_primary_cursor_scope = theme
            .find_scope_index("ui.cursor.primary")
            .unwrap_or(base_cursor_scope);
        let cursor_scope = match mode {
            Mode::Insert => theme.find_scope_index_exact("ui.cursor.insert"),
            Mode::Select => theme.find_scope_index_exact("ui.cursor.select"),
            Mode::Normal => theme.find_scope_index_exact("ui.cursor.normal"),
        }
        .unwrap_or(base_cursor_scope);

        let primary_cursor_scope = match mode {
            Mode::Insert => theme.find_scope_index_exact("ui.cursor.primary.insert"),
            Mode::Select => theme.find_scope_index_exact("ui.cursor.primary.select"),
            Mode::Normal => theme.find_scope_index_exact("ui.cursor.primary.normal"),
        }
        .unwrap_or(base_primary_cursor_scope);

        let mut spans: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
        for (i, range) in selection.iter().enumerate() {
            let selection_is_primary = i == primary_idx;
            let (cursor_scope, selection_scope) = if selection_is_primary {
                (primary_cursor_scope, primary_selection_scope)
            } else {
                (cursor_scope, selection_scope)
            };

            // Special-case: cursor at end of the rope.
            if range.head == range.anchor && range.head == text.len_chars() {
                if !selection_is_primary || cursor_is_block {
                    // Bar and underline cursors are drawn by the terminal
                    // BUG: If the editor area loses focus while having a bar or
                    // underline cursor (eg. when a regex prompt has focus) then
                    // the primary cursor will be invisible. This doesn't happen
                    // with block cursors since we manually draw *all* cursors.
                    spans.push((cursor_scope, range.head..range.head + 1));
                }
                continue;
            }

            let range = range.min_width_1(text);
            if range.head > range.anchor {
                // Standard case.
                let cursor_start = prev_grapheme_boundary(text, range.head);
                // non block cursors look like they exclude the cursor
                let selection_end =
                    if selection_is_primary && !cursor_is_block && mode != Mode::Insert {
                        range.head
                    } else {
                        cursor_start
                    };
                spans.push((selection_scope, range.anchor..selection_end));
                if !selection_is_primary || cursor_is_block {
                    spans.push((cursor_scope, cursor_start..range.head));
                }
            } else {
                // Reverse case.
                let cursor_end = next_grapheme_boundary(text, range.head);
                if !selection_is_primary || cursor_is_block {
                    spans.push((cursor_scope, range.head..cursor_end));
                }
                // non block cursors look like they exclude the cursor
                let selection_start = if selection_is_primary
                    && !cursor_is_block
                    && !(mode == Mode::Insert && cursor_end == range.anchor)
                {
                    range.head
                } else {
                    cursor_end
                };
                spans.push((selection_scope, selection_start..range.anchor));
            }
        }

        spans
    }

    /// Render brace match, etc (meant for the focused view only)
    pub fn highlight_focused_view_elements(
        view: &View,
        doc: &Document,
        theme: &Theme,
    ) -> Vec<(usize, std::ops::Range<usize>)> {
        // Highlight matching braces
        if let Some(syntax) = doc.syntax() {
            let text = doc.text().slice(..);
            use helix_core::match_brackets;
            let pos = doc.selection(view.id).primary().cursor(text);

            if let Some(pos) = match_brackets::find_matching_bracket(syntax, doc.text(), pos) {
                // ensure col is on screen
                if let Some(highlight) = theme.find_scope_index_exact("ui.cursor.match") {
                    return vec![(highlight, pos..pos + 1)];
                }
            }
        }
        Vec::new()
    }

    /// Render bufferline at the top
    pub fn render_bufferline(editor: &Editor, viewport: Rect, surface: &mut Surface) {
        let scratch = PathBuf::from(SCRATCH_BUFFER_NAME); // default filename to use for scratch buffer
        surface.clear_with(
            viewport,
            editor
                .theme
                .try_get("ui.bufferline.background")
                .unwrap_or_else(|| editor.theme.get("ui.statusline")),
        );

        let bufferline_active = editor
            .theme
            .try_get("ui.bufferline.active")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.active"));

        let bufferline_inactive = editor
            .theme
            .try_get("ui.bufferline")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.inactive"));

        let mut x = viewport.x;
        let current_doc = view!(editor).doc;

        for doc in editor.documents() {
            let fname = doc
                .path()
                .unwrap_or(&scratch)
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or_default();

            let style = if current_doc == doc.id() {
                bufferline_active
            } else {
                bufferline_inactive
            };

            let text = format!(" {}{} ", fname, if doc.is_modified() { "[+]" } else { "" });
            let used_width = viewport.x.saturating_sub(x);
            let rem_width = surface.area.width.saturating_sub(used_width);

            x = surface
                .set_stringn(x, viewport.y, text, rem_width as usize, style)
                .0;

            if x >= surface.area.right() {
                break;
            }
        }
    }

    pub fn render_gutter<'d>(
        editor: &'d Editor,
        doc: &'d Document,
        context: &'d Option<Vec<StickyNode>>,
        view: &View,
        theme: &Theme,
        is_focused: bool,
        line_decorations: &mut Vec<Box<(dyn LineDecoration + 'd)>>,
    ) {
        let text = doc.text().slice(..);
        let cursors: Rc<[_]> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let mut offset = 0;
        let viewport = view.area;

        let gutter_style = theme.get("ui.gutter");
        let gutter_selected_style = theme.get("ui.gutter.selected");
        let gutter_style_virtual = theme.get("ui.gutter.virtual");
        let gutter_selected_style_virtual = theme.get("ui.gutter.selected.virtual");

        for gutter_type in view.gutters() {
            let mut gutter = gutter_type.style(editor, doc, view, theme, is_focused);
            let width = gutter_type.width(view, doc);
            // avoid lots of small allocations by reusing a text buffer for each line
            let mut text_to_draw = String::with_capacity(width);
            let cursors = cursors.clone();
            let gutter_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                // TODO handle softwrap in gutters
                let selected = cursors.contains(&pos.doc_line);
                let x = viewport.x + offset;
                let y = viewport.y + pos.visual_line;

                let gutter_style = match (selected, pos.first_visual_line) {
                    (false, true) => gutter_style,
                    (true, true) => gutter_selected_style,
                    (false, false) => gutter_style_virtual,
                    (true, false) => gutter_selected_style_virtual,
                };

                let mut doc_line = Some(pos.doc_line);
                if let Some(current_context) = context
                    .as_ref()
                    .and_then(|c| c.iter().find(|n| n.visual_line == pos.visual_line))
                {
                    doc_line = if current_context.indicator.is_some() {
                        None
                    } else {
                        Some(current_context.line)
                    };
                }

                if let Some(style) =
                    gutter(doc_line, selected, pos.first_visual_line, &mut text_to_draw)
                {
                    renderer.surface.set_stringn(
                        x,
                        y,
                        &text_to_draw,
                        width,
                        gutter_style.patch(style),
                    );
                } else {
                    renderer.surface.set_style(
                        Rect {
                            x,
                            y,
                            width: width as u16,
                            height: 1,
                        },
                        gutter_style,
                    );
                }
                text_to_draw.clear();
            };
            line_decorations.push(Box::new(gutter_decoration));

            offset += width as u16;
        }
    }

    pub fn render_diagnostics(
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        use helix_core::diagnostic::Severity;
        use tui::{
            layout::Alignment,
            text::Text,
            widgets::{Paragraph, Widget, Wrap},
        };

        let cursor = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));

        let diagnostics = doc.shown_diagnostics().filter(|diagnostic| {
            diagnostic.range.start <= cursor && diagnostic.range.end >= cursor
        });

        let warning = theme.get("warning");
        let error = theme.get("error");
        let info = theme.get("info");
        let hint = theme.get("hint");

        let mut lines = Vec::new();
        let background_style = theme.get("ui.background");
        for diagnostic in diagnostics {
            let style = Style::reset()
                .patch(background_style)
                .patch(match diagnostic.severity {
                    Some(Severity::Error) => error,
                    Some(Severity::Warning) | None => warning,
                    Some(Severity::Info) => info,
                    Some(Severity::Hint) => hint,
                });
            let text = Text::styled(&diagnostic.message, style);
            lines.extend(text.lines);
            let code = diagnostic.code.as_ref().map(|x| match x {
                NumberOrString::Number(n) => format!("({n})"),
                NumberOrString::String(s) => format!("({s})"),
            });
            if let Some(code) = code {
                let span = Span::styled(code, style);
                lines.push(span.into());
            }
        }

        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Right)
            .wrap(Wrap { trim: true });
        let width = 100.min(viewport.width);
        let height = 15.min(viewport.height);
        paragraph.render(
            Rect::new(viewport.right() - width, viewport.y + 1, width, height),
            surface,
        );
    }

    /// Render the sticky context
    #[allow(clippy::too_many_arguments)]
    pub fn render_sticky_context(
        doc: &Document,
        view: &View,
        surface: &mut Surface,
        context: &Option<Vec<StickyNode>>,
        doc_annotations: &TextAnnotations,
        line_decoration: &mut [Box<dyn LineDecoration + '_>],
        translated_positions: &mut [TranslatedPosition],
        theme: &Theme,
    ) {
        let Some(context) = context else {
            return;
        };

        let text = doc.text().slice(..);
        let viewport = view.inner_area(doc);

        // backup (status line) shall always exist
        let status_line_style = theme
            .try_get("ui.statusline")
            .expect("`ui.statusline` exists");

        // define sticky context styles
        let context_style = theme
            .try_get("ui.sticky.context")
            .unwrap_or(status_line_style);
        let indicator_style = theme
            .try_get("ui.sticky.indicator")
            .unwrap_or(status_line_style);

        let mut context_area = viewport;
        context_area.height = 1;

        for node in context {
            surface.clear_with(context_area, context_style);

            let mut new_offset = view.offset;
            let mut line_context_area = context_area;

            if let Some(indicator) = node.indicator.as_deref() {
                // set the indicator
                surface.set_stringn(
                    line_context_area.x,
                    line_context_area.y,
                    indicator,
                    indicator.len(),
                    indicator_style,
                );
                continue;
            }

            // get the len of bytes of the text that will be written (the "definition" line)
            let line = text.line(node.line);
            let tab_count = line.chars().take_while(|c| *c == '\t').count();

            let already_written =
                (line.len_bytes() + tab_count.saturating_mul(doc.tab_width() - 1)) as u16;

            let dots = "...";

            let mut cleared_virtual_text_annotations = doc_annotations.clone();
            cleared_virtual_text_annotations.clear_line_annotations();
            cleared_virtual_text_annotations.clear_inline_annotations();

            // if the definition of the function contains multiple lines
            if node.has_context_end {
                let last_line = text.byte_to_line(node.byte_range.end);
                let overdraw_offset = text
                    .line(last_line)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count()
                    + 1;

                // handle tabs
                let overdraw_offset =
                    overdraw_offset + tab_count.saturating_mul(doc.tab_width() - 1);

                // calculation of the correct space on where the end of the signature
                // should be drawn at
                let mut additional_area = line_context_area;
                additional_area.x +=
                    (already_written + dots.len() as u16).saturating_sub(overdraw_offset as u16);

                // render the end of the function definition
                let mut renderer = TextRenderer::new(
                    surface,
                    doc,
                    theme,
                    view.offset.horizontal_offset,
                    additional_area,
                );
                new_offset.anchor = text.byte_to_char(node.byte_range.end);
                let highlights = Self::doc_syntax_highlights(doc, new_offset.anchor, 1, theme);

                render_text(
                    &mut renderer,
                    text,
                    new_offset,
                    &doc.text_format(additional_area.width, Some(theme)),
                    &cleared_virtual_text_annotations,
                    highlights,
                    theme,
                    line_decoration,
                    translated_positions,
                );

                // draw the "..." with the keyword.operator style
                let new_x_location =
                    (already_written + line_context_area.x).saturating_sub(match doc.line_ending {
                        helix_core::LineEnding::Crlf => 2,
                        helix_core::LineEnding::LF => 1,
                    });

                surface.set_stringn(
                    new_x_location,
                    additional_area.y,
                    dots,
                    dots.len(),
                    theme.get("keyword.operator"),
                );
            }

            new_offset.anchor = text.byte_to_char(node.byte_range.start);

            // get all highlights from the latest point
            let highlights = Self::doc_syntax_highlights(doc, new_offset.anchor, 1, theme);

            let mut renderer = TextRenderer::new(
                surface,
                doc,
                theme,
                view.offset.horizontal_offset,
                line_context_area,
            );

            // limit the width to its size - 1, so that it won't draw trailing whitespace characters
            line_context_area.width = already_written - 1;

            render_text(
                &mut renderer,
                text,
                new_offset,
                &doc.text_format(line_context_area.width, Some(theme)),
                &cleared_virtual_text_annotations,
                highlights,
                theme,
                line_decoration,
                translated_positions,
            );

            // next node
            context_area.y += 1;
        }
    }

    fn get_context_paired_range(
        query_match: &QueryMatch,
        start_index: u32,
        end_index: u32,
        top_first_byte: usize,
        last_scan_byte: usize,
    ) -> Option<std::ops::Range<usize>> {
        let end_nodes: Vec<_> = query_match.nodes_for_capture_index(end_index).collect();
        query_match
            .nodes_for_capture_index(start_index)
            .flat_map(move |context| {
                end_nodes
                    .clone()
                    .into_iter()
                    .filter(move |it| {
                        let start_range = context.byte_range();
                        let end = it.start_byte();
                        start_range.contains(&end)
                            && start_range.contains(&top_first_byte)
                            && start_range.contains(&last_scan_byte)
                            // only match @context.end nodes that aren't at the end of the line
                            && context.start_position().row != it.start_position().row
                    })
                    // in some cases, the start byte of a block is on the next line
                    // which causes to show the actual first line of content instead of
                    // the actual wanted "end of signature" line
                    .map(move |it| context.start_byte()..it.start_byte().saturating_sub(1))
            })
            .next()
    }

    /// Calculates the sticky nodes
    fn calculate_sticky_nodes(
        nodes: &Option<Vec<StickyNode>>,
        doc: &Document,
        view: &View,
        config: &helix_view::editor::Config,
    ) -> Option<Vec<StickyNode>> {
        let syntax = doc.syntax()?;
        let tree = syntax.tree();
        let text = doc.text().slice(..);
        let viewport = view.inner_area(doc);
        let cursor_byte = text.char_to_byte(doc.selection(view.id).primary().cursor(text));

        // Use the cached nodes to determine the current topmost viewport
        let anchor_line = text.char_to_line(view.offset.anchor);
        let top_first_byte =
            text.line_to_byte(anchor_line + nodes.as_deref().map_or(0, |v| v.len()));

        let last_scan_byte = if config.sticky_context.follow_cursor {
            cursor_byte
        } else {
            top_first_byte
        };

        let visual_cursor_pos = view
            .screen_coords_at_pos(doc, text, cursor_byte)
            .unwrap_or_default()
            .row as u16;

        if visual_cursor_pos == 0 {
            return None;
        }

        // nothing has changed, so the cached result can be returned
        if let Some(nodes) = nodes {
            if nodes.iter().any(|node| {
                (node.top_first_byte == top_first_byte && node.cursor_byte == last_scan_byte)
                    && (visual_cursor_pos as usize) >= nodes.len()
            }) {
                return Some(nodes.to_vec());
            }
        }

        let context_nodes = doc
            .language_config()
            .and_then(|lang| lang.context_query())?;

        let start_index = context_nodes.query.capture_index_for_name("context")?;
        let end_index = context_nodes
            .query
            .capture_index_for_name("context.end")
            .unwrap_or(start_index);

        // context is list of numbers of lines that should be rendered in the LSP context
        let mut context: HashSet<StickyNode> = HashSet::new();

        let mut cursor = QueryCursor::new();

        // only run the query from start to the cursor location
        cursor.set_byte_range(0..last_scan_byte);
        let query = &context_nodes.query;
        let query_nodes = cursor.matches(query, tree.root_node(), RopeProvider(text));

        for matched_node in query_nodes {
            // find @context.end nodes
            let node_byte_range = Self::get_context_paired_range(
                &matched_node,
                start_index,
                end_index,
                top_first_byte,
                last_scan_byte,
            );
            for node in matched_node.nodes_for_capture_index(start_index) {
                if (!node.byte_range().contains(&last_scan_byte)
                    || !node.byte_range().contains(&top_first_byte))
                    && node_byte_range.is_none()
                {
                    continue;
                }

                context.insert(StickyNode {
                    line: node.start_position().row,
                    visual_line: 0,
                    byte_range: node_byte_range
                        .as_ref()
                        .unwrap_or(&(node.start_byte()..node.start_byte()))
                        .clone(),
                    indicator: None,
                    top_first_byte,
                    cursor_byte: last_scan_byte,
                    has_context_end: node_byte_range.is_some(),
                });
            }
        }
        // context should be filled by now
        if context.is_empty() {
            return None;
        }

        // always cap the maximum amount of sticky contextes to 1/3 of the viewport
        // unless configured otherwise
        let max_lines = config.sticky_context.max_lines;
        let max_nodes_amount = if max_lines == 0 {
            viewport.height as usize / 3
        } else {
            max_lines.min(viewport.height) as usize
        };

        let mut result: Vec<StickyNode> = context.into_iter().collect();
        result.sort_by(|lnode, rnode| lnode.line.cmp(&rnode.line));

        result = result
            .into_iter()
            // only take the nodes until 1 / 3 of the viewport is reached or the maximum amount of sticky nodes
            .take(max_nodes_amount)
            .enumerate()
            .take_while(|(i, _)| {
                *i + Into::<usize>::into(config.sticky_context.indicator)
                    != visual_cursor_pos as usize
            }) // also only nodes that don't overlap with the visual cursor position
            .map(|(i, node)| {
                let mut new_node = node;
                new_node.visual_line = i as u16;
                new_node
            })
            .collect();

        if config.sticky_context.indicator {
            let str = "─".repeat(viewport.width as usize);

            result.push(StickyNode {
                line: usize::MAX,
                visual_line: result.len() as u16,
                byte_range: 0..0,
                indicator: Some(str),
                top_first_byte,
                cursor_byte: last_scan_byte,
                has_context_end: false,
            });
        }

        Some(result)
    }

    /// Apply the highlighting on the lines where a cursor is active
    pub fn cursorline_decorator(
        doc: &Document,
        view: &View,
        theme: &Theme,
    ) -> Box<dyn LineDecoration> {
        let text = doc.text().slice(..);
        // TODO only highlight the visual line that contains the cursor instead of the full visual line
        let primary_line = doc.selection(view.id).primary().cursor_line(text);

        // The secondary_lines do contain the primary_line, it doesn't matter
        // as the else-if clause in the loop later won't test for the
        // secondary_lines if primary_line == line.
        // It's used inside a loop so the collect isn't needless:
        // https://github.com/rust-lang/rust-clippy/issues/6164
        #[allow(clippy::needless_collect)]
        let secondary_lines: Vec<_> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let primary_style = theme.get("ui.cursorline.primary");
        let secondary_style = theme.get("ui.cursorline.secondary");
        let viewport = view.area;

        let line_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
            let area = Rect::new(viewport.x, viewport.y + pos.visual_line, viewport.width, 1);
            if primary_line == pos.doc_line {
                renderer.surface.set_style(area, primary_style);
            } else if secondary_lines.binary_search(&pos.doc_line).is_ok() {
                renderer.surface.set_style(area, secondary_style);
            }
        };

        Box::new(line_decoration)
    }

    /// Apply the highlighting on the columns where a cursor is active
    pub fn highlight_cursorcolumn(
        doc: &Document,
        view: &View,
        surface: &mut Surface,
        theme: &Theme,
        viewport: Rect,
        text_annotations: &TextAnnotations,
    ) {
        let text = doc.text().slice(..);

        // Manual fallback behaviour:
        // ui.cursorcolumn.{p/s} -> ui.cursorcolumn -> ui.cursorline.{p/s}
        let primary_style = theme
            .try_get_exact("ui.cursorcolumn.primary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.primary"));
        let secondary_style = theme
            .try_get_exact("ui.cursorcolumn.secondary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.secondary"));

        let inner_area = view.inner_area(doc);

        let selection = doc.selection(view.id);
        let primary = selection.primary();
        let text_format = doc.text_format(viewport.width, None);
        for range in selection.iter() {
            let is_primary = primary == *range;
            let cursor = range.cursor(text);

            let Position { col, .. } =
                visual_offset_from_block(text, cursor, cursor, &text_format, text_annotations).0;

            // if the cursor is horizontally in the view
            if col >= view.offset.horizontal_offset
                && inner_area.width > (col - view.offset.horizontal_offset) as u16
            {
                let area = Rect::new(
                    inner_area.x + (col - view.offset.horizontal_offset) as u16,
                    view.area.y,
                    1,
                    view.area.height,
                );
                if is_primary {
                    surface.set_style(area, primary_style)
                } else {
                    surface.set_style(area, secondary_style)
                }
            }
        }
    }

    /// Handle events by looking them up in `self.keymaps`. Returns None
    /// if event was handled (a command was executed or a subkeymap was
    /// activated). Only KeymapResult::{NotFound, Cancelled} is returned
    /// otherwise.
    fn handle_keymap_event(
        &mut self,
        mode: Mode,
        cxt: &mut commands::Context,
        event: KeyEvent,
    ) -> Option<KeymapResult> {
        let mut last_mode = mode;
        self.pseudo_pending.extend(self.keymaps.pending());
        let key_result = self.keymaps.get(mode, event);
        cxt.editor.autoinfo = self.keymaps.sticky().map(|node| node.infobox());

        let mut execute_command = |command: &commands::MappableCommand| {
            command.execute(cxt);
            let current_mode = cxt.editor.mode();
            match (last_mode, current_mode) {
                (Mode::Normal, Mode::Insert) => {
                    // HAXX: if we just entered insert mode from normal, clear key buf
                    // and record the command that got us into this mode.

                    // how we entered insert mode is important, and we should track that so
                    // we can repeat the side effect.
                    self.last_insert.0 = command.clone();
                    self.last_insert.1.clear();

                    commands::signature_help_impl(cxt, commands::SignatureHelpInvoked::Automatic);
                }
                (Mode::Insert, Mode::Normal) => {
                    // if exiting insert mode, remove completion
                    self.clear_completion(cxt.editor);
                    cxt.editor.completion_request_handle = None;

                    // TODO: Use an on_mode_change hook to remove signature help
                    cxt.jobs.callback(async {
                        let call: job::Callback =
                            Callback::EditorCompositor(Box::new(|_editor, compositor| {
                                compositor.remove(SignatureHelp::ID);
                            }));
                        Ok(call)
                    });
                }
                _ => (),
            }
            last_mode = current_mode;
        };

        match &key_result {
            KeymapResult::Matched(command) => {
                execute_command(command);
            }
            KeymapResult::Pending(node) => cxt.editor.autoinfo = Some(node.infobox()),
            KeymapResult::MatchedSequence(commands) => {
                for command in commands {
                    execute_command(command);
                }
            }
            KeymapResult::NotFound | KeymapResult::Cancelled(_) => return Some(key_result),
        }
        None
    }

    fn insert_mode(&mut self, cx: &mut commands::Context, event: KeyEvent) {
        if let Some(keyresult) = self.handle_keymap_event(Mode::Insert, cx, event) {
            match keyresult {
                KeymapResult::NotFound => {
                    if let Some(ch) = event.char() {
                        commands::insert::insert_char(cx, ch)
                    }
                }
                KeymapResult::Cancelled(pending) => {
                    for ev in pending {
                        match ev.char() {
                            Some(ch) => commands::insert::insert_char(cx, ch),
                            None => {
                                if let KeymapResult::Matched(command) =
                                    self.keymaps.get(Mode::Insert, ev)
                                {
                                    command.execute(cx);
                                }
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    fn command_mode(&mut self, mode: Mode, cxt: &mut commands::Context, event: KeyEvent) {
        match (event, cxt.editor.count) {
            // count handling
            (key!(i @ '0'), Some(_)) | (key!(i @ '1'..='9'), _) => {
                let i = i.to_digit(10).unwrap() as usize;
                cxt.editor.count =
                    std::num::NonZeroUsize::new(cxt.editor.count.map_or(i, |c| c.get() * 10 + i));
            }
            // special handling for repeat operator
            (key!('.'), _) if self.keymaps.pending().is_empty() => {
                for _ in 0..cxt.editor.count.map_or(1, NonZeroUsize::into) {
                    // first execute whatever put us into insert mode
                    self.last_insert.0.execute(cxt);
                    let mut last_savepoint = None;
                    let mut last_request_savepoint = None;
                    // then replay the inputs
                    for key in self.last_insert.1.clone() {
                        match key {
                            InsertEvent::Key(key) => self.insert_mode(cxt, key),
                            InsertEvent::CompletionApply {
                                trigger_offset,
                                changes,
                            } => {
                                let (view, doc) = current!(cxt.editor);

                                if let Some(last_savepoint) = last_savepoint.as_deref() {
                                    doc.restore(view, last_savepoint, true);
                                }

                                let text = doc.text().slice(..);
                                let cursor = doc.selection(view.id).primary().cursor(text);

                                let shift_position =
                                    |pos: usize| -> usize { pos + cursor - trigger_offset };

                                let tx = Transaction::change(
                                    doc.text(),
                                    changes.iter().cloned().map(|(start, end, t)| {
                                        (shift_position(start), shift_position(end), t)
                                    }),
                                );
                                doc.apply(&tx, view.id);
                            }
                            InsertEvent::TriggerCompletion => {
                                last_savepoint = take(&mut last_request_savepoint);
                            }
                            InsertEvent::RequestCompletion => {
                                let (view, doc) = current!(cxt.editor);
                                last_request_savepoint = Some(doc.savepoint(view));
                            }
                        }
                    }
                }
                cxt.editor.count = None;
            }
            _ => {
                // set the count
                cxt.count = cxt.editor.count;
                // TODO: edge case: 0j -> reset to 1
                // if this fails, count was Some(0)
                // debug_assert!(cxt.count != 0);

                // set the register
                cxt.register = cxt.editor.selected_register.take();

                self.handle_keymap_event(mode, cxt, event);
                if self.keymaps.pending().is_empty() {
                    cxt.editor.count = None
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_completion(
        &mut self,
        editor: &mut Editor,
        savepoint: Arc<SavePoint>,
        items: Vec<CompletionItem>,
        start_offset: usize,
        trigger_offset: usize,
        size: Rect,
    ) -> Option<Rect> {
        let mut completion =
            Completion::new(editor, savepoint, items, start_offset, trigger_offset);

        if completion.is_empty() {
            // skip if we got no completion results
            return None;
        }

        let area = completion.area(size, editor);
        editor.last_completion = None;
        self.last_insert.1.push(InsertEvent::TriggerCompletion);

        // TODO : propagate required size on resize to completion too
        completion.required_size((size.width, size.height));
        self.completion = Some(completion);
        Some(area)
    }

    pub fn clear_completion(&mut self, editor: &mut Editor) {
        self.completion = None;
        if let Some(last_completion) = editor.last_completion.take() {
            match last_completion {
                CompleteAction::Applied {
                    trigger_offset,
                    changes,
                } => self.last_insert.1.push(InsertEvent::CompletionApply {
                    trigger_offset,
                    changes,
                }),
                CompleteAction::Selected { savepoint } => {
                    let (view, doc) = current!(editor);
                    doc.restore(view, &savepoint, false);
                }
            }
        }

        // Clear any savepoints
        editor.clear_idle_timer(); // don't retrigger
    }

    pub fn handle_idle_timeout(&mut self, cx: &mut commands::Context) -> EventResult {
        commands::compute_inlay_hints_for_all_views(cx.editor, cx.jobs);

        if let Some(completion) = &mut self.completion {
            return if completion.ensure_item_resolved(cx) {
                EventResult::Consumed(None)
            } else {
                EventResult::Ignored(None)
            };
        }

        if cx.editor.config().cursor_word {
            crate::commands::highlight_symbol_under_cursor(cx);
        }

        if cx.editor.mode != Mode::Insert || !cx.editor.config().auto_completion {
            return EventResult::Ignored(None);
        }

        crate::commands::insert::idle_completion(cx);

        EventResult::Consumed(None)
    }
}

impl EditorView {
    fn handle_mouse_event(
        &mut self,
        event: &MouseEvent,
        cxt: &mut commands::Context,
    ) -> EventResult {
        if event.kind != MouseEventKind::Moved {
            cxt.editor.reset_idle_timer();
        }

        let config = cxt.editor.config();
        let MouseEvent {
            kind,
            row,
            column,
            modifiers,
            ..
        } = *event;

        let pos_and_view = |editor: &Editor, row, column, ignore_virtual_text| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.pos_at_screen_coords(
                    &editor.documents[&view.doc],
                    row,
                    column,
                    ignore_virtual_text,
                )
                .map(|pos| (pos, view.id))
            })
        };

        let gutter_coords_and_view = |editor: &Editor, row, column| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.gutter_coords_at_screen_coords(row, column)
                    .map(|coords| (coords, view.id))
            })
        };

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let editor = &mut cxt.editor;

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);

                    if modifiers == KeyModifiers::ALT {
                        let selection = doc.selection(view_id).clone();
                        doc.set_selection(view_id, selection.push(Range::point(pos)));
                    } else {
                        doc.set_selection(view_id, Selection::point(pos));
                    }

                    editor.focus(view_id);
                    editor.ensure_cursor_in_view(view_id);

                    return EventResult::Consumed(None);
                }

                if let Some((coords, view_id)) = gutter_coords_and_view(editor, row, column) {
                    editor.focus(view_id);

                    let (view, doc) = current!(cxt.editor);

                    let path = match doc.path() {
                        Some(path) => path.clone(),
                        None => return EventResult::Ignored(None),
                    };

                    if let Some(char_idx) =
                        view.pos_at_visual_coords(doc, coords.row as u16, coords.col as u16, true)
                    {
                        let line = doc.text().char_to_line(char_idx);
                        commands::dap_toggle_breakpoint_impl(cxt, path, line);
                        return EventResult::Consumed(None);
                    }
                }

                EventResult::Ignored(None)
            }

            MouseEventKind::Drag(MouseButton::Left) => {
                let (view, doc) = current!(cxt.editor);

                let pos = match view.pos_at_screen_coords(doc, row, column, true) {
                    Some(pos) => pos,
                    None => return EventResult::Ignored(None),
                };

                let mut selection = doc.selection(view.id).clone();
                let primary = selection.primary_mut();
                *primary = primary.put_cursor(doc.text().slice(..), pos, true);
                doc.set_selection(view.id, selection);
                let view_id = view.id;
                cxt.editor.ensure_cursor_in_view(view_id);
                EventResult::Consumed(None)
            }

            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let current_view = cxt.editor.tree.focus;

                let direction = match event.kind {
                    MouseEventKind::ScrollUp => Direction::Backward,
                    MouseEventKind::ScrollDown => Direction::Forward,
                    _ => unreachable!(),
                };

                match pos_and_view(cxt.editor, row, column, false) {
                    Some((_, view_id)) => cxt.editor.tree.focus = view_id,
                    None => return EventResult::Ignored(None),
                }

                let offset = config.scroll_lines.unsigned_abs();
                commands::scroll(cxt, offset, direction);

                cxt.editor.tree.focus = current_view;
                cxt.editor.ensure_cursor_in_view(current_view);

                EventResult::Consumed(None)
            }

            MouseEventKind::Up(MouseButton::Left) => {
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                let (view, doc) = current!(cxt.editor);

                if doc
                    .selection(view.id)
                    .primary()
                    .slice(doc.text().slice(..))
                    .len_chars()
                    <= 1
                {
                    return EventResult::Ignored(None);
                }

                commands::MappableCommand::yank_main_selection_to_primary_clipboard.execute(cxt);

                EventResult::Consumed(None)
            }

            MouseEventKind::Up(MouseButton::Right) => {
                if let Some((coords, view_id)) = gutter_coords_and_view(cxt.editor, row, column) {
                    cxt.editor.focus(view_id);

                    let (view, doc) = current!(cxt.editor);
                    if let Some(pos) =
                        view.pos_at_visual_coords(doc, coords.row as u16, coords.col as u16, true)
                    {
                        doc.set_selection(view_id, Selection::point(pos));
                        if modifiers == KeyModifiers::ALT {
                            commands::MappableCommand::dap_edit_log.execute(cxt);
                        } else {
                            commands::MappableCommand::dap_edit_condition.execute(cxt);
                        }

                        return EventResult::Consumed(None);
                    }
                }

                EventResult::Ignored(None)
            }

            MouseEventKind::Up(MouseButton::Middle) => {
                let editor = &mut cxt.editor;
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                if modifiers == KeyModifiers::ALT {
                    commands::MappableCommand::replace_selections_with_primary_clipboard
                        .execute(cxt);

                    return EventResult::Consumed(None);
                }

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);
                    doc.set_selection(view_id, Selection::point(pos));
                    cxt.editor.focus(view_id);
                    commands::MappableCommand::paste_primary_clipboard_before.execute(cxt);

                    return EventResult::Consumed(None);
                }

                EventResult::Ignored(None)
            }

            _ => EventResult::Ignored(None),
        }
    }
}

impl Component for EditorView {
    fn handle_event(
        &mut self,
        event: &Event,
        context: &mut crate::compositor::Context,
    ) -> EventResult {
        let mut cx = commands::Context {
            editor: context.editor,
            count: None,
            register: None,
            callback: None,
            on_next_key_callback: None,
            jobs: context.jobs,
        };

        match event {
            Event::Paste(contents) => {
                cx.count = cx.editor.count;
                commands::paste_bracketed_value(&mut cx, contents.clone());
                cx.editor.count = None;

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view, doc) = current!(cx.editor);
                view.ensure_cursor_in_view(doc, config.scrolloff);

                // Store a history state if not in insert mode. Otherwise wait till we exit insert
                // to include any edits to the paste in the history state.
                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }

                EventResult::Consumed(None)
            }
            Event::Resize(_width, _height) => {
                // Ignore this event, we handle resizing just before rendering to screen.
                // Handling it here but not re-rendering will cause flashing
                EventResult::Consumed(None)
            }
            Event::Key(mut key) => {
                cx.editor.reset_idle_timer();
                canonicalize_key(&mut key);

                // clear status
                cx.editor.status_msg = None;

                let mode = cx.editor.mode();
                let (view, _) = current!(cx.editor);
                let focus = view.id;

                if let Some(on_next_key) = self.on_next_key.take() {
                    // if there's a command waiting input, do that first
                    on_next_key(&mut cx, key);
                } else {
                    match mode {
                        Mode::Insert => {
                            // let completion swallow the event if necessary
                            let mut consumed = false;
                            if let Some(completion) = &mut self.completion {
                                let res = {
                                    // use a fake context here
                                    let mut cx = Context {
                                        editor: cx.editor,
                                        jobs: cx.jobs,
                                        scroll: None,
                                    };

                                    if let EventResult::Consumed(callback) =
                                        completion.handle_event(event, &mut cx)
                                    {
                                        consumed = true;
                                        Some(callback)
                                    } else if let EventResult::Consumed(callback) =
                                        completion.handle_event(&Event::Key(key!(Enter)), &mut cx)
                                    {
                                        Some(callback)
                                    } else {
                                        None
                                    }
                                };

                                if let Some(callback) = res {
                                    if callback.is_some() {
                                        // assume close_fn
                                        self.clear_completion(cx.editor);

                                        // In case the popup was deleted because of an intersection w/ the auto-complete menu.
                                        commands::signature_help_impl(
                                            &mut cx,
                                            commands::SignatureHelpInvoked::Automatic,
                                        );
                                    }
                                }
                            }

                            // if completion didn't take the event, we pass it onto commands
                            if !consumed {
                                self.insert_mode(&mut cx, key);

                                // record last_insert key
                                self.last_insert.1.push(InsertEvent::Key(key));

                                // lastly we recalculate completion
                                if let Some(completion) = &mut self.completion {
                                    completion.update(&mut cx);
                                    if completion.is_empty() {
                                        self.clear_completion(cx.editor);
                                    }
                                }
                            }
                        }
                        mode => self.command_mode(mode, &mut cx, key),
                    }
                }

                self.on_next_key = cx.on_next_key_callback.take();
                match self.on_next_key {
                    Some(_) => self.pseudo_pending.push(key),
                    None => self.pseudo_pending.clear(),
                }

                // appease borrowck
                let callback = cx.callback.take();

                // if the command consumed the last view, skip the render.
                // on the next loop cycle the Application will then terminate.
                if cx.editor.should_close() {
                    return EventResult::Ignored(None);
                }

                // if the focused view still exists and wasn't closed
                if cx.editor.tree.contains(focus) {
                    let config = cx.editor.config();
                    let mode = cx.editor.mode();
                    let view = view_mut!(cx.editor, focus);
                    let doc = doc_mut!(cx.editor, &view.doc);

                    view.ensure_cursor_in_view(doc, config.scrolloff);

                    // Store a history state if not in insert mode. This also takes care of
                    // committing changes when leaving insert mode.
                    if mode != Mode::Insert {
                        doc.append_changes_to_history(view);
                    }
                }

                EventResult::Consumed(callback)
            }

            Event::Mouse(event) => self.handle_mouse_event(event, &mut cx),
            Event::IdleTimeout => self.handle_idle_timeout(&mut cx),
            Event::FocusGained => EventResult::Ignored(None),
            Event::FocusLost => {
                if context.editor.config().auto_save {
                    if let Err(e) = commands::typed::write_all_impl(context, false, false) {
                        context.editor.set_error(format!("{}", e));
                    }
                }
                EventResult::Consumed(None)
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // clear with background color
        surface.set_style(area, cx.editor.theme.get("ui.background"));
        let config = cx.editor.config();

        // check if bufferline should be rendered
        use helix_view::editor::BufferLine;
        let use_bufferline = match config.bufferline {
            BufferLine::Always => true,
            BufferLine::Multiple if cx.editor.documents.len() > 1 => true,
            _ => false,
        };

        // -1 for commandline and -1 for bufferline
        let mut editor_area = area.clip_bottom(1);
        if use_bufferline {
            editor_area = editor_area.clip_top(1);
        }

        // if the terminal size suddenly changed, we need to trigger a resize
        cx.editor.resize(editor_area);

        if use_bufferline {
            Self::render_bufferline(cx.editor, area.with_height(1), surface);
        }

        for (view, is_focused) in cx.editor.tree.views() {
            let doc = cx.editor.document(view.doc).unwrap();
            self.render_view(cx.editor, doc, view, area, surface, is_focused);
        }

        if config.auto_info {
            if let Some(mut info) = cx.editor.autoinfo.take() {
                info.render(area, surface, cx);
                cx.editor.autoinfo = Some(info)
            }
        }

        let key_width = 15u16; // for showing pending keys
        let mut status_msg_width = 0;

        // render status msg
        if let Some((status_msg, severity)) = &cx.editor.status_msg {
            status_msg_width = status_msg.width();
            use helix_view::editor::Severity;
            let style = if *severity == Severity::Error {
                cx.editor.theme.get("error")
            } else {
                cx.editor.theme.get("ui.text")
            };

            surface.set_string(
                area.x,
                area.y + area.height.saturating_sub(1),
                status_msg,
                style,
            );
        }

        if area.width.saturating_sub(status_msg_width as u16) > key_width {
            let mut disp = String::new();
            if let Some(count) = cx.editor.count {
                disp.push_str(&count.to_string())
            }
            for key in self.keymaps.pending() {
                disp.push_str(&key.key_sequence_format());
            }
            for key in &self.pseudo_pending {
                disp.push_str(&key.key_sequence_format());
            }
            let style = cx.editor.theme.get("ui.text");
            let macro_width = if cx.editor.macro_recording.is_some() {
                3
            } else {
                0
            };
            surface.set_string(
                area.x + area.width.saturating_sub(key_width + macro_width),
                area.y + area.height.saturating_sub(1),
                disp.get(disp.len().saturating_sub(key_width as usize)..)
                    .unwrap_or(&disp),
                style,
            );
            if let Some((reg, _)) = cx.editor.macro_recording {
                let disp = format!("[{}]", reg);
                let style = style
                    .fg(helix_view::graphics::Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                surface.set_string(
                    area.x + area.width.saturating_sub(3),
                    area.y + area.height.saturating_sub(1),
                    &disp,
                    style,
                );
            }
        }

        if let Some(completion) = self.completion.as_mut() {
            completion.render(area, surface, cx);
        }
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        match editor.cursor() {
            // All block cursors are drawn manually
            (pos, CursorKind::Block) => (pos, CursorKind::Hidden),
            cursor => cursor,
        }
    }
}

fn canonicalize_key(key: &mut KeyEvent) {
    if let KeyEvent {
        code: KeyCode::Char(_),
        modifiers: _,
    } = key
    {
        key.modifiers.remove(KeyModifiers::SHIFT)
    }
}
