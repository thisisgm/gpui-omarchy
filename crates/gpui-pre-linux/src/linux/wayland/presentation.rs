//! Counts a window's frames by `wp_presentation` feedback; only a shown frame advances.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui::{FrameOutcome, FrameTerminal, PresentationTime, PresentedFrames};
use wayland_backend::client::ObjectId;
use wayland_client::Proxy;
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;

/// One feedback object's surface and frame number, 0 until its present succeeds.
pub(crate) struct FrameFeedbackData {
    pub(crate) surface_id: ObjectId,
    frame: AtomicU64,
    terminal_consumed: AtomicBool,
}

impl FrameFeedbackData {
    pub(crate) fn new(surface_id: ObjectId) -> Self {
        Self {
            surface_id,
            frame: AtomicU64::new(0),
            terminal_consumed: AtomicBool::new(false),
        }
    }

    /// Names the frame whose successful present committed this feedback.
    pub(crate) fn adopt(&self, frame: u64) {
        self.frame.store(frame, Ordering::Relaxed);
    }

    pub(crate) fn frame(&self) -> u64 {
        self.frame.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameFeedback {
    Presented { at: Duration, valid: bool },
    Discarded,
}

impl FrameFeedback {
    /// Retains legacy snapshot conversion while validating the additive timestamp.
    pub(crate) fn presented(hi: u32, lo: u32, nanos: u32) -> Self {
        Self::Presented {
            at: presentation_timestamp(hi, lo, nanos),
            valid: nanos < 1_000_000_000,
        }
    }
}

/// Converts a `presented` event's timestamp triple, saturating on a malformed one.
pub(crate) fn presentation_timestamp(tv_sec_hi: u32, tv_sec_lo: u32, tv_nsec: u32) -> Duration {
    let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
    Duration::from_secs(seconds).saturating_add(Duration::from_nanos(u64::from(tv_nsec)))
}

/// A feedback object the counter numbers; the protocol object in a window.
pub(crate) trait FeedbackObject {
    fn feedback_data(&self) -> Option<&FrameFeedbackData>;
}

impl FeedbackObject for wp_presentation_feedback::WpPresentationFeedback {
    fn feedback_data(&self) -> Option<&FrameFeedbackData> {
        self.data()
    }
}

/// The counter a Wayland window keeps.
pub(crate) type WindowFrameCounter =
    PresentedFrameCounter<wp_presentation_feedback::WpPresentationFeedback>;

/// A window's frame counts; frames are numbered when their present succeeds.
pub(crate) struct PresentedFrameCounter<F> {
    supported: bool,
    frames: PresentedFrames,
    // Feedback a failed present left uncommitted, so the next present reuses it.
    uncommitted: Option<F>,
}

impl<F: FeedbackObject> PresentedFrameCounter<F> {
    pub(crate) fn new(supported: bool) -> Self {
        Self {
            supported,
            frames: PresentedFrames::default(),
            uncommitted: None,
        }
    }

    /// The feedback for the next present: the one a failed present kept, else a new `request`.
    pub(crate) fn feedback_for_present(&mut self, request: impl FnOnce() -> F) -> F {
        self.uncommitted.take().unwrap_or_else(request)
    }

    /// Records a present: success numbers a frame and its feedback, failure keeps the feedback.
    pub(crate) fn finish_present(&mut self, succeeded: bool, feedback: Option<F>) -> Option<u64> {
        if !succeeded {
            self.uncommitted = feedback;
            return None;
        }
        self.frames.committed += 1;
        if let Some(data) = feedback.as_ref().and_then(FeedbackObject::feedback_data) {
            data.adopt(self.frames.committed);
            return self.supported.then_some(self.frames.committed);
        }
        None
    }

    /// A commit without a present carried any kept feedback, which stays frame 0.
    pub(crate) fn surface_committed(&mut self) {
        self.uncommitted = None;
    }

    /// Applies feedback for `frame`, returning whether the presented frame advanced.
    pub(crate) fn feedback(&mut self, frame: u64, feedback: FrameFeedback, clock_id: u32) -> bool {
        let FrameFeedback::Presented {
            at: presented_at, ..
        } = feedback
        else {
            return false;
        };
        // Frame 0 is feedback whose present failed, so its commit carried older content.
        if frame == 0 || frame <= self.frames.presented || frame > self.frames.committed {
            return false;
        }
        self.frames.presented = frame;
        self.frames.presented_at = Some(presented_at);
        self.frames.clock_id = clock_id;
        true
    }

    /// Claims each associated terminal once, independently of newest-frame advancement.
    pub(crate) fn terminal(
        &self,
        data: &FrameFeedbackData,
        feedback: FrameFeedback,
        clock_id: Option<u32>,
    ) -> Option<FrameTerminal> {
        let frame = data.frame();
        if !self.supported || frame == 0 || frame > self.frames.committed {
            return None;
        }
        if data.terminal_consumed.swap(true, Ordering::Relaxed) {
            return None;
        }
        let outcome = match feedback {
            FrameFeedback::Presented { at, valid } => FrameOutcome::Presented(PresentationTime {
                timestamp: valid.then_some(at),
                clock_id,
            }),
            FrameFeedback::Discarded => FrameOutcome::Discarded,
        };
        Some(FrameTerminal {
            frame_id: frame,
            outcome,
        })
    }

    pub(crate) fn snapshot(&self) -> Option<PresentedFrames> {
        self.supported.then_some(self.frames)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    const MONOTONIC: u32 = libc::CLOCK_MONOTONIC as u32;

    impl FeedbackObject for Rc<FrameFeedbackData> {
        fn feedback_data(&self) -> Option<&FrameFeedbackData> {
            Some(self)
        }
    }

    fn at(millis: u64) -> FrameFeedback {
        FrameFeedback::Presented {
            at: Duration::from_millis(millis),
            valid: true,
        }
    }

    // One draw as the window does it: the feedback its commit carries, then the present's outcome.
    fn draw(
        counter: &mut PresentedFrameCounter<Rc<FrameFeedbackData>>,
        requests: &Cell<usize>,
        succeeded: bool,
    ) -> Rc<FrameFeedbackData> {
        let feedback = counter.feedback_for_present(|| {
            requests.set(requests.get() + 1);
            Rc::new(FrameFeedbackData::new(ObjectId::null()))
        });
        let _ = counter.finish_present(succeeded, Some(feedback.clone()));
        feedback
    }

    #[test]
    fn presented_feedback_advances_the_counter() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);
        assert_eq!(counter.snapshot(), Some(PresentedFrames::default()));

        let first = draw(&mut counter, &requests, true);
        assert!(counter.feedback(first.frame(), at(16), MONOTONIC));
        assert_eq!(
            counter.snapshot(),
            Some(PresentedFrames {
                committed: 1,
                presented: 1,
                presented_at: Some(Duration::from_millis(16)),
                clock_id: MONOTONIC,
            })
        );
    }

    #[test]
    fn discarded_feedback_does_not_advance() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);
        let first = draw(&mut counter, &requests, true);
        assert!(counter.feedback(first.frame(), at(16), MONOTONIC));
        let second = draw(&mut counter, &requests, true);

