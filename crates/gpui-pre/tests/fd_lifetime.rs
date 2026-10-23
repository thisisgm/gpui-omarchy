use gpui::{App, Context, IntoElement, Render, TestAppContext, Window};
use std::{
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};

thread_local! {
    static QUIT_HOOKS: Cell<usize> = const { Cell::new(0) };
    static SHUTDOWNS: Cell<usize> = const { Cell::new(0) };
    static ROOT_DROPS: Cell<usize> = const { Cell::new(0) };
    static CLEANUP_TASKS: Cell<usize> = const { Cell::new(0) };
    static APPS_RELEASED: RefCell<Vec<Box<dyn Fn() -> bool>>> = const { RefCell::new(Vec::new()) };
}

struct Root;

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        ROOT_DROPS.with(|count| count.set(count.get() + 1));
    }
}

fn reset() {
    QUIT_HOOKS.with(|count| count.set(0));
    SHUTDOWNS.with(|count| count.set(0));
    ROOT_DROPS.with(|count| count.set(0));
    CLEANUP_TASKS.with(|count| count.set(0));
    APPS_RELEASED.with(|apps| apps.borrow_mut().clear());
}

fn observe_shutdown(cx: &mut App) {
    cx.on_app_quit(|_| async {
        SHUTDOWNS.with(|count| count.set(count.get() + 1));
    })
    .detach();
}

fn retain_visual(cx: &mut TestAppContext) {
    let weak = Rc::downgrade(&cx.app);
    APPS_RELEASED.with(|apps| {
        apps.borrow_mut()
            .push(Box::new(move || weak.upgrade().is_none()))
    });
    let executor = cx.foreground_executor.clone();
    cx.on_quit(move || {
        QUIT_HOOKS.with(|count| count.set(count.get() + 1));
        executor
            .spawn(async {
                CLEANUP_TASKS.with(|count| count.set(count.get() + 1));
            })
            .detach();
    });
    cx.update(observe_shutdown);
    let (view, _) = cx.add_window_view(|_, _| Root);
    drop(view);
}

fn assert_contexts_released(count: usize) {
    QUIT_HOOKS.with(|value| assert_eq!(value.get(), count, "actual TestAppContext::quit ran"));
    SHUTDOWNS.with(|value| assert_eq!(value.get(), count, "actual App::shutdown ran"));
    ROOT_DROPS.with(|value| assert_eq!(value.get(), count, "window roots were reclaimed"));
    CLEANUP_TASKS.with(|value| assert_eq!(value.get(), count, "teardown dispatch still ran"));
    APPS_RELEASED.with(|apps| {
        let apps = apps.borrow();
        assert_eq!(apps.len(), count);
        assert!(
            apps.iter().all(|released| released()),
            "retained visual contexts released their Apps"
        );
    });
}

fn assert_original_panic(result: std::thread::Result<()>) {
    let error = result.expect_err("the original body panic must escape the macro");
    assert_eq!(
        error.downcast_ref::<&str>(),
        Some(&"fd-lifetime-body-panic")
    );
}

#[gpui::test]
fn normal_context_fixture(cx: &mut TestAppContext) {
    reset();
    retain_visual(cx);
}

#[test]
fn fd_lifetime_sync_macro_quits_and_releases_a_retained_visual_context() {
    normal_context_fixture();
    assert_contexts_released(1);
}

#[gpui::test]
#[should_panic(expected = "fd-lifetime-body-panic")]
fn panic_context_fixture(cx: &mut TestAppContext) {
    reset();
    retain_visual(cx);
    panic!("fd-lifetime-body-panic");
}

#[test]
fn fd_lifetime_sync_macro_cleans_up_and_preserves_the_body_panic() {
    assert_original_panic(catch_unwind(AssertUnwindSafe(panic_context_fixture)));
    assert_contexts_released(1);
}

#[gpui::test]
fn normal_app_fixture(cx: &mut App) {
    reset();
    observe_shutdown(cx);
}

