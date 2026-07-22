//! Text editing owned by the session: the focused control's buffer
//! (caret, anchor, selection), the horizontal display window that keeps
//! the caret visible in single-line inputs, textarea caret-following
//! scroll, and the overlay geometry the shell draws.
//!
//! The shell only translates platform key events into [`EditOp`]s and
//! renders the returned geometry — all editing state lives here, next to
//! the layout and measurer that give it meaning.

use crate::{ResourceLoader, Session};
use lumen_engine::{Rect, TextStyle};
use lumen_html::NodeId;

/// A single-line-oriented text buffer: caret and selection over a
/// string, with the keyboard operations native inputs have. Positions
/// are in chars.
#[derive(Debug, Clone)]
pub struct TextBuffer {
    pub text: String,
    pub caret: usize,
    /// Selection anchor (== caret when nothing is selected).
    pub anchor: usize,
}

impl TextBuffer {
    #[must_use]
    pub fn with_all_selected(text: String) -> Self {
        let len = text.chars().count();
        Self {
            text,
            caret: len,
            anchor: 0,
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: 0,
        }
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Selection bounds in document order.
    #[must_use]
    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    #[must_use]
    pub fn slice(&self, start: usize, end: usize) -> String {
        self.text.chars().skip(start).take(end - start).collect()
    }

    #[must_use]
    pub fn selected_text(&self) -> String {
        let (start, end) = self.selection();
        self.slice(start, end)
    }

    fn byte_of(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(index, _)| index)
    }

    pub fn delete_selection(&mut self) {
        let (start, end) = self.selection();
        if start == end {
            return;
        }
        let (from, to) = (self.byte_of(start), self.byte_of(end));
        self.text.replace_range(from..to, "");
        self.caret = start;
        self.anchor = start;
    }

    pub fn insert(&mut self, input: &str) {
        self.delete_selection();
        let at = self.byte_of(self.caret);
        self.text.insert_str(at, input);
        self.caret += input.chars().count();
        self.anchor = self.caret;
    }

    pub fn backspace(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.caret == 0 {
            return;
        }
        let (from, to) = (self.byte_of(self.caret - 1), self.byte_of(self.caret));
        self.text.replace_range(from..to, "");
        self.caret -= 1;
        self.anchor = self.caret;
    }

    pub fn delete_forward(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.caret >= self.char_count() {
            return;
        }
        let (from, to) = (self.byte_of(self.caret), self.byte_of(self.caret + 1));
        self.text.replace_range(from..to, "");
    }

    /// Moves the caret by one; without `select`, a selection collapses to
    /// its matching edge first (as native inputs do).
    pub fn step(&mut self, forward: bool, select: bool) {
        if !select && self.has_selection() {
            let (start, end) = self.selection();
            self.caret = if forward { end } else { start };
        } else if forward {
            self.caret = (self.caret + 1).min(self.char_count());
        } else {
            self.caret = self.caret.saturating_sub(1);
        }
        if !select {
            self.anchor = self.caret;
        }
    }

    pub fn move_to(&mut self, index: usize, select: bool) {
        self.caret = index.min(self.char_count());
        if !select {
            self.anchor = self.caret;
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.char_count();
    }

    /// Start of the word before the caret (Option+Left target).
    #[must_use]
    pub fn previous_word(&self) -> usize {
        let mut index = self.caret.min(self.char_count());
        // Walk backwards over the text without materializing a Vec<char>:
        // skip trailing non-alphanumerics, then the word itself.
        let mut rev = self.text[..self.byte_of(index)].chars().rev().peekable();
        while rev.next_if(|ch| !ch.is_alphanumeric()).is_some() {
            index -= 1;
        }
        while rev.next_if(|ch| ch.is_alphanumeric()).is_some() {
            index -= 1;
        }
        index
    }

    /// End of the word after the caret (Option+Right target).
    #[must_use]
    pub fn next_word(&self) -> usize {
        let count = self.char_count();
        let mut index = self.caret.min(count);
        let mut chars = self.text[self.byte_of(index)..].chars().peekable();
        while chars.next_if(|ch| !ch.is_alphanumeric()).is_some() {
            index += 1;
        }
        while chars.next_if(|ch| ch.is_alphanumeric()).is_some() {
            index += 1;
        }
        index
    }
}

/// Caret motions an [`EditOp::Move`] can request.
#[derive(Debug, Clone, Copy)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
}

