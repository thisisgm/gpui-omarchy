use super::*;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

#[derive(Default)]
struct Host {
    live: RefCell<BTreeMap<&'static str, u64>>,
    binds: RefCell<BTreeMap<&'static str, usize>>,
    releases: RefCell<BTreeMap<&'static str, usize>>,
    actions: RefCell<Vec<Action>>,
    generation: Cell<u64>,
    keyboard_focus: Cell<bool>,
    pointer_state: Cell<bool>,
}

impl Host {
    fn event(&self, available: Capability) {
        capabilities(self, &1, &(), available);
    }
    fn id(&self, name: &str) -> Option<u64> {
        self.live.borrow().get(name).copied()
    }
    fn count(&self, name: &str, release: bool) -> usize {
        let map = if release {
            self.releases.borrow()
        } else {
            self.binds.borrow()
        };
        map.get(name).copied().unwrap_or(0)
    }
}

impl SeatHost for Host {
    type Seat = u32;
    type Queue = ();

    fn held(&self, seat: &u32) -> (bool, Held) {
        (
            *seat == 1,
            Held {
                keyboard: self.id("keyboard").is_some(),
                text_input: self.id("text").is_some(),
                pointer: self.id("pointer").is_some(),
                cursor: self.id("cursor").is_some(),
                pinch: self.id("pinch").is_some(),
                touch: self.id("touch").is_some(),
                keyboard_focus: self.keyboard_focus.get(),
                pointer_state: self.pointer_state.get(),
            },
        )
    }

    fn apply(&self, _: &u32, _: &(), action: Action) {
        self.actions.borrow_mut().push(action);
        let (name, bind) = match action {
            Action::BindKeyboard => ("keyboard", true),
            Action::BindTextInput => ("text", true),
            Action::BindPointer => ("pointer", true),
            Action::BindCursor => ("cursor", true),
            Action::BindPinch => ("pinch", true),
            Action::BindTouch => ("touch", true),
            Action::ReleaseKeyboard => ("keyboard", false),
            Action::ReleaseTextInput => ("text", false),
            Action::ReleasePointer => ("pointer", false),
            Action::ReleaseCursor => ("cursor", false),
            Action::ReleasePinch => ("pinch", false),
            Action::ReleaseTouch => ("touch", false),
            Action::KeyboardLost => {
                self.keyboard_focus.set(false);
                return;
            }
            Action::ClearPointer => {
                self.pointer_state.set(false);
                return;
            }
            Action::ResetTextInput => return,
        };
        if bind {
            assert!(
                !matches!(action, Action::BindCursor | Action::BindPinch)
                    || self.id("pointer").is_some(),
                "{name} bound without a live pointer"
            );
            self.generation.set(self.generation.get() + 1);
            assert!(
                self.live
                    .borrow_mut()
                    .insert(name, self.generation.get())
                    .is_none(),
                "{name} rebound"
            );
            *self.binds.borrow_mut().entry(name).or_default() += 1;
        } else {
            assert!(
                self.live.borrow_mut().remove(name).is_some(),
                "{name} released twice"
            );
            *self.releases.borrow_mut().entry(name).or_default() += 1;
        }
    }
}

fn keyboard_pointer() -> Capability {
    Capability::Keyboard | Capability::Pointer
}

#[test]
fn first_keyboard_pointer_event_binds_devices_and_their_dependents() {
    let host = Host::default();
    host.event(keyboard_pointer());
    for name in ["keyboard", "text", "pointer", "cursor", "pinch"] {
        assert_eq!(host.count(name, false), 1, "missing {name}");
        assert_eq!(host.count(name, true), 0);
    }
    assert_eq!(host.id("touch"), None);
}

#[test]
fn repeated_capabilities_keep_every_object_without_release_or_rebind() {
    let host = Host::default();
    host.event(keyboard_pointer());
    let before = host.live.borrow().clone();
    host.actions.borrow_mut().clear();
    host.event(keyboard_pointer());
    assert!(host.actions.borrow().is_empty());
    assert_eq!(*host.live.borrow(), before);
}

#[test]
fn hotplug_3_7_3_keeps_keyboard_and_pointer_and_releases_touch_once() {
    let host = Host::default();
    host.event(keyboard_pointer());
    let before = host.live.borrow().clone();
    host.event(keyboard_pointer() | Capability::Touch);
    assert_eq!(host.count("touch", false), 1);
    host.event(keyboard_pointer());
    assert_eq!(host.count("touch", true), 1);
    assert_eq!(*host.live.borrow(), before);
    for name in ["keyboard", "text", "pointer", "cursor", "pinch"] {
        assert_eq!(host.count(name, false), 1);
        assert_eq!(host.count(name, true), 0);
    }
}

#[test]
fn keyboard_only_then_pointer_only_releases_keyboard_focus_and_text_input() {
    let host = Host::default();
    host.event(Capability::Keyboard);
    host.keyboard_focus.set(true);
    host.event(Capability::Pointer);
    assert!(!host.keyboard_focus.get());
    for name in ["keyboard", "text"] {
        assert_eq!(host.id(name), None);
        assert_eq!(host.count(name, true), 1);
    }
    assert!(host.id("pointer").is_some());
}

#[test]
fn pointer_return_recreates_cursor_and_pinch_and_clears_old_pointer_state() {
    let host = Host::default();
    host.event(keyboard_pointer());
    let before = host.live.borrow().clone();
    host.pointer_state.set(true);
    host.event(Capability::Keyboard);
    assert!(!host.pointer_state.get());
    for name in ["pointer", "cursor", "pinch"] {
        assert_eq!(host.id(name), None, "{name} retained after removal");
        assert_eq!(host.count(name, true), 1);
    }
    host.event(keyboard_pointer());
    for name in ["pointer", "cursor", "pinch"] {
        assert_ne!(host.id(name), before.get(name).copied());
        assert_eq!(host.count(name, false), 2);
    }
}

#[test]
fn keyboard_return_recreates_the_text_input_object() {
    let host = Host::default();
    host.event(keyboard_pointer());
    let old = host.id("text");
    host.event(Capability::Pointer);
    assert_eq!(host.id("text"), None);
    host.event(keyboard_pointer());
    assert_ne!(host.id("text"), old);
    assert_eq!(host.count("text", false), 2);
    assert_eq!(host.count("text", true), 1);
}

#[test]
fn a_foreign_seat_can_neither_bind_nor_remove_the_current_seats_objects() {
    let host = Host::default();
    capabilities(&host, &2, &(), keyboard_pointer());
    assert!(host.actions.borrow().is_empty());
    host.event(keyboard_pointer());
    let before = host.live.borrow().clone();
    host.actions.borrow_mut().clear();
    capabilities(&host, &2, &(), Capability::empty());
    assert_eq!(*host.live.borrow(), before);
    assert!(host.actions.borrow().is_empty());
}
