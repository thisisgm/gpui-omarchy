use super::*;
use std::cell::{Cell, RefCell};
use x11rb::protocol::xproto::{self, SelectionNotifyEvent};
#[derive(Default)]
struct FakeWire {
    next: Cell<u32>,
    reply_delay: Cell<std::time::Duration>,
    owner: Cell<Window>,
    reply_ready: Cell<Option<Instant>>,
    owner_ready: Cell<Option<Instant>>,
    owner_delay: Cell<std::time::Duration>,
    reads: RefCell<VecDeque<PropertyData>>,
    writes: RefCell<Vec<(Window, Atom, Atom, u8, Vec<u8>)>>,
    conversions: RefCell<Vec<(Window, Atom, Atom)>>,
    notifications: RefCell<Vec<Atom>>,
    destroyed: RefCell<Vec<Window>>,
}
impl SelectionWire for FakeWire {
    fn atom(&self, name: &str) -> anyhow::Result<Atom> {
        Ok(match name {
            transfer::URI_LIST => 10,
            transfer::GNOME_FILES => 11,
            "UTF8_STRING" => 12,
            "text/plain;charset=utf-8" => 13,
            "text/plain" => 14,
            _ => 100 + name.bytes().map(u32::from).sum::<u32>(),
        })
    }
    fn window(&self) -> anyhow::Result<Window> {
        let window = self.next.get() + 100;
        self.next.set(window);
        Ok(window)
    }
    fn destroy(&self, window: Window) {
        self.destroyed.borrow_mut().push(window);
    }
    fn owner(&self, _: Atom) -> anyhow::Result<Window> {
        if !self.owner_delay.get().is_zero() {
            let ready = self.owner_ready.get().unwrap_or_else(|| {
                let ready = Instant::now() + self.owner_delay.get();
                self.owner_ready.set(Some(ready));
                ready
            });
            if Instant::now() < ready {
                return Err(super::super::selection_wire::Pending.into());
            }
        }
        self.owner_ready.set(None);
        Ok(self.owner.get())
    }
    fn own(&self, window: Window, _: Atom) -> anyhow::Result<()> {
        self.owner.set(window);
        Ok(())
    }
    fn convert(
        &self,
        window: Window,
        selection: Atom,
        target: Atom,
        _: Atom,
    ) -> anyhow::Result<()> {
        self.conversions
            .borrow_mut()
            .push((window, selection, target));
        Ok(())
    }
    fn property(&self, _: Window, _: Atom, _: u32) -> anyhow::Result<PropertyData> {
        if !self.reply_delay.get().is_zero() {
            let ready = self.reply_ready.get().unwrap_or_else(|| {
                let ready = Instant::now() + self.reply_delay.get();
                self.reply_ready.set(Some(ready));
                ready
            });
            if Instant::now() < ready {
                return Err(super::super::selection_wire::Pending.into());
            }
        }
        self.reply_ready.set(None);
        Ok(self
            .reads
            .borrow_mut()
            .pop_front()
            .expect("fake must supply exactly one property reply per read"))
    }
    fn delete(&self, _: Window, _: Atom) -> anyhow::Result<()> {
        Ok(())
    }
    fn put(
        &self,
        window: Window,
        property: Atom,
        target: Atom,
        format: u8,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        self.writes
            .borrow_mut()
            .push((window, property, target, format, bytes.to_vec()));
        Ok(())
    }
    fn notify(&self, _: &SelectionRequestEvent, property: Atom) -> anyhow::Result<()> {
        self.notifications.borrow_mut().push(property);
        Ok(())
    }
    fn watch_property(&self, _: Window) -> anyhow::Result<()> {
        Ok(())
    }
}
fn clipboard() -> Clipboard<FakeWire> {
    Clipboard {
        wire: FakeWire::default(),
        window: 50,
        atoms: Atoms {
            CLIPBOARD: 1,
            PRIMARY: 2,
            TARGETS: 3,
            INCR: 40,
            UTF8_STRING: 12,
            GPUI_SELECTION: 6,
        },
        selections: [Selection::default(), Selection::default()],
        outgoing: HashMap::new(),
        incoming: HashMap::new(),
        changes: Vec::new(),
        decode_jobs: Vec::new(),
        budget: ReceiveBudget::default(),
        handover: None,
        handover_sent: false,
        handover_checked: false,
        initialized: true,
        initial_owners: [false, false],
        init_deadline: Instant::now() + TRANSFER_TIMEOUT,
        failed: false,
        queued_events: VecDeque::new(),
        queued_sets: [None, None],
        clear_pending: None,
    }
}
#[test]
fn review_x11_handover_waits_for_pending_ownership_setup() {
    let mut clipboard = clipboard();
    clipboard.initialized = false;
    clipboard
        .set(false, ClipboardItem::new_string("queued owner".into()))
        .unwrap();
    let receiver = clipboard.begin_handover().unwrap().unwrap();
    assert!(!clipboard.handover_sent);
    clipboard.tick();
    assert!(clipboard.handover_sent);
    assert_eq!(
        clipboard.cached(false).unwrap().text().as_deref(),
        Some("queued owner")
    );
    let manager = clipboard.wire.atom("CLIPBOARD_MANAGER").unwrap();
    let target = clipboard.wire.atom("SAVE_TARGETS").unwrap();
    clipboard
        .handle_event(&Event::SelectionNotify(SelectionNotifyEvent {
            requestor: clipboard.window,
            selection: manager,
            target,
            ..Default::default()
        }))
        .unwrap();
    futures::executor::block_on(receiver).unwrap();
    assert!(!clipboard.handover_pending());
}
#[test]
fn review_x11_late_decoded_success_cannot_overwrite_timeout_or_retry() {
    for retry in [false, true] {
        let mut clipboard = clipboard();
        clipboard.replaced(false, 88);
        let receiver = clipboard.read(false);
        let window = clipboard.selections[0].pending.unwrap();
        let target = clipboard.atoms.UTF8_STRING;
        let targets = clipboard.atoms.TARGETS;
        notify(
            &mut clipboard,
            window,
            targets,
            target.to_ne_bytes().to_vec(),
            32,
            AtomEnum::ATOM.into(),
        );
        clipboard.wire.reads.borrow_mut().push_back(PropertyData {
            type_: target,
            format: 8,
            bytes: b"queued success".to_vec(),
            remaining: 0,
        });
        clipboard.receive(window, true).unwrap();
        let decoded = clipboard.decode_jobs.pop().unwrap().run();
        assert!(decoded.result.is_ok());
        clipboard.complete_selection(false, Err(ClipboardReadError::TimedOut));
        assert_eq!(
            futures::executor::block_on(receiver).unwrap(),
            Err(ClipboardReadError::TimedOut)
        );
        let mut next = retry.then(|| clipboard.read(false));
        if retry {
            clipboard.selections[0].decoding = true;
            clipboard.selections[0].decode_deadline = Some(Instant::now() + TRANSFER_TIMEOUT);
        }
        clipboard.finish_decoded(decoded);
        assert!(clipboard.cached(false).is_none());
        if let Some(next) = next.as_mut() {
            assert_eq!(next.try_recv().unwrap(), None);
        }
    }
}
fn request(clipboard: &Clipboard<FakeWire>, target: Atom) -> SelectionRequestEvent {
    SelectionRequestEvent {
        owner: clipboard.window,
        requestor: 90,
        selection: clipboard.atoms.CLIPBOARD,
        target,
        property: 91,
        ..Default::default()
    }
}
fn notify(
    clipboard: &mut Clipboard<FakeWire>,
    window: Window,
    target: Atom,
    bytes: Vec<u8>,
    format: u8,
    type_: Atom,
) {
    clipboard.wire.reads.borrow_mut().push_back(PropertyData {
        type_,
        format,
        bytes,
        remaining: 0,
    });
    let event = Event::SelectionNotify(SelectionNotifyEvent {
        requestor: window,
        selection: clipboard.selection_atom(clipboard.incoming[&window].primary),
        target,
        property: clipboard.atoms.GPUI_SELECTION,
        ..Default::default()
    });
    assert!(clipboard.handle_event(&event).unwrap());
    flush_decode(clipboard);
}
fn flush_decode(clipboard: &mut Clipboard<FakeWire>) {
    for job in std::mem::take(&mut clipboard.decode_jobs) {
        let decoded = job.run();
        clipboard.finish_decoded(decoded);
    }
}
fn property(
    clipboard: &mut Clipboard<FakeWire>,
    window: Window,
    state: Property,
    bytes: Option<Vec<u8>>,
    type_: Atom,
) {
    if let Some(bytes) = bytes {
        clipboard.wire.reads.borrow_mut().push_back(PropertyData {
            type_,
            format: 8,
            bytes,
            remaining: 0,
        });
    }
    let event = Event::PropertyNotify(xproto::PropertyNotifyEvent {
        window,
        atom: if state == Property::DELETE {
            91
        } else {
            clipboard.atoms.GPUI_SELECTION
        },
        state,
        ..Default::default()
    });
    assert!(clipboard.handle_event(&event).unwrap());
    flush_decode(clipboard);
}
#[test]
fn targets_lists_exactly_the_representations_served() {
    let mut clipboard = clipboard();
    let data = transfer::file_payload(["/fixture/a b".into()], gpui::SelectionIntent::Cut).unwrap();
    clipboard
        .set(false, ClipboardItem::new_mime(data.clone()))
        .unwrap();
    clipboard
        .serve(&request(&clipboard, clipboard.atoms.TARGETS))
        .unwrap();
    let targets: Vec<_> = clipboard.wire.writes.borrow()[0]
        .4
        .chunks_exact(4)
        .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
        .collect();
    let mut expected = vec![clipboard.atoms.TARGETS];
    expected.extend(
        data.representations()
            .iter()
            .map(|entry| clipboard.wire.atom(entry.mime_type()).unwrap()),
    );
    assert_eq!(targets, expected);
    for entry in data.representations() {
        clipboard
            .serve(&request(
                &clipboard,
                clipboard.wire.atom(entry.mime_type()).unwrap(),
            ))
            .unwrap();
        assert_eq!(
            &clipboard.wire.writes.borrow().last().unwrap().4,
            &entry.bytes().to_vec()
        );
    }
    clipboard.serve(&request(&clipboard, 99999)).unwrap();
    assert_eq!(clipboard.wire.notifications.borrow().last(), Some(&NONE));
}
#[test]
fn foreign_owner_serves_each_file_mime_and_utf8() {
    for (mime, bytes) in [
        (transfer::URI_LIST, b"file:///fixture/a\r\n".to_vec()),
        (transfer::GNOME_FILES, b"cut\nfile:///fixture/a\n".to_vec()),
        ("UTF8_STRING", b"plain text".to_vec()),
        ("text/plain;charset=UTF-8", b"upper charset".to_vec()),
        ("TEXT", b"legacy text".to_vec()),
        ("STRING", vec![b'c', 0xe9]),
    ] {
        let mut clipboard = clipboard();
        clipboard.replaced(false, 88);
        let receiver = clipboard.read(false);
        let window = clipboard.selections[0].pending.unwrap();
        let atom = clipboard.wire.atom(mime).unwrap();
        let targets = clipboard.atoms.TARGETS;
        notify(
            &mut clipboard,
            window,
            targets,
            atom.to_ne_bytes().to_vec(),
            32,
            AtomEnum::ATOM.into(),
        );
        assert_eq!(clipboard.wire.conversions.borrow().last().unwrap().2, atom);
        notify(&mut clipboard, window, atom, bytes.clone(), 8, atom);
        let item = futures::executor::block_on(receiver)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(&*item.mime_payload().unwrap().get(mime).unwrap(), &bytes);
        assert!(clipboard.incoming.is_empty());
    }
}
#[test]
fn incoming_incr_over_256_kib_reassembles_without_blocking() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    let target = clipboard.atoms.UTF8_STRING;
    let targets = clipboard.atoms.TARGETS;
    notify(
        &mut clipboard,
        window,
        targets,
        target.to_ne_bytes().to_vec(),
        32,
        AtomEnum::ATOM.into(),
    );
    let expected = vec![b'a'; 300 * 1024];
    let incr = clipboard.atoms.INCR;
    notify(
        &mut clipboard,
        window,
        target,
        (expected.len() as u32).to_ne_bytes().to_vec(),
        32,
        incr,
    );
    for chunk in expected.chunks(CHUNK) {
        property(
            &mut clipboard,
            window,
            Property::NEW_VALUE,
            Some(chunk.to_vec()),
            target,
        );
    }
    property(
        &mut clipboard,
        window,
        Property::NEW_VALUE,
        Some(vec![]),
        target,
    );
    let item = futures::executor::block_on(receiver)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(item.text().unwrap().as_bytes(), expected);
}
#[test]
fn outgoing_incr_keeps_old_snapshot_after_selection_replacement() {
    let mut clipboard = clipboard();
    let expected = vec![b'x'; 300 * 1024];
    clipboard
        .set(
            false,
            ClipboardItem::new_string(String::from_utf8(expected.clone()).unwrap()),
        )
        .unwrap();
    clipboard
        .serve(&request(&clipboard, clipboard.atoms.UTF8_STRING))
        .unwrap();
    assert_eq!(clipboard.wire.writes.borrow()[0].2, clipboard.atoms.INCR);
    clipboard
        .set(false, ClipboardItem::new_string("new".into()))
        .unwrap();
    while clipboard.active() {
        property(&mut clipboard, 90, Property::DELETE, None, 0);
    }
    let sent: Vec<_> = clipboard
        .wire
        .writes
        .borrow()
        .iter()
        .skip(1)
        .flat_map(|write| write.4.clone())
        .collect();
    assert_eq!(sent, expected);
    assert!(clipboard.wire.writes.borrow().last().unwrap().4.is_empty());
}
#[test]
fn xfixes_ownership_loss_clears_cut_state_and_cancels_old_reads() {
    let mut clipboard = clipboard();
    clipboard
        .set(
            false,
            ClipboardItem::new_mime(
                transfer::file_payload(["/fixture/a".into()], gpui::SelectionIntent::Cut).unwrap(),
            ),
        )
        .unwrap();
    clipboard.wire.owner.set(88);
    let event = Event::XfixesSelectionNotify(x11rb::protocol::xfixes::SelectionNotifyEvent {
        window: clipboard.window,
        owner: 88,
        selection: clipboard.atoms.CLIPBOARD,
        ..Default::default()
    });
    assert!(clipboard.handle_event(&event).unwrap());
    assert!(clipboard.cached(false).is_none());
    assert!(clipboard.changes.iter().any(|change| !change.owned));
    let receiver = clipboard.read(false);
    clipboard.wire.owner.set(89);
    let event = Event::XfixesSelectionNotify(x11rb::protocol::xfixes::SelectionNotifyEvent {
        window: clipboard.window,
        owner: 89,
        selection: clipboard.atoms.CLIPBOARD,
        ..Default::default()
    });
    clipboard.handle_event(&event).unwrap();
    assert_eq!(
        futures::executor::block_on(receiver).unwrap(),
        Err(ClipboardReadError::Cancelled)
    );
}
#[test]
fn primary_and_clipboard_have_independent_owners_and_payloads() {
    let mut clipboard = clipboard();
    clipboard
        .set(false, ClipboardItem::new_string("clipboard".into()))
        .unwrap();
    clipboard
        .set(true, ClipboardItem::new_string("primary".into()))
        .unwrap();
    assert_eq!(
        clipboard.cached(false).unwrap().text().unwrap(),
        "clipboard"
    );
    assert_eq!(clipboard.cached(true).unwrap().text().unwrap(), "primary");
    clipboard.replaced(true, 88);
    assert!(clipboard.cached(true).is_none());
    assert_eq!(
        clipboard.cached(false).unwrap().text().unwrap(),
        "clipboard"
    );
}
#[test]
fn expired_or_oversized_incr_fails_and_releases_requestor() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    clipboard.incoming.get_mut(&window).unwrap().deadline = Instant::now();
    clipboard.tick();
    assert_eq!(
        futures::executor::block_on(receiver).unwrap(),
        Err(ClipboardReadError::TimedOut)
    );
    assert!(clipboard.wire.destroyed.borrow().contains(&window));
    let receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    let targets = clipboard.atoms.TARGETS;
    let incr = clipboard.atoms.INCR;
    notify(
        &mut clipboard,
        window,
        targets,
        (BYTE_LIMIT as u32 + 1).to_ne_bytes().to_vec(),
        32,
        incr,
    );
    assert_eq!(
        futures::executor::block_on(receiver).unwrap(),
        Err(ClipboardReadError::TooLarge)
    );
}