/// One editing operation on the focused control (platform-agnostic —
/// the shell translates key events into these).
#[derive(Debug, Clone)]
pub enum EditOp {
    Insert(String),
    Backspace { word: bool },
    DeleteForward,
    Move { motion: Motion, select: bool },
    SelectAll,
}

/// What an [`EditOp`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditResult {
    /// Text changed (the page re-rendered).
    Edited,
    /// Only the caret/selection moved.
    Moved,
    Ignored,
}

/// The focused control's editing state.
pub(crate) struct TextEdit {
    pub(crate) node: NodeId,
    pub(crate) buffer: TextBuffer,
    /// Chars scrolled off the left edge of a single-line display window.
    pub(crate) window: usize,
}

/// Overlay geometry for the shell to draw, in page coordinates.
#[derive(Debug, Clone, Copy)]
pub struct EditOverlay {
    /// Caret line rect; `None` when scrolled out of the control's box.
    pub caret: Option<Rect>,
    /// Selection highlight (single-line values only).
    pub selection: Option<Rect>,
}

impl<L: ResourceLoader> Session<L> {
    /// Begins editing a text control. `at_x` positions the caret at that
    /// page x (single-line); otherwise everything is selected with the
    /// caret at the end. Returns whether editing began.
    pub fn begin_edit(&mut self, node: NodeId, at_x: Option<f32>) -> bool {
        if !self.is_text_input(node) && !self.is_textarea(node) {
            return false;
        }
        let value = self.form_value(node);
        let mut buffer = TextBuffer::with_all_selected(value);
        // Keep the window when re-clicking the control already edited.
        let window = match &self.editor {
            Some(edit) if edit.node == node => edit.window,
            _ => 0,
        };
        self.end_edit();
        if self.is_textarea(node) {
            buffer.move_to(usize::MAX, false);
        } else if let Some(x) = at_x
            && let Some(index) = self.caret_index_at_x(node, window, x)
        {
            buffer.move_to(index, false);
        }
        self.editor = Some(TextEdit {
            node,
            buffer,
            window,
        });
        self.sync_edit_display(false);
        true
    }

    /// Ends editing, restoring the full (head-clipped) value text when
    /// the display was windowed. A number input's value is also clamped
    /// into its min/max range here: the upper bound is already enforced
    /// while typing (see `clamp_number_edit`), the lower bound only once
    /// typing is done.
    pub fn end_edit(&mut self) {
        let Some(edit) = self.editor.take() else {
            return;
        };
        let mut value = edit.buffer.text;
        let mut clamped = false;
        if let Some((min, max)) = self.number_bounds(edit.node)
            && let Ok(parsed) = value.trim().parse::<f32>()
        {
            let mut bounded = parsed;
            if let Some(min) = min {
                bounded = bounded.max(min);
            }
            if let Some(max) = max {
                bounded = bounded.min(max);
            }
            if bounded != parsed {
                value = format_number(bounded);
                clamped = true;
            }
        }
        if edit.window != 0 || clamped {
            self.set_form_value(edit.node, &value);
        }
    }

    /// The node being edited, if any.
    #[must_use]
    pub fn editing(&self) -> Option<NodeId> {
        self.editor.as_ref().map(|edit| edit.node)
    }

    /// The live edit buffer (for clipboard access).
    #[must_use]
    pub fn edit_buffer(&self) -> Option<&TextBuffer> {
        self.editor.as_ref().map(|edit| &edit.buffer)
    }

