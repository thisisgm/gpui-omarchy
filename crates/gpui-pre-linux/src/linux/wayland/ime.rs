use gpui::{Bounds, Pixels};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ChangeCause;

use super::ime_context::{Preedit, SurroundingText};

/// One text-input-v3 batch, applied to the field when its `done` arrives.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ImeBatch {
    pub commit: Option<String>,
    pub preedit: Option<Preedit>,
    pub delete: (u32, u32),
}

/// What a `done` asks of the owning window.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ImeAction {
    /// A single ASCII byte outside composition, delivered as a key so bindings still see it.
    Key(String),
    Apply(ImeBatch),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImeCursorRectangle {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl From<Bounds<Pixels>> for ImeCursorRectangle {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: bounds.origin.x.as_f32() as i32,
            y: bounds.origin.y.as_f32() as i32,
            width: bounds.size.width.as_f32() as i32,
            height: bounds.size.height.as_f32() as i32,
        }
    }
}

/// The text-input-v3 requests a session sends; the session is their only caller.
pub(crate) trait TextInputRequests {
    fn request_enable(&self);
    fn request_disable(&self);
    fn request_surrounding_text(&self, text: String, cursor: i32, anchor: i32);
    fn request_change_cause(&self, cause: ChangeCause);
    fn request_cursor_rectangle(&self, rectangle: ImeCursorRectangle);
    fn request_commit(&self);
}

struct AppPublication<S> {
    surface: S,
    context: Option<SurroundingText>,
    bounds: Option<Bounds<Pixels>>,
}

/// The seat's text-input owner, pending batch, commit serial and published state.
pub(crate) struct ImeSession<S> {
    owner: Option<S>,
    enabled: Option<bool>,
    pending: ImeBatch,
    commits: u32,
    publish: bool,
    composing: bool,
    context: Option<SurroundingText>,
    rectangle: Option<ImeCursorRectangle>,
    queued: Option<(S, Bounds<Pixels>)>,
    blocked: Option<AppPublication<S>>,
}

impl<S> Default for ImeSession<S> {
    fn default() -> Self {
        Self {
            owner: None,
            enabled: None,
            pending: ImeBatch::default(),
            commits: 0,
            publish: true,
            composing: false,
            context: None,
            rectangle: None,
            queued: None,
            blocked: None,
        }
    }
}

impl<S: Clone + PartialEq> ImeSession<S> {
    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    pub fn composing(&self) -> bool {
        self.composing
    }

    pub fn owner(&self) -> Option<&S> {
        self.owner.as_ref()
    }

    pub fn owns(&self, surface: &S) -> bool {
        self.owner.as_ref() == Some(surface)
    }

    /// Batch events count only while an entered surface has text input enabled.
    pub fn accepts_events(&self) -> bool {
        self.owner.is_some() && self.enabled == Some(true)
    }

    pub fn set_commit(&mut self, text: Option<String>) {
        if self.accepts_events() {
            self.pending.commit = text;
        }
    }

    pub fn set_preedit(&mut self, preedit: Option<Preedit>) {
        if self.accepts_events() {
            self.pending.preedit = preedit;
        }
    }

    pub fn set_delete(&mut self, before: u32, after: u32) {
        if self.accepts_events() {
            self.pending.delete = (before, after);
        }
    }

    fn forget(&mut self) {
        self.pending = ImeBatch::default();
        self.composing = false;
        self.context = None;
        self.rectangle = None;
        self.queued = None;
        self.blocked = None;
    }

    fn commit(&mut self, text_input: &impl TextInputRequests) {
        text_input.request_commit();
        self.commits = self.commits.wrapping_add(1);
    }

    /// Text-input focus entered `surface`; returns the previous owner, whose mark must go.
    pub fn enter(&mut self, surface: S) -> Option<S> {
        let previous = self.owner.replace(surface);
        self.forget();
        previous
    }

    /// Enables text input for `surface` with its current caret and context.
    pub fn enable(
        &mut self,
        text_input: &impl TextInputRequests,
        surface: &S,
        bounds: Option<Bounds<Pixels>>,
        context: Option<SurroundingText>,
    ) {
        if !self.owns(surface) {
            return;
        }
        let bounds = self
            .queued
            .take()
            .map(|(_, bounds)| bounds)
            .or_else(|| self.blocked.take().and_then(|blocked| blocked.bounds))
            .or(bounds);
        self.forget();
        self.enabled = Some(true);
        self.publish = true;
        text_input.request_enable();
        if let Some(rectangle) = bounds.map(ImeCursorRectangle::from) {
            text_input.request_cursor_rectangle(rectangle);
            self.rectangle = Some(rectangle);
        }
        if let Some(context) = &context {
            text_input.request_surrounding_text(
                context.text.clone(),
                context.cursor,
                context.anchor,
            );
        }
        self.context = context;
        self.commit(text_input);
    }

