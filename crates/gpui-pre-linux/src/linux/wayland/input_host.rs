use super::super::touch_events::TouchHost;
use super::*;

impl TouchHost for WaylandClientStatePtr {
    type Surface = ObjectId;
    type Window = WaylandWindowStatePtr;

    fn with_touch<R>(&self, f: impl FnOnce(&mut TouchState<ObjectId>, Modifiers) -> R) -> R {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        let modifiers = state.modifiers;
        f(&mut state.touch, modifiers)
    }

    fn window(&self, surface: &ObjectId) -> Option<Self::Window> {
        get_window(&mut self.get_client().borrow_mut(), surface)
    }

    fn input(&self, window: &Self::Window, input: PlatformInput) {
        window.handle_input(input);
    }

    fn hover(&self, window: &Self::Window, hovered: bool) {
        window.set_hovered(hovered);
    }

    fn mark_hover_cleared(&self) {
        self.get_client().borrow_mut().touch_cleared_hover = true;
    }

    fn with_serials<R>(&self, f: impl FnOnce(&mut SerialTracker) -> R) -> R {
        f(&mut self.get_client().borrow_mut().serial_tracker)
    }

    fn with_click<R>(&self, f: impl FnOnce(&mut ClickState) -> R) -> R {
        f(&mut self.get_client().borrow_mut().click)
    }

    fn take_hover_cleared(&self) -> bool {
        std::mem::take(&mut self.get_client().borrow_mut().touch_cleared_hover)
    }

    fn keyboard_window(&self) -> Option<Self::Window> {
        self.get_client().borrow().keyboard_focused_window.clone()
    }

    fn ime_pressed(&self, window: &Self::Window) -> bool {
        ime_events::pointer_pressed(self, window)
    }

    fn take_compose(&self) -> Option<String> {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        let text = state.pre_edit_text.take()?;
        state.compose_state.as_mut()?.reset();
        Some(text)
    }

    fn insert_text(&self, window: &Self::Window, text: String) {
        window.handle_ime(ImeInput::InsertText(text));
    }
}
