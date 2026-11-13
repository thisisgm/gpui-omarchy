use std::time::Instant;

use gpui::{MouseButton, Pixels, Point};

use crate::linux::{DOUBLE_CLICK_INTERVAL, is_within_click_distance};

pub(super) struct ClickState {
    last_mouse_button: Option<MouseButton>,
    last_click: Instant,
    last_location: Point<Pixels>,
    current_count: usize,
}

impl Default for ClickState {
    fn default() -> Self {
        Self {
            last_mouse_button: None,
            last_click: Instant::now(),
            last_location: Point::default(),
            current_count: 0,
        }
    }
}

impl ClickState {
    pub fn clear(&mut self) {
        self.last_mouse_button = None;
    }

    pub fn press(&mut self, button: MouseButton, position: Point<Pixels>, now: Instant) -> usize {
        if now.duration_since(self.last_click) < DOUBLE_CLICK_INTERVAL
            && self.last_mouse_button == Some(button)
            && is_within_click_distance(self.last_location, position)
        {
            self.current_count = self.current_count.saturating_add(1);
        } else {
            self.current_count = 1;
        }
        self.last_click = now;
        self.last_mouse_button = Some(button);
        self.last_location = position;
        self.current_count
    }

    pub fn count(&self) -> usize {
        self.current_count
    }
}
