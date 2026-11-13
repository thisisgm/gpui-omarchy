use std::time::Duration;

use super::*;

fn position(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

fn seat() -> TouchState<u32> {
    let mut state = TouchState::default();
    assert_eq!(
        state.capability(true, Modifiers::default()).0,
        CapabilityChange::Bind
    );
    state
}

fn down(state: &mut TouchState<u32>, id: i32, at: Point<Pixels>, now: Instant) {
    state.down(id, Some(7), at, now);
}

fn tap(state: &mut TouchState<u32>, at: Point<Pixels>, now: Instant) -> Vec<Input<u32>> {
    down(state, 0, at, now);
    state.up(0, Modifiers::default())
}

fn click_count(events: &[Input<u32>]) -> usize {
    let PlatformInput::MouseDown(down) = &events[0].event else {
        panic!("tap must press before releasing");
    };
    let PlatformInput::MouseUp(up) = &events[1].event else {
        panic!("tap must release its press");
    };
    assert_eq!(down.button, MouseButton::Left);
    assert_eq!(up.button, MouseButton::Left);
    assert_eq!(down.position, up.position);
    assert_eq!(down.click_count, up.click_count);
    down.click_count
}

fn scrolls(events: &[Input<u32>]) -> Vec<(TouchPhase, Point<Pixels>, Point<Pixels>)> {
    events
        .iter()
        .filter_map(|input| match &input.event {
            PlatformInput::ScrollWheel(event) => {
                let ScrollDelta::Pixels(delta) = event.delta else {
                    panic!("touch must scroll pixels");
                };
                Some((event.touch_phase, event.position, delta))
            }
            PlatformInput::MouseDown(_) | PlatformInput::MouseUp(_) => {
                panic!("pan sent a mouse button")
            }
            PlatformInput::MouseMove(event) => {
                assert_eq!(event.pressed_button, None);
                None
            }
            PlatformInput::MouseExited(_) => None,
            _ => panic!("unexpected input"),
        })
        .collect()
}

fn hover_cleared(events: &[Input<u32>]) {
    let tail = &events[events.len() - 2..];
    let PlatformInput::MouseMove(moved) = &tail[0].event else {
        panic!("cached hit test must move off the surface");
    };
    assert!(moved.position.x < px(0.) && moved.position.y < px(0.));
    assert_eq!(moved.pressed_button, None);
    let PlatformInput::MouseExited(exit) = &tail[1].event else {
        panic!("hover callbacks must receive an exit");
    };
    assert_eq!(exit.position, moved.position);
    assert_eq!(exit.pressed_button, None);
}

#[test]
fn tap_defers_the_press_and_release_until_lift_then_clears_hover() {
    let mut state = seat();
    let at = position(90., 120.);
    down(&mut state, 0, at, Instant::now());
    assert!(state.motion(0, at, Modifiers::default()).is_empty());
    let events = state.up(0, Modifiers::shift());
    assert_eq!(events.len(), 4);
    assert_eq!(click_count(&events), 1);
    assert!(events.iter().all(|event| event.surface == 7));
    let PlatformInput::MouseDown(event) = &events[0].event else {
        unreachable!()
    };
    assert_eq!(event.position, at);
    assert_eq!(event.modifiers, Modifiers::shift());
    hover_cleared(&events);
    assert!(state.up(0, Modifiers::default()).is_empty());
}

#[test]
fn double_tap_uses_the_pointer_interval_and_per_axis_distance() {
    let now = Instant::now();
    for (distance, milliseconds, expected) in [(5., 399, 2), (5.001, 399, 1), (0., 400, 1)] {
        let mut state = seat();
        assert_eq!(click_count(&tap(&mut state, position(20., 30.), now)), 1);
        let events = tap(
            &mut state,
            position(20. + distance, 30. + distance),
            now + Duration::from_millis(milliseconds),
        );
        assert_eq!(
            click_count(&events),
            expected,
            "distance {distance}, interval {milliseconds}"
        );
    }
}

#[test]
fn tap_count_never_carries_to_a_different_window() {
    let now = Instant::now();
    let mut state = seat();
    tap(&mut state, position(20., 30.), now);
    state.down(
        0,
        Some(8),
        position(20., 30.),
        now + Duration::from_millis(10),
    );
    assert_eq!(click_count(&state.up(0, Modifiers::default())), 1);
}

#[test]
fn slop_is_inclusive_at_eight_pixels_and_pan_stays_a_pan_after_return() {
    let now = Instant::now();
    for (x, y, clicks) in [
        (8., 0., true),
        (0., -8., true),
        (8.001, 0., false),
        (6., 6., false),
    ] {
        let mut state = seat();
        down(&mut state, 0, Point::default(), now);
        let moved = state.motion(0, position(x, y), Modifiers::default());
        if clicks {
            assert!(moved.is_empty());
            assert_eq!(click_count(&state.up(0, Modifiers::default())), 1);
        } else {
            assert_eq!(scrolls(&moved)[0].0, TouchPhase::Started);
            assert_eq!(scrolls(&moved)[1].2, position(x, y));
            state.motion(0, Point::default(), Modifiers::default());
            let ended = state.up(0, Modifiers::default());
            assert_eq!(scrolls(&ended)[0].0, TouchPhase::Ended);
            hover_cleared(&ended);
        }
    }
}

#[test]
fn pan_preserves_pixel_deltas_both_axes_target_and_touchpad_phases() {
    let mut state = seat();
    let at = position(100., 200.);
    down(&mut state, 0, at, Instant::now());
    assert!(
        state
            .motion(0, position(100., 204.), Modifiers::default())
            .is_empty()
    );
    let first = state.motion(0, position(103., 220.), Modifiers::default());
    assert_eq!(
        scrolls(&first),
        [
            (TouchPhase::Started, at, Point::default()),
            (TouchPhase::Moved, at, position(3., 20.)),
        ]
    );
    let next = state.motion(0, position(90., 205.), Modifiers::default());
    assert_eq!(
        scrolls(&next),
        [(TouchPhase::Moved, at, position(-13., -15.))]
    );
    let last = state.up(0, Modifiers::default());
    assert_eq!(scrolls(&last), [(TouchPhase::Ended, at, Point::default())]);
    assert!(state.motion(0, at, Modifiers::default()).is_empty());
}

#[test]
fn extra_fingers_never_click_scroll_or_take_over_the_first_contact() {
    let mut state = seat();
    let now = Instant::now();
    let at = position(20., 30.);
    down(&mut state, 0, at, now);
    down(&mut state, 1, at, now);
    assert!(
        state
            .motion(1, position(100., 300.), Modifiers::default())
            .is_empty()
    );
    assert_eq!(click_count(&state.up(0, Modifiers::default())), 1);
    down(&mut state, 2, at, now);
    assert!(state.up(2, Modifiers::default()).is_empty());
    assert!(
        state
            .motion(1, position(200., 300.), Modifiers::default())
            .is_empty()
    );
    assert!(state.up(1, Modifiers::default()).is_empty());
    assert_eq!(
        click_count(&tap(&mut state, at, now + Duration::from_secs(1))),
        1
    );
}

#[test]
fn only_the_accepted_primary_down_can_replace_the_activation_serial() {
    let mut state = seat();
    let now = Instant::now();
    assert!(state.down(0, Some(7), Point::default(), now));
    assert!(!state.down(1, Some(7), Point::default(), now));
    assert!(!state.down(0, Some(7), Point::default(), now));
    state.capability(false, Modifiers::default());
    assert!(!state.down(2, Some(7), Point::default(), now));
}

#[test]
fn cancel_never_clicks_ends_scroll_once_and_resets_the_next_tap() {
    for panning in [false, true] {
        let mut state = seat();
        let now = Instant::now();
        tap(&mut state, Point::default(), now);
        down(&mut state, 0, Point::default(), now);
        if panning {
            state.motion(0, position(30., 30.), Modifiers::default());
        }
        let cancelled = state.cancel(Modifiers::default());
        assert_eq!(scrolls(&cancelled).len(), usize::from(panning));
        if panning {
            assert_eq!(scrolls(&cancelled)[0].0, TouchPhase::Cancelled);
        }
        hover_cleared(&cancelled);
        assert!(state.up(0, Modifiers::default()).is_empty());
        assert!(state.cancel(Modifiers::default()).is_empty());
        assert_eq!(click_count(&tap(&mut state, Point::default(), now)), 1);
    }
}

#[test]
fn capability_removal_cancels_mid_gesture_and_rebinds_once_without_a_restart() {
    let mut state = seat();
    let modifiers = Modifiers::default();
    assert_eq!(
        state.capability(true, modifiers).0,
        CapabilityChange::Unchanged
    );
    let now = Instant::now();
    down(&mut state, 0, Point::default(), now);
    state.motion(0, position(0., 100.), modifiers);
    let (change, events) = state.capability(false, modifiers);
    assert_eq!(change, CapabilityChange::Release);
    assert_eq!(
        scrolls(&events),
        [(TouchPhase::Cancelled, Point::default(), Point::default())]
    );
    assert_eq!(
        state.capability(false, modifiers).0,
        CapabilityChange::Unchanged
    );
    assert!(state.up(0, modifiers).is_empty());
    assert!(tap(&mut state, Point::default(), now).is_empty());
    assert_eq!(state.capability(true, modifiers).0, CapabilityChange::Bind);
    assert_eq!(click_count(&tap(&mut state, Point::default(), now)), 1);
}

#[test]
fn losing_the_touched_window_cancels_only_its_gesture() {
    let mut state = seat();
    let modifiers = Modifiers::default();
    let now = Instant::now();
    down(&mut state, 0, Point::default(), now);
    assert!(state.cancel_surface(&8, modifiers).is_empty());
    state.motion(0, position(0., 100.), modifiers);
    let events = state.cancel_surface(&7, modifiers);
    assert_eq!(scrolls(&events)[0].0, TouchPhase::Cancelled);
    hover_cleared(&events);
    assert!(state.up(0, modifiers).is_empty());
    assert_eq!(click_count(&tap(&mut state, Point::default(), now)), 1);
}

#[test]
fn a_tap_after_a_scroll_clicks_without_reusing_the_old_tap_count() {
    let mut state = seat();
    let now = Instant::now();
    tap(&mut state, Point::default(), now);
    down(&mut state, 0, Point::default(), now);
    state.motion(0, position(0., 100.), Modifiers::default());
    scrolls(&state.up(0, Modifiers::default()));
    assert_eq!(click_count(&tap(&mut state, Point::default(), now)), 1);
}

#[test]
fn unknown_surface_reserves_the_first_contact_until_every_finger_lifts() {
    let mut state = seat();
    let now = Instant::now();
    state.down(0, None, Point::default(), now);
    down(&mut state, 1, Point::default(), now);
    assert!(state.up(0, Modifiers::default()).is_empty());
    assert!(state.up(1, Modifiers::default()).is_empty());
    assert_eq!(click_count(&tap(&mut state, Point::default(), now)), 1);
}