#[test]
fn manager_handover_uses_the_same_connection_and_waits_for_acknowledgement() {
    let mut clipboard = clipboard();
    clipboard
        .set(false, ClipboardItem::new_string("saved".into()))
        .unwrap();
    let receiver = clipboard.begin_handover().unwrap().unwrap();
    assert!(clipboard.handover_pending());
    let target = clipboard.wire.atom("SAVE_TARGETS").unwrap();
    let manager = clipboard.wire.atom("CLIPBOARD_MANAGER").unwrap();
    assert_eq!(
        clipboard.wire.conversions.borrow().last(),
        Some(&(clipboard.window, manager, target))
    );
    clipboard
        .serve(&request(&clipboard, clipboard.atoms.UTF8_STRING))
        .unwrap();
    assert_eq!(&clipboard.wire.writes.borrow().last().unwrap().4, b"saved");
    clipboard
        .handle_event(&Event::SelectionNotify(SelectionNotifyEvent {
            requestor: clipboard.window,
            selection: manager,
            target,
            ..Default::default()
        }))
        .unwrap();
    futures::executor::block_on(receiver).unwrap();
    assert!(!clipboard.handover_pending());
}
#[test]
fn manager_handover_deadline_cannot_hold_shutdown() {
    let mut clipboard = clipboard();
    clipboard
        .set(false, ClipboardItem::new_string("saved".into()))
        .unwrap();
    let receiver = clipboard.begin_handover().unwrap().unwrap();
    clipboard.handover.as_mut().unwrap().0 = Instant::now();
    clipboard.tick();
    futures::executor::block_on(receiver).unwrap();
    assert!(!clipboard.active());
}