#[test]
fn fd_lifetime_app_argument_shuts_down_after_its_borrow_is_released() {
    normal_app_fixture();
    SHUTDOWNS.with(|value| assert_eq!(value.get(), 1));
}

#[gpui::test]
#[should_panic(expected = "fd-lifetime-body-panic")]
fn panic_app_fixture(cx: &mut App) {
    reset();
    observe_shutdown(cx);
    panic!("fd-lifetime-body-panic");
}

#[test]
fn fd_lifetime_app_argument_shuts_down_on_unwind_without_a_borrow_panic() {
    assert_original_panic(catch_unwind(AssertUnwindSafe(panic_app_fixture)));
    SHUTDOWNS.with(|value| assert_eq!(value.get(), 1));
}

#[gpui::test]
async fn normal_async_fixture(cx: &mut TestAppContext) {
    reset();
    retain_visual(cx);
    futures::future::ready(()).await;
}

#[test]
fn fd_lifetime_async_macro_quits_and_releases_a_retained_visual_context() {
    normal_async_fixture();
    assert_contexts_released(1);
}

#[gpui::test]
#[should_panic(expected = "fd-lifetime-body-panic")]
async fn panic_async_fixture(cx: &mut TestAppContext) {
    reset();
    retain_visual(cx);
    futures::future::ready(()).await;
    panic!("fd-lifetime-body-panic");
}

#[test]
fn fd_lifetime_async_macro_cleans_up_and_preserves_the_body_panic() {
    assert_original_panic(catch_unwind(AssertUnwindSafe(panic_async_fixture)));
    assert_contexts_released(1);
}

#[gpui::test]
#[should_panic(expected = "fd-lifetime-body-panic")]
fn multiple_context_fixture(first: &mut TestAppContext, second: &mut TestAppContext) {
    reset();
    retain_visual(first);
    retain_visual(second);
    panic!("fd-lifetime-body-panic");
}

#[test]
fn fd_lifetime_macro_cleans_up_every_context_on_body_panic() {
    assert_original_panic(catch_unwind(AssertUnwindSafe(multiple_context_fixture)));
    assert_contexts_released(2);
}

#[gpui::test]
#[should_panic]
fn cleanup_without_body_fixture(first: &mut TestAppContext, second: &mut TestAppContext) {
    reset();
    first.on_quit(|| panic!("fd-lifetime-cleanup-panic"));
    second.on_quit(|| QUIT_HOOKS.with(|count| count.set(count.get() + 1)));
    second.update(observe_shutdown);
}

#[test]
fn fd_lifetime_cleanup_failure_still_shuts_down_the_other_context() {
    assert!(catch_unwind(AssertUnwindSafe(cleanup_without_body_fixture)).is_err());
    QUIT_HOOKS.with(|count| assert_eq!(count.get(), 1, "later context quit hook ran"));
    SHUTDOWNS.with(|count| assert_eq!(count.get(), 1, "later App shutdown ran"));
}

#[gpui::test]
#[should_panic(expected = "fd-lifetime-body-panic")]
fn expected_body_cleanup_fixture(first: &mut TestAppContext, second: &mut TestAppContext) {
    if std::env::var_os("GPUI_FD_CLEANUP_NEGATIVE_CHILD").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        first.on_quit(|| panic!("fd-lifetime-cleanup-panic"));
        second.on_quit(|| eprintln!("fd-lifetime-second-context-cleaned"));
    }
    panic!("fd-lifetime-body-panic");
}

#[test]
fn fd_lifetime_expected_body_does_not_accept_cleanup_failure() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "expected_body_cleanup_fixture",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GPUI_FD_CLEANUP_NEGATIVE_CHILD", "1")
        .output()
        .expect("the same test binary starts the exact negative fixture");
    let body = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "matching expected body panic hid cleanup failure:\n{body}"
    );
    assert!(
        body.contains("fd-lifetime-second-context-cleaned"),
        "remaining teardown was skipped:\n{body}"
    );
    assert!(
        body.contains("test result: FAILED. 0 passed; 1 failed;"),
        "the exact expected-panic fixture must fail:\n{body}"
    );
}
