//! Key translation for the shell's own bars (address, find): winit key
//! events applied to a [`TextInput`] buffer. The buffer type itself is
//! the session's [`lumen_browser::TextBuffer`] — one editing model for
//! bars and in-page controls alike.

use winit::keyboard::{Key, NamedKey};

/// The bar edit buffer (an alias of the session's text buffer).
pub(crate) type TextInput = lumen_browser::TextBuffer;

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