#[test]
fn stale_clear_and_xfixes_notifications_do_not_clear_a_new_owner() {
    let mut clipboard = clipboard();
    clipboard
        .set(false, ClipboardItem::new_string("new source".into()))
        .unwrap();
    let clear = Event::SelectionClear(xproto::SelectionClearEvent {
        owner: clipboard.window,
        selection: clipboard.atoms.CLIPBOARD,
        ..Default::default()
    });
    clipboard.handle_event(&clear).unwrap();
    let stale = Event::XfixesSelectionNotify(x11rb::protocol::xfixes::SelectionNotifyEvent {
        window: clipboard.window,
        owner: 88,
        selection: clipboard.atoms.CLIPBOARD,
        ..Default::default()
    });
    clipboard.handle_event(&stale).unwrap();
    assert_eq!(
        clipboard.cached(false).unwrap().text().unwrap(),
        "new source"
    );
}
#[test]
fn clearing_a_foreign_clipboard_is_refused() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    clipboard.wire.owner.set(88);
    assert!(!clipboard.clear_if_owned().unwrap());
    assert_eq!(clipboard.wire.owner.get(), 88);
    clipboard
        .set(false, ClipboardItem::new_string("owned".into()))
        .unwrap();
    assert!(clipboard.clear_if_owned().unwrap());
    assert!(clipboard.cached(false).is_none());
}

