//! One watcher per device polls each presented frame's fence; `poll(Wait)` would hold the fence lock and stall submits.
use gpui::GpuCompletion;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

/// How often the watcher looks at the fence while any frame is in flight; a target, never a bound.
pub const POLL_INTERVAL: Duration = Duration::from_micros(250);
/// A frame unresolved this long after submission is reported unobserved.
const STALE: Duration = Duration::from_secs(1);
/// Completions a window keeps for draining; older ones are counted as lost.
const RETAINED: usize = 512;
/// Frames in flight beyond this are reported unobserved instead of tracked.
const IN_FLIGHT: usize = 64;

/// A clock the caller names, for example CLOCK_MONOTONIC, and its clockid_t.
#[derive(Clone, Copy)]
pub struct FenceClock {
    pub now: fn() -> Duration,
    pub clock_id: u32,
}

/// One window's completions, kept apart from the device's shared watcher.
#[derive(Default)]
pub struct GpuCompletionSink {
    done: Mutex<VecDeque<GpuCompletion>>,
    lost: AtomicUsize,
}

impl GpuCompletionSink {
    fn push(&self, completion: GpuCompletion) {
        let mut done = self.done.lock();
        if done.len() == RETAINED {
            done.pop_front();
            self.lost.fetch_add(1, Ordering::AcqRel);
        }
        done.push_back(completion);
    }

    /// Completions in resolution order, and how many were dropped since the last drain.
    pub fn drain(&self) -> (Vec<GpuCompletion>, usize) {
        let done = self.done.lock().drain(..).collect();
        (done, self.lost.swap(0, Ordering::AcqRel))
    }
}

struct Pending {
    frame_id: u64,
    submitted_at: Duration,
    resolved: Arc<AtomicBool>,
    sink: Arc<GpuCompletionSink>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Pending {
    /// Exactly one of the fence callback, the stale sweep or the watcher's drop reports a frame.
    fn resolve(
        &self,
        not_before: Option<Duration>,
        completed_at: Option<Duration>,
        clock_id: u32,
    ) -> bool {
        if self.resolved.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.sink.push(GpuCompletion {
            frame_id: self.frame_id,
            submitted_at: self.submitted_at,
            not_before,
            completed_at,
            clock_id,
        });
        (self.wake)();
        true
    }
}

struct Watched {
    clock: FenceClock,
    pending: Mutex<VecDeque<Arc<Pending>>>,
    in_flight: AtomicUsize,
    stopping: AtomicBool,
    /// Polls started so far; a frame is bracketed only by a poll that started after it registered.
    polls: AtomicU64,
    /// The number of the poll before the current one, and when it started in nanoseconds.
    previous_poll: (AtomicU64, AtomicU64),
}

impl Watched {
    fn settle(
        &self,
        pending: &Pending,
        not_before: Option<Duration>,
        completed_at: Option<Duration>,
    ) {
        if pending.resolve(not_before, completed_at, self.clock.clock_id) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Reports stale frames unobserved and forgets resolved ones.
    fn sweep(&self, now: Duration) {
        let mut pending = self.pending.lock();
        for frame in pending.iter() {
            if now.saturating_sub(frame.submitted_at) >= STALE {
                self.settle(frame, None, None);
            }
        }
        pending.retain(|frame| !frame.resolved.load(Ordering::Acquire));
    }
}

/// The device's single watcher; every window rendering on that device shares it.
pub struct GpuFence {
    watched: Arc<Watched>,
    watcher: Option<JoinHandle<()>>,
    watcher_id: ThreadId,
}

impl GpuFence {
    fn spawn(poll: impl Fn() + Send + 'static, clock: FenceClock) -> std::io::Result<Self> {
        let watched = Arc::new(Watched {
            clock,
            pending: Mutex::new(VecDeque::new()),
            in_flight: AtomicUsize::new(0),
            stopping: AtomicBool::new(false),
            polls: AtomicU64::new(0),
            previous_poll: (AtomicU64::new(0), AtomicU64::new(0)),
        });
        let shared = watched.clone();
        let watcher = thread::Builder::new()
            .name("gpui-gpu-fence".into())
            .spawn(move || {
                let mut previous = (0, Duration::ZERO);
                while !shared.stopping.load(Ordering::Acquire) {
                    if shared.in_flight.load(Ordering::Acquire) == 0 {
                        thread::park();
                        continue;
                    }
                    let number = shared.polls.fetch_add(1, Ordering::AcqRel) + 1;
                    let started = (shared.clock.now)();
                    shared.previous_poll.0.store(previous.0, Ordering::Release);
                    let at = previous.1.as_nanos() as u64;
                    shared.previous_poll.1.store(at, Ordering::Release);
                    poll();
                    previous = (number, started);
                    shared.sweep((shared.clock.now)());
                    thread::sleep(POLL_INTERVAL);
                }
            })?;
        let watcher_id = watcher.thread().id();
        Ok(Self {
            watched,
            watcher: Some(watcher),
            watcher_id,
        })
    }

