use proc_macro2::{Ident, TokenStream};
use quote::quote;

pub(super) fn context(cx: &Ident, lock: Option<&Ident>) -> TokenStream {
    let release_lock = lock.map(|lock| {
        quote! {
            __gpui_cleanup(&mut __gpui_failure, || drop(#lock));
        }
    });
    quote! {
        #release_lock
        __gpui_cleanup(&mut __gpui_failure, || #cx.run_until_parked());
        __gpui_cleanup(&mut __gpui_failure, || {
            #cx.update(|cx| cx.background_executor().forbid_parking());
        });
        __gpui_cleanup(&mut __gpui_failure, || #cx.quit());
        __gpui_cleanup(&mut __gpui_failure, || #cx.run_until_parked());
        __gpui_cleanup(&mut __gpui_failure, || drop(#cx));
    }
}

pub(super) fn wrap(
    body: TokenStream,
    after_body: TokenStream,
    contexts: TokenStream,
    refcounts: TokenStream,
) -> TokenStream {
    quote! {
        struct __GpuiCleanupFailure {
            _previous: std::mem::ManuallyDrop<Option<Box<dyn std::any::Any + Send>>>,
            _cleanup: std::mem::ManuallyDrop<Box<dyn std::any::Any + Send>>,
        }

        fn __gpui_cleanup(
            failure: &mut Option<Box<dyn std::any::Any + Send>>,
            cleanup: impl FnOnce(),
        ) {
            if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
                // Retain failed cleanup distinctly without running payload destructors.
                let previous = failure.take();
                *failure = Some(Box::new(__GpuiCleanupFailure {
                    _previous: std::mem::ManuallyDrop::new(previous),
                    _cleanup: std::mem::ManuallyDrop::new(error),
                }));
            }
        }

        let mut __gpui_failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #body
        })).err();
        #after_body
        #contexts
        __gpui_cleanup(&mut __gpui_failure, || dispatcher.drain_tasks());
        __gpui_cleanup(&mut __gpui_failure, || drop(dispatcher));
        #refcounts
        if let Some(error) = __gpui_failure {
            std::panic::resume_unwind(error);
        }
    }
}