#[test]
fn foreign_primary_file_selection_uses_the_same_mime_reader() {
    let mut clipboard = clipboard();
    clipboard.replaced(true, 88);
    let receiver = clipboard.read(true);
    let window = clipboard.selections[1].pending.unwrap();
    let mime = clipboard.wire.atom(transfer::GNOME_FILES).unwrap();
    let targets = clipboard.atoms.TARGETS;
    notify(
        &mut clipboard,
        window,
        targets,
        mime.to_ne_bytes().to_vec(),
        32,
        AtomEnum::ATOM.into(),
    );
    notify(
        &mut clipboard,
        window,
        mime,
        b"cut\nfile:///fixture/a\n".to_vec(),
        8,
        mime,
    );
    let item = futures::executor::block_on(receiver)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        item.mime_payload().unwrap().intent(),
        gpui::SelectionIntent::Cut
    );
    assert!(clipboard.cached(false).is_none());
}
#[test]
fn owner_without_targets_can_still_convert_utf8_text() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    loop {
        let target = clipboard.incoming[&window].target;
        if target == clipboard.atoms.UTF8_STRING {
            break;
        }
        clipboard
            .handle_event(&Event::SelectionNotify(SelectionNotifyEvent {
                requestor: window,
                selection: clipboard.atoms.CLIPBOARD,
                target,
                property: NONE,
                ..Default::default()
            }))
            .unwrap();
    }
    let target = clipboard.atoms.UTF8_STRING;
    notify(
        &mut clipboard,
        window,
        target,
        b"legacy owner".to_vec(),
        8,
        target,
    );
    let item = futures::executor::block_on(receiver)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(item.text().unwrap(), "legacy owner");
}