    /// Applies one editing operation to the focused control.
    pub fn edit(&mut self, op: EditOp) -> EditResult {
        let Some(edit) = self.editor.as_mut() else {
            return EditResult::Ignored;
        };
        let buffer = &mut edit.buffer;
        let result = match op {
            EditOp::Insert(text) => {
                buffer.insert(&text);
                EditResult::Edited
            }
            EditOp::Backspace { word } => {
                if word {
                    if !buffer.has_selection() {
                        let target = buffer.previous_word();
                        buffer.anchor = buffer.caret;
                        buffer.caret = target;
                    }
                    buffer.delete_selection();
                } else {
                    buffer.backspace();
                }
                EditResult::Edited
            }
            EditOp::DeleteForward => {
                buffer.delete_forward();
                EditResult::Edited
            }
            EditOp::Move { motion, select } => {
                match motion {
                    Motion::Left => buffer.step(false, select),
                    Motion::Right => buffer.step(true, select),
                    Motion::WordLeft => {
                        let target = buffer.previous_word();
                        buffer.move_to(target, select);
                    }
                    Motion::WordRight => {
                        let target = buffer.next_word();
                        buffer.move_to(target, select);
                    }
                    Motion::LineStart => buffer.move_to(0, select),
                    Motion::LineEnd => buffer.move_to(usize::MAX, select),
                }
                EditResult::Moved
            }
            EditOp::SelectAll => {
                buffer.select_all();
                EditResult::Moved
            }
        };
        self.sync_edit_display(result == EditResult::Edited);
        if result == EditResult::Edited {
            self.clamp_number_edit();
        }
        result
    }

    /// Keeps a number input from exceeding its max while typing.
    /// (Deliberate deviation from the HTML spec, which only clamps when
    /// stepping or submitting.) Only the upper bound is enforced here:
    /// digits only ever grow a value, so once past max it stays past —
    /// whereas clamping to min mid-edit would mangle text the user is
    /// still typing ("-123" would become "023"). The lower bound is
    /// enforced when editing ends (see `end_edit`).
    fn clamp_number_edit(&mut self) {
        let Some(node) = self.editing() else {
            return;
        };
        let Some((_, max)) = self.number_bounds(node) else {
            return;
        };
        let Some(max) = max else {
            return;
        };
        let Some(edit) = self.editor.as_mut() else {
            return;
        };
        // Only clamp complete numbers; partial states like "" or "-"
        // are left alone so typing is not interrupted mid-edit.
        let Ok(value) = edit.buffer.text.trim().parse::<f32>() else {
            return;
        };
        if value <= max {
            return;
        }
        edit.buffer.text = format_number(max);
        edit.buffer.move_to(usize::MAX, false);
        self.sync_edit_display(true);
    }

    /// The parsed `min`/`max` attributes of a number input, `None` for
    /// any other node.
    fn number_bounds(&self, node: NodeId) -> Option<(Option<f32>, Option<f32>)> {
        if !self.is_number_input(node) {
            return None;
        }
        let page = self.page.as_ref()?;
        let element = page.document.element(node)?;
        let attr = |name: &str| -> Option<f32> {
            element
                .attributes
                .get(name)
                .and_then(|value| value.parse().ok())
        };
        Some((attr("min"), attr("max")))
    }

    /// Drag-selects: extends the selection to the caret index at page x.
    pub fn edit_drag_to(&mut self, x: f32) {
        let Some(edit) = self.editor.as_ref() else {
            return;
        };
        let (node, window) = (edit.node, edit.window);
        if self.is_textarea(node) {
            return;
        }
        if let Some(index) = self.caret_index_at_x(node, window, x)
            && let Some(edit) = self.editor.as_mut()
        {
            edit.buffer.move_to(index, true);
            self.sync_edit_display(false);
        }
    }

    /// What a control renders for a value: passwords display bullets.
    pub(crate) fn control_display_text(&self, node: NodeId, value: &str) -> String {
        let is_password = self
            .page
            .as_ref()
            .and_then(|page| page.document.element(node))
            .and_then(|element| element.attributes.get("type"))
            == Some("password");
        if is_password {
            "\u{2022}".repeat(value.chars().count())
        } else {
            value.to_string()
        }
    }

    /// The text style a control's value renders with.
    fn control_text_style(&self, node: NodeId) -> Option<TextStyle> {
        let style = self.page.as_ref()?.styles.by_node.get(&node)?;
        Some(TextStyle {
            font_size: style.font_size,
            font_weight: style.font_weight,
            monospace: style.monospace,
            letter_spacing: style.letter_spacing,
        })
    }

