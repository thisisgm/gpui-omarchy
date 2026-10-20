//! Turns one `wl_pointer` frame's axis events into GPUI scroll events, with stroke phases for a touchpad.

use gpui::{ScrollDelta, TouchPhase, point, px};
use wayland_backend::protocol::WEnum;
use wayland_client::protocol::wl_pointer::{self, Axis, AxisSource};

use crate::linux::SCROLL_LINES;

/// GPUI's scale on a continuous axis value.
const CONTINUOUS_SCALE: f32 = 3.0;

/// The axis events of one `wl_pointer` frame, plus whether a finger stroke is open across frames.
///
/// The frame is read whole at `wl_pointer.frame` because a compositor may send `axis` before
/// `axis_source` in the same frame (Hyprland does). A frame with no `axis_source` counts as a
/// wheel frame, as qtwayland resets its frame source to wheel. Only a finger source is a
/// terminated stroke (wl_pointer.axis_source, qtwayland `isDefinitelyTerminated`): it opens with
/// a zero-delta `Started` and ends with a zero-delta `Ended` on its `axis_stop`. Every other
/// source scrolls with `Moved` alone and never opens, extends or ends a stroke. Outside a finger
/// stroke a frame with a discrete value scrolls by lines and one without scrolls its continuous
/// value, the value qtwayland turns into its angle delta.
#[derive(Default)]
pub(crate) struct ScrollFrame {
    source: Option<AxisSource>,
    continuous: Option<(f32, f32)>,
    discrete: Option<(f32, f32)>,
    stopped: bool,
    stroke: bool,
}

impl ScrollFrame {
    /// Takes one pointer event; false when it is not a scroll event.
    pub(crate) fn event(
        &mut self,
        event: &wl_pointer::Event,
        shift: bool,
        vertical: f32,
        horizontal: f32,
    ) -> bool {
        // Shift turns a vertical scroll horizontal, and each axis takes its own direction.
        let oriented = |axis: Axis| match axis {
            Axis::VerticalScroll if shift => Some((Axis::HorizontalScroll, horizontal)),
            Axis::VerticalScroll => Some((axis, vertical)),
            Axis::HorizontalScroll => Some((axis, horizontal)),
            _ => None,
        };
        let add = |sum: &mut Option<(f32, f32)>, axis: Axis, value: f32| {
            let sum = sum.get_or_insert((0.0, 0.0));
            if axis == Axis::VerticalScroll {
                sum.1 += value;
            } else {
                sum.0 += value;
            }
        };
        match event {
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(source),
            } => self.source = Some(*source),
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => {
                if let Some((axis, modifier)) = oriented(*axis) {
                    add(
                        &mut self.continuous,
                        axis,
                        *value as f32 * CONTINUOUS_SCALE * modifier,
                    );
                }
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => {
                if let Some((axis, modifier)) = oriented(*axis) {
                    add(
                        &mut self.discrete,
                        axis,
                        *discrete as f32 * modifier * SCROLL_LINES,
                    );
                }
            }
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => {
                if let Some((axis, modifier)) = oriented(*axis) {
                    let notches = *value120 as f32 / 120.0;
                    add(&mut self.discrete, axis, notches * modifier * SCROLL_LINES);
                }
            }
            wl_pointer::Event::AxisStop { .. } => self.stopped = true,
            _ => return false,
        }
        true
    }