#[test]
fn same_foreign_owner_replacing_its_selection_invalidates_the_cached_bytes() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    clipboard.wire.owner.set(88);
    clipboard.selections[0].cached = Some(ClipboardItem::new_string("old bytes".into()));
    let generation = clipboard.selections[0].generation;
    let event = Event::XfixesSelectionNotify(x11rb::protocol::xfixes::SelectionNotifyEvent {
        window: clipboard.window,
        owner: 88,
        selection: clipboard.atoms.CLIPBOARD,
        ..Default::default()
    });
    clipboard.handle_event(&event).unwrap();
    assert!(clipboard.cached(false).is_none());
    assert!(clipboard.selections[0].generation > generation);
    assert!(clipboard.selections[0].pending.is_some());
}

#[test]
fn review_x11_slow_reply_does_not_block_or_outrun_deadline() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    let target = clipboard.atoms.UTF8_STRING;
    let targets = clipboard.atoms.TARGETS;
    notify(
        &mut clipboard,
        window,
        targets,
        target.to_ne_bytes().to_vec(),
        32,
        AtomEnum::ATOM.into(),
    );
    clipboard.incoming.get_mut(&window).unwrap().deadline =
        Instant::now() + std::time::Duration::from_millis(20);
    clipboard
        .wire
        .reply_delay
        .set(std::time::Duration::from_millis(80));
    let start = Instant::now();
    notify(&mut clipboard, window, target, b"late".to_vec(), 8, target);
    println!("review: event handler returned in {:?}", start.elapsed());
    assert!(start.elapsed() < std::time::Duration::from_millis(20));
    std::thread::sleep(std::time::Duration::from_millis(30));
    clipboard.tick();
    assert_eq!(
        futures::executor::block_on(receiver).unwrap(),
        Err(ClipboardReadError::TimedOut)
    );
}

