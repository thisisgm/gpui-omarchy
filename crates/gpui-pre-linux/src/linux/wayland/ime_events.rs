use gpui::{Bounds, Pixels};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ChangeCause;

use super::ime::{ImeAction, ImeSession, TextInputRequests};
use super::ime_context::SurroundingText;
use super::window::ImeInput;

/// A window as the client's text-input routing reaches it.
pub(crate) trait ImeWindow {
    fn ime_input(&self, input: ImeInput);
    /// A lone ASCII commit outside composition, delivered as a key press.
    fn ime_key(&self, text: String);
    /// None while GPUI holds the window's input handler, like `ime_context`.
    fn ime_area(&self) -> Option<Bounds<Pixels>>;
    fn ime_context(&self) -> Option<SurroundingText>;
}

/// The client as its text-input routing needs it; each call borrows the client only while it runs.
pub(crate) trait ImeHost: Sized + 'static {
    type Surface: Clone + PartialEq;
    type TextInput: TextInputRequests;
    type Window: ImeWindow;

    fn with_session<R>(
        &self,
        f: impl FnOnce(&mut ImeSession<Self::Surface>, Option<&Self::TextInput>) -> R,
    ) -> R;
    fn window(&self, surface: &Self::Surface) -> Option<Self::Window>;
    /// Runs `task` from the event loop after the current frame has ended.
    fn defer(&self, task: impl FnOnce(&Self) + 'static);
}

/// Text-input focus entered `surface`: the old owner's mark goes, then the new owner is enabled.
pub(crate) fn enter<H: ImeHost>(host: &H, surface: H::Surface) {
    let old = host.with_session(|ime, _| ime.enter(surface));
    if let Some(window) = old.and_then(|old| host.window(&old)) {
        window.ime_input(ImeInput::DeleteText);
    }
    enable(host);
}

/// Enables text input for the owner with its current caret and context.
pub(crate) fn enable<H: ImeHost>(host: &H) {
    let Some(owner) = host.with_session(|ime, _| ime.owner().cloned()) else {
        return;
    };
    let (area, context) = host.window(&owner).map_or((None, None), |window| {
        (window.ime_area(), window.ime_context())
    });
    host.with_session(|ime, text_input| {
        if let Some(text_input) = text_input {
            ime.enable(text_input, &owner, area, context);
        }
    });
}

/// Text-input focus left `surface`: its composition ends and its mark goes.
pub(crate) fn leave<H: ImeHost>(host: &H, surface: &H::Surface) {
    if host.with_session(|ime, text_input| ime.release(text_input, surface))
        && let Some(window) = host.window(surface)
    {
        window.ime_input(ImeInput::DeleteText);
    }
}

/// Keyboard focus left `surface`; it can precede the text-input leave, so the composition and mark end here.
pub(crate) fn keyboard_left<H: ImeHost>(host: &H, surface: &H::Surface) {
    host.with_session(|ime, text_input| ime.release(text_input, surface));
    if let Some(window) = host.window(surface) {
        window.ime_input(ImeInput::DeleteText);
    }
}

/// A press in `window` ends its composition by a disable and enable, since text-input-v3 has no reset.
pub(crate) fn pointer_pressed<H: ImeHost>(host: &H, window: &H::Window) -> bool {
    if !host.with_session(|ime, text_input| ime.composing() && text_input.is_some()) {
        return false;
    }
    host.with_session(|ime, text_input| ime.disable(text_input));
    window.ime_input(ImeInput::UnmarkText);
    enable(host);
    true
}

/// The text-input object was destroyed with the seat's keyboard, so the old owner's mark goes.
pub(crate) fn text_input_destroyed<H: ImeHost>(host: &H) {
    let old = host.with_session(|ime, _| ime.reset());
    if let Some(window) = old.and_then(|old| host.window(&old)) {
        window.ime_input(ImeInput::DeleteText);
    }
}