    pub fn disable(&mut self, text_input: Option<&impl TextInputRequests>) {
        self.enabled = Some(false);
        self.forget();
        if let Some(text_input) = text_input {
            text_input.request_disable();
            self.commit(text_input);
        }
    }

    /// `surface` lost text-input or keyboard focus, or closed; true when it owned composition.
    pub fn release(&mut self, text_input: Option<&impl TextInputRequests>, surface: &S) -> bool {
        if !self.owns(surface) {
            return false;
        }
        self.owner = None;
        if self.enabled == Some(true) {
            self.disable(text_input);
        } else {
            self.forget();
        }
        true
    }

    /// The text-input object was destroyed; returns the owner, whose mark must go.
    pub fn reset(&mut self) -> Option<S> {
        let owner = self.owner.take();
        *self = Self::default();
        owner
    }

    /// Consumes the batch; a serial other than the commit count applies text but publishes nothing.
    pub fn done(&mut self, serial: u32) -> Option<(S, ImeAction)> {
        let batch = std::mem::take(&mut self.pending);
        if !self.accepts_events() {
            return None;
        }
        self.publish = serial == self.commits;
        let was_composing = self.composing;
        self.composing = batch
            .preedit
            .as_ref()
            .is_some_and(|preedit| !preedit.text.is_empty());
        let key = batch.delete == (0, 0)
            && !was_composing
            && !self.composing
            && batch.commit.as_ref().is_some_and(|text| text.len() == 1);
        let action = match batch.commit {
            Some(text) if key => ImeAction::Key(text),
            _ => ImeAction::Apply(batch),
        };
        Some((self.owner.clone()?, action))
    }

    pub fn wants_publish(&self, surface: &S) -> bool {
        self.publish && self.owns(surface) && self.enabled == Some(true)
    }

    /// Sends what changed since the last publication, in one commit.
    pub fn publish(
        &mut self,
        text_input: &impl TextInputRequests,
        surface: &S,
        mut context: Option<SurroundingText>,
        bounds: Option<Bounds<Pixels>>,
        cause: ChangeCause,
    ) {
        if !self.wants_publish(surface) {
            if cause == ChangeCause::Other && self.owns(surface) && self.enabled == Some(true) {
                self.blocked = Some(AppPublication {
                    surface: surface.clone(),
                    context,
                    bounds,
                });
            }
            return;
        }
        let mut changed = false;
        let mut bounds = bounds.map(ImeCursorRectangle::from);
        if context.is_some() != self.context.is_some() {
            if self.composing {
                context = self.context.clone();
            } else {
                // Enable is the only request that withdraws surrounding-text support.
                text_input.request_disable();
                text_input.request_enable();
                self.context = None;
                // The reset drops the rectangle, so the one last sent goes out again.
                bounds = bounds.or(self.rectangle.take());
                changed = true;
            }
        }
        if let Some(context) = context.filter(|context| self.context.as_ref() != Some(context)) {
            text_input.request_surrounding_text(
                context.text.clone(),
                context.cursor,
                context.anchor,
            );
            text_input.request_change_cause(cause);
            self.context = Some(context);
            changed = true;
        }
        if let Some(rectangle) = bounds.filter(|rectangle| self.rectangle != Some(*rectangle)) {
            text_input.request_cursor_rectangle(rectangle);
            self.rectangle = Some(rectangle);
            changed = true;
        }
        if changed {
            self.commit(text_input);
        }
    }

    pub fn publish_blocked(
        &mut self,
        text_input: &impl TextInputRequests,
        current_context: Option<&SurroundingText>,
    ) -> bool {
        if !self.publish {
            return false;
        }
        if let Some((surface, bounds)) = self.queued.take() {
            self.blocked = Some(AppPublication {
                surface,
                context: current_context.cloned(),
                bounds: Some(bounds),
            });
        }
        let Some(blocked) = self.blocked.take() else {
            return false;
        };
        if !self.wants_publish(&blocked.surface) {
            return false;
        }
        let context = if current_context == blocked.context.as_ref() {
            blocked.context
        } else {
            current_context.cloned()
        };
        self.publish(
            text_input,
            &blocked.surface,
            context,
            blocked.bounds,
            ChangeCause::Other,
        );
        true
    }

    /// Holds an app-side caret move; true when the caller must schedule the deferred publish.
    pub fn queue_position(&mut self, surface: S, bounds: Bounds<Pixels>) -> bool {
        if !self.owns(&surface) || self.enabled != Some(true) {
            return false;
        }
        self.queued.replace((surface, bounds)).is_none()
    }

    pub fn take_queued(&mut self) -> Option<(S, Bounds<Pixels>)> {
        self.queued.take()
    }
}

