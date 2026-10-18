use gpui::{
    Context, ListHorizontalSizingBehavior, ListSizingBehavior, TestAppContext,
    UniformListScrollHandle, Window, div, point, prelude::*, px, size, uniform_list,
};
use std::{cell::RefCell, ops::Range, rc::Rc};

struct TestList {
    count: usize,
    fixed: bool,
    unconstrained: bool,
    handle: UniformListScrollHandle,
    calls: Rc<RefCell<Vec<Range<usize>>>>,
}

impl Render for TestList {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let calls = self.calls.clone();
        let list = uniform_list("fixed-size-parity", self.count, move |range, _, _| {
            calls.borrow_mut().push(range.clone());
            range
                .map(|_| div().w(px(320.0)).h(px(20.0)).into_any_element())
                .collect()
        })
        .with_sizing_behavior(ListSizingBehavior::Infer)
        .with_horizontal_sizing_behavior(if self.unconstrained {
            ListHorizontalSizingBehavior::Unconstrained
        } else {
            ListHorizontalSizingBehavior::FitList
        })
        .track_scroll(&self.handle);
        if self.fixed {
            list.fixed_item_size(size(px(320.0), px(20.0)))
        } else {
            list
        }
    }
}

#[gpui::test]
fn fixed_item_size_matches_measured_layout(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();

    for count in [0, 1, 47, 1_000] {
        for offset in [0.0, 45.0, 20_000.0] {
            for unconstrained in [false, true] {
                let mut measured = None;
                for fixed in [false, true] {
                    let handle = UniformListScrollHandle::new();
                    handle
                        .0
                        .borrow()
                        .base_handle
                        .set_offset(point(px(0.0), px(-offset)));
                    let calls = Rc::new(RefCell::new(Vec::<Range<usize>>::new()));
                    cx.draw(
                        point(px(0.0), px(0.0)),
                        size(px(100.0), px(200.0)),
                        |_, cx| {
                            cx.new(|_| TestList {
                                count,
                                fixed,
                                unconstrained,
                                handle: handle.clone(),
                                calls: calls.clone(),
                            })
                            .into_any_element()
                        },
                    );

                    let scroll = handle.0.borrow();
                    let last_size = scroll.last_item_size.expect("prepaint captured sizes");
                    let visible = calls.borrow().last().cloned();
                    assert_eq!(last_size.contents.height, px(20.0) * count);
                    let layout = (
                        last_size.item,
                        last_size.contents,
                        scroll.base_handle.offset(),
                        visible,
                    );
                    if fixed {
                        assert_eq!(Some(&layout), measured.as_ref());
                        assert_eq!(calls.borrow().len(), usize::from(count > 0));
                    } else {
                        assert_eq!(calls.borrow().len(), if count > 0 { 3 } else { 0 });
                        measured = Some(layout);
                    }
                }
            }
        }
    }
}
