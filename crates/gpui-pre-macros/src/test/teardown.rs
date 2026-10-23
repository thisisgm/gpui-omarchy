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
        fn __gpui_cleanup(
            failure: &mut Option<Box<dyn std::any::Any + Send>>,
            cleanup: impl FnOnce(),
        ) {
            if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
                if failure.is_none() {
                    *failure = Some(error);
                } else {
                    eprintln!("additional test cleanup panic; preserving first panic");
                    // A secondary panic payload may itself panic when dropped.
                    std::mem::forget(error);
                }
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