/// Stand-ins the session and event tests share: a recording text input and caret helpers.
#[cfg(test)]
pub(crate) mod testing {
    use std::cell::{Cell, RefCell};

    use gpui::{point, px, size};

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Request {
        Enable,
        Disable,
        Surrounding(String, i32, i32),
        Cause(ChangeCause),
        Rectangle(i32, i32, i32, i32),
        Commit,
    }

    #[derive(Default)]
    pub(crate) struct FakeTextInput {
        requests: RefCell<Vec<Request>>,
        commits: Cell<u32>,
    }

    impl FakeTextInput {
        pub fn take(&self) -> Vec<Request> {
            std::mem::take(&mut self.requests.borrow_mut())
        }

        /// Every commit sent so far, which is the serial a matching `done` carries.
        pub fn commits(&self) -> u32 {
            self.commits.get()
        }
    }

    impl TextInputRequests for FakeTextInput {
        fn request_enable(&self) {
            self.requests.borrow_mut().push(Request::Enable);
        }
        fn request_disable(&self) {
            self.requests.borrow_mut().push(Request::Disable);
        }
        fn request_surrounding_text(&self, text: String, cursor: i32, anchor: i32) {
            self.requests
                .borrow_mut()
                .push(Request::Surrounding(text, cursor, anchor));
        }
        fn request_change_cause(&self, cause: ChangeCause) {
            self.requests.borrow_mut().push(Request::Cause(cause));
        }
        fn request_cursor_rectangle(&self, rectangle: ImeCursorRectangle) {
            self.requests.borrow_mut().push(Request::Rectangle(
                rectangle.x,
                rectangle.y,
                rectangle.width,
                rectangle.height,
            ));
        }
        fn request_commit(&self) {
            self.requests.borrow_mut().push(Request::Commit);
            self.commits.set(self.commits.get().wrapping_add(1));
        }
    }

    pub(crate) fn caret(x: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(20.25)), size(px(1.0), px(18.75)))
    }

    pub(crate) fn preedit(text: &str, begin: i32, end: i32) -> Option<Preedit> {
        Some(Preedit::new(text.into(), begin, end))
    }
}

#[cfg(test)]
mod tests {
    use super::super::ime_context::{FakeField, surrounding};
    use super::testing::{FakeTextInput, Request, caret, preedit};
    use super::*;

    // The client's order: enter, then enable with the owner's caret.
    fn enabled(text_input: &FakeTextInput, surface: u32) -> ImeSession<u32> {
        let mut session = ImeSession::default();
        session.enter(surface);
        session.enable(text_input, &surface, Some(caret(10.0)), None);
        session
    }

