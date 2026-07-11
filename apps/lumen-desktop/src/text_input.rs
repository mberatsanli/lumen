//! Single-line text editing state shared by the address bar, the find
//! bar and in-page form controls: a caret and selection over a string,
//! plus the keyboard behavior native inputs have.

use winit::keyboard::{Key, NamedKey};

/// A single-line editable text field (address bar, find bar): a caret
/// and selection with the usual keyboard operations. Positions are in
/// chars.
pub(crate) struct TextInput {
    pub(crate) text: String,
    pub(crate) caret: usize,
    /// Selection anchor (== caret when nothing is selected).
    pub(crate) anchor: usize,
}

impl TextInput {
    pub(crate) fn with_all_selected(text: String) -> Self {
        let len = text.chars().count();
        Self {
            text,
            caret: len,
            anchor: 0,
        }
    }

    pub(crate) fn empty() -> Self {
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
    pub(crate) fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub(crate) fn slice(&self, start: usize, end: usize) -> String {
        self.text.chars().skip(start).take(end - start).collect()
    }

    pub(crate) fn selected_text(&self) -> String {
        let (start, end) = self.selection();
        self.slice(start, end)
    }

    fn byte_of(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(index, _)| index)
    }

    pub(crate) fn delete_selection(&mut self) {
        let (start, end) = self.selection();
        if start == end {
            return;
        }
        let (from, to) = (self.byte_of(start), self.byte_of(end));
        self.text.replace_range(from..to, "");
        self.caret = start;
        self.anchor = start;
    }

    pub(crate) fn insert(&mut self, input: &str) {
        self.delete_selection();
        let at = self.byte_of(self.caret);
        self.text.insert_str(at, input);
        self.caret += input.chars().count();
        self.anchor = self.caret;
    }

    fn backspace(&mut self) {
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

    fn delete_forward(&mut self) {
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
    fn step(&mut self, forward: bool, select: bool) {
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

    pub(crate) fn move_to(&mut self, index: usize, select: bool) {
        self.caret = index.min(self.char_count());
        if !select {
            self.anchor = self.caret;
        }
    }

    pub(crate) fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.char_count();
    }

    /// Start of the word before the caret (Option+Left target).
    fn previous_word(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.caret.min(chars.len());
        while index > 0 && !chars[index - 1].is_alphanumeric() {
            index -= 1;
        }
        while index > 0 && chars[index - 1].is_alphanumeric() {
            index -= 1;
        }
        index
    }

    /// End of the word after the caret (Option+Right target).
    fn next_word(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.caret.min(chars.len());
        while index < chars.len() && !chars[index].is_alphanumeric() {
            index += 1;
        }
        while index < chars.len() && chars[index].is_alphanumeric() {
            index += 1;
        }
        index
    }
}

/// What an editing key did to a [`TextInput`].
pub(crate) enum EditOutcome {
    /// Text changed.
    Changed,
    /// Only the caret/selection moved.
    Moved,
    Submit,
    Cancel,
    Copy,
    Cut,
    Paste,
    Ignored,
}

/// Applies one key to a text input. Clipboard actions are reported, not
/// performed (the caller owns the clipboard).
pub(crate) fn apply_edit(
    input: &mut TextInput,
    key: &Key,
    command: bool,
    shift: bool,
    alt: bool,
) -> EditOutcome {
    match key {
        Key::Named(NamedKey::Enter) => EditOutcome::Submit,
        Key::Named(NamedKey::Escape) => EditOutcome::Cancel,
        Key::Named(NamedKey::Backspace) if alt => {
            // Option+Backspace deletes the previous word.
            if !input.has_selection() {
                let target = input.previous_word();
                input.anchor = input.caret;
                input.caret = target;
            }
            input.delete_selection();
            EditOutcome::Changed
        }
        Key::Named(NamedKey::Backspace) => {
            input.backspace();
            EditOutcome::Changed
        }
        Key::Named(NamedKey::Delete) => {
            input.delete_forward();
            EditOutcome::Changed
        }
        Key::Named(NamedKey::ArrowLeft) if command => {
            input.move_to(0, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowRight) if command => {
            input.move_to(usize::MAX, shift);
            EditOutcome::Moved
        }
        // Option+arrows step words (with Shift: extend the selection).
        Key::Named(NamedKey::ArrowLeft) if alt => {
            let target = input.previous_word();
            input.move_to(target, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowRight) if alt => {
            let target = input.next_word();
            input.move_to(target, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowLeft) => {
            input.step(false, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::ArrowRight) => {
            input.step(true, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::Home) => {
            input.move_to(0, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::End) => {
            input.move_to(usize::MAX, shift);
            EditOutcome::Moved
        }
        Key::Named(NamedKey::Space) => {
            input.insert(" ");
            EditOutcome::Changed
        }
        Key::Character(text) if command => match text.as_str() {
            "a" => {
                input.select_all();
                EditOutcome::Moved
            }
            "c" => EditOutcome::Copy,
            "x" => EditOutcome::Cut,
            "v" => EditOutcome::Paste,
            _ => EditOutcome::Ignored,
        },
        Key::Character(text) => {
            input.insert(text);
            EditOutcome::Changed
        }
        _ => EditOutcome::Ignored,
    }
}
