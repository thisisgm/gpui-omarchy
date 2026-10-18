use std::{sync::{Arc, atomic::Ordering}, marker::PhantomData};
use gpui::{MimePayload, NativeDragAction, Pixels, Point};
use crate::linux::transfer::{self, Cancel};

pub(crate) struct DragState<Offer, Window> {
    pub offer: Option<Offer>,
    pub window: Option<Window>,
    pub position: Point<Pixels>,
    pub generation: u64,
    pub cancel: Cancel,
    pub dropped: bool,
    pub entered: bool,
    pub data: Option<MimePayload>,
    pub offered_types: Arc<[String]>,
    pub action: Option<NativeDragAction>,
    _window: PhantomData<Window>,
}
impl<Offer, Window> Default for DragState<Offer, Window> {
    fn default() -> Self { Self { offer: None, window: None, position: Point::default(), generation: 0, cancel: transfer::cancelled(), dropped: false, entered: false, data: None, offered_types: Arc::default(), action: None, _window: PhantomData } }
}
impl<Offer, Window> DragState<Offer, Window> {
    pub fn clear(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.generation = self.generation.wrapping_add(1);
        self.offer = None; self.window = None; self.data = None; self.entered = false; self.dropped = false; self.action = None;
    }
    pub fn begin(&mut self, offer: Offer, window: Window, position: Point<Pixels>, types: Vec<String>) -> u64 {
        self.clear(); self.cancel = transfer::cancelled();
        self.offer = Some(offer); self.window = Some(window); self.position = position; self.offered_types = types.into();
        self.generation
    }
    pub fn complete(&mut self, generation: u64, data: MimePayload) -> bool {
        if generation != self.generation || self.offer.is_none() || self.cancel.load(Ordering::Acquire) { return false; }
        self.data = Some(data); true
    }
    pub fn leave(&mut self) -> bool {
        if self.dropped { return false; }
        self.clear(); true
    }
    pub fn drop_pending(&mut self) { if self.offer.is_some() { self.dropped = true; } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{MimeRepresentation, SelectionIntent, px};
    fn payload() -> MimePayload { MimePayload::new([MimeRepresentation::new("text/uri-list", &b"file:///a\r\n"[..])], SelectionIntent::Copy) }
    #[test]
    fn fast_release_before_eof_survives_drop_and_leave() {
        let mut drag = DragState::default();
        let generation = drag.begin(1, 2, Point::default(), vec![]);
        drag.drop_pending(); assert!(!drag.leave());
        assert!(drag.complete(generation, payload()));
        assert!(drag.dropped); assert_eq!(drag.window, Some(2));
    }
    #[test]
    fn stale_completion_after_leave_or_new_drag_never_retargets() {
        let mut drag = DragState::default();
        let old = drag.begin(1, 2, Point::default(), vec![]);
        assert!(drag.leave()); assert!(!drag.complete(old, payload()));
        let old = drag.begin(3, 4, Point::default(), vec![]);
        let new = drag.begin(5, 6, Point::default(), vec![]);
        assert!(!drag.complete(old, payload())); assert!(drag.complete(new, payload()));
        assert_eq!(drag.window, Some(6));
    }
    #[test]
    fn pending_read_retains_latest_motion() {
        let mut drag = DragState::default();
        let generation = drag.begin(1, 2, Point::default(), vec![]);
        drag.position = Point::new(px(91.), px(42.));
        assert!(drag.complete(generation, payload()));
        assert_eq!(drag.position, Point::new(px(91.), px(42.)));
    }
}
