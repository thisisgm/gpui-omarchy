use super::*;
use std::{cell::Cell, time::Duration};

#[test]
fn fd_lifetime_registered_sources_release_the_loop_and_common() {
    let event_loop = EventLoop::<()>::try_new().unwrap();
    let handle = event_loop.handle();
    let weak_loop = handle.downgrade();
    let (common, main_receiver, power_receiver) = LinuxCommon::new(event_loop.get_signal());
    let common = Rc::new(RefCell::new(common));
    let weak_common = Rc::downgrade(&common);
    LinuxCommon::register_sources(&common, &handle, main_receiver, power_receiver).unwrap();

    drop(common);
    assert!(
        weak_common.upgrade().is_some(),
        "live sources retain their common state"
    );
    drop(handle);
    drop(event_loop);

    assert!(
        weak_loop.upgrade().is_none(),
        "sources must not own their event loop"
    );
    assert!(
        weak_common.upgrade().is_none(),
        "dropping the owner releases common state"
    );
}

#[test]
fn fd_lifetime_live_foreground_runnable_and_after_hook_still_dispatch() {
    let mut event_loop = EventLoop::<()>::try_new().unwrap();
    let handle = event_loop.handle();
    let (common, main_receiver, power_receiver) = LinuxCommon::new(event_loop.get_signal());
    let common = Rc::new(RefCell::new(common));
    let ran = Rc::new(Cell::new(0));
    let after = Rc::new(Cell::new(0));
    common.borrow_mut().after_runnable = Some(Rc::new({
        let after = after.clone();
        move || after.set(after.get() + 1)
    }));
    LinuxCommon::register_sources(&common, &handle, main_receiver, power_receiver).unwrap();
    let task = common.borrow().foreground_executor.spawn({
        let ran = ran.clone();
        async move { ran.set(ran.get() + 1) }
    });

    event_loop.dispatch(Some(Duration::ZERO), &mut ()).unwrap();

    assert_eq!(
        ran.get(),
        1,
        "the live main queue must execute its runnable"
    );
    assert_eq!(
        after.get(),
        1,
        "the existing after-runnable hook must execute"
    );
    drop(task);
    drop(common);
    drop(handle);
    drop(event_loop);
}