/// Keyboard capability disappeared; clear the focused field even without a text-input owner.
pub(crate) fn keyboard_removed<H: ImeHost>(host: &H, focused: Option<&H::Surface>) {
    if let Some(surface) = focused {
        keyboard_left(host, surface);
    }
    text_input_destroyed(host);
}

/// The window on `surface` is closing, so its composition ends with it.
pub(crate) fn window_closed<H: ImeHost>(host: &H, surface: &H::Surface) {
    host.with_session(|ime, text_input| ime.release(text_input, surface));
}

/// Delivers the batch a `done` closes to its owner, then publishes the owner's new state.
pub(crate) fn done<H: ImeHost>(host: &H, serial: u32) {
    let Some((surface, action)) = host.with_session(|ime, _| ime.done(serial)) else {
        return;
    };
    let Some(window) = host.window(&surface) else {
        return;
    };
    match action {
        ImeAction::Key(text) => window.ime_key(text),
        ImeAction::Apply(batch) => window.ime_input(ImeInput::Apply(batch)),
    }
    if !host.with_session(|ime, _| ime.wants_publish(&surface)) {
        return;
    }
    let area = window.ime_area();
    let context = window.ime_context();
    if host.with_session(|ime, text_input| {
        text_input.is_some_and(|text_input| ime.publish_blocked(text_input, context.as_ref()))
    }) {
        return;
    }
    publish(host, &surface, context, area, ChangeCause::InputMethod);
}

/// GPUI moves the caret while it holds the input handler, so the context is read once the frame ends.
pub(crate) fn caret_moved<H: ImeHost>(host: &H, surface: H::Surface, bounds: Bounds<Pixels>) {
    if host.with_session(|ime, _| ime.queue_position(surface, bounds)) {
        host.defer(publish_queued);
    }
}

fn publish_queued<H: ImeHost>(host: &H) {
    let Some((surface, bounds)) = host.with_session(|ime, _| ime.take_queued()) else {
        return;
    };
    let Some(window) = host.window(&surface) else {
        return;
    };
    let context = window.ime_context();
    publish(host, &surface, context, Some(bounds), ChangeCause::Other);
}