    /// The caret index (in value chars) under page x, given the current
    /// display window. Prefix widths grow monotonically, so a single pass
    /// with one reused buffer finds the first prefix reaching the click —
    /// no per-probe string rebuilds like a binary search would need.
    fn caret_index_at_x(&self, node: NodeId, window: usize, x: f32) -> Option<usize> {
        let page = self.page.as_ref()?;
        let content = page.layout.find_by_node(node)?.content_box();
        let value: String = self
            .control_display_text(node, &self.form_value(node))
            .chars()
            .skip(window)
            .collect();
        let text_style = self.control_text_style(node)?;
        let measurer = self.effective_measurer();
        let relative = (x - content.x).max(0.0);
        if relative <= 0.0 {
            return Some(window);
        }
        let mut prefix = String::with_capacity(value.len());
        for (index, ch) in value.chars().enumerate() {
            prefix.push(ch);
            if measurer.measure(&prefix, &text_style).width >= relative {
                return Some(window + index + 1);
            }
        }
        Some(window + value.chars().count())
    }

    /// Pushes the edited value into the page. Single-line inputs keep
    /// the caret visible by rendering a windowed tail of the value;
    /// textareas scroll their inner offset to the caret's line instead.
    fn sync_edit_display(&mut self, text_changed: bool) {
        let Some(edit) = self.editor.as_ref() else {
            return;
        };
        let (node, caret) = (edit.node, edit.buffer.caret);
        let value = edit.buffer.text.clone();
        if self.is_textarea(node) {
            if text_changed {
                self.set_form_value(node, &value);
            }
            self.follow_textarea_caret(node, caret, &value);
            return;
        }
        if value.is_empty() {
            if let Some(edit) = self.editor.as_mut() {
                edit.window = 0;
            }
            if text_changed {
                self.set_form_value(node, "");
            }
            return;
        }
        let display_full = self.control_display_text(node, &value);
        let Some(content) = self
            .page
            .as_ref()
            .and_then(|page| page.layout.find_by_node(node))
            .map(|laid| laid.content_box())
        else {
            return;
        };
        let Some(text_style) = self.control_text_style(node) else {
            return;
        };
        let chars: Vec<char> = display_full.chars().collect();
        let previous_window = edit.window;
        let start = {
            let measurer = self.effective_measurer();
            let width_of = |from: usize, to: usize| {
                let slice: String = chars[from.min(chars.len())..to.min(chars.len())]
                    .iter()
                    .collect::<String>();
                measurer.measure(&slice, &text_style).width
            };
            // Leave room for the caret line at the right edge. Both scans
            // binary-search a monotonic width, so long values stay cheap.
            let budget = (content.width - 4.0).max(10.0);
            let mut start = previous_window.min(caret);
            if width_of(start, caret) > budget {
                // Slide right to the smallest start that fits ..caret.
                let (mut low, mut high) = (start, caret);
                while low < high {
                    let mid = low + (high - low) / 2;
                    if width_of(mid, caret) > budget {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                start = low;
            }
            // Refill from the left when deletions free up room: the
            // smallest start whose tail still fits.
            if start > 0 && width_of(start - 1, chars.len()) <= budget {
                let (mut low, mut high) = (0usize, start - 1);
                while low < high {
                    let mid = low + (high - low) / 2;
                    if width_of(mid, chars.len()) <= budget {
                        high = mid;
                    } else {
                        low = mid + 1;
                    }
                }
                start = low;
            }
            start
        };
        if let Some(edit) = self.editor.as_mut() {
            edit.window = start;
        }
        if text_changed || start != previous_window {
            let display: String = chars[start..].iter().collect();
            self.set_form_value_display(node, &value, &display);
        }
    }

    /// Scrolls a textarea's inner offset so the caret's line stays
    /// inside the visible box.
    fn follow_textarea_caret(&mut self, node: NodeId, caret: usize, value: &str) {
        let caret_line = value
            .chars()
            .take(caret)
            .filter(|character| *character == '\n')
            .count();
        let Some((line_height, content_height)) = self.page.as_ref().and_then(|page| {
            let laid = page.layout.find_by_node(node)?;
            let style = page.styles.by_node.get(&node)?;
            Some((style.line_height, laid.content_box().height))
        }) else {
            return;
        };
        let offset = self.scroll_offsets.get(&node).copied().unwrap_or(0.0);
        let caret_top = caret_line as f32 * line_height;
        let delta = if caret_top < offset {
            caret_top - offset
        } else if caret_top + line_height > offset + content_height {
            caret_top + line_height - (offset + content_height)
        } else {
            0.0
        };
        if delta != 0.0 {
            self.scroll_inner(node, delta);
        }
    }

    /// Overlay geometry (page coordinates) for the caret and selection
    /// of the edited control. `None` while nothing is being edited.
    #[must_use]
    pub fn edit_overlay(&self) -> Option<EditOverlay> {
        let edit = self.editor.as_ref()?;
        let page = self.page.as_ref()?;
        let node = edit.node;
        let content = page.layout.find_by_node(node)?.content_box();
        let style = page.styles.by_node.get(&node)?;
        let text_style = self.control_text_style(node)?;
        let measurer = self.effective_measurer();

        let display_all = self.control_display_text(node, &edit.buffer.text);
        let multiline = display_all.contains('\n');
        let window = if multiline { 0 } else { edit.window };
        let display: String = display_all.chars().skip(window).collect();
        let caret_index = edit.buffer.caret.saturating_sub(window);
        let width_to = |index: usize| {
            let prefix: String = display.chars().take(index).collect();
            measurer.measure(&prefix, &text_style).width
        };

        // Multiline caret sits on its line, shifted by the inner scroll.
        let inner_offset = self.scroll_offsets.get(&node).copied().unwrap_or(0.0);
        let caret_line = display
            .chars()
            .take(caret_index)
            .filter(|character| *character == '\n')
            .count();
        let line_offset = caret_line as f32 * style.line_height - inner_offset;
        let last_line_start = display
            .chars()
            .take(caret_index)
            .collect::<String>()
            .rfind('\n')
            .map(|at| at + 1)
            .unwrap_or(0);

        let (start, end) = edit.buffer.selection();
        let selection = (start != end && !multiline).then(|| {
            let (start, end) = (start.saturating_sub(window), end.saturating_sub(window));
            let x0 = (content.x + width_to(start)).min(content.x + content.width);
            let x1 = (content.x + width_to(end)).min(content.x + content.width);
            Rect {
                x: x0,
                y: content.y,
                width: x1 - x0,
                height: content.height,
            }
        });

        let caret_prefix: String = display
            .chars()
            .take(caret_index)
            .collect::<String>()
            .get(last_line_start..)
            .unwrap_or("")
            .to_string();
        let caret_x = (content.x + measurer.measure(&caret_prefix, &text_style).width)
            .min(content.x + content.width - 1.5);
        let caret_height = style.line_height.min(content.height);
        // A caret line scrolled past the box's edges just hides.
        let caret = (line_offset > -0.5 && line_offset + caret_height <= content.height + 0.5)
            .then_some(Rect {
                x: caret_x,
                y: content.y + line_offset,
                width: 1.5,
                height: caret_height,
            });

        Some(EditOverlay { caret, selection })
    }
}

/// Formats a clamped number the way number inputs display it: integral
/// values without a fraction.
fn format_number(value: f32) -> String {
    if (value - value.round()).abs() < 1e-4 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResourceLoader;
    use lumen_engine::Size;
    use lumen_platform::{LoadError, ResourceRequest, ResourceResponse, Url};

    struct FakeLoader {
        pages: std::collections::HashMap<String, Vec<u8>>,
    }

    impl FakeLoader {
        fn new(pages: &[(&str, &str)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(url, html)| ((*url).to_string(), html.as_bytes().to_vec()))
                    .collect(),
            }
        }
    }

    impl ResourceLoader for FakeLoader {
        fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
            let body = self
                .pages
                .get(request.url.as_str())
                .ok_or_else(|| LoadError::Http(format!("404: {}", request.url)))?;
            Ok(ResourceResponse {
                final_url: request.url.clone(),
                content_type: None,
                body: body.clone(),
                set_cookies: Vec::new(),
            })
        }
    }

