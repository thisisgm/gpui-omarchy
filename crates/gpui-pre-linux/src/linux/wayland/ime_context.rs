use gpui::{PlatformInputHandler, UTF16Selection};
use std::ops::Range;

use super::ime::ImeBatch;
use super::window::ImeInput;

const CONTEXT_UNITS: usize = 1000;
const MAX_BYTES: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Preedit {
    pub text: String,
    pub selection: Option<Range<usize>>,
    pub cursor_visible: bool,
}

impl Preedit {
    pub fn new(text: String, begin: i32, end: i32) -> Self {
        let selection = usize::try_from(begin)
            .ok()
            .zip(usize::try_from(end).ok())
            .and_then(|(begin, end)| {
                Some(byte_to_utf16(&text, begin.min(end))?..byte_to_utf16(&text, begin.max(end))?)
            });
        Self {
            text,
            cursor_visible: selection.is_some(),
            selection,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SurroundingText {
    pub text: String,
    pub cursor: i32,
    pub anchor: i32,
    base_utf16: usize,
}

pub(crate) struct Deletion {
    pub range: Range<usize>,
    pub retained: String,
    pub selection: UTF16Selection,
}

fn utf16_to_byte(text: &str, offset: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units += ch.len_utf16();
    }
    (units == offset).then_some(text.len())
}

fn byte_to_utf16(text: &str, offset: usize) -> Option<usize> {
    text.get(..offset)
        .map(|prefix| prefix.encode_utf16().count())
}

impl SurroundingText {
    fn new(
        mut text: String,
        actual: Range<usize>,
        selection: Range<usize>,
        reversed: bool,
        marked: Option<Range<usize>>,
    ) -> Option<Self> {
        if text.contains('\0') {
            return None;
        }
        let mut low = selection.start.checked_sub(actual.start)?;
        let mut high = selection.end.checked_sub(actual.start)?;
        if let Some(marked) = marked {
            let start = marked.start.checked_sub(actual.start)?;
            let end = marked.end.checked_sub(actual.start)?;
            let removed = utf16_to_byte(&text, start)?..utf16_to_byte(&text, end)?;
            text.replace_range(removed, "");
            let project = |index: usize| {
                if index < start {
                    index
                } else {
                    start + index.saturating_sub(end)
                }
            };
            low = project(low);
            high = project(high);
        }
        let low = utf16_to_byte(&text, low)?;
        let high = utf16_to_byte(&text, high)?;
        let extra = MAX_BYTES.checked_sub(high.checked_sub(low)?)?;
        let mut start = low.saturating_sub(extra / 2);
        let mut end = text.len().min(start.saturating_add(MAX_BYTES));
        start = start.min(end.saturating_sub(MAX_BYTES));
        while !text.is_char_boundary(start) {
            start += 1;
        }
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if low < start || high > end {
            return None;
        }
        let base_utf16 = actual.start.checked_add(byte_to_utf16(&text, start)?)?;
        let (cursor, anchor) = if reversed { (low, high) } else { (high, low) };
        Some(Self {
            text: text[start..end].to_owned(),
            cursor: (cursor - start).try_into().ok()?,
            anchor: (anchor - start).try_into().ok()?,
            base_utf16,
        })
    }

    pub fn deletion(&self, before: u32, after: u32) -> Option<Deletion> {
        let low = self.cursor.min(self.anchor) as usize;
        let high = self.cursor.max(self.anchor) as usize;
        let start = low.checked_sub(before as usize)?;
        let end = high.checked_add(after as usize)?;
        let from = self
            .base_utf16
            .checked_add(byte_to_utf16(&self.text, start)?)?;
        let to = self
            .base_utf16
            .checked_add(byte_to_utf16(&self.text, end)?)?;
        let retained = self.text.get(low..high)?.to_owned();
        Some(Deletion {
            range: from..to,
            selection: UTF16Selection {
                range: from..from + retained.encode_utf16().count(),
                reversed: self.cursor < self.anchor,
            },
            retained,
        })
    }
}

/// The editor calls IME composition makes, so a test can stand in a plain text field.
pub(crate) trait ImeTarget {
    fn selected_text_range(&mut self) -> Option<UTF16Selection>;
    fn marked_text_range(&mut self) -> Option<Range<usize>>;
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
    ) -> Option<String>;
    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str);
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
    );
    fn set_selected_text_range(&mut self, selection: UTF16Selection);
    fn unmark_text(&mut self);
    fn set_ime_cursor_visible(&mut self, visible: bool);
}

impl ImeTarget for PlatformInputHandler {
    fn selected_text_range(&mut self) -> Option<UTF16Selection> {
        PlatformInputHandler::selected_text_range(self, false)
    }