#[test]
fn review_x11_replaced_selections_cannot_accumulate_unbounded_decode_jobs() {
    let mut clipboard = clipboard();
    for _ in 0..MAX_TRANSFERS + 1 {
        clipboard.replaced(false, 88);
        let _receiver = clipboard.read(false);
        let window = clipboard.selections[0].pending.unwrap();
        let target = clipboard.atoms.UTF8_STRING;
        let targets = clipboard.atoms.TARGETS;
        notify(
            &mut clipboard,
            window,
            targets,
            target.to_ne_bytes().to_vec(),
            32,
            AtomEnum::ATOM.into(),
        );
        clipboard.wire.reads.borrow_mut().push_back(PropertyData {
            type_: target,
            format: 8,
            bytes: b"queued data".to_vec(),
            remaining: 0,
        });
        clipboard.receive(window, true).unwrap();
        assert!(
            clipboard.decode_jobs.len() <= MAX_TRANSFERS,
            "outstanding jobs exceeded transfer bound: {}",
            clipboard.decode_jobs.len()
        );
        // The real client drains these jobs into background tasks without admission or cancellation.
        // Keep the jobs here until the final assertion rather than completing that background work.
        let jobs = std::mem::take(&mut clipboard.decode_jobs);
        REVIEW_JOBS.with(|pending| {
            let mut pending = pending.borrow_mut();
            pending.retain(DecodeJob::live);
            pending.extend(jobs);
        });
        let usage = clipboard.budget.0.lock().unwrap();
        assert!(usage.transfers <= MAX_TRANSFERS);
        assert!(usage.bytes <= BYTE_LIMIT);
    }
    REVIEW_JOBS.with(|pending| {
        assert!(
            pending.borrow().len() <= MAX_TRANSFERS,
            "outstanding jobs exceeded transfer bound: {}",
            pending.borrow().len()
        )
    });
}
thread_local! {
    static REVIEW_JOBS: RefCell<Vec<DecodeJob>> = const { RefCell::new(Vec::new()) };
}