    /// The pre-optimization `previous_word` (Vec<char> based), kept as the
    /// reference the allocation-free version must match exactly.
    fn reference_previous_word(text: &str, caret: usize) -> usize {
        let chars: Vec<char> = text.chars().collect();
        let mut index = caret.min(chars.len());
        while index > 0 && !chars[index - 1].is_alphanumeric() {
            index -= 1;
        }
        while index > 0 && chars[index - 1].is_alphanumeric() {
            index -= 1;
        }
        index
    }

    fn reference_next_word(text: &str, caret: usize) -> usize {
        let chars: Vec<char> = text.chars().collect();
        let mut index = caret.min(chars.len());
        while index < chars.len() && !chars[index].is_alphanumeric() {
            index += 1;
        }
        while index < chars.len() && chars[index].is_alphanumeric() {
            index += 1;
        }
        index
    }

    #[test]
    fn word_motions_match_reference_on_every_caret() {
        let samples = [
            "",
            "a",
            "hello world",
            "  spaced  out  ",
            "foo--bar__baz",
            "merhaba dünya, nasılsın?",
            "ünïcodé wörds ✓ ok",
            "one\ntwo\tthree",
        ];
        for text in samples {
            for caret in 0..=text.chars().count() + 1 {
                let buffer = TextBuffer {
                    text: text.to_string(),
                    caret,
                    anchor: caret,
                };
                assert_eq!(
                    buffer.previous_word(),
                    reference_previous_word(text, caret),
                    "previous_word({text:?}, {caret})"
                );
                assert_eq!(
                    buffer.next_word(),
                    reference_next_word(text, caret),
                    "next_word({text:?}, {caret})"
                );
            }
        }
    }