    #[test]
    fn ime_batches_wait_for_done_and_reset_after_delivery() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        session.set_commit(Some("日本".into()));
        session.set_preedit(preedit("語", 3, 3));
        session.set_delete(3, 1);
        let (surface, action) = session.done(1).unwrap();
        assert_eq!(surface, 1);
        let ImeAction::Apply(batch) = action else {
            panic!("a composed batch must not become a key");
        };
        assert_eq!(batch.delete, (3, 1));
        assert_eq!(batch.commit.as_deref(), Some("日本"));
        assert_eq!(
            batch.preedit.map(|preedit| preedit.text).as_deref(),
            Some("語")
        );
        assert_eq!(
            session.done(1),
            Some((1, ImeAction::Apply(ImeBatch::default())))
        );
    }

    #[test]
    fn ime_serial_counts_requests_not_previous_done_events() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        session.publish(&text_input, &1, None, Some(caret(20.0)), ChangeCause::Other);
        assert_eq!(text_input.commits(), 2);
        session.done(1);
        assert!(!session.wants_publish(&1));
        session.done(2);
        assert!(session.wants_publish(&1));
        session.publish(&text_input, &1, None, Some(caret(30.0)), ChangeCause::Other);
        session.done(2);
        assert!(!session.wants_publish(&1));
        session.done(3);
        assert!(session.wants_publish(&1));
    }

    #[test]
    fn ime_serial_wraps_and_repeated_matching_done_is_valid() {
        let text_input = FakeTextInput::default();
        let mut session = ImeSession {
            commits: u32::MAX - 1,
            ..Default::default()
        };
        session.enter(1);
        session.enable(&text_input, &1, None, None);
        session.done(u32::MAX);
        assert!(session.wants_publish(&1));
        session.publish(&text_input, &1, None, Some(caret(10.0)), ChangeCause::Other);
        session.done(0);
        assert!(session.wants_publish(&1));
        session.done(0);
        assert!(session.wants_publish(&1));
    }

    #[test]
    fn ime_cursor_rectangle_is_cached_by_its_protocol_coordinates() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        session.publish(&text_input, &1, None, Some(caret(10.0)), ChangeCause::Other);
        session.publish(
            &text_input,
            &1,
            None,
            Some(caret(20.0)),
            ChangeCause::InputMethod,
        );
        session.publish(&text_input, &1, None, Some(caret(20.0)), ChangeCause::Other);
        session.publish(
            &text_input,
            &1,
            None,
            Some(caret(20.75)),
            ChangeCause::Other,
        );
        assert_eq!(
            text_input.take(),
            [
                Request::Enable,
                Request::Rectangle(10, 20, 1, 18),
                Request::Commit,
                Request::Rectangle(20, 20, 1, 18),
                Request::Commit
            ]
        );
    }

    #[test]
    fn ime_stale_done_applies_text_but_publishes_nothing() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        text_input.take();
        session.set_preedit(preedit("ka", 2, 2));
        assert!(matches!(session.done(0), Some((1, ImeAction::Apply(_)))));
        session.publish(
            &text_input,
            &1,
            None,
            Some(caret(20.0)),
            ChangeCause::InputMethod,
        );
        assert!(text_input.take().is_empty());
        session.done(1);
        session.publish(
            &text_input,
            &1,
            None,
            Some(caret(20.0)),
            ChangeCause::InputMethod,
        );
        assert_eq!(
            text_input.take(),
            [Request::Rectangle(20, 20, 1, 18), Request::Commit]
        );
    }

    #[test]
    fn ime_release_ignores_another_surface_and_disables_the_owner() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        session.enter(2);
        session.enable(&text_input, &2, None, None);
        text_input.take();
        assert!(!session.release(Some(&text_input), &1));
        assert!(session.owns(&2));
        assert!(text_input.take().is_empty());
        assert!(session.release(Some(&text_input), &2));
        assert_eq!(session.owner(), None);
        assert_eq!(session.enabled(), Some(false));
        assert_eq!(text_input.take(), [Request::Disable, Request::Commit]);
        assert!(!session.release(Some(&text_input), &2));
    }

    #[test]
    fn ime_enter_drops_the_pending_batch_of_the_old_surface() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        session.set_preedit(preedit("ni", 2, 2));
        session.done(1);
        session.set_commit(Some("日本".into()));
        assert_eq!(session.enter(2), Some(1));
        assert!(!session.composing());
        session.enable(&text_input, &2, None, None);
        assert_eq!(
            session.done(2),
            Some((2, ImeAction::Apply(ImeBatch::default())))
        );
    }

    #[test]
    fn ime_text_input_reset_returns_the_owner_and_restarts_serials() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        session.set_preedit(preedit("ni", 2, 2));
        session.done(1);
        assert_eq!(session.reset(), Some(1));
        assert_eq!(session.owner(), None);
        assert_eq!(session.enabled(), None);
        assert!(!session.composing());
        session.enter(1);
        session.enable(&text_input, &1, None, None);
        session.done(1);
        assert!(
            session.wants_publish(&1),
            "a new text-input object counts from zero"
        );
    }

    #[test]
    fn ime_caret_queue_coalesces_moves_of_the_owner() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        assert!(
            !session.queue_position(2, caret(20.0)),
            "only the owner queues"
        );
        assert!(session.queue_position(1, caret(20.0)));
        assert!(
            !session.queue_position(1, caret(22.0)),
            "one deferred publish at a time"
        );
        assert_eq!(session.take_queued(), Some((1, caret(22.0))));
        assert_eq!(session.take_queued(), None);
        assert!(session.queue_position(1, caret(24.0)));
        session.disable(Some(&text_input));
        assert_eq!(session.take_queued(), None, "disable drops a queued caret");
        assert!(!session.queue_position(1, caret(24.0)));
    }

    #[test]
    fn ime_context_presence_change_resends_the_rectangle_without_bounds() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        let context = surrounding(&mut FakeField::new("a", 1..1)).unwrap();
        let sent = Request::Surrounding(context.text.clone(), context.cursor, context.anchor);
        session.publish(&text_input, &1, Some(context), None, ChangeCause::Other);
        assert_eq!(
            text_input.take(),
            [
                Request::Enable,
                Request::Rectangle(10, 20, 1, 18),
                Request::Commit,
                Request::Disable,
                Request::Enable,
                sent,
                Request::Cause(ChangeCause::Other),
                Request::Rectangle(10, 20, 1, 18),
                Request::Commit
            ]
        );
    }

    #[test]
    fn ime_enable_starts_a_fresh_publication_after_a_stale_done() {
        let text_input = FakeTextInput::default();
        let mut session = enabled(&text_input, 1);
        session.done(0);
        assert!(!session.wants_publish(&1));
        session.disable(Some(&text_input));
        session.enable(&text_input, &1, Some(caret(10.0)), None);
        assert!(session.wants_publish(&1));
    }
}
