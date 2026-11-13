use wayland_client::protocol::wl_seat::Capability;

#[derive(Clone, Copy, Default)]
pub(super) struct Held {
    pub keyboard: bool,
    pub text_input: bool,
    pub pointer: bool,
    pub cursor: bool,
    pub pinch: bool,
    pub touch: bool,
    pub keyboard_focus: bool,
    pub pointer_state: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    BindTouch,
    ReleaseTouch,
    BindKeyboard,
    ReleaseKeyboard,
    BindTextInput,
    ReleaseTextInput,
    ResetTextInput,
    KeyboardLost,
    BindPointer,
    ReleasePointer,
    BindCursor,
    ReleaseCursor,
    BindPinch,
    ReleasePinch,
    ClearPointer,
}

pub(super) trait SeatHost {
    type Seat;
    type Queue;

    fn held(&self, seat: &Self::Seat) -> (bool, Held);
    fn apply(&self, seat: &Self::Seat, queue: &Self::Queue, action: Action);
}

/// The protocol handler only executes this plan; no callback runs under the snapshot's borrow.
pub(super) fn capabilities<H: SeatHost>(
    host: &H,
    seat: &H::Seat,
    queue: &H::Queue,
    available: Capability,
) {
    let (current, held) = host.held(seat);
    for action in plan(current, available, held) {
        host.apply(seat, queue, action);
    }
}

fn plan(current: bool, available: Capability, held: Held) -> Vec<Action> {
    use Action::*;
    if !current {
        return Vec::new();
    }
    let mut actions = Vec::new();
    match (available.contains(Capability::Touch), held.touch) {
        (true, false) => actions.push(BindTouch),
        (false, true) => actions.push(ReleaseTouch),
        _ => {}
    }
    if available.contains(Capability::Keyboard) {
        if !held.keyboard {
            if held.text_input {
                actions.push(ReleaseTextInput);
            }
            actions.extend([BindKeyboard, BindTextInput, ResetTextInput]);
        }
    } else if held.keyboard || held.text_input || held.keyboard_focus {
        if held.keyboard {
            actions.push(ReleaseKeyboard);
        }
        if held.text_input {
            actions.push(ReleaseTextInput);
        }
        actions.push(KeyboardLost);
    }
    if available.contains(Capability::Pointer) {
        if !held.pointer {
            if held.cursor {
                actions.push(ReleaseCursor);
            }
            if held.pinch {
                actions.push(ReleasePinch);
            }
            actions.extend([BindPointer, BindCursor, BindPinch]);
        }
    } else if held.pointer || held.cursor || held.pinch || held.pointer_state {
        if held.cursor {
            actions.push(ReleaseCursor);
        }
        if held.pinch {
            actions.push(ReleasePinch);
        }
        if held.pointer {
            actions.push(ReleasePointer);
        }
        actions.push(ClearPointer);
    }
    actions
}

#[cfg(test)]
#[path = "seat_events_tests.rs"]
mod tests;