        assert!(!counter.feedback(second.frame(), FrameFeedback::Discarded, MONOTONIC));

        let frames = counter.snapshot().unwrap();
        assert_eq!((frames.committed, frames.presented), (2, 1));
        assert_eq!(frames.presented_at, Some(Duration::from_millis(16)));
    }

    #[test]
    fn feedback_of_a_failed_present_does_not_advance() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);
        let first = draw(&mut counter, &requests, true);
        assert!(counter.feedback(first.frame(), at(16), MONOTONIC));

        // A failed present numbers no frame; a bare commit then carries its feedback as frame 0.
        let failed = draw(&mut counter, &requests, false);
        counter.surface_committed();
        assert_eq!(failed.frame(), 0);
        assert_eq!(counter.snapshot().unwrap().committed, 1);
        assert!(!counter.feedback(failed.frame(), at(33), MONOTONIC));
        assert_eq!(counter.snapshot().unwrap().presented, 1);

        // The retry that succeeds is the next frame, and its own new feedback counts it.
        let retry = draw(&mut counter, &requests, true);
        assert!(!Rc::ptr_eq(&failed, &retry));
        assert_eq!(failed.frame(), 0);
        assert!(counter.feedback(retry.frame(), at(50), MONOTONIC));
        let frames = counter.snapshot().unwrap();
        assert_eq!((frames.committed, frames.presented), (2, 2));
        assert_eq!(frames.presented_at, Some(Duration::from_millis(50)));
        assert_eq!(requests.get(), 3);
    }

    #[test]
    fn failed_presents_without_a_commit_reuse_one_feedback() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);

        // Retries before the first present commit nothing, so one feedback waits for the commit.
        let failed = draw(&mut counter, &requests, false);
        for _ in 0..4 {
            assert!(Rc::ptr_eq(&failed, &draw(&mut counter, &requests, false)));
        }
        let presented = draw(&mut counter, &requests, true);

        assert!(Rc::ptr_eq(&failed, &presented));
        assert_eq!(requests.get(), 1);
        assert_eq!(presented.frame(), 1);
        assert!(counter.feedback(presented.frame(), at(16), MONOTONIC));
        assert_eq!(counter.snapshot().unwrap().presented, 1);

        // A presented frame leaves nothing kept, so the next frame requests its own feedback.
        let next = draw(&mut counter, &requests, true);
        assert!(!Rc::ptr_eq(&presented, &next));
        assert_eq!((requests.get(), next.frame()), (2, 2));
    }

    #[test]
    fn presented_frame_never_moves_back_or_past_the_commits() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);
        let first = draw(&mut counter, &requests, true);
        let second = draw(&mut counter, &requests, true);
        assert!(counter.feedback(second.frame(), at(33), MONOTONIC));

        assert!(!counter.feedback(first.frame(), at(16), MONOTONIC));
        assert!(!counter.feedback(second.frame(), at(50), MONOTONIC));
        assert!(!counter.feedback(second.frame() + 1, at(66), MONOTONIC));

        let frames = counter.snapshot().unwrap();
        assert_eq!(frames.presented, second.frame());
        assert_eq!(frames.presented_at, Some(Duration::from_millis(33)));
    }

    #[test]
    fn unsupported_compositor_reports_no_feedback() {
        let mut counter = PresentedFrameCounter::<Rc<FrameFeedbackData>>::new(false);
        let _ = counter.finish_present(true, None);

        assert_eq!(counter.snapshot(), None);
    }

    #[test]
    fn timestamp_combines_seconds_and_saturates() {
        assert_eq!(
            presentation_timestamp(1, 2, 3),
            Duration::new((1 << 32) + 2, 3)
        );
        assert_eq!(
            presentation_timestamp(u32::MAX, u32::MAX, u32::MAX),
            Duration::MAX
        );
    }

    #[test]
    fn terminal_feedback_is_per_id_out_of_order_and_once() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);
        let first = draw(&mut counter, &requests, true);
        let second = draw(&mut counter, &requests, true);
        let discarded = draw(&mut counter, &requests, true);
        assert_eq!(
            counter.terminal(&second, at(33), Some(MONOTONIC)),
            Some(FrameTerminal {
                frame_id: second.frame(),
                outcome: FrameOutcome::Presented(PresentationTime {
                    timestamp: Some(Duration::from_millis(33)),
                    clock_id: Some(MONOTONIC)
                }),
            })
        );
        assert!(counter.feedback(second.frame(), at(33), MONOTONIC));
        assert_eq!(
            counter
                .terminal(&first, at(16), Some(MONOTONIC))
                .unwrap()
                .frame_id,
            first.frame()
        );
        assert!(!counter.feedback(first.frame(), at(16), MONOTONIC));
        assert_eq!(
            counter.terminal(&discarded, FrameFeedback::Discarded, None),
            Some(FrameTerminal {
                frame_id: discarded.frame(),
                outcome: FrameOutcome::Discarded,
            })
        );
        assert!(!counter.feedback(discarded.frame(), FrameFeedback::Discarded, MONOTONIC));
        for data in [&first, &second, &discarded] {
            assert_eq!(counter.terminal(data, at(99), Some(MONOTONIC)), None);
            assert_eq!(counter.terminal(data, FrameFeedback::Discarded, None), None);
        }
        assert_eq!(counter.snapshot().unwrap().presented, second.frame());
    }

    #[test]
    fn terminal_feedback_zero_future_and_failed_bare_commit_are_not_consumed() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);
        let failed = draw(&mut counter, &requests, false);
        assert_eq!(failed.frame(), 0);
        assert_eq!(counter.terminal(&failed, at(1), None), None);
        counter.surface_committed();
        assert_eq!(
            counter.terminal(&failed, FrameFeedback::Discarded, None),
            None
        );
        let issued = draw(&mut counter, &requests, true);
        let future = FrameFeedbackData::new(ObjectId::null());
        future.adopt(issued.frame() + 1);
        assert_eq!(counter.terminal(&future, at(2), None), None);
        // Identity rejection must happen before the object's once-only claim.
        future.adopt(issued.frame());
        assert!(counter.terminal(&future, at(2), None).is_some());
        assert_eq!(counter.terminal(&future, at(2), None), None);
        assert!(!failed.terminal_consumed.load(Ordering::Relaxed));
    }

    #[test]
    fn terminal_feedback_receipt_requires_success_and_associated_feedback() {
        let mut counter = PresentedFrameCounter::new(true);
        let feedback = Rc::new(FrameFeedbackData::new(ObjectId::null()));
        assert_eq!(counter.finish_present(false, Some(feedback.clone())), None);
        let kept = counter.feedback_for_present(|| panic!("failed present must reuse feedback"));
        assert!(Rc::ptr_eq(&feedback, &kept));
        assert_eq!(counter.finish_present(true, Some(kept)), Some(1));
        assert_eq!(feedback.frame(), 1);
        assert_eq!(counter.finish_present(true, None), None);
        assert_eq!(counter.snapshot().unwrap().committed, 2);
        let mut unsupported = PresentedFrameCounter::new(false);
        assert_eq!(
            unsupported.finish_present(true, Some(feedback.clone())),
            None
        );
        assert_eq!(unsupported.terminal(&feedback, at(3), None), None);
        assert_eq!(unsupported.snapshot(), None);
    }

    #[test]
    fn terminal_feedback_clock_is_announced_and_timestamp_is_validated() {
        let mut counter = PresentedFrameCounter::new(true);
        let requests = Cell::new(0);
        for clock_id in [None, Some(0), Some(MONOTONIC)] {
            for nanos in [999_999_999, 1_000_000_000, u32::MAX] {
                let data = draw(&mut counter, &requests, true);
                let feedback = FrameFeedback::presented(1, 2, nanos);
                let expected = (nanos < 1_000_000_000).then(|| Duration::new((1 << 32) + 2, nanos));
                assert_eq!(
                    counter.terminal(&data, feedback, clock_id),
                    Some(FrameTerminal {
                        frame_id: data.frame(),
                        outcome: FrameOutcome::Presented(PresentationTime {
                            timestamp: expected,
                            clock_id
                        }),
                    })
                );
            }
        }
        let data = draw(&mut counter, &requests, true);
        let malformed = FrameFeedback::presented(u32::MAX, u32::MAX, u32::MAX);
        // Legacy snapshot conversion remains saturating, while the new payload refuses it.
        assert!(counter.feedback(data.frame(), malformed, MONOTONIC));
        assert_eq!(
            counter.snapshot().unwrap().presented_at,
            Some(Duration::MAX)
        );
        assert_eq!(
            counter.terminal(&data, malformed, None).unwrap().outcome,
            FrameOutcome::Presented(PresentationTime {
                timestamp: None,
                clock_id: None
            })
        );
    }
}