#[test]
fn review_x11_foreign_custom_mime_survives_public_payload_roundtrip() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    let mime = "application/x-test-token";
    let target = clipboard.wire.atom(mime).unwrap();
    let targets = clipboard.atoms.TARGETS;
    notify(
        &mut clipboard,
        window,
        targets,
        target.to_ne_bytes().to_vec(),
        32,
        AtomEnum::ATOM.into(),
    );
    let result = futures::executor::block_on(receiver).unwrap();
    assert!(
        result.is_ok(),
        "public MIME representation was rejected before requesting its bytes: {result:?}"
    );
}

#[test]
fn review_x11_text_target_accepts_supported_actual_string_encoding() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    let target = clipboard.wire.atom("TEXT").unwrap();
    let encoding = clipboard.wire.atom("STRING").unwrap();
    let targets = clipboard.atoms.TARGETS;
    notify(
        &mut clipboard,
        window,
        targets,
        target.to_ne_bytes().to_vec(),
        32,
        AtomEnum::ATOM.into(),
    );
    notify(
        &mut clipboard,
        window,
        target,
        vec![b'c', 0xe9],
        8,
        encoding,
    );
    let result = futures::executor::block_on(receiver).unwrap();
    assert!(
        result.is_ok(),
        "valid TEXT conversion in supported STRING encoding rejected: {result:?}"
    );
    assert_eq!(result.unwrap().unwrap().text().as_deref(), Some("c\u{e9}"));
}

#[test]
fn review_x11_handover_setup_is_inside_the_absolute_deadline() {
    let mut clipboard = clipboard();
    clipboard
        .set(false, ClipboardItem::new_string("saved".into()))
        .unwrap();
    clipboard
        .wire
        .owner_delay
        .set(std::time::Duration::from_millis(80));
    let start = Instant::now();
    let receiver = clipboard.begin_handover().unwrap().unwrap();
    assert!(start.elapsed() < std::time::Duration::from_millis(20));
    assert!(!clipboard.handover_sent);
    clipboard.handover.as_mut().unwrap().0 = Instant::now();
    clipboard.tick();
    futures::executor::block_on(receiver).unwrap();
    assert!(!clipboard.handover_pending());
    assert!(clipboard.wire.conversions.borrow().is_empty());
}
#[test]
fn review_x11_decode_completion_rechecks_absolute_deadline() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let receiver = clipboard.read(false);
    let generation = clipboard.selections[0].generation;
    clipboard.selections[0].decoding = true;
    clipboard.selections[0].decode_deadline = Some(Instant::now());
    clipboard.finish_decode(
        false,
        generation,
        Ok(ClipboardItem::new_string("late decode".into())),
    );
    assert_eq!(
        futures::executor::block_on(receiver).unwrap(),
        Err(ClipboardReadError::TimedOut)
    );
    assert!(clipboard.cached(false).is_none());
}