fn publish<H: ImeHost>(
    host: &H,
    surface: &H::Surface,
    context: Option<SurroundingText>,
    bounds: Option<Bounds<Pixels>>,
    cause: ChangeCause,
) {
    host.with_session(|ime, text_input| {
        if let Some(text_input) = text_input {
            ime.publish(text_input, surface, context, bounds, cause);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::ops::Range;
    use std::rc::Rc;

    use super::super::ime::testing::{FakeTextInput, Request, caret, preedit};
    use super::super::ime_context::{FakeField, handle, surrounding};
    use super::*;

    // A window with one text field; `busy` stands for GPUI holding its input handler.
    struct FakeWindow {
        field: RefCell<FakeField>,
        caret: Cell<Bounds<Pixels>>,
        context: Cell<bool>,
        busy: Cell<bool>,
        keys: RefCell<Vec<String>>,
    }

    impl FakeWindow {
        fn text(&self) -> String {
            self.field.borrow().text.clone()
        }

        fn marked(&self) -> Option<Range<usize>> {
            self.field.borrow().marked.clone()
        }
    }

    impl ImeWindow for Rc<FakeWindow> {
        fn ime_input(&self, input: ImeInput) {
            if !self.busy.get() {
                handle(&mut *self.field.borrow_mut(), input);
            }
        }

        fn ime_key(&self, text: String) {
            self.keys.borrow_mut().push(text);
        }

        fn ime_area(&self) -> Option<Bounds<Pixels>> {
            (!self.busy.get()).then(|| self.caret.get())
        }

        fn ime_context(&self) -> Option<SurroundingText> {
            if self.busy.get() || !self.context.get() {
                return None;
            }
            surrounding(&mut *self.field.borrow_mut())
        }
    }

    // The client's side of the seam: one session, one text input, its windows and its idle queue.
    #[derive(Default)]
    struct FakeHost {
        session: RefCell<ImeSession<u32>>,
        text_input: FakeTextInput,
        windows: RefCell<Vec<(u32, Rc<FakeWindow>)>>,
        idle: RefCell<Vec<Box<dyn FnOnce(&FakeHost)>>>,
    }

    impl ImeHost for FakeHost {
        type Surface = u32;
        type TextInput = FakeTextInput;
        type Window = Rc<FakeWindow>;

        fn with_session<R>(
            &self,
            f: impl FnOnce(&mut ImeSession<u32>, Option<&FakeTextInput>) -> R,
        ) -> R {
            f(&mut self.session.borrow_mut(), Some(&self.text_input))
        }

        fn window(&self, surface: &u32) -> Option<Rc<FakeWindow>> {
            self.windows
                .borrow()
                .iter()
                .find(|(id, _)| id == surface)
                .map(|(_, window)| window.clone())
        }

        fn defer(&self, task: impl FnOnce(&Self) + 'static) {
            self.idle.borrow_mut().push(Box::new(task));
        }
    }

    impl FakeHost {
        fn open(&self, surface: u32, text: &str, selection: Range<usize>) -> Rc<FakeWindow> {
            let window = Rc::new(FakeWindow {
                field: RefCell::new(FakeField::new(text, selection)),
                caret: Cell::new(caret(10.0)),
                context: Cell::new(true),
                busy: Cell::new(false),
                keys: RefCell::default(),
            });
            self.windows.borrow_mut().push((surface, window.clone()));
            window
        }

        // Opens a window and gives it text-input focus, as the compositor's `enter` does.
        fn focused(&self, surface: u32, text: &str, selection: Range<usize>) -> Rc<FakeWindow> {
            let window = self.open(surface, text, selection);
            enter(self, surface);
            window
        }

        fn close(&self, surface: u32) {
            window_closed(self, &surface);
            self.windows.borrow_mut().retain(|(id, _)| *id != surface);
        }

        // The event loop going idle after a frame.
        fn idle(&self) {
            let tasks = std::mem::take(&mut *self.idle.borrow_mut());
            for task in tasks {
                task(self);
            }
        }

        fn ime(&self) -> std::cell::RefMut<'_, ImeSession<u32>> {
            self.session.borrow_mut()
        }

        fn take(&self) -> Vec<Request> {
            self.text_input.take()
        }
    }

    // GPUI moves the caret inside a frame, with the window's input handler taken.
    fn move_caret_in_frame(host: &FakeHost, surface: u32, window: &FakeWindow, x: f32) {
        window.busy.set(true);
        caret_moved(host, surface, caret(x));
        window.busy.set(false);
    }

    #[test]
    fn ime_caret_move_reads_the_context_after_the_frame() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 2..2);
        host.take();
        window.field.borrow_mut().selection = 1..1;
        window.busy.set(true);
        caret_moved(&host, 1, caret(20.0));
        caret_moved(&host, 1, caret(22.0));
        window.busy.set(false);
        assert!(host.take().is_empty(), "nothing is sent inside the frame");
        host.idle();
        assert_eq!(
            host.take(),
            [
                Request::Surrounding("ab".into(), 1, 1),
                Request::Cause(ChangeCause::Other),
                Request::Rectangle(22, 20, 1, 18),
                Request::Commit
            ],
            "one publication with the moved caret, no disable or enable"
        );
        host.idle();
        assert!(host.take().is_empty(), "the move is published once");
    }

    #[test]
    fn ime_preedit_marks_its_utf16_range_and_moves_the_caret_inside_it() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 1..1);
        host.ime().set_preedit(preedit("中\u{20000}x", 3, 7));
        window.caret.set(caret(12.0));
        done(&host, 1);
        assert!(window.keys.borrow().is_empty());
        assert_eq!(window.text(), "a中\u{20000}xb");
        assert_eq!(window.marked(), Some(1..5));
        assert_eq!(window.field.borrow().selection, 2..4);
        assert!(window.field.borrow().cursor_visible);
        assert!(host.ime().composing());

        host.ime().set_preedit(preedit("中", -1, -1));
        window.caret.set(caret(14.0));
        done(&host, 2);
        assert_eq!(window.text(), "a中b", "a new preedit replaces the old one");
        assert_eq!(window.marked(), Some(1..2));
        assert!(
            !window.field.borrow().cursor_visible,
            "a -1 cursor hides the caret"
        );

        host.ime().set_preedit(None);
        done(&host, 3);
        assert_eq!(
            window.text(),
            "ab",
            "an empty batch cancels the composition"
        );
        assert_eq!(window.marked(), None);
        assert!(window.field.borrow().cursor_visible);
        assert!(!host.ime().composing());
    }

    #[test]
    fn ime_hidden_preedit_cursor_hides_the_caret_until_the_composition_ends() {
        let host = FakeHost::default();
        host.open(2, "", 0..0);
        let window = host.focused(1, "ab", 1..1);
        let visible = || window.field.borrow().cursor_visible;
        let compose = |text: Option<&str>, begin: i32, end: i32| {
            host.ime()
                .set_preedit(text.and_then(|text| preedit(text, begin, end)));
            done(&host, host.text_input.commits());
        };

        compose(Some("你"), 3, 3);
        assert!(visible(), "the caret drawn is the engine's cursor");
        assert_eq!(window.field.borrow().selection, 2..2);
        compose(Some("你"), -1, -1);
        assert!(!visible(), "a -1 cursor leaves no caret while composing");
        compose(Some("你好"), -1, -1);
        assert!(
            !visible(),
            "the caret stays hidden while the preedit changes"
        );
        assert_eq!(window.marked(), Some(1..3));

        host.ime().set_commit(Some("你好".into()));
        compose(None, 0, 0);
        assert_eq!(window.text(), "a你好b");
        assert!(visible(), "the caret returns after a commit");

        compose(Some("ni"), -1, -1);
        assert!(!visible());
        compose(None, 0, 0);
        assert_eq!(window.text(), "a你好b");
        assert_eq!(window.marked(), None);
        assert!(
            visible(),
            "the caret returns after Escape cancels the preedit"
        );

        compose(Some("n"), -1, -1);
        assert!(!visible());
        host.ime().set_commit(Some("n".into()));
        compose(None, 0, 0);
        assert!(
            window.keys.borrow().is_empty(),
            "a one-byte commit that ends a composition is not a key"
        );
        assert_eq!(window.text(), "a你好nb");
        assert_eq!(window.marked(), None);
        assert!(visible(), "the caret returns after a one-byte commit");

        compose(Some("ka"), -1, -1);
        assert!(!visible());
        host.take();
        assert!(pointer_pressed(&host, &window));
        assert!(
            host.take()
                .starts_with(&[Request::Disable, Request::Commit, Request::Enable]),
            "a press restarts the input method"
        );
        assert_eq!(window.text(), "a你好nkab", "a press keeps the preedit text");
        assert_eq!(window.marked(), None);
        assert!(
            visible(),
            "the caret returns after a click ends the composition"
        );
        assert!(!pointer_pressed(&host, &window), "no composition is left");

        let before = window.text();
        compose(Some("ta"), -1, -1);
        assert!(!visible());
        keyboard_left(&host, &1);
        leave(&host, &1);
        enter(&host, 2);
        assert_eq!(
            window.text(),
            before,
            "the field that lost focus drops its mark"
        );
        assert_eq!(window.marked(), None);
        assert!(visible(), "the caret returns after focus loss");
    }

    #[test]
    fn ime_preedit_context_omits_the_preedit_text() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 1..1);
        assert_eq!(
            host.take(),
            [
                Request::Enable,
                Request::Rectangle(10, 20, 1, 18),
                Request::Surrounding("ab".into(), 1, 1),
                Request::Commit
            ]
        );
        host.ime().set_preedit(preedit("ni", 2, 2));
        window.caret.set(caret(12.0));
        done(&host, 1);
        assert_eq!(window.text(), "anib");
        assert_eq!(
            host.take(),
            [Request::Rectangle(12, 20, 1, 18), Request::Commit],
            "the context stays `ab` with the caret at 1, so only the caret moves"
        );
    }

    #[test]
    fn ime_commit_replaces_the_preedit_exactly_once() {
        let host = FakeHost::default();
        let window = host.focused(1, "", 0..0);
        host.ime().set_preedit(preedit("nihao", 5, 5));
        window.caret.set(caret(40.0));
        done(&host, 1);
        host.ime().set_commit(Some("你好".into()));
        host.ime().set_preedit(None);
        window.caret.set(caret(30.0));
        done(&host, 2);
        assert_eq!(window.text(), "你好");
        assert_eq!(window.marked(), None);
        assert_eq!(window.field.borrow().selection, 2..2);
        assert!(!host.ime().composing());
        assert_eq!(
            host.take()[6..],
            [
                Request::Surrounding("你好".into(), 6, 6),
                Request::Cause(ChangeCause::InputMethod),
                Request::Rectangle(30, 20, 1, 18),
                Request::Commit
            ],
            "the input method learns the committed context and caret"
        );
        done(&host, 3);
        assert_eq!(
            window.text(),
            "你好",
            "a repeated done must not commit again"
        );
    }

    #[test]
    fn ime_commit_of_one_ascii_byte_is_a_key_only_outside_composition() {
        let host = FakeHost::default();
        let window = host.focused(1, "", 0..0);
        host.ime().set_commit(Some("a".into()));
        done(&host, 1);
        assert_eq!(*window.keys.borrow(), ["a"]);
        assert_eq!(
            window.text(),
            "",
            "the key path leaves insertion to the key binding"
        );

        host.ime().set_preedit(preedit("ㅎ", 3, 3));
        done(&host, 1);
        host.ime().set_commit(Some("a".into()));
        done(&host, 1);
        assert_eq!(*window.keys.borrow(), ["a"], "no key mid-composition");
        assert_eq!(
            window.text(),
            "a",
            "an ASCII commit that ends a composition replaces it"
        );
        assert_eq!(window.marked(), None);
    }

    #[test]
    fn ime_deletes_surrounding_bytes_around_the_selection() {
        let host = FakeHost::default();
        let window = host.focused(1, "a中z", 2..2);
        host.ime().set_delete(1, 0);
        host.ime().set_commit(Some("x".into()));
        done(&host, 1);
        assert_eq!(
            window.text(),
            "a中xz",
            "a deletion ending inside a codepoint is refused and the commit still lands"
        );

        let host = FakeHost::default();
        let window = host.focused(1, "a中\u{20000}z", 1..2);
        host.ime().set_delete(1, 4);
        done(&host, 1);
        assert_eq!(window.text(), "中z");
        assert_eq!(
            window.field.borrow().selection,
            0..1,
            "the selected text survives the deletion"
        );
    }

    #[test]
    fn ime_deletion_counts_from_the_start_of_the_replaced_preedit() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 2..2);
        host.ime().set_preedit(preedit("ni", 2, 2));
        done(&host, 1);
        host.ime().set_delete(1, 0);
        host.ime().set_commit(Some("你".into()));
        done(&host, 1);
        assert_eq!(window.text(), "a你");
        assert_eq!(window.marked(), None);
        assert_eq!(window.field.borrow().selection, 2..2);
    }

    #[test]
    fn ime_cursor_rectangle_is_published_after_a_matching_done_only() {
        let host = FakeHost::default();
        let window = host.focused(1, "", 0..0);
        host.take();
        host.ime().set_preedit(preedit("ka", 2, 2));
        window.caret.set(caret(20.0));
        done(&host, 0);
        assert_eq!(window.text(), "ka", "a stale done still applies its text");
        assert!(host.take().is_empty(), "a stale done publishes nothing");

        host.ime().set_preedit(preedit("か", 3, 3));
        window.caret.set(caret(20.75));
        done(&host, 1);
        assert_eq!(window.text(), "か");
        assert_eq!(
            host.take(),
            [Request::Rectangle(20, 20, 1, 18), Request::Commit]
        );
        move_caret_in_frame(&host, 1, &window, 20.25);
        host.idle();
        assert!(
            host.take().is_empty(),
            "the same protocol rectangle is not sent twice"
        );
    }

    #[test]
    fn ime_context_support_changes_reset_only_outside_composition() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 2..2);
        host.ime().set_preedit(preedit("ni", 2, 2));
        done(&host, 1);
        host.take();
        window.context.set(false);
        move_caret_in_frame(&host, 1, &window, 30.0);
        host.idle();
        assert_eq!(
            host.take(),
            [Request::Rectangle(30, 20, 1, 18), Request::Commit],
            "mid-composition the last context stands"
        );
        window.context.set(true);
        host.ime().set_commit(Some("你".into()));
        host.ime().set_preedit(None);
        window.caret.set(caret(20.0));
        done(&host, 2);
        host.take();
        window.context.set(false);
        move_caret_in_frame(&host, 1, &window, 20.0);
        host.idle();
        assert_eq!(
            host.take(),
            [
                Request::Disable,
                Request::Enable,
                Request::Rectangle(20, 20, 1, 18),
                Request::Commit
            ],
            "a field without context withdraws it through enable"
        );
    }

    #[test]
    fn ime_focus_change_mid_composition_clears_the_old_field() {
        let host = FakeHost::default();
        let second = host.open(2, "", 0..0);
        second.caret.set(caret(50.0));
        let first = host.focused(1, "", 0..0);
        host.ime().set_preedit(preedit("nihao", 5, 5));
        done(&host, 1);
        assert_eq!(first.marked(), Some(0..5));

        host.ime().set_commit(Some("你好".into()));
        enter(&host, 2);
        assert_eq!(first.text(), "", "the old owner's mark is cleared");
        assert_eq!(first.marked(), None);
        assert!(!host.ime().composing());
        assert!(host.ime().owns(&2));

        let serial = host.text_input.commits();
        done(&host, serial);
        assert_eq!(
            second.text(),
            "",
            "the batch begun on the old surface is dropped"
        );
        host.ime().set_preedit(preedit("a", 1, 1));
        second.caret.set(caret(52.0));
        done(&host, serial);
        assert_eq!(second.text(), "a");
        assert_eq!(second.marked(), Some(0..1));
        assert_eq!(
            first.text(),
            "",
            "nothing reaches the field that lost focus"
        );

        host.take();
        keyboard_left(&host, &2);
        assert_eq!(
            host.take(),
            [Request::Disable, Request::Commit],
            "the keyboard leave ends the composition"
        );
        assert_eq!(
            second.text(),
            "",
            "the field that lost focus drops its mark"
        );
        host.ime().set_commit(Some("x".into()));
        done(&host, host.text_input.commits());
        leave(&host, &2);
        assert!(
            host.take().is_empty(),
            "the text-input leave after it sends nothing"
        );
        enter(&host, 1);
        host.ime().set_preedit(preedit("b", 1, 1));
        done(&host, host.text_input.commits());
        assert_eq!(
            second.text(),
            "",
            "late events after the keyboard leave are ignored"
        );
        assert_eq!(first.text(), "b", "the next owner composes at once");
    }

    #[test]
    fn ime_text_input_destroyed_mid_composition_clears_the_field() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 1..1);
        host.ime().set_preedit(preedit("ka", -1, -1));
        done(&host, host.text_input.commits());
        host.take();
        text_input_destroyed(&host);
        assert!(
            host.take().is_empty(),
            "a destroyed text input takes no requests"
        );
        assert_eq!(window.text(), "ab", "the field drops its mark");
        assert_eq!(window.marked(), None);
        assert!(window.field.borrow().cursor_visible);
        assert_eq!(host.ime().owner(), None);
        host.ime().set_commit(Some("か".into()));
        done(&host, 0);
        assert_eq!(
            window.text(),
            "ab",
            "late events from the old object are ignored"
        );
    }

    #[test]
    fn ime_leave_mid_composition_disables_and_ignores_late_events() {
        let host = FakeHost::default();
        let window = host.focused(1, "", 0..0);
        host.ime().set_preedit(preedit("ka", 2, 2));
        done(&host, 1);
        host.take();
        leave(&host, &1);
        assert_eq!(host.take(), [Request::Disable, Request::Commit]);
        assert_eq!(
            window.text(),
            "",
            "the field that lost focus drops its mark"
        );
        assert_eq!(window.marked(), None);
        assert!(!host.ime().composing());
        host.ime().set_commit(Some("か".into()));
        done(&host, 2);
        move_caret_in_frame(&host, 1, &window, 10.0);
        host.idle();
        assert_eq!(window.text(), "");
        assert!(host.take().is_empty());
    }

    #[test]
    fn ime_window_closed_mid_composition_drops_its_batch() {
        let host = FakeHost::default();
        let next = host.open(2, "", 0..0);
        let closing = host.focused(1, "", 0..0);
        host.ime().set_preedit(preedit("ni", 2, 2));
        done(&host, 1);
        host.ime().set_commit(Some("你".into()));
        host.take();

        host.close(1);
        assert_eq!(
            host.take(),
            [Request::Disable, Request::Commit],
            "closing the owner ends its text input"
        );
        assert!(!host.ime().composing());
        assert_eq!(host.ime().owner(), None);
        done(&host, 2);
        assert_eq!(
            closing.text(),
            "ni",
            "a done for the closed window is ignored"
        );

        enter(&host, 2);
        let serial = host.text_input.commits();
        host.ime().set_preedit(preedit("a", 1, 1));
        next.caret.set(caret(12.0));
        done(&host, serial);
        assert_eq!(next.text(), "a");
        assert!(
            host.ime().wants_publish(&2),
            "the next window's serials still match"
        );
    }

    #[test]
    fn fix1_a1_blocked_app_state_survives_until_matching_done_or_enable() {
        for reenable in [false, true] {
            let host = FakeHost::default();
            let window = host.focused(1, "ab", 2..2);
            host.take();
            move_caret_in_frame(&host, 1, &window, 20.0);
            host.idle();
            host.take();
            done(&host, 1);
            assert!(host.take().is_empty(), "stale done must not publish");
            for (text, x) in [("abc", 40.0), ("abcd", 60.0)] {
                *window.field.borrow_mut() = FakeField::new(text, text.len()..text.len());
                move_caret_in_frame(&host, 1, &window, x);
                host.idle();
                assert!(host.take().is_empty(), "blocked updates must wait");
            }
            if reenable {
                enable(&host);
            } else {
                done(&host, host.text_input.commits());
            }
            let requests = host.take();
            assert!(
                requests.contains(&Request::Rectangle(60, 20, 1, 18)),
                "latest blocked caret: {requests:?}"
            );
            assert!(
                requests.contains(&Request::Surrounding("abcd".into(), 4, 4)),
                "latest blocked context: {requests:?}"
            );
            assert_eq!(
                requests.iter().filter(|r| **r == Request::Commit).count(),
                1
            );
            assert!(!requests.contains(&Request::Rectangle(40, 20, 1, 18)));
        }
    }

    #[test]
    fn fix1_b1_pointer_restart_publishes_the_retained_preedit() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 1..1);
        host.ime().set_preedit(preedit("ni", 2, 2));
        done(&host, host.text_input.commits());
        host.take();
        assert!(pointer_pressed(&host, &window));
        host.idle();
        assert_eq!(window.text(), "anib");
        assert_eq!(window.marked(), None);
        assert_eq!(
            host.take(),
            [
                Request::Disable,
                Request::Commit,
                Request::Enable,
                Request::Rectangle(10, 20, 1, 18),
                Request::Surrounding("anib".into(), 3, 3),
                Request::Commit
            ]
        );
    }

    #[test]
    fn fix1_b2_keyboard_removal_clears_a_mark_without_an_ime_owner() {
        let host = FakeHost::default();
        let window = host.open(1, "ab", 1..1);
        window.ime_input(ImeInput::SetMarkedText("x".into()));
        assert_eq!(host.ime().owner(), None);
        keyboard_removed(&host, Some(&1));
        assert_eq!(window.text(), "ab");
        assert_eq!(window.marked(), None);
        assert!(window.field.borrow().cursor_visible);
        assert_eq!(host.ime().owner(), None);
        assert!(host.take().is_empty());
    }
    #[test]
    fn fix1_a1_matching_done_preserves_its_edit_with_a_blocked_app_update() {
        let host = FakeHost::default();
        let window = host.focused(1, "ab", 2..2);
        host.take();
        move_caret_in_frame(&host, 1, &window, 20.0);
        host.idle();
        host.take();
        done(&host, 1);
        *window.field.borrow_mut() = FakeField::new("abcd", 4..4);
        move_caret_in_frame(&host, 1, &window, 60.0);
        host.idle();
        assert!(host.take().is_empty());
        host.ime().set_commit(Some("中".into()));
        done(&host, host.text_input.commits());
        let requests = host.take();
        assert!(
            requests.contains(&Request::Surrounding("abcd中".into(), 7, 7)),
            "{requests:?}"
        );
        assert!(requests.contains(&Request::Rectangle(60, 20, 1, 18)));
    }
    #[test]
    fn fix1_a1_replay_respects_withdrawn_surrounding_text_support() {
        for reenable in [false, true] {
            let host = FakeHost::default();
            let window = host.focused(1, "ab", 2..2);
            host.take();
            move_caret_in_frame(&host, 1, &window, 20.0);
            host.idle();
            host.take();
            done(&host, 1);
            *window.field.borrow_mut() = FakeField::new("private", 7..7);
            move_caret_in_frame(&host, 1, &window, 60.0);
            host.idle();
            assert!(host.take().is_empty());
            window.context.set(false);
            if reenable {
                enable(&host);
            } else {
                done(&host, host.text_input.commits());
            }
            let requests = host.take();
            assert!(
                !requests
                    .iter()
                    .any(|r| matches!(r, Request::Surrounding(_, _, _))),
                "{requests:?}"
            );
            assert!(requests.contains(&Request::Enable));
            assert!(requests.contains(&Request::Rectangle(60, 20, 1, 18)));
        }
    }
    #[test]
    fn fix1_a1_replay_coalesces_a_newer_caret_before_idle() {
        for reenable in [false, true] {
            let host = FakeHost::default();
            let window = host.focused(1, "ab", 2..2);
            host.take();
            move_caret_in_frame(&host, 1, &window, 20.0);
            host.idle();
            host.take();
            done(&host, 1);
            *window.field.borrow_mut() = FakeField::new("abc", 3..3);
            move_caret_in_frame(&host, 1, &window, 40.0);
            host.idle();
            *window.field.borrow_mut() = FakeField::new("abcd", 4..4);
            move_caret_in_frame(&host, 1, &window, 60.0);
            assert!(host.take().is_empty());
            if reenable {
                enable(&host);
            } else {
                done(&host, host.text_input.commits());
            }
            let requests = host.take();
            assert!(
                requests.contains(&Request::Rectangle(60, 20, 1, 18)),
                "{requests:?}"
            );
            assert!(requests.contains(&Request::Surrounding("abcd".into(), 4, 4)));
            assert_eq!(
                requests.iter().filter(|r| **r == Request::Commit).count(),
                1
            );
            host.idle();
            assert!(
                host.take().is_empty(),
                "deferred callbacks must not republish replayed state"
            );
        }
    }
}
