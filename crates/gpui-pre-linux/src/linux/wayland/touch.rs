use std::time::Instant;

use gpui::{
    GestureTuning, Modifiers, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point,
    px,
};

use crate::linux::{DOUBLE_CLICK_INTERVAL, is_within_click_distance};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CapabilityChange {
    Bind,
    Release,
    Unchanged,
}

pub(super) struct Input<S> {
    pub surface: S,
    pub event: PlatformInput,
}

struct Contact<S> {
    id: i32,
    surface: S,
    start: Point<Pixels>,
    position: Point<Pixels>,
    down_at: Instant,
    scrolling: bool,
}

struct Tap<S> {
    surface: S,
    position: Point<Pixels>,
    down_at: Instant,
    count: usize,
}

/// One seat's direct touch. A pending finger never sends a held mouse button.
pub(super) struct TouchState<S> {
    available: bool,
    contacts: Vec<i32>,
    active: Option<Contact<S>>,
    last_tap: Option<Tap<S>>,
}

impl<S> Default for TouchState<S> {
    fn default() -> Self {
        Self {
            available: false,
            contacts: Vec::new(),
            active: None,
            last_tap: None,
        }
    }
}

impl<S: Clone + PartialEq> TouchState<S> {
    pub fn capability(
        &mut self,
        available: bool,
        modifiers: Modifiers,
    ) -> (CapabilityChange, Vec<Input<S>>) {
        match (std::mem::replace(&mut self.available, available), available) {
            (false, true) => (CapabilityChange::Bind, Vec::new()),
            (true, false) => (CapabilityChange::Release, self.cancel(modifiers)),
            _ => (CapabilityChange::Unchanged, Vec::new()),
        }
    }

    pub fn down(
        &mut self,
        id: i32,
        surface: Option<S>,
        position: Point<Pixels>,
        now: Instant,
    ) -> bool {
        if !self.available || self.contacts.contains(&id) {
            return false;
        }
        let first = self.contacts.is_empty();
        self.contacts.push(id);
        if first && let Some(surface) = surface {
            self.active = Some(Contact {
                id,
                surface,
                start: position,
                position,
                down_at: now,
                scrolling: false,
            });
            return true;
        }
        false
    }

    pub fn motion(
        &mut self,
        id: i32,
        position: Point<Pixels>,
        modifiers: Modifiers,
    ) -> Vec<Input<S>> {
        let Some(touch) = self.active.as_mut().filter(|touch| touch.id == id) else {
            return Vec::new();
        };
        let previous = touch.position;
        touch.position = position;
        let mut events = Vec::new();
        if !touch.scrolling {
            // Match the portable GPUI recognizer's eight logical pixel radial slop.
            if (position - touch.start).magnitude()
                <= f64::from(GestureTuning::default().touch_slop)
            {
                return events;
            }
            touch.scrolling = true;
            self.last_tap = None;
            events.push(touch.scroll(Point::default(), TouchPhase::Started, modifiers));
            events.push(touch.scroll(position - touch.start, TouchPhase::Moved, modifiers));
        } else {
            events.push(touch.scroll(position - previous, TouchPhase::Moved, modifiers));
        }
        events
    }

    pub fn up(&mut self, id: i32, modifiers: Modifiers) -> Vec<Input<S>> {
        self.contacts.retain(|contact| *contact != id);
        if !self.active.as_ref().is_some_and(|touch| touch.id == id) {
            return Vec::new();
        }
        let Some(touch) = self.active.take() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if touch.scrolling {
            events.push(touch.scroll(Point::default(), TouchPhase::Ended, modifiers));
        } else {
            // Use the pointer's strict interval and inclusive per-axis distance rule.
            let count = self
                .last_tap
                .as_ref()
                .filter(|tap| {
                    tap.surface == touch.surface
                        && touch.down_at.duration_since(tap.down_at) < DOUBLE_CLICK_INTERVAL
                        && is_within_click_distance(tap.position, touch.position)
                })
                .map_or(1, |tap| tap.count.saturating_add(1));
            self.last_tap = Some(Tap {
                surface: touch.surface.clone(),
                position: touch.position,
                down_at: touch.down_at,
                count,
            });
            events.push(touch.input(PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: touch.position,
                modifiers,
                click_count: count,
                first_mouse: false,
            })));
            events.push(touch.input(PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: touch.position,
                modifiers,
                click_count: count,
            })));
        }
        touch.clear_hover(&mut events, modifiers);
        events
    }

    pub fn cancel(&mut self, modifiers: Modifiers) -> Vec<Input<S>> {
        self.contacts.clear();
        self.last_tap = None;
        self.cancel_active(modifiers)
    }

    pub fn reset_tap_history(&mut self) {
        self.last_tap = None;
    }

    pub fn cancel_surface(&mut self, surface: &S, modifiers: Modifiers) -> Vec<Input<S>> {
        if self
            .last_tap
            .as_ref()
            .is_some_and(|tap| &tap.surface == surface)
        {
            self.last_tap = None;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|touch| &touch.surface == surface)
        {
            return self.cancel_active(modifiers);
        }
        Vec::new()
    }

    fn cancel_active(&mut self, modifiers: Modifiers) -> Vec<Input<S>> {
        let Some(touch) = self.active.take() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if touch.scrolling {
            events.push(touch.scroll(Point::default(), TouchPhase::Cancelled, modifiers));
        }
        touch.clear_hover(&mut events, modifiers);
        events
    }
}

impl<S: Clone> Contact<S> {
    fn input(&self, event: PlatformInput) -> Input<S> {
        Input {
            surface: self.surface.clone(),
            event,
        }
    }

    fn scroll(&self, delta: Point<Pixels>, phase: TouchPhase, modifiers: Modifiers) -> Input<S> {
        self.input(PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: self.start,
            delta: ScrollDelta::Pixels(delta),
            modifiers,
            touch_phase: phase,
        }))
    }

    fn clear_hover(&self, events: &mut Vec<Input<S>>, modifiers: Modifiers) {
        // Also move GPUI's cached hit-test outside: an exit alone can re-hover on the next draw.
        let outside = point(px(-1.), px(-1.));
        events.push(self.input(PlatformInput::MouseMove(MouseMoveEvent {
            position: outside,
            pressed_button: None,
            modifiers,
        })));
        events.push(self.input(PlatformInput::MouseExited(MouseExitEvent {
            position: outside,
            pressed_button: None,
            modifiers,
        })));
    }
}

#[cfg(test)]
#[path = "touch_tests.rs"]
mod tests;
