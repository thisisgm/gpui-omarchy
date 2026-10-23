use super::*;
use gpui::{BackgroundExecutor, Task};
use std::{
    future::Future,
    sync::{Arc, mpsc},
};

struct RunnableQueue(mpsc::Sender<RunnableVariant>);

impl PlatformDispatcher for RunnableQueue {
    fn is_main_thread(&self) -> bool {
        true
    }

    fn dispatch(&self, runnable: RunnableVariant, _: Priority) {
        assert!(
            self.0.send(runnable).is_ok(),
            "the runnable receiver is alive"
        );
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        self.dispatch(runnable, priority);
    }

    fn dispatch_after(&self, _: Duration, _: RunnableVariant) {
        panic!("the recording dispatcher must not schedule timers");
    }

    fn spawn_realtime(&self, _: Box<dyn FnOnce() + Send>) {
        panic!("the recording dispatcher must not start threads");
    }
}

fn runnable(future: impl Future<Output = ()> + Send + 'static) -> (RunnableVariant, Task<()>) {
    let (sender, receiver) = mpsc::channel();
    let executor = BackgroundExecutor::new(Arc::new(RunnableQueue(sender)));
    let task = executor.spawn(future);
    (
        receiver.recv().expect("executor scheduled the future"),
        task,
    )
}

fn live_timer_barrier(dispatcher: &LinuxDispatcher) {
    let (sender, receiver) = mpsc::channel();
    let (runnable, task) = runnable(async move {
        sender.send(()).unwrap();
    });
    dispatcher.dispatch_after(Duration::ZERO, runnable);
    receiver.recv().expect("a live zero-delay timer dispatches");
    drop(task);
}

#[test]
fn fd_lifetime_timer_loop_releases_pending_runnables_after_sender_close() {
    let (main_sender, _main_receiver) = PriorityQueueCalloopReceiver::new();
    let dispatcher = LinuxDispatcher::new(main_sender);
    let lifetime = Arc::new(());
    let weak_lifetime = Arc::downgrade(&lifetime);
    let (runnable, task) = runnable(async move {
        let _lifetime = lifetime;
    });
    dispatcher.dispatch_after(Duration::from_secs(86_400), runnable);

    // FIFO insertion plus a real zero-delay callback proves the pending timer was installed.
    live_timer_barrier(&dispatcher);
    assert!(
        weak_lifetime.upgrade().is_some(),
        "a pending timer owns its runnable"
    );
    let LinuxDispatcher {
        main_sender,
        timer_sender,
        background_sender,
        _background_threads: threads,
        ..
    } = dispatcher;
    drop(main_sender);
    drop(timer_sender);
    drop(background_sender);
    for thread in threads {
        thread.join().expect("dispatcher worker stopped normally");
    }

    assert!(
        weak_lifetime.upgrade().is_none(),
        "timer loop destruction drops pending work"
    );
    drop(task);
}

#[test]
fn fd_lifetime_live_timer_dispatches_before_shutdown() {
    let (main_sender, _main_receiver) = PriorityQueueCalloopReceiver::new();
    let dispatcher = LinuxDispatcher::new(main_sender);
    live_timer_barrier(&dispatcher);
    let LinuxDispatcher {
        main_sender,
        timer_sender,
        background_sender,
        _background_threads: threads,
        ..
    } = dispatcher;
    drop(main_sender);
    drop(timer_sender);
    drop(background_sender);
    for thread in threads {
        thread.join().expect("dispatcher worker stopped normally");
    }
}