    /// Returns the frame's completion callback, or `None` after reporting it unobserved when too many are in flight.
    fn register(
        &self,
        frame_id: u64,
        submitted_at: Duration,
        sink: &Arc<GpuCompletionSink>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Registered<impl FnOnce() + Send + 'static>> {
        let pending = Arc::new(Pending {
            frame_id,
            submitted_at,
            resolved: Arc::new(AtomicBool::new(false)),
            sink: sink.clone(),
            wake,
        });
        if self.watched.in_flight.load(Ordering::Acquire) >= IN_FLIGHT {
            pending.resolve(None, None, self.watched.clock.clock_id);
            return None;
        }
        self.watched.in_flight.fetch_add(1, Ordering::AcqRel);
        let armed = Arc::new(AtomicU64::new(u64::MAX));
        let registered_after = armed.clone();
        self.watched.pending.lock().push_back(pending.clone());
        if let Some(watcher) = &self.watcher {
            watcher.thread().unpark();
        }
        let watched = self.watched.clone();
        let watcher_id = self.watcher_id;
        let done = move || {
            let completed_at = (watched.clock.now)();
            // Only a watcher poll that started after registration and missed the fence bounds it from below.
            let (number, at) = &watched.previous_poll;
            let not_before = (thread::current().id() == watcher_id
                && number.load(Ordering::Acquire) > registered_after.load(Ordering::Acquire))
            .then(|| Duration::from_nanos(at.load(Ordering::Acquire)));
            watched.settle(&pending, not_before, Some(completed_at));
        };
        Some(Registered { done, armed })
    }