    fn marked_text_range(&mut self) -> Option<Range<usize>> {
        PlatformInputHandler::marked_text_range(self)
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
    ) -> Option<String> {
        PlatformInputHandler::text_for_range(self, range, adjusted)
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str) {
        PlatformInputHandler::replace_text_in_range(self, range, text)
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
    ) {
        PlatformInputHandler::replace_and_mark_text_in_range(self, range, text, selection)
    }

    fn set_selected_text_range(&mut self, selection: UTF16Selection) {
        PlatformInputHandler::set_selected_text_range(self, selection)
    }

    fn unmark_text(&mut self) {
        PlatformInputHandler::unmark_text(self)
    }

    fn set_ime_cursor_visible(&mut self, visible: bool) {
        PlatformInputHandler::set_ime_cursor_visible(self, visible)
    }
}

/// Carries out one edit the client routed to the field.
pub(crate) fn handle(target: &mut impl ImeTarget, input: ImeInput) {
    match input {
        ImeInput::Apply(batch) => apply(target, batch),
        ImeInput::InsertText(text) => {
            target.set_ime_cursor_visible(true);
            target.replace_text_in_range(None, &text);
        }
        ImeInput::SetMarkedText(text) => {
            target.replace_and_mark_text_in_range(None, &text, None);
        }
        ImeInput::UnmarkText => {
            target.set_ime_cursor_visible(true);
            target.unmark_text();
        }
        ImeInput::DeleteText => {
            target.set_ime_cursor_visible(true);
            if let Some(marked) = target.marked_text_range() {
                target.replace_text_in_range(Some(marked), "");
            }
        }
    }
}

/// Applies a done batch: remove preedit, delete around selection, commit, then mark new preedit.
pub(crate) fn apply(target: &mut impl ImeTarget, batch: ImeBatch) {
    target.set_ime_cursor_visible(true);
    if let Some(marked) = target.marked_text_range() {
        target.replace_text_in_range(Some(marked), "");
    }
    let mut selection = None;
    if batch.delete != (0, 0) {
        let context = surrounding(target);
        let deletion = context
            .as_ref()
            .and_then(|text| text.deletion(batch.delete.0, batch.delete.1));
        if let Some(deletion) = deletion {
            target.replace_text_in_range(Some(deletion.range), &deletion.retained);
            selection = Some(deletion.selection.range.clone());
            target.set_selected_text_range(deletion.selection);
        } else {
            log::warn!(
                "wayland: refused IME deletion outside valid UTF-8 context: before={} after={} context_bytes={} context_available={}",
                batch.delete.0,
                batch.delete.1,
                context.as_ref().map_or(0, |text| text.text.len()),
                context.is_some()
            );
        }
    }
    if let Some(text) = batch.commit {
        target.replace_text_in_range(selection.take(), &text);
    }
    if let Some(preedit) = batch.preedit.filter(|preedit| !preedit.text.is_empty()) {
        target.replace_and_mark_text_in_range(selection, &preedit.text, preedit.selection);
        target.set_ime_cursor_visible(preedit.cursor_visible);
    }
}

pub(crate) fn surrounding(handler: &mut impl ImeTarget) -> Option<SurroundingText> {
    let selection = handler.selected_text_range()?;
    if selection.range.len() > CONTEXT_UNITS * 2 {
        return None;
    }
    let marked = handler.marked_text_range();
    let low = marked.as_ref().map_or(selection.range.start, |range| {
        range.start.min(selection.range.start)
    });
    let high = marked.as_ref().map_or(selection.range.end, |range| {
        range.end.max(selection.range.end)
    });
    let requested = low.saturating_sub(CONTEXT_UNITS)..high.saturating_add(CONTEXT_UNITS);
    let mut adjusted = None;
    let text = handler.text_for_range(requested.clone(), &mut adjusted)?;
    let actual = adjusted.unwrap_or(requested);
    SurroundingText::new(text, actual, selection.range, selection.reversed, marked)
}

/// A single-line field with the editor's replace and mark semantics, in UTF-16 units.
#[cfg(test)]
pub(crate) struct FakeField {
    pub text: String,
    pub selection: Range<usize>,
    pub reversed: bool,
    pub marked: Option<Range<usize>>,
    pub cursor_visible: bool,
}

#[cfg(test)]
impl FakeField {
    pub fn new(text: &str, selection: Range<usize>) -> Self {
        Self {
            text: text.into(),
            selection,
            reversed: false,
            marked: None,
            cursor_visible: true,
        }
    }

    fn bytes(&self, range: &Range<usize>) -> Range<usize> {
        utf16_to_byte(&self.text, range.start).unwrap()
            ..utf16_to_byte(&self.text, range.end).unwrap()
    }

    fn replace(&mut self, range: Option<Range<usize>>, text: &str) -> Range<usize> {
        let range = range
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        let bytes = self.bytes(&range);
        self.text.replace_range(bytes, text);
        self.reversed = false;
        range.start..range.start + text.encode_utf16().count()
    }
}

