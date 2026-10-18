use crate::linux::transfer::{self, Cancel};
use gpui::{MimePayload, NativeDragAction, Pixels, Point};
use std::{
    marker::PhantomData,
    sync::{Arc, atomic::Ordering},
};

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
    fn default() -> Self {
        Self {
            offer: None,
            window: None,
            position: Point::default(),
            generation: 0,
            cancel: transfer::cancelled(),
            dropped: false,
            entered: false,
            data: None,
            offered_types: Arc::default(),
            action: None,
            _window: PhantomData,
        }
    }
}
impl<Offer, Window> DragState<Offer, Window> {
    pub fn clear(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.generation = self.generation.wrapping_add(1);
        self.offer = None;
        self.window = None;
        self.data = None;
        self.entered = false;
        self.dropped = false;
        self.action = None;
    }
    pub fn begin(
        &mut self,
        offer: Offer,
        window: Window,
        position: Point<Pixels>,
        types: Vec<String>,
    ) -> u64 {
        self.clear();
        self.cancel = transfer::cancelled();
        self.offer = Some(offer);
        self.window = Some(window);
        self.position = position;
        self.offered_types = types.into();
        self.generation
    }
    pub fn complete(&mut self, generation: u64, data: MimePayload) -> bool {
        if generation != self.generation
            || self.offer.is_none()
            || self.cancel.load(Ordering::Acquire)
        {
            return false;
        }
        self.data = Some(data);
        true
    }
    pub fn leave(&mut self) -> bool {
        if self.dropped {
            return false;
        }
        self.clear();
        true
    }
    pub fn drop_pending(&mut self) {
        if self.offer.is_some() {
            self.dropped = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{MimeRepresentation, SelectionIntent, px};
    fn payload() -> MimePayload {
        MimePayload::new(
            [MimeRepresentation::new(
                "text/uri-list",
                &b"file:///a\r\n"[..],
            )],
            SelectionIntent::Copy,
        )
    }
    #[test]
    fn fast_release_before_eof_survives_drop_and_leave() {
        let mut drag = DragState::default();
        let generation = drag.begin(1, 2, Point::default(), vec![]);
        drag.drop_pending();
        assert!(!drag.leave());
        assert!(drag.complete(generation, payload()));
        assert!(drag.dropped);
        assert_eq!(drag.window, Some(2));
    }
    #[test]
    fn stale_completion_after_leave_or_new_drag_never_retargets() {
        let mut drag = DragState::default();
        let old = drag.begin(1, 2, Point::default(), vec![]);
        assert!(drag.leave());
        assert!(!drag.complete(old, payload()));
        let old = drag.begin(3, 4, Point::default(), vec![]);
        let new = drag.begin(5, 6, Point::default(), vec![]);
        assert!(!drag.complete(old, payload()));
        assert!(drag.complete(new, payload()));
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

pub(crate) trait NativeOffer {
    fn version(&self) -> u32;
    fn accept(&self, serial: u32, mime: Option<String>);
    fn actions(&self, actions: gpui::NativeDragActions);
    fn finish(&self);
}
impl NativeOffer for wayland_client::protocol::wl_data_offer::WlDataOffer {
    fn version(&self) -> u32 {
        wayland_client::Proxy::version(self)
    }
    fn accept(&self, serial: u32, mime: Option<String>) {
        self.accept(serial, mime);
    }
    fn actions(&self, actions: gpui::NativeDragActions) {
        use wayland_client::protocol::wl_data_device_manager::DndAction;
        let mut supported = DndAction::empty();
        if actions.copy {
            supported |= DndAction::Copy;
        }
        if actions.move_ {
            supported |= DndAction::Move;
        }
        let preferred = match actions.preferred {
            NativeDragAction::Copy => DndAction::Copy,
            NativeDragAction::Move => DndAction::Move,
        };
        self.set_actions(supported, preferred);
    }
    fn finish(&self) {
        self.finish();
    }
}
#[cfg(test)]
pub(crate) fn accept_offer(
    offer: &impl NativeOffer,
    serial: u32,
    advertised: &[String],
    arbitrary: bool,
) -> Option<Vec<String>> {
    accept_offer_with_actions(
        offer,
        serial,
        advertised,
        arbitrary,
        gpui::NativeDragActions::default(),
    )
}
pub(crate) fn accept_offer_with_actions(
    offer: &impl NativeOffer,
    serial: u32,
    advertised: &[String],
    arbitrary: bool,
    actions: gpui::NativeDragActions,
) -> Option<Vec<String>> {
    let valid = match actions.preferred {
        NativeDragAction::Copy => actions.copy,
        NativeDragAction::Move => actions.move_,
    };
    if !valid || (offer.version() < 3 && !actions.copy) {
        offer.accept(serial, None);
        return None;
    }
    let types = transfer::selected_types(advertised, arbitrary);
    let selected = transfer::preferred_drag_type(&types, arbitrary);
    offer.accept(serial, selected.cloned());
    selected?;
    if offer.version() >= 3 {
        offer.actions(actions);
    }
    Some(types)
}
pub(crate) fn finish_offer(
    offer: &impl NativeOffer,
    accepted: bool,
    dropped: bool,
    action: Option<NativeDragAction>,
) {
    if accepted && dropped && action.is_some() && offer.version() >= 3 {
        offer.finish();
    }
}
pub(crate) fn lift_action(
    actions: gpui::NativeDragActions,
    modifiers: gpui::Modifiers,
) -> Option<NativeDragAction> {
    let chosen = if modifiers.control && actions.copy {
        NativeDragAction::Copy
    } else if modifiers.shift && actions.move_ {
        NativeDragAction::Move
    } else {
        actions.preferred
    };
    match chosen {
        NativeDragAction::Copy if actions.copy => Some(chosen),
        NativeDragAction::Move if actions.move_ => Some(chosen),
        _ => None,
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Offer {
        version: u32,
        accepts: RefCell<Vec<(u32, Option<String>)>>,
        actions: Cell<usize>,
        finishes: Cell<usize>,
        chosen: RefCell<Vec<gpui::NativeDragActions>>,
    }
    impl Offer {
        fn new(version: u32) -> Self {
            Self {
                version,
                accepts: RefCell::default(),
                actions: Cell::new(0),
                finishes: Cell::new(0),
                chosen: RefCell::new(Vec::new()),
            }
        }
    }
    impl NativeOffer for Offer {
        fn version(&self) -> u32 {
            self.version
        }
        fn accept(&self, serial: u32, mime: Option<String>) {
            self.accepts.borrow_mut().push((serial, mime));
        }
        fn actions(&self, actions: gpui::NativeDragActions) {
            self.actions.set(self.actions.get() + 1);
            self.chosen.borrow_mut().push(actions);
        }
        fn finish(&self) {
            self.finishes.set(self.finishes.get() + 1);
        }
    }
    #[test]
    fn mime_announcement_waits_for_the_enter_serial() {
        let offer = Offer::new(3);
        let types = vec![transfer::URI_LIST.to_owned()];
        assert!(offer.accepts.borrow().is_empty());
        assert!(accept_offer(&offer, 987, &types, false).is_some());
        assert_eq!(
            *offer.accepts.borrow(),
            vec![(987, Some(transfer::URI_LIST.to_owned()))]
        );
    }
    #[test]
    fn plain_text_only_offer_is_rejected_without_requesting_uri_data() {
        let offer = Offer::new(3);
        assert!(accept_offer(&offer, 987, &["text/plain".into()], false).is_none());
        assert_eq!(*offer.accepts.borrow(), vec![(987, None)]);
        assert_eq!(offer.actions.get(), 0);
    }
    #[test]
    fn opaque_tokens_are_accepted_only_with_application_policy() {
        let offer = Offer::new(3);
        assert!(accept_offer(&offer, 1, &["application/x-test-token".into()], false).is_none());
        assert!(accept_offer(&offer, 2, &["application/x-test-token".into()], true).is_some());
    }
    #[test]
    fn finish_requires_a_drop_acceptance_selected_action_and_version_three() {
        let offer = Offer::new(3);
        finish_offer(&offer, false, true, Some(NativeDragAction::Copy));
        finish_offer(&offer, true, false, Some(NativeDragAction::Copy));
        finish_offer(&offer, true, true, None);
        assert_eq!(offer.finishes.get(), 0);
        finish_offer(&offer, true, true, Some(NativeDragAction::Copy));
        assert_eq!(offer.finishes.get(), 1);
        let old = Offer::new(2);
        accept_offer(&old, 1, &[transfer::URI_LIST.into()], false);
        finish_offer(&old, true, true, Some(NativeDragAction::Copy));
        assert_eq!(old.actions.get(), 0);
        assert_eq!(old.finishes.get(), 0);
    }
    #[test]
    fn lift_preserves_control_shift_and_preferred_policy() {
        let actions = gpui::NativeDragActions {
            copy: true,
            move_: true,
            preferred: NativeDragAction::Move,
        };
        assert_eq!(
            lift_action(actions, gpui::Modifiers::default()),
            Some(NativeDragAction::Move)
        );
        assert_eq!(
            lift_action(
                actions,
                gpui::Modifiers {
                    control: true,
                    ..Default::default()
                }
            ),
            Some(NativeDragAction::Copy)
        );
        assert_eq!(
            lift_action(
                actions,
                gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                }
            ),
            Some(NativeDragAction::Move)
        );
    }
}

#[cfg(test)]
mod action_policy_tests {
    use super::*;
    use std::cell::Cell;
    struct Offer {
        move_: Cell<bool>,
    }
    impl NativeOffer for Offer {
        fn version(&self) -> u32 {
            3
        }
        fn accept(&self, _: u32, _: Option<String>) {}
        fn actions(&self, actions: gpui::NativeDragActions) {
            self.move_
                .set(actions.move_ && actions.preferred == NativeDragAction::Move);
        }
        fn finish(&self) {}
    }
    #[test]
    fn explicit_destination_policy_can_negotiate_move_before_drop() {
        let offer = Offer {
            move_: Cell::new(false),
        };
        let actions = gpui::NativeDragActions {
            copy: false,
            move_: true,
            preferred: NativeDragAction::Move,
        };
        assert!(
            accept_offer_with_actions(&offer, 77, &[transfer::URI_LIST.to_owned()], false, actions)
                .is_some()
        );
        assert!(offer.move_.get());
    }
    #[test]
    fn ordinary_destination_policy_still_requests_copy() {
        let offer = Offer {
            move_: Cell::new(false),
        };
        assert!(accept_offer(&offer, 77, &[transfer::URI_LIST.to_owned()], false).is_some());
        assert!(!offer.move_.get());
    }
}