    /// The pre-optimization binary search, kept as the reference the
    /// single-pass `caret_index_at_x` must match exactly.
    fn reference_caret_index_at_x(
        session: &Session<FakeLoader>,
        node: NodeId,
        window: usize,
        x: f32,
    ) -> Option<usize> {
        let page = session.page.as_ref()?;
        let content = page.layout.find_by_node(node)?.content_box();
        let value: String = session
            .control_display_text(node, &session.form_value(node))
            .chars()
            .skip(window)
            .collect();
        let text_style = session.control_text_style(node)?;
        let measurer = session.effective_measurer();
        let relative = (x - content.x).max(0.0);
        let count = value.chars().count();
        let width_to = |index: usize| {
            let prefix: String = value.chars().take(index).collect();
            measurer.measure(&prefix, &text_style).width
        };
        let (mut low, mut high) = (0usize, count);
        while low < high {
            let mid = low + (high - low) / 2;
            if width_to(mid) >= relative {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        Some(window + low)
    }

    #[test]
    fn caret_index_at_x_matches_reference_binary_search() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<form><input id='q' type='text' value='hello dünya'></form>",
            )]),
            Size {
                width: 800.0,
                height: 600.0,
            },
        );
        session
            .load(Url::parse("https://a.test/").unwrap())
            .unwrap();
        let field = session
            .page()
            .unwrap()
            .document
            .get_element_by_id("q")
            .unwrap();
        let content = session
            .page()
            .unwrap()
            .layout
            .find_by_node(field)
            .unwrap()
            .content_box();
        // Sweep the click x across and past the box in fine steps.
        let chars = "hello dünya".chars().count();
        let mut seen = Vec::new();
        let mut x = content.x - 5.0;
        while x <= content.x + content.width + 5.0 {
            let actual = session.caret_index_at_x(field, 0, x);
            let expected = reference_caret_index_at_x(&session, field, 0, x);
            assert_eq!(actual, expected, "caret_index_at_x at x={x}");
            seen.push(actual.unwrap());
            x += 0.5;
        }
        // The sweep covers every caret slot from 0 to the char count.
        assert_eq!(seen.first(), Some(&0));
        assert_eq!(seen.last(), Some(&chars));
        for index in 0..=chars {
            assert!(seen.contains(&index), "caret slot {index} unreachable");
        }
    }
}