    /// Call right after the frame's last `Queue::submit`; the callback covers all work submitted so far.
    pub fn track(
        &self,
        queue: &wgpu::Queue,
        frame_id: u64,
        submitted_at: Duration,
        sink: &Arc<GpuCompletionSink>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) {
        if let Some(registered) = self.register(frame_id, submitted_at, sink, wake) {
            queue.on_submitted_work_done(registered.done);
            // Armed after the callback is attached, so a poll that could not yet see it never bounds it.
            self.arm(&registered.armed);
        }
    }
}

/// A frame's completion callback and the poll count that arms its lower bound.
struct Registered<F> {
    done: F,
    armed: Arc<AtomicU64>,
}

impl GpuFence {
    fn arm(&self, armed: &AtomicU64) {
        armed.store(
            self.watched.polls.load(Ordering::Acquire),
            Ordering::Release,
        );
    }
}

impl Drop for GpuFence {
    fn drop(&mut self) {
        self.watched.stopping.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            watcher.thread().unpark();
            let _ = watcher.join();
        }
        for frame in self.watched.pending.lock().drain(..) {
            self.watched.settle(&frame, None, None);
        }
    }
}

static FENCES: Mutex<Vec<(Weak<wgpu::Device>, Weak<GpuFence>)>> = Mutex::new(Vec::new());

/// The one watcher for `device`, started on first use.
pub fn fence_for(device: &Arc<wgpu::Device>, clock: FenceClock) -> std::io::Result<Arc<GpuFence>> {
    let mut fences = FENCES.lock();
    fences.retain(|(device, fence)| device.strong_count() > 0 && fence.strong_count() > 0);
    if let Some(fence) = fences
        .iter()
        .find(|(known, _)| {
            known
                .upgrade()
                .is_some_and(|known| Arc::ptr_eq(&known, device))
        })
        .and_then(|(_, fence)| fence.upgrade())
    {
        return Ok(fence);
    }
    let polled = Arc::downgrade(device);
    let fence = Arc::new(GpuFence::spawn(
        move || {
            if let Some(device) = polled.upgrade()
                && let Err(error) = device.poll(wgpu::PollType::Poll)
            {
                log::warn!("GPU fence poll failed: {error:?}");
            }
        },
        clock,
    )?);
    fences.push((Arc::downgrade(device), Arc::downgrade(&fence)));
    Ok(fence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    static NOW_NS: AtomicU64 = AtomicU64::new(0);
    // The fake clock is process-wide, so its tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn clock() -> FenceClock {
        FenceClock {
            now: || Duration::from_nanos(NOW_NS.load(Ordering::Acquire)),
            clock_id: 1,
        }
    }

    fn quiet() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    #[test]
    fn retention_is_bounded_and_counts_what_it_drops() {
        let sink = GpuCompletionSink::default();
        for frame_id in 1..=(RETAINED as u64 + 3) {
            sink.push(GpuCompletion {
                frame_id,
                submitted_at: Duration::ZERO,
                not_before: None,
                completed_at: None,
                clock_id: 1,
            });
        }
        let (done, lost) = sink.drain();
        assert_eq!((done.len(), done[0].frame_id, lost), (RETAINED, 4, 3));
    }

    #[test]
    fn a_fence_seen_by_the_watcher_is_bracketed_and_reported_once() {
        let _turn = SERIAL.lock();
        let (queue, callbacks) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock();
        let polled = gate.clone();
        let aged: Vec<(u32, Box<dyn FnOnce() + Send>)> = Vec::new();
        let callbacks = Mutex::new((callbacks, aged));
        // The fence signals only after two polls have looked at the attached callback and missed it.
        let fence = GpuFence::spawn(
            move || {
                let _gate = polled.lock();
                let (incoming, aged) = &mut *callbacks.lock();
                aged.extend(incoming.try_iter().map(|done| (0, done)));
                for (age, _) in aged.iter_mut() {
                    *age += 1;
                }
                let (due, waiting) = std::mem::take(aged)
                    .into_iter()
                    .partition(|(age, _)| *age > 2);
                *aged = waiting;
                for (_, done) in due {
                    done();
                }
            },
            clock(),
        )
        .unwrap();
        let sink = Arc::new(GpuCompletionSink::default());
        NOW_NS.store(5_000_000, Ordering::Release);
        let registered = fence
            .register(7, Duration::from_millis(4), &sink, quiet())
            .unwrap();
        queue.send(Box::new(registered.done)).unwrap();
        fence.arm(&registered.armed);
        drop(held);
        let completion = loop {
            if let Some(completion) = sink.drain().0.pop() {
                break completion;
            }
            thread::sleep(POLL_INTERVAL);
        };
        assert_eq!(completion.frame_id, 7);
        assert_eq!(completion.completed_at, Some(Duration::from_millis(5)));
        assert_eq!(
            completion.not_before,
            Some(Duration::from_millis(5)),
            "a poll after arming missed it"
        );
        thread::sleep(POLL_INTERVAL * 8);
        assert!(
            sink.drain().0.is_empty(),
            "the stale sweep never repeats a resolved frame"
        );
    }

    #[test]
    fn a_poll_already_running_when_armed_never_bounds_the_frame() {
        let _turn = SERIAL.lock();
        let (queue, callbacks) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock();
        let polled = gate.clone();
        let aged: Vec<(u32, Box<dyn FnOnce() + Send>)> = Vec::new();
        let callbacks = Mutex::new((callbacks, aged));
        // Poll 1 is held inside poll() across the attach and the arm; poll 2 then sees the fence.
        let fence = GpuFence::spawn(
            move || {
                let _gate = polled.lock();
                let (incoming, aged) = &mut *callbacks.lock();
                aged.extend(incoming.try_iter().map(|done| (0, done)));
                for (age, _) in aged.iter_mut() {
                    *age += 1;
                }
                let (due, waiting) = std::mem::take(aged)
                    .into_iter()
                    .partition(|(age, _)| *age > 1);
                *aged = waiting;
                for (_, done) in due {
                    done();
                }
            },
            clock(),
        )
        .unwrap();
        let sink = Arc::new(GpuCompletionSink::default());
        NOW_NS.store(5_000_000, Ordering::Release);
        let registered = fence
            .register(9, Duration::from_millis(4), &sink, quiet())
            .unwrap();
        while fence.watched.polls.load(Ordering::Acquire) == 0 {
            thread::sleep(POLL_INTERVAL);
        }
        queue.send(Box::new(registered.done)).unwrap();
        fence.arm(&registered.armed);
        assert_eq!(
            registered.armed.load(Ordering::Acquire),
            1,
            "poll 1 is still inside poll()"
        );
        drop(held);
        let completion = loop {
            if let Some(completion) = sink.drain().0.pop() {
                break completion;
            }
            thread::sleep(POLL_INTERVAL);
        };
        assert_eq!(
            completion.not_before, None,
            "poll 1 started before the callback was attached"
        );
    }

    #[test]
    fn a_fence_seen_before_arming_has_no_lower_bound() {
        let _turn = SERIAL.lock();
        let (queue, callbacks) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let callbacks = Mutex::new(callbacks);
        let fence = GpuFence::spawn(
            move || {
                while let Ok(done) = callbacks.lock().try_recv() {
                    done();
                }
            },
            clock(),
        )
        .unwrap();
        let sink = Arc::new(GpuCompletionSink::default());
        NOW_NS.store(5_000_000, Ordering::Release);
        let registered = fence
            .register(8, Duration::from_millis(4), &sink, quiet())
            .unwrap();
        // Never armed: a callback that fires before track arms it has nothing that bounds it.
        queue.send(Box::new(registered.done)).unwrap();
        let completion = loop {
            if let Some(completion) = sink.drain().0.pop() {
                break completion;
            }
            thread::sleep(POLL_INTERVAL);
        };
        assert_eq!(
            (completion.not_before, completion.completed_at),
            (None, Some(Duration::from_millis(5)))
        );
    }

    #[test]
    fn a_callback_outside_the_watcher_has_no_lower_bound() {
        let _turn = SERIAL.lock();
        let fence = GpuFence::spawn(|| {}, clock()).unwrap();
        let sink = Arc::new(GpuCompletionSink::default());
        NOW_NS.store(9_000_000, Ordering::Release);
        let registered = fence
            .register(3, Duration::from_millis(8), &sink, quiet())
            .unwrap();
        fence.arm(&registered.armed);
        (registered.done)();
        let (done, _) = sink.drain();
        assert_eq!(
            (done[0].not_before, done[0].completed_at),
            (None, Some(Duration::from_millis(9)))
        );
    }

    #[test]
    fn stale_overflowing_and_dropped_frames_are_reported_unobserved_never_lost() {
        let _turn = SERIAL.lock();
        let fence = GpuFence::spawn(|| {}, clock()).unwrap();
        let sink = Arc::new(GpuCompletionSink::default());
        NOW_NS.store(0, Ordering::Release);
        let late = fence
            .register(1, Duration::ZERO, &sink, quiet())
            .unwrap()
            .done;
        NOW_NS.store(STALE.as_nanos() as u64, Ordering::Release);
        while sink.drain().0.iter().all(|c| c.frame_id != 1) {
            thread::sleep(POLL_INTERVAL);
        }
        late();
        assert!(
            sink.drain().0.is_empty(),
            "a callback after the stale report is not a second report"
        );
        let mut held = Vec::new();
        for frame_id in 10..10 + IN_FLIGHT as u64 {
            held.push(
                fence
                    .register(frame_id, (clock().now)(), &sink, quiet())
                    .unwrap(),
            );
        }
        assert!(
            fence
                .register(99, (clock().now)(), &sink, quiet())
                .is_none()
        );
        drop(fence);
        let (done, lost) = sink.drain();
        assert_eq!((done.len(), lost), (IN_FLIGHT + 1, 0));
        assert!(done.iter().all(|c| c.completed_at.is_none()));
    }
}