    /// Ends the frame and returns its scroll events in order; empty when the frame held none.
    pub(crate) fn frame(&mut self) -> Vec<(TouchPhase, ScrollDelta)> {
        let source = self.source.take().unwrap_or(AxisSource::Wheel);
        let continuous = self.continuous.take();
        let discrete = self.discrete.take();
        let stopped = std::mem::take(&mut self.stopped);
        let zero = ScrollDelta::Pixels(point(px(0.0), px(0.0)));
        let mut events = Vec::new();
        match continuous {
            // A discrete value restates the notch, as qtwayland's angle delta prefers it, so that frame scrolls by lines.
            Some(_) if discrete.is_some() && source != AxisSource::Finger => {}
            Some((x, y)) => {
                let delta = ScrollDelta::Pixels(point(px(x), px(y)));
                if source == AxisSource::Finger && (self.stroke || (x, y) != (0.0, 0.0)) {
                    if !std::mem::replace(&mut self.stroke, true) {
                        events.push((TouchPhase::Started, zero));
                    }
                }
                events.push((TouchPhase::Moved, delta));
            }
            None => {}
        }
        if events.is_empty()
            && let Some((x, y)) = discrete
        {
            events.push((TouchPhase::Moved, ScrollDelta::Lines(point(x, y))));
        }
        // An axis value and its axis_stop in one frame are the stroke's last motion, then its end.
        if stopped && std::mem::take(&mut self.stroke) {
            events.push((TouchPhase::Ended, zero));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: f64 = -20.0;

    fn axis(value: f64) -> wl_pointer::Event {
        wl_pointer::Event::Axis {
            time: 0,
            axis: WEnum::Value(Axis::VerticalScroll),
            value,
        }
    }

    fn source(source: AxisSource) -> wl_pointer::Event {
        wl_pointer::Event::AxisSource {
            axis_source: WEnum::Value(source),
        }
    }

    fn stop() -> wl_pointer::Event {
        wl_pointer::Event::AxisStop {
            time: 0,
            axis: WEnum::Value(Axis::VerticalScroll),
        }
    }

    fn value120(value120: i32) -> wl_pointer::Event {
        wl_pointer::Event::AxisValue120 {
            axis: WEnum::Value(Axis::VerticalScroll),
            value120,
        }
    }

    /// One sent event as plain values, since ScrollDelta has no equality: phase, pixels or lines, x, y.
    type Sent = (TouchPhase, bool, f32, f32);

    /// Feeds one frame's events in the order given and returns what the frame sends.
    fn frame(scroll: &mut ScrollFrame, events: &[wl_pointer::Event]) -> Vec<Sent> {
        for event in events {
            assert!(scroll.event(event, false, -1.0, -1.0));
        }
        let plain = |(phase, delta)| match delta {
            ScrollDelta::Pixels(at) => (phase, true, f32::from(at.x), f32::from(at.y)),
            ScrollDelta::Lines(at) => (phase, false, at.x, at.y),
        };
        scroll.frame().into_iter().map(plain).collect()
    }

    fn pixels(phase: TouchPhase, y: f32) -> Sent {
        (phase, true, 0.0, y)
    }

    /// Hyprland 0.56.2 sends axis, then axis_source, in each frame, and axis(0) with axis_stop at the lift.
    fn finger_stroke(scroll: &mut ScrollFrame) -> Vec<Vec<Sent>> {
        let mut frames = Vec::new();
        for _ in 0..3 {
            frames.push(frame(scroll, &[axis(UP), source(AxisSource::Finger)]));
        }
        frames.push(frame(
            scroll,
            &[axis(0.0), source(AxisSource::Finger), stop()],
        ));
        frames
    }

    #[test]
    fn a_finger_stroke_opens_with_started_moves_and_ends_on_its_axis_stop() {
        let mut scroll = ScrollFrame::default();
        let frames = finger_stroke(&mut scroll);
        let moved = pixels(TouchPhase::Moved, 60.0);
        assert_eq!(frames[0], [pixels(TouchPhase::Started, 0.0), moved]);
        assert_eq!(frames[1], [moved]);
        assert_eq!(frames[2], [moved]);
        let lift = [
            pixels(TouchPhase::Moved, 0.0),
            pixels(TouchPhase::Ended, 0.0),
        ];
        assert_eq!(frames[3], lift, "axis_stop is the lift");
        assert!(
            frame(&mut scroll, &[stop()]).is_empty(),
            "a stop with no open stroke sends nothing"
        );
        let next = frame(&mut scroll, &[axis(UP), source(AxisSource::Finger)]);
        assert_eq!(
            next[0],
            pixels(TouchPhase::Started, 0.0),
            "the next stroke opens again"
        );
    }

    #[test]
    fn a_continuous_or_tilt_source_scrolls_with_moved_alone_and_opens_no_stroke() {
        for unterminated in [AxisSource::Continuous, AxisSource::WheelTilt] {
            let mut scroll = ScrollFrame::default();
            for _ in 0..3 {
                let sent = frame(&mut scroll, &[axis(UP), source(unterminated)]);
                assert_eq!(sent, [pixels(TouchPhase::Moved, 60.0)], "{unterminated:?}");
            }
            // Hyprland ends a trackpoint scroll with axis(0) and axis_stop; that ends nothing.
            let last = frame(&mut scroll, &[axis(0.0), source(unterminated), stop()]);
            assert_eq!(last, [pixels(TouchPhase::Moved, 0.0)], "{unterminated:?}");
        }
    }

    #[test]
    fn a_wheel_notch_after_a_finger_stroke_scrolls_lines_and_opens_no_stroke() {
        let mut scroll = ScrollFrame::default();
        finger_stroke(&mut scroll);
        // Hyprland's wheel frame: axis first, under the finger stroke's source until axis_source lands.
        let notch = frame(
            &mut scroll,
            &[axis(15.0), source(AxisSource::Wheel), value120(120)],
        );
        let lines = (TouchPhase::Moved, false, 0.0, -3.0);
        assert_eq!(notch, [lines]);
        let open = frame(&mut scroll, &[axis(UP), source(AxisSource::Finger)]);
        assert_eq!(
            open[0],
            pixels(TouchPhase::Started, 0.0),
            "the next stroke still opens"
        );
        let notch = frame(
            &mut scroll,
            &[axis(15.0), source(AxisSource::Wheel), value120(120)],
        );
        assert_eq!(notch, [lines], "a notch never extends a stroke");
    }

    #[test]
    fn a_discrete_value_scrolls_lines_for_every_source_but_the_finger() {
        let lines = [(TouchPhase::Moved, false, 0.0, -3.0)];
        let sources = [
            AxisSource::Wheel,
            AxisSource::WheelTilt,
            AxisSource::Continuous,
        ];
        for source_kind in sources {
            let mut scroll = ScrollFrame::default();
            let notch = frame(
                &mut scroll,
                &[axis(15.0), source(source_kind), value120(120)],
            );
            assert_eq!(notch, lines, "{source_kind:?}");
        }
    }

    #[test]
    fn a_wheel_frame_without_a_discrete_value_scrolls_its_continuous_value() {
        let mut scroll = ScrollFrame::default();
        let sent = frame(&mut scroll, &[axis(10.0), source(AxisSource::Wheel)]);
        assert_eq!(sent, [pixels(TouchPhase::Moved, -30.0)]);
        let bare = frame(&mut scroll, &[axis(10.0)]);
        assert_eq!(
            bare,
            [pixels(TouchPhase::Moved, -30.0)],
            "no axis_source is a wheel"
        );
    }

    #[test]
    fn a_frame_with_no_scroll_event_sends_nothing() {
        let mut scroll = ScrollFrame::default();
        assert!(scroll.frame().is_empty());
        assert!(!scroll.event(&wl_pointer::Event::Frame, false, -1.0, -1.0));
    }
}