#[cfg(test)]
impl ImeTarget for FakeField {
    fn selected_text_range(&mut self) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.selection.clone(),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&mut self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
    ) -> Option<String> {
        let len = self.text.encode_utf16().count();
        let range = range.start.min(len)..range.end.min(len);
        let bytes = self.bytes(&range);
        *adjusted = Some(range);
        Some(self.text[bytes].to_owned())
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str) {
        let inserted = self.replace(range, text);
        self.marked = None;
        self.selection = inserted.end..inserted.end;
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
    ) {
        let inserted = self.replace(range, text);
        self.selection = selection.map_or(inserted.end..inserted.end, |selection| {
            inserted.start + selection.start..inserted.start + selection.end
        });
        self.marked = Some(inserted);
    }

    fn set_selected_text_range(&mut self, selection: UTF16Selection) {
        self.selection = selection.range;
        self.reversed = selection.reversed;
    }

    fn unmark_text(&mut self) {
        self.marked = None;
    }

    fn set_ime_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ime_preedit_cursor_converts_bytes_to_utf16() {
        let preedit = Preedit::new("中\u{20000}a".into(), 3, 7);
        assert_eq!(preedit.selection, Some(1..3));
        assert!(preedit.cursor_visible);
        assert!(!Preedit::new("中".into(), -1, -1).cursor_visible);
        assert!(Preedit::new("中".into(), 1, 1).selection.is_none());
    }

    #[test]
    fn ime_surrounding_omits_preedit_and_preserves_direction() {
        let text = SurroundingText::new("a中\u{20000}z".into(), 10..15, 12..14, true, Some(11..14))
            .unwrap();
        assert_eq!(text.text, "az");
        assert_eq!((text.cursor, text.anchor), (1, 1));
        let text = SurroundingText::new("a中z".into(), 0..3, 1..2, true, None).unwrap();
        assert_eq!((text.cursor, text.anchor), (1, 4));
    }

    #[test]
    fn ime_surrounding_is_bounded_and_keeps_complete_codepoints() {
        let text = "中".repeat(3000);
        let surrounding = SurroundingText::new(text, 0..3000, 1500..1500, false, None).unwrap();
        assert!(surrounding.text.len() <= MAX_BYTES);
        assert!(
            surrounding
                .text
                .is_char_boundary(surrounding.cursor as usize)
        );
        assert!(SurroundingText::new("a\0b".into(), 0..3, 1..1, false, None).is_none());
        assert!(SurroundingText::new("中".repeat(2000), 0..2000, 0..2000, false, None).is_none());
    }

    #[test]
    fn ime_deletion_keeps_selection_and_rejects_partial_codepoints() {
        let text =
            SurroundingText::new("a中\u{20000}z".into(), 10..15, 12..14, false, None).unwrap();
        let deletion = text.deletion(3, 1).unwrap();
        assert_eq!(deletion.range, 11..15);
        assert_eq!(deletion.retained, "\u{20000}");
        assert_eq!(deletion.selection.range, 11..13);
        assert!(!deletion.selection.reversed);
        assert!(text.deletion(1, 0).is_none());
        assert!(text.deletion(u32::MAX, 0).is_none());
    }

    #[test]
    fn fix1_a2_deletion_preserves_reversed_selection() {
        for after in [0, 1] {
            let mut field = FakeField::new("abcd", 1..3);
            field.reversed = true;
            apply(
                &mut field,
                ImeBatch {
                    delete: (1, after),
                    ..ImeBatch::default()
                },
            );
            assert_eq!(field.text, if after == 0 { "bcd" } else { "bc" });
            assert_eq!(field.selection, 0..2);
            assert!(field.reversed);
            let context = surrounding(&mut field).unwrap();
            assert_eq!((context.cursor, context.anchor), (0, 2));
        }
    }

    #[test]
    fn fix1_a4_refused_deletion_warning_names_lengths_and_context() {
        struct Capture(std::sync::Mutex<Vec<String>>);
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                if record.args().to_string().contains("refused IME deletion") {
                    self.0.lock().unwrap().push(record.args().to_string());
                }
            }
            fn flush(&self) {}
        }
        static CAPTURE: Capture = Capture(std::sync::Mutex::new(Vec::new()));
        log::set_logger(&CAPTURE).unwrap();
        log::set_max_level(log::LevelFilter::Warn);
        for (before, after) in [(1, 0), (u32::MAX, 3)] {
            let mut field = FakeField::new("é", 1..1);
            apply(
                &mut field,
                ImeBatch {
                    delete: (before, after),
                    ..ImeBatch::default()
                },
            );
        }
        let messages = CAPTURE.0.lock().unwrap().clone();
        for expected in [
            "before=1 after=0 context_bytes=2",
            "before=4294967295 after=3 context_bytes=2",
        ] {
            assert!(
                messages.iter().any(|message| message.contains(expected)),
                "{messages:?}"
            );
        }
    }
}
