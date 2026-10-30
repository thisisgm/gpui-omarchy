use super::clipboard::{DataOffer, ReadCompletion, ReadRequest, ReadSlot, ReceiveData};
use crate::linux::transfer::{self, Cancel, TransferPermit};
use gpui::{
    ClipboardEntry, ClipboardItem, ClipboardReadError, ExternalPaths, MimePayload,
    NativeDragAction, NativeDragActions, Pixels, Point,
};
use std::{
    marker::PhantomData,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

pub(crate) struct DragState<Offer, Window> {
    pub offer: Option<Offer>,
    pub window: Option<Window>,
    pub position: Point<Pixels>,
    pub generation: u64,
    pub cancel: Cancel,
    pub pending: Option<ReadSlot>,
    pub dropped: bool,
    pub entered: bool,
    pub refused: bool,
    pub data: Option<MimePayload>,
    pub paths: ExternalPaths,
    pub offered_types: Arc<[String]>,
    pub action: Option<NativeDragAction>,
    pub serial: u32,
    // Hyprland reports a destroyed live offer as dnd_finished, so an offer that left lives until the next Enter.
    held: Option<Offer>,
    // Hyprland's own data device cancels a drop on finish without acceptance, where other compositors cancel on destroy.
    hyprland: bool,
    // The type Hyprland accepts only at the final allowed submission, immediately before finish.
    selected: Option<String>,
    _window: PhantomData<Window>,
}
impl<Offer, Window> Default for DragState<Offer, Window> {
    fn default() -> Self {
        Self::new(false)
    }
}
impl<Offer, Window> DragState<Offer, Window> {
    pub fn new(hyprland: bool) -> Self {
        Self {
            offer: None,
            window: None,
            position: Point::default(),
            generation: 0,
            cancel: transfer::cancelled(),
            pending: None,
            dropped: false,
            entered: false,
            refused: false,
            data: None,
            paths: ExternalPaths::default(),
            offered_types: Arc::default(),
            action: None,
            serial: 0,
            held: None,
            hyprland,
            selected: None,
            _window: PhantomData,
        }
    }
    pub fn clear(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(pending) = self.pending.take() {
            pending.cancel();
        }
        self.generation = self.generation.wrapping_add(1);
        self.offer = None;
        self.window = None;
        self.data = None;
        self.paths = ExternalPaths::default();
        self.entered = false;
        self.dropped = false;
        self.refused = false;
        self.action = None;
        self.selected = None;
    }
    pub fn begin(
        &mut self,
        offer: Offer,
        window: Window,
        position: Point<Pixels>,
        types: Vec<String>,
        serial: u32,
    ) -> u64 {
        self.clear();
        self.held = None;
        self.cancel = transfer::cancelled();
        self.offer = Some(offer);
        self.window = Some(window);
        self.position = position;
        self.offered_types = types.into();
        self.serial = serial;
        self.generation
    }
    fn hold(&mut self, offer: Offer) {
        self.held = Some(offer);
    }
    // Pairs an entered offer with its window, holding an offer that no window takes.
    pub fn target(&mut self, offer: Offer, window: Option<Window>) -> Option<(Offer, Window)> {
        let Some(window) = window else {
            self.hold(offer);
            return None;
        };
        Some((offer, window))
    }
    // Returns an entered offer whose drag is still current after the destination policy, holding it otherwise.
    pub fn resume(&mut self, generation: u64, offer: Offer) -> Option<Offer> {
        if self.generation != generation {
            self.hold(offer);
            return None;
        }
        Some(offer)
    }
    pub fn complete(&mut self, generation: u64, item: ClipboardItem) -> bool {
        if generation != self.generation
            || self.offer.is_none()
            || self.cancel.load(Ordering::Acquire)
        {
            return false;
        }
        let Some(data) = item.mime_payload().cloned() else {
            return false;
        };
        self.paths = item
            .entries
            .into_iter()
            .find_map(|entry| {
                if let ClipboardEntry::ExternalPaths(paths) = entry {
                    Some(paths)
                } else {
                    None
                }
            })
            .unwrap_or_default();
        self.data = Some(data);
        true
    }
    pub fn leave(&mut self) -> bool {
        if self.dropped {
            return false;
        }
        if let Some(offer) = self.offer.take() {
            self.held = Some(offer);
        }
        self.clear();
        true
    }
}
impl<Offer: NativeOffer, Window> DragState<Offer, Window> {
    pub fn is_hyprland(&self) -> bool {
        self.hyprland
    }

    pub fn native_started(&mut self) {
        if self.hyprland && self.dropped {
            self.clear();
        }
    }

    pub fn drop_pending(&mut self) {
        if self.refused {
            self.settle();
        } else if self.offer.is_some() {
            self.dropped = true;
        }
    }
    // Withdraws acceptance and stops the read; the offer lives until Drop settles it or the next Enter.
    pub fn refuse(&mut self) {
        let Some(offer) = self.offer.take() else {
            return;
        };
        offer.accept(self.serial, None);
        let dropped = self.dropped;
        self.clear();
        self.offer = Some(offer);
        self.refused = true;
        if dropped {
            self.settle();
        }
    }
    // Ends a refused drop as a cancel, by finish on Hyprland and by destroy elsewhere.
    fn settle(&mut self) {
        if self.hyprland
            && let Some(offer) = &self.offer
            && offer.version() >= 3
        {
            offer.finish();
        }
        self.clear();
    }
    pub fn publish(
        &mut self,
        generation: u64,
        result: Result<ReadCompletion, ClipboardReadError>,
    ) -> bool {
        if self.generation != generation {
            return false;
        }
        ReadCompletion::publish(result, |result| match result {
            Ok(item) => self.complete(generation, item),
            Err(error) => {
                log::debug!("native drop receive: {error}");
                self.refuse();
                false
            }
        })
    }
    pub fn admit(&mut self, generation: u64, allowed: bool) -> bool {
        if self.generation != generation {
            return false;
        }
        if !allowed {
            self.refuse();
            return false;
        }
        self.entered = true;
        true
    }
    pub fn submit(&mut self, generation: u64, accepted: bool) {
        if self.generation != generation {
            return;
        }
        if !accepted {
            self.refuse();
            return;
        }
        if let Some(offer) = &self.offer {
            if self.dropped
                && self.action.is_some()
                && let Some(mime) = self.selected.take()
            {
                offer.accept(self.serial, Some(mime));
            }
            finish_offer(offer, accepted, self.dropped, self.action);
        }
        self.clear();
    }
}
impl<T: NativeOffer + ReceiveData, Window> DragState<DataOffer<T>, Window> {
    // Takes an offer at Enter: a refused one is kept, an accepted one has its types received and its type accepted.
    pub fn enter(
        &mut self,
        offer: DataOffer<T>,
        window: Window,
        position: Point<Pixels>,
        serial: u32,
        selection: Option<(Vec<String>, String, NativeDragActions)>,
        flush: impl FnOnce() -> Result<(), ClipboardReadError>,
    ) -> Option<ReadRequest<T>> {
        let generation = self.begin(
            offer.clone(),
            window,
            position,
            offer.mime_types.clone(),
            serial,
        );
        let Some((types, mime, actions)) = selection else {
            self.refused = true;
            return None;
        };
        // Hyprland's receive alone names the source's target; an accept now would let a later finish report success.
        if self.hyprland {
            self.action = hyprland_action(actions, offer.source_actions);
            if self.action.is_none() {
                self.refused = true;
                return None;
            }
            self.selected = Some(mime);
        } else {
            offer.accept(serial, Some(mime));
        }
        if offer.inner.version() < 3 {
            self.action = Some(NativeDragAction::Copy);
        }
        let started = TransferPermit::acquire().and_then(|permit| {
            let files = types
                .iter()
                .map(|mime| offer.receive(mime).map(|file| (mime.clone(), file)))
                .collect::<Result<Vec<_>, _>>()?;
            flush()?;
            Ok((permit, files))
        });
        let (permit, files) = match started {
            Ok(started) => started,
            Err(error) => {
                log::debug!("native drop receive: {error}");
                self.refuse();
                return None;
            }
        };
        let slot = ReadSlot::default();
        self.pending = Some(slot.clone());
        Some(ReadRequest {
            generation,
            cancel: self.cancel.clone(),
            deadline: Instant::now() + transfer::TRANSFER_TIMEOUT,
            files,
            _offer: offer,
            _permit: permit,
            slot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{MimeRepresentation, SelectionIntent, px};
    // These lifecycle tests stand in an integer for an offer that ignores protocol requests.
    impl NativeOffer for i32 {
        fn version(&self) -> u32 {
            3
        }
        fn accept(&self, _: u32, _: Option<String>) {}
        fn actions(&self, _: gpui::NativeDragActions) {}
        fn finish(&self) {}
    }
    fn payload() -> ClipboardItem {
        ClipboardItem::new_mime(MimePayload::new(
            [MimeRepresentation::new(
                "text/uri-list",
                &b"file:///a\r\n"[..],
            )],
            SelectionIntent::Copy,
        ))
    }
    #[test]
    fn fast_release_before_eof_survives_drop_and_leave() {
        let mut drag = DragState::default();
        let generation = drag.begin(1, 2, Point::default(), vec![], 0);
        drag.drop_pending();
        assert!(!drag.leave());
        assert!(drag.complete(generation, payload()));
        assert!(drag.dropped);
        assert_eq!(drag.window, Some(2));
    }
    #[test]
    fn stale_completion_after_leave_or_new_drag_never_retargets() {
        let mut drag = DragState::default();
        let old = drag.begin(1, 2, Point::default(), vec![], 0);
        assert!(drag.leave());
        assert!(!drag.complete(old, payload()));
        let old = drag.begin(3, 4, Point::default(), vec![], 0);
        let new = drag.begin(5, 6, Point::default(), vec![], 0);
        assert!(!drag.complete(old, payload()));
        assert!(drag.complete(new, payload()));
        assert_eq!(drag.window, Some(6));
    }
    #[test]
    fn pending_read_retains_latest_motion() {
        let mut drag = DragState::default();
        let generation = drag.begin(1, 2, Point::default(), vec![], 0);
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
impl<T: NativeOffer + ReceiveData> NativeOffer for DataOffer<T> {
    fn version(&self) -> u32 {
        self.inner.version()
    }
    fn accept(&self, serial: u32, mime: Option<String>) {
        self.inner.accept(serial, mime);
    }
    fn actions(&self, actions: gpui::NativeDragActions) {
        self.inner.actions(actions);
    }
    fn finish(&self) {
        self.inner.finish();
    }
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
) -> Option<(Vec<String>, String, NativeDragActions)> {
    accept_offer_with_actions(
        offer,
        serial,
        advertised,
        arbitrary,
        gpui::NativeDragActions::default(),
    )
}
// Selects the types to receive and the type to accept, or refuses the offer; DragState::enter sends the accept.
pub(crate) fn accept_offer_with_actions(
    offer: &impl NativeOffer,
    serial: u32,
    advertised: &[String],
    arbitrary: bool,
    actions: gpui::NativeDragActions,
) -> Option<(Vec<String>, String, NativeDragActions)> {
    let valid = match actions.preferred {
        NativeDragAction::Copy => actions.copy,
        NativeDragAction::Move => actions.move_,
    };
    if !valid || (offer.version() < 3 && !actions.copy) {
        offer.accept(serial, None);
        return None;
    }
    let types = transfer::selected_types(advertised, arbitrary);
    let Some(selected) = transfer::preferred_drag_type(&types, arbitrary).cloned() else {
        offer.accept(serial, None);
        return None;
    };
    if offer.version() >= 3 {
        offer.actions(actions);
    }
    Some((types, selected, actions))
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
// Names a global Hyprland first registers in v0.47.0; v0.41 to v0.46 run its data device without it and take the ordinary path.
pub(crate) fn hyprland_data_device(interface: &str) -> bool {
    interface == "hyprland_surface_manager_v1"
}
// Hyprland never sends a negotiated action, so the destination takes its preferred action, else its other one, within the source's.
pub(crate) fn hyprland_action(
    destination: NativeDragActions,
    source: Option<wayland_client::protocol::wl_data_device_manager::DndAction>,
) -> Option<NativeDragAction> {
    use wayland_client::protocol::wl_data_device_manager::DndAction;
    let allowed = |action: &NativeDragAction| {
        let (wanted, bit) = match action {
            NativeDragAction::Copy => (destination.copy, DndAction::Copy),
            NativeDragAction::Move => (destination.move_, DndAction::Move),
        };
        wanted && source.is_none_or(|source| source.contains(bit))
    };
    let other = match destination.preferred {
        NativeDragAction::Copy => NativeDragAction::Move,
        NativeDragAction::Move => NativeDragAction::Copy,
    };
    [destination.preferred, other].into_iter().find(allowed)
}
// The action a source reports once its drop is performed: the compositor's, else the only one it offered, as Hyprland never sends one.
pub(crate) fn performed_action(
    reported: Option<NativeDragAction>,
    offered: &[NativeDragAction],
) -> Option<NativeDragAction> {
    match (reported, offered) {
        (Some(action), _) => Some(action),
        (None, [only]) => Some(*only),
        _ => None,
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
    fn selection_names_the_type_and_leaves_its_accept_to_the_enter() {
        let offer = Offer::new(3);
        let types = vec![transfer::URI_LIST.to_owned()];
        let selection = accept_offer(&offer, 987, &types, false);
        assert_eq!(
            selection,
            Some((
                types,
                transfer::URI_LIST.to_owned(),
                NativeDragActions::default()
            ))
        );
        assert!(offer.accepts.borrow().is_empty());
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
    fn only_hyprland_with_its_own_data_device_is_recognised() {
        assert!(!hyprland_data_device("hyprland_focus_grab_manager_v1"));
        assert!(hyprland_data_device("hyprland_surface_manager_v1"));
        assert!(!hyprland_data_device("hyprland_toplevel_export_manager_v1"));
        assert!(!hyprland_data_device(
            "hyprland_global_shortcuts_manager_v1"
        ));
    }
    #[test]
    fn performed_drop_of_a_single_offered_action_takes_that_action() {
        assert_eq!(
            performed_action(None, &[NativeDragAction::Move]),
            Some(NativeDragAction::Move)
        );
        assert_eq!(
            performed_action(Some(NativeDragAction::Copy), &[NativeDragAction::Move]),
            Some(NativeDragAction::Copy)
        );
        assert_eq!(
            performed_action(None, &[NativeDragAction::Copy, NativeDragAction::Move]),
            None
        );
    }
    #[test]
    fn hyprland_action_is_the_destination_preference_within_the_source_actions() {
        use wayland_client::protocol::wl_data_device_manager::DndAction;
        let copy = NativeDragActions::default();
        let moving = NativeDragActions {
            copy: true,
            move_: true,
            preferred: NativeDragAction::Move,
        };
        let both = Some(DndAction::Copy | DndAction::Move);
        assert_eq!(hyprland_action(copy, both), Some(NativeDragAction::Copy));
        assert_eq!(hyprland_action(moving, both), Some(NativeDragAction::Move));
        assert_eq!(
            hyprland_action(moving, Some(DndAction::Copy)),
            Some(NativeDragAction::Copy)
        );
        assert_eq!(hyprland_action(copy, Some(DndAction::Move)), None);
        assert_eq!(hyprland_action(copy, Some(DndAction::Ask)), None);
        assert_eq!(hyprland_action(moving, None), Some(NativeDragAction::Move));
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

#[cfg(test)]
mod decoded_paths_tests {
    use super::*;
    #[test]
    fn review_drag_uses_decoded_paths_without_reparsing_for_entry_or_drop() {
        let payload = MimePayload::new(
            [gpui::MimeRepresentation::new(
                "text/uri-list",
                &b"unparsed bytes"[..],
            )],
            gpui::SelectionIntent::Copy,
        );
        let paths = ExternalPaths(vec!["/already/decoded".into()].into());
        let item = ClipboardItem {
            entries: vec![
                ClipboardEntry::Mime(payload),
                ClipboardEntry::ExternalPaths(paths.clone()),
            ],
        };
        let mut drag = DragState::default();
        let generation = drag.begin(1, 2, Point::default(), vec![], 0);
        assert!(drag.complete(generation, item));
        assert_eq!(drag.paths, paths);
        drag.drop_pending();
        assert!(!drag.leave());
        assert_eq!(drag.paths, paths);
        drag.clear();
        assert!(drag.paths.paths().is_empty());
    }
}

#[cfg(test)]
mod offer_life_tests {
    use super::*;
    use std::{cell::RefCell, fs::File, io::Write, os::fd::BorrowedFd, rc::Rc};
    #[derive(Debug, Clone, PartialEq)]
    enum Request {
        Accept(u32, Option<String>),
        Actions,
        Receive,
        Finish,
        Destroy,
    }
    #[derive(Clone, Default)]
    struct Offer(Rc<RefCell<Vec<Request>>>);
    impl ReceiveData for Offer {
        fn receive_data(&self, _: String, fd: BorrowedFd<'_>) {
            let mut pipe = File::from(fd.try_clone_to_owned().unwrap());
            pipe.write_all(b"file:///a\r\n").unwrap();
            self.0.borrow_mut().push(Request::Receive);
        }
        fn destroy_offer(&self) {
            self.0.borrow_mut().push(Request::Destroy);
        }
    }
    impl NativeOffer for Offer {
        fn version(&self) -> u32 {
            3
        }
        fn accept(&self, serial: u32, mime: Option<String>) {
            self.0.borrow_mut().push(Request::Accept(serial, mime));
        }
        fn actions(&self, _: gpui::NativeDragActions) {
            self.0.borrow_mut().push(Request::Actions);
        }
        fn finish(&self) {
            self.0.borrow_mut().push(Request::Finish);
        }
    }
    fn offer() -> (DataOffer<Offer>, Rc<RefCell<Vec<Request>>>) {
        let fake = Offer::default();
        let log = fake.0.clone();
        let mut offer = DataOffer::new(fake);
        offer.add_mime_type(transfer::URI_LIST.into());
        (offer, log)
    }
    #[test]
    fn refused_hover_keeps_the_offer_until_the_next_enter() {
        let (refused, log) = offer();
        let mut drag = DragState::default();
        drag.begin(refused, 2, Point::default(), vec![], 987);
        drag.refuse();
        assert!(drag.leave());
        assert_eq!(*log.borrow(), [Request::Accept(987, None)]);
        drag.begin(offer().0, 2, Point::default(), vec![], 988);
        assert_eq!(
            *log.borrow(),
            [Request::Accept(987, None), Request::Destroy]
        );
    }
    #[test]
    fn release_over_a_refusing_window_ends_the_offer_only_at_drop() {
        let (refused, log) = offer();
        let mut drag = DragState::default();
        drag.begin(refused, 2, Point::default(), vec![], 987);
        drag.refuse();
        assert_eq!(*log.borrow(), [Request::Accept(987, None)]);
        drag.drop_pending();
        assert_eq!(
            *log.borrow(),
            [Request::Accept(987, None), Request::Destroy]
        );
    }
    #[test]
    fn refusal_after_drop_withdraws_acceptance_before_the_offer_ends() {
        let (refused, log) = offer();
        let mut drag = DragState::default();
        drag.begin(refused, 2, Point::default(), vec![], 987);
        drag.drop_pending();
        drag.refuse();
        assert_eq!(
            *log.borrow(),
            [Request::Accept(987, None), Request::Destroy]
        );
    }
    fn uri() -> Option<String> {
        Some(transfer::URI_LIST.into())
    }
    // Mirrors the Enter handler: the destination policy answers, then DragState takes the offer.
    fn enter(
        drag: &mut DragState<DataOffer<Offer>, i32>,
        offer: DataOffer<Offer>,
        flush: Result<(), ClipboardReadError>,
    ) -> Option<ReadRequest<Offer>> {
        let selection = accept_offer(&offer.inner, 987, &offer.mime_types, false);
        drag.enter(offer, 2, Point::default(), 987, selection, || flush)
    }
    // Enter, a finished read and an allowed entry, as the client runs them before a decision.
    fn entered(drag: &mut DragState<DataOffer<Offer>, i32>) -> Rc<RefCell<Vec<Request>>> {
        let (offer, log) = offer();
        let request = enter(drag, offer, Ok(())).unwrap();
        let generation = request.generation;
        assert!(drag.publish(generation, request.run()));
        assert!(drag.admit(generation, true));
        drag.action = Some(NativeDragAction::Copy);
        log
    }
    #[test]
    fn hyprland_drop_takes_the_destination_action() {
        let (offer, _) = offer();
        let mut drag = DragState::new(true);
        let selection = accept_offer(&offer.inner, 987, &offer.mime_types, false);
        assert!(
            drag.enter(offer, 2, Point::default(), 987, selection, || Ok(()))
                .is_some()
        );
        assert_eq!(drag.action, Some(NativeDragAction::Copy));
    }
    #[test]
    fn hyprland_drop_from_a_move_only_source_into_a_copy_destination_is_refused_at_enter() {
        use wayland_client::protocol::wl_data_device_manager::DndAction;
        let (mut offer, log) = offer();
        offer.source_actions = Some(DndAction::Move);
        let mut drag = DragState::new(true);
        let selection = accept_offer(&offer.inner, 987, &offer.mime_types, false);
        assert!(
            drag.enter(offer, 2, Point::default(), 987, selection, || Ok(()))
                .is_none()
        );
        assert!(drag.refused);
        assert_eq!(drag.action, None);
        assert!(drag.pending.is_none());
        assert_eq!(*log.borrow(), [Request::Actions]);
        drag.drop_pending();
        assert_eq!(
            *log.borrow(),
            [Request::Actions, Request::Finish, Request::Destroy]
        );
    }
    #[test]
    fn other_compositors_leave_the_drop_action_to_their_action_event() {
        let (offer, _) = offer();
        let mut drag = DragState::new(false);
        let selection = accept_offer(&offer.inner, 987, &offer.mime_types, false);
        assert!(
            drag.enter(offer, 2, Point::default(), 987, selection, || Ok(()))
                .is_some()
        );
        assert_eq!(drag.action, None);
    }
    #[test]
    fn hyprland_refused_drop_is_finished_without_acceptance_then_destroyed() {
        let (refused, log) = offer();
        let mut drag = DragState::new(true);
        drag.begin(refused, 2, Point::default(), vec![], 987);
        drag.refuse();
        drag.drop_pending();
        assert_eq!(
            *log.borrow(),
            [
                Request::Accept(987, None),
                Request::Finish,
                Request::Destroy
            ]
        );
    }
    #[test]
    fn enter_failure_refuses_and_keeps_the_offer() {
        let (offer, log) = offer();
        let mut drag = DragState::new(true);
        let failed = Err(ClipboardReadError::Transport("flush".into()));
        assert!(enter(&mut drag, offer, failed).is_none());
        assert!(drag.refused);
        assert_eq!(
            *log.borrow(),
            [
                Request::Actions,
                Request::Receive,
                Request::Accept(987, None)
            ]
        );
        drag.drop_pending();
        assert_eq!(log.borrow()[3..], [Request::Finish, Request::Destroy]);
    }
    #[test]
    fn enter_rejection_keeps_the_offer_until_the_next_enter() {
        let (rejected, log) = offer();
        let mut drag = DragState::new(true);
        let selection = accept_offer(&rejected.inner, 987, &["text/plain".into()], false);
        assert!(
            drag.enter(rejected, 2, Point::default(), 987, selection, || Ok(()))
                .is_none()
        );
        assert!(drag.refused);
        assert!(drag.leave());
        assert_eq!(*log.borrow(), [Request::Accept(987, None)]);
        drag.begin(offer().0, 2, Point::default(), vec![], 988);
        assert_eq!(
            *log.borrow(),
            [Request::Accept(987, None), Request::Destroy]
        );
    }
    #[test]
    fn failed_read_refuses_and_keeps_the_offer() {
        let (offer, log) = offer();
        let mut drag = DragState::new(true);
        let request = enter(&mut drag, offer, Ok(())).unwrap();
        let generation = request.generation;
        drop(request);
        assert!(!drag.publish(generation, Err(ClipboardReadError::TimedOut)));
        assert!(drag.refused);
        assert_eq!(log.borrow()[2..], [Request::Accept(987, None)]);
        drag.drop_pending();
        assert_eq!(log.borrow()[3..], [Request::Finish, Request::Destroy]);
    }
    #[test]
    fn refused_entry_keeps_the_offer() {
        let (offer, log) = offer();
        let mut drag = DragState::new(true);
        let request = enter(&mut drag, offer, Ok(())).unwrap();
        let generation = request.generation;
        assert!(drag.publish(generation, request.run()));
        assert!(!drag.admit(generation, false));
        assert!(drag.refused);
        assert!(drag.data.is_none());
        assert_eq!(log.borrow()[2..], [Request::Accept(987, None)]);
        drag.drop_pending();
        assert_eq!(log.borrow()[3..], [Request::Finish, Request::Destroy]);
    }
    #[test]
    fn refused_submit_ends_as_a_cancel_on_each_compositor() {
        for (hyprland, end) in [
            (true, &[Request::Finish, Request::Destroy][..]),
            (false, &[Request::Destroy][..]),
        ] {
            let mut drag = DragState::new(hyprland);
            let log = entered(&mut drag);
            let generation = drag.generation;
            drag.drop_pending();
            drag.submit(generation, false);
            let at = if hyprland { 2 } else { 3 };
            assert_eq!(log.borrow()[at], Request::Accept(987, None));
            assert_eq!(log.borrow()[at + 1..], *end);
        }
    }
    #[test]
    fn accepted_drop_finishes_before_the_offer_goes() {
        let mut drag = DragState::new(true);
        let log = entered(&mut drag);
        let generation = drag.generation;
        drag.drop_pending();
        drag.submit(generation, true);
        assert_eq!(
            log.borrow()[2..],
            [
                Request::Accept(987, uri()),
                Request::Finish,
                Request::Destroy
            ]
        );
    }
    #[test]
    fn hyprland_accepts_the_type_only_at_final_allowed_submission() {
        for (hyprland, at_enter) in [
            (
                false,
                &[
                    Request::Actions,
                    Request::Accept(987, uri()),
                    Request::Receive,
                ][..],
            ),
            (true, &[Request::Actions, Request::Receive][..]),
        ] {
            let (entered, log) = offer();
            let mut drag = DragState::new(hyprland);
            let request = enter(&mut drag, entered, Ok(())).unwrap();
            let generation = request.generation;
            assert_eq!(*log.borrow(), *at_enter);
            assert!(drag.publish(generation, request.run()));
            assert!(drag.admit(generation, true));
            let accepts = log
                .borrow()
                .iter()
                .filter(|request| **request == Request::Accept(987, uri()))
                .count();
            assert_eq!(accepts, usize::from(!hyprland));
            assert_eq!(log.borrow().len(), if hyprland { 2 } else { 3 });
            drag.action = Some(NativeDragAction::Copy);
            drag.drop_pending();
            drag.submit(generation, true);
            let requests = log.borrow();
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| **r == Request::Accept(987, uri()))
                    .count(),
                1
            );
            assert_eq!(
                requests[requests.len() - 2..],
                [Request::Finish, Request::Destroy]
            );
        }
    }
    // On Hyprland only final allowed submission accepts a type, so finishing a refused read cancels.
    #[test]
    fn fast_release_then_failed_read_ends_as_a_cancel() {
        let (offer, log) = offer();
        let mut drag = DragState::new(true);
        let request = enter(&mut drag, offer, Ok(())).unwrap();
        let generation = request.generation;
        drag.drop_pending();
        assert!(!drag.leave());
        drop(request);
        assert!(!drag.publish(generation, Err(ClipboardReadError::TimedOut)));
        assert_eq!(
            *log.borrow(),
            [
                Request::Actions,
                Request::Receive,
                Request::Accept(987, None),
                Request::Finish,
                Request::Destroy
            ]
        );
    }
    #[test]
    fn enter_without_a_window_keeps_the_offer_until_the_next_enter() {
        let (entered, log) = offer();
        let mut drag = DragState::new(true);
        assert!(drag.target(entered, None).is_none());
        assert_eq!(*log.borrow(), [] as [Request; 0]);
        drag.begin(offer().0, 2, Point::default(), vec![], 988);
        assert_eq!(*log.borrow(), [Request::Destroy]);
    }
    #[test]
    fn enter_that_lost_the_policy_race_keeps_the_offer_until_the_next_enter() {
        let (entered, log) = offer();
        let mut drag = DragState::<_, i32>::new(true);
        let generation = drag.generation;
        drag.clear();
        assert!(drag.resume(generation, entered).is_none());
        assert_eq!(*log.borrow(), [] as [Request; 0]);
        drag.begin(offer().0, 2, Point::default(), vec![], 988);
        assert_eq!(*log.borrow(), [Request::Destroy]);
    }
    #[test]
    fn new_drag_after_drop_on_hyprland_053_gets_no_late_finish() {
        for success in [false, true] {
            let (old, old_log) = offer();
            let mut drag = DragState::new(true);
            let request = enter(&mut drag, old, Ok(())).unwrap();
            let generation = request.generation;
            drag.drop_pending();
            assert!(!drag.leave());
            // The worker reply remains queued until after the newer Enter.
            let result = if success {
                request.run()
            } else {
                drop(request);
                Err(ClipboardReadError::TimedOut)
            };
            assert_eq!(result.is_ok(), success);
            let (new, new_log) = offer();
            let new_request = enter(&mut drag, new, Ok(())).unwrap();
            let current = drag.generation;
            let before = new_log.borrow().clone();
            assert!(!drag.publish(generation, result));
            drag.submit(generation, true);
            assert_eq!(
                *old_log.borrow(),
                [Request::Actions, Request::Receive, Request::Destroy]
            );
            assert_eq!(
                *new_log.borrow(),
                before,
                "the newer drag must be untouched"
            );
            assert_eq!(drag.generation, current);
            assert!(drag.publish(current, new_request.run()));
            assert!(drag.admit(current, true));
            drag.drop_pending();
            drag.submit(current, true);
            assert!(
                new_log.borrow().contains(&Request::Finish),
                "ordinary success must finish"
            );
        }
    }
    #[test]
    fn fix1_c3_hyprland_refusal_never_follows_type_acceptance() {
        let mut drag = DragState::new(true);
        let log = entered(&mut drag);
        drag.drop_pending();
        drag.submit(drag.generation, false);
        assert!(
            !log.borrow()
                .iter()
                .any(|r| matches!(r, Request::Accept(_, Some(_)))),
            "{log:?}"
        );
        assert_eq!(
            log.borrow()[2..],
            [
                Request::Accept(987, None),
                Request::Finish,
                Request::Destroy
            ]
        );
    }

    #[test]
    fn fix1_c1_native_drag_cancels_a_pending_incoming_drop() {
        for success in [false, true] {
            let mut drag = DragState::new(true);
            let (old, old_log) = offer();
            let request = enter(&mut drag, old, Ok(())).unwrap();
            let generation = request.generation;
            drag.drop_pending();
            let result = if success {
                request.run()
            } else {
                drop(request);
                Err(ClipboardReadError::TimedOut)
            };
            assert_eq!(result.is_ok(), success);
            drag.native_started();
            assert!(!drag.publish(generation, result));
            assert_eq!(
                *old_log.borrow(),
                [Request::Actions, Request::Receive, Request::Destroy]
            );
        }
    }
}
