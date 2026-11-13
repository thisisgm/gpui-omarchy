use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{PlatformInput, point, px};

use super::*;

#[derive(Default)]
struct Window {
    borrowed: RefCell<()>,
    events: RefCell<Vec<PlatformInput>>,
    hover: RefCell<Vec<bool>>,
    id: u32,
}

struct Host {
    touch: RefCell<TouchState<u32>>,
    window: Rc<Window>,
    keyboard: Rc<Window>,
    serials: RefCell<SerialTracker>,
    click: RefCell<ClickState>,
    hover_cleared: Cell<bool>,
    compose: RefCell<Option<String>>,
    resets: Cell<usize>,
    ime_active: Cell<bool>,
    ime_owners: RefCell<Vec<u32>>,
    inserted: RefCell<Vec<(u32, String)>>,
}

impl Host {
    fn new() -> Self {
        let mut touch = TouchState::default();
        touch.capability(true, Modifiers::default());
        Self {
            touch: RefCell::new(touch),
            window: Rc::new(Window {
                id: 7,
                ..Window::default()
            }),
            keyboard: Rc::new(Window {
                id: 8,
                ..Window::default()
            }),
            serials: RefCell::new(SerialTracker::new()),
            click: RefCell::new(ClickState::default()),
            hover_cleared: Cell::new(false),
            compose: RefCell::new(None),
            resets: Cell::new(0),
            ime_active: Cell::new(false),
            ime_owners: RefCell::new(Vec::new()),
            inserted: RefCell::new(Vec::new()),
        }
    }

    fn tap(&self, now: Instant) {
        down(self, 0, Some(7), point(px(30.), px(50.)), now, 10);
        up(self, 0);
    }

    fn last_count(&self) -> usize {
        self.window
            .events
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlatformInput::MouseDown(event) => Some(event.click_count),
                _ => None,
            })
            .expect("tap sent no press")
    }
}

impl TouchHost for Host {
    type Surface = u32;
    type Window = Rc<Window>;

    fn with_touch<R>(&self, f: impl FnOnce(&mut TouchState<u32>, Modifiers) -> R) -> R {
        f(&mut self.touch.borrow_mut(), Modifiers::default())
    }

    fn window(&self, surface: &u32) -> Option<Self::Window> {
        (*surface == 7).then(|| self.window.clone())
    }

    fn input(&self, window: &Self::Window, input: PlatformInput) {
        let _borrow = window.borrowed.borrow();
        window.events.borrow_mut().push(input);
    }

    fn hover(&self, window: &Self::Window, hovered: bool) {
        let _borrow = window.borrowed.borrow();
        window.hover.borrow_mut().push(hovered);
    }

    fn mark_hover_cleared(&self) {
        self.hover_cleared.set(true);
    }
    fn with_serials<R>(&self, f: impl FnOnce(&mut SerialTracker) -> R) -> R {
        f(&mut self.serials.borrow_mut())
    }
    fn with_click<R>(&self, f: impl FnOnce(&mut ClickState) -> R) -> R {
        f(&mut self.click.borrow_mut())
    }
    fn take_hover_cleared(&self) -> bool {
        self.hover_cleared.replace(false)
    }
    fn keyboard_window(&self) -> Option<Self::Window> {
        Some(self.keyboard.clone())
    }
    fn ime_pressed(&self, window: &Self::Window) -> bool {
        self.ime_owners.borrow_mut().push(window.id);
        self.ime_active.get()
    }
    fn take_compose(&self) -> Option<String> {
        let text = self.compose.borrow_mut().take();
        if text.is_some() {
            self.resets.set(self.resets.get() + 1);
        }
        text
    }
    fn insert_text(&self, window: &Self::Window, text: String) {
        self.inserted.borrow_mut().push((window.id, text));
    }
}

#[test]
fn accepted_down_updates_mouse_press_and_ignored_fingers_leave_serial_unchanged() {
    let host = Host::new();
    let now = Instant::now();
    down(&host, 0, Some(7), Point::default(), now, 41);
    assert_eq!(
        host.serials.borrow().get(SerialKind::MousePress).as_raw(),
        41
    );
    down(&host, 1, Some(7), Point::default(), now, 99);
    assert_eq!(
        host.serials.borrow().get(SerialKind::MousePress).as_raw(),
        41
    );
    down(&host, 0, Some(7), Point::default(), now, 100);
    assert_eq!(
        host.serials.borrow().get(SerialKind::MousePress).as_raw(),
        41
    );
}

