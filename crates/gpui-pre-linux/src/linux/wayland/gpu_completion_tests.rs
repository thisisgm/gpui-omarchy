use std::{cell::RefCell, rc::Rc, time::Duration};

use gpui::{
    App, Application, Bounds, Context, FrameFeedbackEvent, GpuCompletion, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};

const FRAMES: usize = 120;

#[derive(Default)]
struct Proof {
    submitted: Vec<u64>,
    completed: Vec<GpuCompletion>,
}

struct Probe(usize);

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.0 += 1;
        div().size_full().bg(rgb(if self.0.is_multiple_of(2) {
            0x234567
        } else {
            0x345678
        }))
    }
}

#[test]
#[ignore = "requires a native Wayland compositor and GPU; run with --ignored --nocapture"]
fn gpu_fence_completion_during_native_frame_present() {
    run_probe(false);
}

#[test]
#[ignore = "requires a native Wayland compositor and GPU; run with --ignored --nocapture"]
fn gpu_fence_registration_during_native_frame_request() {
    run_probe(true);
}

fn run_probe(register_during_frame: bool) {
    assert!(std::env::var_os("WAYLAND_DISPLAY").is_some());
    let proof = Rc::new(RefCell::new(Proof::default()));
    Application::new_inaccessible(crate::current_platform(false)).run({
        let proof = proof.clone();
        move |cx: &mut App| {
            cx.open_window(
                WindowOptions {
                    app_id: Some("gpu-completion-probe".into()),
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(320.0), px(240.0)),
                        cx,
                    ))),
                    ..Default::default()
                },
                |window, cx| {
                    window
                        .observe_frame_feedback(cx, {
                            let proof = proof.clone();
                            move |event, window, _| {
                                if let FrameFeedbackEvent::Submitted { frame_id, .. } = event {
                                    let mut proof = proof.borrow_mut();
                                    assert_eq!(frame_id, proof.submitted.len() as u64 + 1);
                                    proof.submitted.push(frame_id);
                                    if proof.submitted.len() < FRAMES {
                                        window.on_next_frame(|window, _| window.refresh());
                                    }
                                }
                            }
                        })
                        .expect("native compositor must support presentation feedback")
                        .detach();
                    let register = {
                        let proof = proof.clone();
                        move |window: &mut Window, cx: &mut App| {
                            window
                                .observe_gpu_completion(cx, move |completion, _, cx| {
                                    let mut proof = proof.borrow_mut();
                                    assert!(proof.submitted.contains(&completion.frame_id));
                                    assert!(
                                        !proof.completed.iter().any(
                                            |previous| previous.frame_id == completion.frame_id
                                        ),
                                        "each receipt must complete only once"
                                    );
                                    assert_eq!(completion.clock_id, libc::CLOCK_MONOTONIC as u32);
                                    if let Some(completed_at) = completion.completed_at {
                                        assert!(completed_at >= completion.submitted_at);
                                        if let Some(not_before) = completion.not_before {
                                            assert!(not_before >= completion.submitted_at);
                                            assert!(not_before <= completed_at);
                                        }
                                    }
                                    println!("GPU_COMPLETION {completion:?}");
                                    proof.completed.push(completion);
                                    if proof.completed.len() == FRAMES {
                                        cx.quit();
                                    }
                                })
                                .expect("native compositor must support GPU completion")
                                .detach();
                        }
                    };
                    if register_during_frame {
                        window.on_next_frame(register);
                    } else {
                        register(window, cx);
                    }
                    cx.new(|_| Probe(0))
                },
            )
            .expect("native window must open");
            let timeout = cx.background_executor().timer(Duration::from_secs(20));
            cx.spawn(async move |cx| {
                timeout.await;
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
    let proof = proof.borrow();
    assert_eq!(proof.submitted.len(), FRAMES);
    assert_eq!(proof.completed.len(), FRAMES);
    let observed = proof
        .completed
        .iter()
        .filter(|completion| completion.completed_at.is_some())
        .count();
    assert!(observed > 0, "a live GPU must report observed completions");
    println!(
        "NATIVE_GPU_PASS submitted={} completions={} observed={} unobserved={}",
        proof.submitted.len(),
        proof.completed.len(),
        observed,
        proof.completed.len() - observed,
    );
}