#[test]
fn review_x11_cancelled_queued_decode_releases_payload_and_skips_decode() {
    let mut clipboard = clipboard();
    clipboard.replaced(false, 88);
    let _receiver = clipboard.read(false);
    let window = clipboard.selections[0].pending.unwrap();
    let target = clipboard.atoms.UTF8_STRING;
    let targets = clipboard.atoms.TARGETS;
    notify(
        &mut clipboard,
        window,
        targets,
        target.to_ne_bytes().to_vec(),
        32,
        AtomEnum::ATOM.into(),
    );
    clipboard.wire.reads.borrow_mut().push_back(PropertyData {
        type_: target,
        format: 8,
        bytes: b"queued".to_vec(),
        remaining: 0,
    });
    clipboard.receive(window, true).unwrap();
    let job = clipboard.decode_jobs.pop().unwrap();
    assert_eq!(clipboard.budget.0.lock().unwrap().bytes, 6);
    clipboard.replaced(false, 89);
    assert_eq!(clipboard.budget.0.lock().unwrap().bytes, 0);
    assert_eq!(clipboard.budget.0.lock().unwrap().transfers, 0);
    assert!(job.slot.lock().unwrap().is_none());
    assert_eq!(job.run().result, Err(ClipboardReadError::Cancelled));
}
#[test]
fn review_x11_byte_budget_is_aggregate_and_held_through_publish() {
    let budget = ReceiveBudget::default();
    let mut first = budget.acquire().unwrap();
    first.reserve(BYTE_LIMIT).unwrap();
    let mut second = budget.acquire().unwrap();
    assert_eq!(second.reserve(1), Err(ClipboardReadError::TooLarge));
    drop(first);
    second.reserve(1).unwrap();
    let slot = Arc::new(Mutex::new(Some(DecodeInput {
        representations: vec![MimeRepresentation::new("UTF8_STRING", &b"x"[..])],
        permit: second,
    })));
    let job = DecodeJob {
        primary: false,
        generation: 1,
        cancel: transfer::cancelled(),
        deadline: Instant::now() + TRANSFER_TIMEOUT,
        slot,
    };
    let decoded = job.run();
    assert!(decoded.result.is_ok());
    assert_eq!(budget.0.lock().unwrap().bytes, 1);
    assert_eq!(budget.0.lock().unwrap().transfers, 1);
    drop(decoded);
    assert_eq!(budget.0.lock().unwrap().bytes, 0);
    assert_eq!(budget.0.lock().unwrap().transfers, 0);
}

#[test]
fn review_x11_startup_prefetches_both_existing_foreign_owners() {
    let mut clipboard = clipboard();
    clipboard.wire.owner.set(88);
    clipboard.initial_owners = [true, true];
    clipboard.tick();
    for index in 0..2 {
        assert_eq!(clipboard.selections[index].owner, 88);
        assert!(clipboard.selections[index].pending.is_some());
        assert!(clipboard.selections[index].requested.is_some());
    }
    assert_eq!(clipboard.wire.conversions.borrow().len(), 2);
}
#[test]
fn review_x11_uncached_sync_read_starts_acquisition_once() {
    let mut clipboard = clipboard();
    clipboard.replaced(true, 88);
    assert!(clipboard.cached_or_request(true).is_none());
    assert!(clipboard.cached_or_request(true).is_none());
    assert!(clipboard.selections[1].pending.is_some());
    assert_eq!(clipboard.wire.conversions.borrow().len(), 1);
}

#[test]
fn review_x11_initialization_timeout_stops_timer_and_completes_reads() {
    let mut clipboard = clipboard();
    clipboard.initialized = false;
    clipboard.initial_owners = [true, true];
    clipboard.init_deadline = Instant::now();
    let receiver = clipboard.read(false);
    clipboard.tick();
    assert_eq!(
        futures::executor::block_on(receiver).unwrap(),
        Err(ClipboardReadError::TimedOut)
    );
    assert!(!clipboard.active());
    assert!(clipboard.failed);
}