#[test]
fn focus_loss_cancels_the_contact_and_dispatches_before_the_stale_lift() {
    let host = Host::new();
    down(&host, 0, Some(7), Point::default(), Instant::now(), 1);
    motion(&host, 0, point(px(0.), px(50.)));
    host.window.events.borrow_mut().clear();
    focus_lost(&host, &7);
    let events = host.window.events.borrow();
    assert!(
        matches!(&events[0], PlatformInput::ScrollWheel(event) if event.touch_phase == gpui::TouchPhase::Cancelled)
    );
    assert!(matches!(events.last(), Some(PlatformInput::MouseExited(_))));
    let count = events.len();
    drop(events);
    up(&host, 0);
    assert_eq!(host.window.events.borrow().len(), count);
}

#[test]
fn next_motion_restores_hover_once_after_touch() {
    let host = Host::new();
    host.tap(Instant::now());
    assert_eq!(*host.window.hover.borrow(), [false]);
    pointer_motion(&host, &host.window);
    pointer_motion(&host, &host.window);
    assert_eq!(*host.window.hover.borrow(), [false, true]);
}

#[test]
fn next_button_restores_hover_once_after_touch() {
    let host = Host::new();
    host.tap(Instant::now());
    assert_eq!(*host.window.hover.borrow(), [false]);
    pointer_button(&host, &host.window);
    pointer_button(&host, &host.window);
    assert_eq!(*host.window.hover.borrow(), [false, true]);
}

#[test]
fn tap_commits_pending_compose_in_the_keyboard_window() {
    let host = Host::new();
    *host.compose.borrow_mut() = Some("accent".into());
    host.tap(Instant::now());
    assert_eq!(*host.inserted.borrow(), [(8, "accent".into())]);
    assert_eq!(host.resets.get(), 1);
    assert!(host.compose.borrow().is_none());
}

#[test]
fn mouse_and_touch_share_the_ime_owner_and_skip_dead_key_fallback_when_composing() {
    let host = Host::new();
    host.ime_active.set(true);
    *host.compose.borrow_mut() = Some("keep".into());
    pointer_pressed(&host, MouseButton::Left, Point::default(), Instant::now());
    host.tap(Instant::now());
    assert_eq!(*host.ime_owners.borrow(), [8, 8]);
    assert!(host.inserted.borrow().is_empty());
    assert_eq!(host.resets.get(), 0);
}

#[test]
fn mouse_tap_mouse_does_not_accumulate_a_double_click() {
    let host = Host::new();
    let now = Instant::now();
    let at = point(px(30.), px(50.));
    assert_eq!(pointer_pressed(&host, MouseButton::Left, at, now), 1);
    host.tap(now + Duration::from_millis(150));
    assert_eq!(
        pointer_pressed(
            &host,
            MouseButton::Left,
            at,
            now + Duration::from_millis(300)
        ),
        1
    );
}

#[test]
fn tap_mouse_tap_does_not_accumulate_a_double_tap() {
    let host = Host::new();
    let now = Instant::now();
    host.tap(now);
    pointer_pressed(
        &host,
        MouseButton::Left,
        point(px(30.), px(50.)),
        now + Duration::from_millis(150),
    );
    host.tap(now + Duration::from_millis(300));
    assert_eq!(host.last_count(), 1);
}

#[test]
fn dropping_a_borrowed_window_discards_pending_and_pan_input() {
    for pan in [false, true] {
        let host = Host::new();
        let now = Instant::now();
        host.tap(now);
        host.window.events.borrow_mut().clear();
        down(
            &host,
            0,
            Some(7),
            point(px(30.), px(50.)),
            now + Duration::from_millis(10),
            10,
        );
        if pan {
            host.touch
                .borrow_mut()
                .motion(0, point(px(30.), px(100.)), Modifiers::default());
        }
        let _drop_borrow = host.window.borrowed.borrow_mut();
        window_dropped(&host, &7);
        assert!(
            host.window.events.borrow().is_empty(),
            "a dropping window must receive no input"
        );
        assert!(
            host.touch
                .borrow_mut()
                .up(0, Modifiers::default())
                .is_empty()
        );
        drop(_drop_borrow);
        host.tap(now + Duration::from_millis(25));
        assert_eq!(
            host.last_count(),
            1,
            "closing the surface must clear its tap history"
        );
    }
}
