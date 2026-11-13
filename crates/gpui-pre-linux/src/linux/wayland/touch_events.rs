use std::time::Instant;

use gpui::{Modifiers, MouseButton, Pixels, PlatformInput, Point};

use super::touch::{Input, TouchState};
use super::{
    click::ClickState,
    serial::{SerialKind, SerialTracker},
};

pub(super) trait TouchHost {
    type Surface: Clone + PartialEq;
    type Window;

    fn with_touch<R>(&self, f: impl FnOnce(&mut TouchState<Self::Surface>, Modifiers) -> R) -> R;
    fn window(&self, surface: &Self::Surface) -> Option<Self::Window>;
    fn input(&self, window: &Self::Window, input: PlatformInput);
    fn hover(&self, window: &Self::Window, hovered: bool);
    fn mark_hover_cleared(&self);
    fn with_serials<R>(&self, f: impl FnOnce(&mut SerialTracker) -> R) -> R;
    fn with_click<R>(&self, f: impl FnOnce(&mut ClickState) -> R) -> R;
    fn take_hover_cleared(&self) -> bool;
    fn keyboard_window(&self) -> Option<Self::Window>;
    fn ime_pressed(&self, window: &Self::Window) -> bool;
    fn take_compose(&self) -> Option<String>;
    fn insert_text(&self, window: &Self::Window, text: String);
}

pub(super) fn dispatch<H: TouchHost>(host: &H, events: Vec<Input<H::Surface>>) {
    for input in events {
        if let Some(window) = host.window(&input.surface) {
            if matches!(input.event, PlatformInput::MouseDown(_)) {
                primary_press(host);
                host.with_click(|click| click.clear());
            }
            if matches!(input.event, PlatformInput::MouseExited(_)) {
                host.mark_hover_cleared();
                host.hover(&window, false);
            }
            host.input(&window, input.event);
        }
    }
}

pub(super) fn focus_lost<H: TouchHost>(host: &H, surface: &H::Surface) {
    let events = host.with_touch(|touch, modifiers| touch.cancel_surface(surface, modifiers));
    dispatch(host, events);
}

pub(super) fn window_dropped<H: TouchHost>(host: &H, surface: &H::Surface) {
    // The platform window is mutably borrowed by Drop; it must receive no input.
    host.with_touch(|touch, modifiers| {
        touch.cancel_surface(surface, modifiers);
    });
}

pub(super) fn capability<H: TouchHost>(host: &H, available: bool) {
    let (_, events) = host.with_touch(|touch, modifiers| touch.capability(available, modifiers));
    dispatch(host, events);
}

pub(super) fn down<H: TouchHost>(
    host: &H,
    id: i32,
    surface: Option<H::Surface>,
    position: Point<Pixels>,
    now: Instant,
    serial: u32,
) {
    if host.with_touch(|touch, _| touch.down(id, surface, position, now)) {
        host.with_serials(|serials| serials.update(SerialKind::MousePress, serial));
    }
}

pub(super) fn motion<H: TouchHost>(host: &H, id: i32, position: Point<Pixels>) {
    let events = host.with_touch(|touch, modifiers| touch.motion(id, position, modifiers));
    dispatch(host, events);
}

pub(super) fn up<H: TouchHost>(host: &H, id: i32) {
    let events = host.with_touch(|touch, modifiers| touch.up(id, modifiers));
    dispatch(host, events);
}

pub(super) fn cancel<H: TouchHost>(host: &H) {
    let events = host.with_touch(|touch, modifiers| touch.cancel(modifiers));
    dispatch(host, events);
}

pub(super) fn primary_press<H: TouchHost>(host: &H) {
    if let Some(window) = host.keyboard_window() {
        if !host.ime_pressed(&window)
            && let Some(text) = host.take_compose()
        {
            host.insert_text(&window, text);
        }
    }
}

pub(super) fn pointer_pressed<H: TouchHost>(
    host: &H,
    button: MouseButton,
    position: Point<Pixels>,
    now: Instant,
) -> usize {
    primary_press(host);
    host.with_touch(|touch, _| touch.reset_tap_history());
    host.with_click(|click| click.press(button, position, now))
}

pub(super) fn pointer_motion<H: TouchHost>(host: &H, window: &H::Window) {
    restore_hover(host, window);
}

pub(super) fn pointer_button<H: TouchHost>(host: &H, window: &H::Window) {
    restore_hover(host, window);
}

fn restore_hover<H: TouchHost>(host: &H, window: &H::Window) {
    if host.take_hover_cleared() {
        host.hover(window, true);
    }
}

#[cfg(test)]
#[path = "touch_events_tests.rs"]
mod tests;
