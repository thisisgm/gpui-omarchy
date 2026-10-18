//! Counts a window's frames by `wp_presentation` feedback; only a shown frame advances.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::PresentedFrames;
use wayland_backend::client::ObjectId;

/// One feedback object's surface and frame number, 0 until its present succeeds.
pub(crate) struct FrameFeedbackData {
    pub(crate) surface_id: ObjectId,
    frame: AtomicU64,
}

impl FrameFeedbackData {
    pub(crate) fn new(surface_id: ObjectId) -> Self {
        Self {
            surface_id,
            frame: AtomicU64::new(0),
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
    Presented(Duration),
    Discarded,
}

/// Converts a `presented` event's timestamp triple, saturating on a malformed one.
pub(crate) fn presentation_timestamp(tv_sec_hi: u32, tv_sec_lo: u32, tv_nsec: u32) -> Duration {
    let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
    Duration::from_secs(seconds).saturating_add(Duration::from_nanos(u64::from(tv_nsec)))
}

/// A window's frame counts; frames are numbered when their present succeeds.
#[derive(Debug)]
pub(crate) struct PresentedFrameCounter {
    supported: bool,
    frames: PresentedFrames,
}

impl PresentedFrameCounter {
    pub(crate) fn new(supported: bool) -> Self {
        Self {
            supported,
            frames: PresentedFrames::default(),
        }
    }

    /// Records a present; only one that reached the compositor numbers a frame and its feedback.
    pub(crate) fn finish_present(&mut self, succeeded: bool, feedback: Option<&FrameFeedbackData>) {
        if !succeeded {
            return;
        }
        self.frames.committed += 1;
        if let Some(feedback) = feedback {
            feedback.adopt(self.frames.committed);
        }
    }

    /// Applies feedback for `frame`, returning whether the presented frame advanced.
    pub(crate) fn feedback(&mut self, frame: u64, feedback: FrameFeedback, clock_id: u32) -> bool {
        let FrameFeedback::Presented(presented_at) = feedback else {
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

    pub(crate) fn snapshot(&self) -> Option<PresentedFrames> {
        self.supported.then_some(self.frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONOTONIC: u32 = libc::CLOCK_MONOTONIC as u32;

    fn at(millis: u64) -> FrameFeedback {
        FrameFeedback::Presented(Duration::from_millis(millis))
    }

    // One draw: a feedback object for the commit, then the present's outcome.
    fn draw(counter: &mut PresentedFrameCounter, succeeded: bool) -> FrameFeedbackData {
        let feedback = FrameFeedbackData::new(ObjectId::null());
        counter.finish_present(succeeded, Some(&feedback));
        feedback
    }

    #[test]
    fn presented_feedback_advances_the_counter() {
        let mut counter = PresentedFrameCounter::new(true);
        assert_eq!(counter.snapshot(), Some(PresentedFrames::default()));

        let first = draw(&mut counter, true);
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
        let first = draw(&mut counter, true);
        assert!(counter.feedback(first.frame(), at(16), MONOTONIC));
        let second = draw(&mut counter, true);

        assert!(!counter.feedback(second.frame(), FrameFeedback::Discarded, MONOTONIC));

        let frames = counter.snapshot().unwrap();
        assert_eq!((frames.committed, frames.presented), (2, 1));
        assert_eq!(frames.presented_at, Some(Duration::from_millis(16)));
    }

    #[test]
    fn feedback_of_a_failed_present_does_not_advance() {
        let mut counter = PresentedFrameCounter::new(true);
        let first = draw(&mut counter, true);
        assert!(counter.feedback(first.frame(), at(16), MONOTONIC));

        // A failed present numbers no frame, and a later bare commit may still carry its feedback.
        let failed = draw(&mut counter, false);
        assert_eq!(failed.frame(), 0);
        assert_eq!(counter.snapshot().unwrap().committed, 1);
        assert!(!counter.feedback(failed.frame(), at(33), MONOTONIC));
        assert_eq!(counter.snapshot().unwrap().presented, 1);

        // The retry that succeeds is the next frame, and its own feedback counts it.
        let retry = draw(&mut counter, true);
        assert!(counter.feedback(retry.frame(), at(50), MONOTONIC));
        let frames = counter.snapshot().unwrap();
        assert_eq!((frames.committed, frames.presented), (2, 2));
        assert_eq!(frames.presented_at, Some(Duration::from_millis(50)));
    }

    #[test]
    fn presented_frame_never_moves_back_or_past_the_commits() {
        let mut counter = PresentedFrameCounter::new(true);
        let first = draw(&mut counter, true);
        let second = draw(&mut counter, true);
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
        let mut counter = PresentedFrameCounter::new(false);
        counter.finish_present(true, None);

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
}
