use super::*;
use std::cell::{Cell, RefCell};
#[derive(Default)]
struct FakeWire {
    next: Cell<u32>,
    owner: Cell<Window>,
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
        handover: None,
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
    for (primary, generation, representations) in std::mem::take(&mut clipboard.decode_jobs) {
        clipboard.finish_decode(primary, generation, transfer::decode(representations));
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
