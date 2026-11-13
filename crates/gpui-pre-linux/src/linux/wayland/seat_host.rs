use super::super::{
    seat_events::{Action, Held, SeatHost},
    touch_events,
};
use super::*;

impl SeatHost for WaylandClientStatePtr {
    type Seat = wl_seat::WlSeat;
    type Queue = QueueHandle<Self>;

    fn held(&self, seat: &Self::Seat) -> (bool, Held) {
        let client = self.get_client();
        let state = client.borrow();
        (
            seat == &state.wl_seat,
            Held {
                keyboard: state.wl_keyboard.is_some(),
                text_input: state.text_input.is_some(),
                pointer: state.wl_pointer.is_some(),
                cursor: state.cursor_shape_device.is_some(),
                pinch: state.pinch_gesture.is_some(),
                touch: state.wl_touch.is_some(),
                keyboard_focus: state.keyboard_focused_window.is_some(),
                pointer_state: state.mouse_focused_window.is_some()
                    || state.mouse_location.is_some()
                    || state.button_pressed.is_some()
                    || state.cursor_hidden_window.is_some(),
            },
        )
    }

    fn apply(&self, seat: &Self::Seat, qh: &Self::Queue, action: Action) {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        match action {
            Action::BindTouch => {
                state.wl_touch = Some(seat.get_touch(qh, ()));
                drop(state);
                touch_events::capability(self, true);
            }
            Action::ReleaseTouch => {
                if let Some(touch) = state.wl_touch.take() {
                    touch.release();
                }
                drop(state);
                touch_events::capability(self, false);
            }
            Action::BindKeyboard => state.wl_keyboard = Some(seat.get_keyboard(qh, ())),
            Action::ReleaseKeyboard => {
                if let Some(keyboard) = state.wl_keyboard.take() {
                    keyboard.release();
                }
            }
            Action::BindTextInput => {
                state.text_input = state
                    .globals
                    .text_input_manager
                    .as_ref()
                    .map(|manager| manager.get_text_input(seat, qh, ()));
            }
            Action::ReleaseTextInput => {
                if let Some(text) = state.text_input.take() {
                    text.destroy();
                }
            }
            Action::ResetTextInput => {
                drop(state);
                ime_events::text_input_destroyed(self);
            }
            Action::KeyboardLost => {
                let focused = state.keyboard_focused_window.take();
                state.repeat.current_id += 1;
                state.enter_token.take();
                state.clear_keyboard_composition();
                drop(state);
                let surface = focused.as_ref().map(|window| window.surface().id());
                ime_events::keyboard_removed(self, surface.as_ref());
                if let Some(window) = focused {
                    window.set_focused(false);
                }
            }
            Action::BindPointer => state.wl_pointer = Some(seat.get_pointer(qh, ())),
            Action::ReleasePointer => {
                if let Some(pointer) = state.wl_pointer.take() {
                    pointer.release();
                }
            }
            Action::BindCursor => {
                state.cursor_shape_device = state.wl_pointer.as_ref().and_then(|pointer| {
                    state
                        .globals
                        .cursor_shape_manager
                        .as_ref()
                        .map(|manager| manager.get_pointer(pointer, qh, ()))
                });
            }
            Action::ReleaseCursor => {
                if let Some(cursor) = state.cursor_shape_device.take() {
                    cursor.destroy();
                }
            }
            Action::BindPinch => {
                state.pinch_gesture = state.wl_pointer.as_ref().and_then(|pointer| {
                    state
                        .globals
                        .gesture_manager
                        .as_ref()
                        .map(|manager| manager.get_pinch_gesture(pointer, qh, ()))
                });
            }
            Action::ReleasePinch => {
                if let Some(pinch) = state.pinch_gesture.take() {
                    pinch.destroy();
                }
            }
            Action::ClearPointer => {
                state.scroll_frame = ScrollFrame::default();
                state.cursor_hidden_window = None;
                let position = state
                    .mouse_location
                    .take()
                    .unwrap_or(point(px(-1.), px(-1.)));
                let pressed_button = state.button_pressed.take();
                let focused = state.mouse_focused_window.take();
                let modifiers = state.modifiers;
                drop(state);
                if let Some(window) = focused {
                    window.handle_input(PlatformInput::MouseExited(MouseExitEvent {
                        position,
                        pressed_button,
                        modifiers,
                    }));
                    window.set_hovered(false);
                }
            }
        }
    }
}
