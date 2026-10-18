use crate::linux::transfer::{self, BYTE_LIMIT, TRANSFER_TIMEOUT, TYPE_LIMIT};
use futures::channel::oneshot;
use gpui::{ClipboardChange, ClipboardItem, ClipboardReadError, MimePayload, MimeRepresentation};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Instant,
};
use x11rb::{
    NONE,
    protocol::{
        Event,
        xproto::{Atom, AtomEnum, Property, SelectionRequestEvent, Window},
    },
    xcb_ffi::XCBConnection,
};

pub(super) const CHUNK: usize = 64 * 1024;
const INCR_THRESHOLD: usize = 256 * 1024;
const MAX_TRANSFERS: usize = 16;
fn preferred_mime_types() -> Vec<String> {
    use strum::IntoEnumIterator;
    let mut names = vec![
        transfer::GNOME_FILES.to_owned(),
        transfer::URI_LIST.to_owned(),
    ];
    names.extend(gpui::ImageFormat::iter().map(|format| format.mime_type().to_owned()));
    names.extend(
        [
            "UTF8_STRING",
            "text/plain;charset=utf-8",
            "text/plain;charset=UTF-8",
            "STRING",
            "TEXT",
            "text/plain",
        ]
        .map(str::to_owned),
    );
    names
}

type ReadResult = Result<Option<ClipboardItem>, ClipboardReadError>;

x11rb::atom_manager! {
    pub Atoms: AtomCookies { CLIPBOARD, PRIMARY, TARGETS, INCR, UTF8_STRING, GPUI_SELECTION, }
}

pub(crate) struct PropertyData {
    pub type_: Atom,
    pub format: u8,
    pub bytes: Vec<u8>,
    pub remaining: u32,
}

pub(crate) trait SelectionWire {
    fn watch_selection(&self, _window: Window, _selection: Atom) -> anyhow::Result<()> {
        Ok(())
    }
    fn atom(&self, name: &str) -> anyhow::Result<Atom>;
    fn name(&self, atom: Atom) -> anyhow::Result<String>;
    fn window(&self) -> anyhow::Result<Window>;
    fn destroy(&self, window: Window);
    fn owner(&self, selection: Atom) -> anyhow::Result<Window>;
    fn own(&self, owner: Window, selection: Atom) -> anyhow::Result<()>;
    fn convert(
        &self,
        window: Window,
        selection: Atom,
        target: Atom,
        property: Atom,
    ) -> anyhow::Result<()>;
    fn property(&self, window: Window, property: Atom, offset: u32)
    -> anyhow::Result<PropertyData>;
    fn delete(&self, window: Window, property: Atom) -> anyhow::Result<()>;
    fn put(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        format: u8,
        bytes: &[u8],
    ) -> anyhow::Result<()>;
    fn notify(&self, request: &SelectionRequestEvent, property: Atom) -> anyhow::Result<()>;
    fn watch_property(&self, window: Window) -> anyhow::Result<()>;
}

use super::selection_wire::{Server, pending};
fn wire_error(error: anyhow::Error) -> ClipboardReadError {
    error
        .downcast::<ClipboardReadError>()
        .unwrap_or_else(transfer::transport)
}
#[derive(Default)]
struct Usage {
    transfers: usize,
    bytes: usize,
}
#[derive(Default, Clone)]
struct ReceiveBudget(Arc<Mutex<Usage>>);
struct ReceivePermit {
    budget: ReceiveBudget,
    bytes: usize,
}
impl ReceiveBudget {
    fn acquire(&self) -> Result<ReceivePermit, ClipboardReadError> {
        let mut usage = self.0.lock().unwrap();
        if usage.transfers >= MAX_TRANSFERS {
            return Err(ClipboardReadError::Unavailable);
        }
        usage.transfers += 1;
        Ok(ReceivePermit {
            budget: self.clone(),
            bytes: 0,
        })
    }
}
impl ReceivePermit {
    fn reserve(&mut self, bytes: usize) -> Result<(), ClipboardReadError> {
        let mut usage = self.budget.0.lock().unwrap();
        if bytes > BYTE_LIMIT.saturating_sub(usage.bytes) {
            return Err(ClipboardReadError::TooLarge);
        }
        usage.bytes += bytes;
        self.bytes += bytes;
        Ok(())
    }
}
impl Drop for ReceivePermit {
    fn drop(&mut self) {
        let mut usage = self.budget.0.lock().unwrap();
        usage.transfers -= 1;
        usage.bytes -= self.bytes;
    }
}
struct DecodeInput {
    representations: Vec<MimeRepresentation>,
    offered_types: Vec<String>,
    permit: ReceivePermit,
}
type DecodeSlot = Arc<Mutex<Option<DecodeInput>>>;
pub(crate) struct DecodeJob {
    pub primary: bool,
    pub generation: u64,
    cancel: transfer::Cancel,
    deadline: Instant,
    slot: DecodeSlot,
}
pub(crate) struct Decoded {
    pub primary: bool,
    pub generation: u64,
    pub result: Result<ClipboardItem, ClipboardReadError>,
    cancel: transfer::Cancel,
    deadline: Instant,
    _permit: Option<ReceivePermit>,
}
impl DecodeJob {
    fn live(&self) -> bool {
        !self.cancel.load(Ordering::Acquire) && self.slot.lock().unwrap().is_some()
    }
    pub fn run(self) -> Decoded {
        let input = self.slot.lock().unwrap().take();
        let (result, permit) = if let Some(input) = input {
            let result = transfer::check(&self.cancel, self.deadline)
                .and_then(|()| transfer::decode_offered(input.representations, input.offered_types))
                .and_then(|item| transfer::check(&self.cancel, self.deadline).map(|()| item));
            (result, Some(input.permit))
        } else {
            (Err(ClipboardReadError::Cancelled), None)
        };
        Decoded {
            primary: self.primary,
            generation: self.generation,
            result,
            cancel: self.cancel,
            deadline: self.deadline,
            _permit: permit,
        }
    }
}
#[derive(Default)]
struct Selection {
    owner: Window,
    generation: u64,
    item: Option<ClipboardItem>,
    snapshot: Option<Vec<(Atom, Arc<[u8]>)>>,
    cached: Option<ClipboardItem>,
    pending: Option<Window>,
    decoding: bool,
    waiters: Vec<oneshot::Sender<ReadResult>>,
    requested: Option<Instant>,
    decode_deadline: Option<Instant>,
    cancel: transfer::Cancel,
    queued_decode: Option<DecodeSlot>,
}
struct Outgoing {
    target: Atom,
    bytes: Arc<[u8]>,
    offset: usize,
    deadline: Instant,
}
struct Incoming {
    primary: bool,
    generation: u64,
    owner: Window,
    deadline: Instant,
    target: Atom,
    actual_type: Option<Atom>,
    mime: Option<String>,
    types: VecDeque<(Atom, String)>,
    representations: Vec<MimeRepresentation>,
    chunk: Vec<u8>,
    total: usize,
    incr: bool,
    offset: u32,
    draining: bool,
    probing: bool,
    waiting: Option<bool>,
    available: Vec<Atom>,
    names: Vec<(Atom, String)>,
    naming: bool,
    permit: ReceivePermit,
}

pub(crate) struct Clipboard<W: SelectionWire = Server> {
    wire: W,
    window: Window,
    atoms: Atoms,
    selections: [Selection; 2],
    outgoing: HashMap<(Window, Atom), Outgoing>,
    incoming: HashMap<Window, Incoming>,
    pub changes: Vec<ClipboardChange>,
    pub decode_jobs: Vec<DecodeJob>,
    budget: ReceiveBudget,
    handover: Option<(Instant, Vec<oneshot::Sender<()>>)>,
    handover_sent: bool,
    handover_checked: bool,
    initialized: bool,
    initial_owners: [bool; 2],
    init_deadline: Instant,
    failed: bool,
    queued_events: VecDeque<(Instant, Event)>,
    queued_sets: [Option<(Instant, ClipboardItem)>; 2],
    clear_pending: Option<Instant>,
}
impl Clipboard<Server> {
    pub fn new(connection: Arc<XCBConnection>, root: Window) -> anyhow::Result<Self> {
        Ok(Self {
            wire: Server::new(connection, root)?,
            window: NONE,
            atoms: Atoms {
                CLIPBOARD: NONE,
                PRIMARY: NONE,
                TARGETS: NONE,
                INCR: NONE,
                UTF8_STRING: NONE,
                GPUI_SELECTION: NONE,
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
            initialized: false,
            initial_owners: [true, true],
            init_deadline: Instant::now() + TRANSFER_TIMEOUT,
            failed: false,
            queued_events: VecDeque::new(),
            queued_sets: [None, None],
            clear_pending: None,
        })
    }
}
impl<W: SelectionWire> Clipboard<W> {
    fn selection_atom(&self, primary: bool) -> Atom {
        if primary {
            self.atoms.PRIMARY
        } else {
            self.atoms.CLIPBOARD
        }
    }
    fn index(&self, selection: Atom) -> Option<usize> {
        if selection == self.atoms.CLIPBOARD {
            Some(0)
        } else if selection == self.atoms.PRIMARY {
            Some(1)
        } else {
            None
        }
    }
    pub fn cached(&self, primary: bool) -> Option<ClipboardItem> {
        let selection = &self.selections[primary as usize];
        selection.item.clone().or_else(|| selection.cached.clone())
    }
    pub fn cached_or_request(&mut self, primary: bool) -> Option<ClipboardItem> {
        let item = self.cached(primary);
        if item.is_none() {
            let _ = self.read(primary);
        }
        item
    }
    fn initial_owner(&mut self, primary: bool) -> anyhow::Result<()> {
        let index = primary as usize;
        let owner = self.wire.owner(self.selection_atom(primary))?;
        self.initial_owners[index] = false;
        if self.selections[index].generation != 0 {
            return Ok(());
        }
        self.selections[index].owner = owner;
        if owner != NONE && owner != self.window {
            let _ = self.read(primary);
        } else if owner == NONE {
            self.complete_selection(primary, Ok(None));
        }
        Ok(())
    }
    fn initialize(&mut self) -> anyhow::Result<()> {
        let names = [
            "CLIPBOARD",
            "PRIMARY",
            "TARGETS",
            "INCR",
            "UTF8_STRING",
            "GPUI_SELECTION",
        ];
        let atoms: Vec<_> = names
            .iter()
            .map(|name| self.wire.atom(name))
            .collect::<Result<_, _>>()?;
        for name in preferred_mime_types().iter().chain(
            ["CLIPBOARD_MANAGER", "SAVE_TARGETS"]
                .map(str::to_owned)
                .iter(),
        ) {
            self.wire.atom(name)?;
        }
        if self.window == NONE {
            self.window = self.wire.window()?;
        }
        self.atoms = Atoms {
            CLIPBOARD: atoms[0],
            PRIMARY: atoms[1],
            TARGETS: atoms[2],
            INCR: atoms[3],
            UTF8_STRING: atoms[4],
            GPUI_SELECTION: atoms[5],
        };
        for atom in [self.atoms.CLIPBOARD, self.atoms.PRIMARY] {
            self.wire.watch_selection(self.window, atom)?;
        }
        self.initialized = true;
        Ok(())
    }
    pub fn clear_if_owned(&mut self) -> Result<bool, ClipboardReadError> {
        if self.selections[0].owner != self.window || self.window == NONE {
            return Ok(false);
        }
        self.clear_pending = Some(Instant::now() + TRANSFER_TIMEOUT);
        match self.try_clear() {
            Ok(()) => Ok(true),
            Err(error) if pending(&error) => Ok(true),
            Err(error) => Err(wire_error(error)),
        }
    }
    fn try_clear(&mut self) -> anyhow::Result<()> {
        if self.wire.owner(self.atoms.CLIPBOARD)? == self.window {
            self.wire.own(NONE, self.atoms.CLIPBOARD)?;
            self.replaced(false, NONE);
        }
        self.clear_pending = None;
        Ok(())
    }
    pub fn set(&mut self, primary: bool, item: ClipboardItem) -> Result<(), ClipboardReadError> {
        self.queued_sets[primary as usize] = Some((Instant::now() + TRANSFER_TIMEOUT, item));
        match self.try_set(primary) {
            Ok(()) => Ok(()),
            Err(error) if pending(&error) => Ok(()),
            Err(error) => {
                self.queued_sets[primary as usize] = None;
                Err(wire_error(error))
            }
        }
    }
    fn try_set(&mut self, primary: bool) -> anyhow::Result<()> {
        if self.failed {
            return Err(ClipboardReadError::Unavailable.into());
        }
        if !self.initialized {
            return Err(super::selection_wire::Pending.into());
        }
        let (deadline, item) = self.queued_sets[primary as usize].as_ref().unwrap();
        let deadline = *deadline;
        let item = item.clone();
        let mut payload = transfer::snapshot(&item)?;
        if let Some(text) = item.text() {
            let mut representations = payload.representations().to_vec();
            let bytes: Arc<[u8]> = text.as_bytes().into();
            representations.push(MimeRepresentation::new(
                "text/plain;charset=UTF-8",
                bytes.clone(),
            ));
            representations.push(MimeRepresentation::new("TEXT", bytes));
            if let Some(bytes) = text
                .chars()
                .map(|character| u8::try_from(character as u32).ok())
                .collect::<Option<Vec<_>>>()
            {
                representations.push(MimeRepresentation::new("STRING", bytes));
            }
            payload = MimePayload::new(representations, payload.intent());
        }
        let representations = self.intern(&payload)?;
        if Instant::now() >= deadline {
            return Err(ClipboardReadError::TimedOut.into());
        }
        self.wire
            .own(self.window, self.selection_atom(primary))
            .map_err(transfer::transport)?;
        self.replaced(primary, self.window);
        let selection = &mut self.selections[primary as usize];
        selection.item = Some(item);
        selection.snapshot = Some(representations);
        self.queued_sets[primary as usize] = None;
        Ok(())
    }
    fn intern(&self, payload: &MimePayload) -> anyhow::Result<Vec<(Atom, Arc<[u8]>)>> {
        payload
            .representations()
            .iter()
            .map(|representation| {
                self.wire
                    .atom(representation.mime_type())
                    .map(|atom| (atom, representation.bytes().clone()))
            })
            .collect()
    }
    fn replaced(&mut self, primary: bool, owner: Window) {
        let selection = &mut self.selections[primary as usize];
        selection.cancel.store(true, Ordering::Release);
        if let Some(slot) = selection.queued_decode.take() {
            slot.lock().unwrap().take();
        }
        selection.cancel = transfer::cancelled();
        selection.decoding = false;
        selection.decode_deadline = None;
        selection.requested = None;
        if let Some(window) = selection.pending.take() {
            self.incoming.remove(&window);
            self.wire.destroy(window);
        }
        for waiter in selection.waiters.drain(..) {
            let _ = waiter.send(Err(ClipboardReadError::Cancelled));
        }
        selection.generation = selection.generation.wrapping_add(1);
        selection.owner = owner;
        selection.item = None;
        selection.snapshot = None;
        selection.cached = None;
        selection.decoding = false;
        self.changes.push(ClipboardChange {
            primary,
            generation: selection.generation,
            owned: owner == self.window,
            error: None,
        });
    }
    pub fn read(&mut self, primary: bool) -> oneshot::Receiver<ReadResult> {
        let (sender, receiver) = oneshot::channel();
        if self.failed {
            let _ = sender.send(Err(ClipboardReadError::Unavailable));
            return receiver;
        }
        if self.selections[primary as usize].waiters.len() >= 64 {
            let _ = sender.send(Err(ClipboardReadError::Unavailable));
            return receiver;
        }
        if let Some(item) = self.cached(primary) {
            let _ = sender.send(Ok(Some(item)));
            return receiver;
        }
        if self.initialized
            && !self.initial_owners[primary as usize]
            && self.selections[primary as usize].owner == NONE
        {
            let _ = sender.send(Ok(None));
            return receiver;
        }
        self.selections[primary as usize].waiters.push(sender);
        self.selections[primary as usize]
            .requested
            .get_or_insert(Instant::now() + TRANSFER_TIMEOUT);
        self.drive_read(primary);
        receiver
    }
    fn drive_read(&mut self, primary: bool) {
        let selection = &self.selections[primary as usize];
        if !self.initialized
            || self.initial_owners[primary as usize]
            || selection.requested.is_none()
            || selection.pending.is_some()
            || selection.decoding
        {
            return;
        }
        if let Err(error) = self.begin(primary) {
            if !pending(&error) {
                self.complete_selection(primary, Err(wire_error(error)));
            }
        }
    }
    fn begin(&mut self, primary: bool) -> anyhow::Result<()> {
        if self.incoming.len() >= MAX_TRANSFERS {
            return Err(ClipboardReadError::Unavailable.into());
        }
        let window = self.wire.window()?;
        let permit = self.budget.acquire()?;
        let selection = &self.selections[primary as usize];
        let incoming = Incoming {
            primary,
            generation: selection.generation,
            owner: selection.owner,
            deadline: selection.requested.unwrap(),
            target: self.atoms.TARGETS,
            actual_type: None,
            mime: None,
            types: VecDeque::new(),
            representations: Vec::new(),
            chunk: Vec::new(),
            total: 0,
            incr: false,
            offset: 0,
            draining: false,
            probing: false,
            waiting: None,
            available: Vec::new(),
            names: Vec::new(),
            naming: false,
            permit,
        };
        self.incoming.insert(window, incoming);
        self.selections[primary as usize].pending = Some(window);
        self.wire.convert(
            window,
            self.selection_atom(primary),
            self.atoms.TARGETS,
            self.atoms.GPUI_SELECTION,
        )
    }
    fn complete_selection(&mut self, primary: bool, result: ReadResult) {
        let selection = &mut self.selections[primary as usize];
        selection.cancel.store(true, Ordering::Release);
        if let Some(slot) = selection.queued_decode.take() {
            slot.lock().unwrap().take();
        }
        selection.cancel = transfer::cancelled();
        selection.decoding = false;
        selection.decode_deadline = None;
        selection.requested = None;
        if let Some(window) = selection.pending.take() {
            self.incoming.remove(&window);
            self.wire.destroy(window);
        }
        if let Ok(item) = &result {
            selection.cached = item.clone();
        }
        if let Err(error) = &result {
            self.changes.push(ClipboardChange {
                primary,
                generation: selection.generation,
                owned: false,
                error: Some(error.clone()),
            });
        }
        for waiter in selection.waiters.drain(..) {
            let _ = waiter.send(result.clone());
        }
    }
    pub fn finish_decoded(&mut self, decoded: Decoded) {
        if decoded.cancel.load(Ordering::Acquire) {
            return;
        }
        let result =
            transfer::check(&decoded.cancel, decoded.deadline).and_then(|()| decoded.result);
        self.finish_decode(decoded.primary, decoded.generation, result);
    }
    fn finish_decode(
        &mut self,
        primary: bool,
        generation: u64,
        result: Result<ClipboardItem, ClipboardReadError>,
    ) {
        if self.selections[primary as usize].generation == generation
            && self.selections[primary as usize].decoding
        {
            let result = if self.selections[primary as usize]
                .decode_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                Err(ClipboardReadError::TimedOut)
            } else {
                result
            };
            self.complete_selection(primary, result.map(Some));
        }
    }
    pub fn begin_handover(&mut self) -> anyhow::Result<Option<oneshot::Receiver<()>>> {
        if self.selections[0].item.is_none() && self.queued_sets[0].is_none() {
            return Ok(None);
        }
        let (sender, receiver) = oneshot::channel();
        if let Some((_, waiters)) = self.handover.as_mut() {
            if waiters.len() < MAX_TRANSFERS {
                waiters.push(sender);
            }
            return Ok(Some(receiver));
        }
        self.handover = Some((Instant::now() + TRANSFER_TIMEOUT, vec![sender]));
        self.handover_sent = false;
        self.handover_checked = false;
        if let Err(error) = self.setup_handover() {
            if !pending(&error) {
                self.finish_handover();
                return Err(error);
            }
        }
        Ok(Some(receiver))
    }
    fn setup_handover(&mut self) -> anyhow::Result<()> {
        if !self.initialized || self.queued_sets[0].is_some() {
            return Err(super::selection_wire::Pending.into());
        }
        if !self.handover_checked {
            if self.wire.owner(self.atoms.CLIPBOARD)? != self.window {
                self.finish_handover();
                return Ok(());
            }
            self.handover_checked = true;
        }
        let manager = self.wire.atom("CLIPBOARD_MANAGER")?;
        let target = self.wire.atom("SAVE_TARGETS")?;
        if self.wire.owner(manager)? == NONE {
            self.finish_handover();
            return Ok(());
        }
        if self
            .handover
            .as_ref()
            .is_some_and(|(deadline, _)| Instant::now() >= *deadline)
        {
            self.finish_handover();
            return Ok(());
        }
        self.wire.convert(self.window, manager, target, NONE)?;
        self.handover_sent = true;
        Ok(())
    }
    pub fn handover_pending(&self) -> bool {
        self.handover.is_some()
    }
    fn finish_handover(&mut self) {
        if let Some((_, waiters)) = self.handover.take() {
            for waiter in waiters {
                let _ = waiter.send(());
            }
        }
    }
    pub fn handle_event(&mut self, event: &Event) -> anyhow::Result<bool> {
        match self.try_event(event) {
            Err(error) if pending(&error) => {
                if self.queued_events.len() >= 128 {
                    self.queued_events.pop_front();
                }
                self.queued_events
                    .push_back((Instant::now() + TRANSFER_TIMEOUT, event.clone()));
                Ok(true)
            }
            result => result,
        }
    }
    fn try_event(&mut self, event: &Event) -> anyhow::Result<bool> {
        match event {
            Event::SelectionNotify(event)
                if event.requestor == self.window && self.handover.is_some() =>
            {
                if event.selection == self.wire.atom("CLIPBOARD_MANAGER")?
                    && event.target == self.wire.atom("SAVE_TARGETS")?
                {
                    self.finish_handover();
                }
            }
            Event::XfixesSelectionNotify(event) if event.window == self.window => {
                if let Some(index) = self.index(event.selection) {
                    if self.wire.owner(event.selection)? != event.owner {
                        return Ok(true);
                    }
                    if event.owner != self.selections[index].owner
                        || (event.owner != NONE && event.owner != self.window)
                    {
                        self.replaced(index == 1, event.owner);
                        if event.owner != NONE && event.owner != self.window {
                            let _ = self.read(index == 1);
                        }
                    }
                }
            }
            Event::SelectionClear(event) if event.owner == self.window => {
                if let Some(index) = self.index(event.selection) {
                    let owner = self.wire.owner(event.selection)?;
                    if owner != self.window && self.selections[index].owner == self.window {
                        self.replaced(index == 1, owner);
                    }
                }
            }
            Event::SelectionRequest(request) if request.owner == self.window => {
                self.serve(request)?
            }
            Event::SelectionNotify(event) if self.incoming.contains_key(&event.requestor) => {
                let window = event.requestor;
                let incoming = &self.incoming[&window];
                if event.selection != self.selection_atom(incoming.primary)
                    || event.target != incoming.target
                {
                    return Ok(true);
                }
                let primary = incoming.primary;
                if event.property == NONE {
                    if incoming.target == self.atoms.TARGETS {
                        let types = self.desired_types().map_err(transfer::transport)?;
                        let incoming = self.incoming.get_mut(&window).unwrap();
                        incoming.types = types.into();
                        incoming.probing = true;
                    }
                    if self.incoming[&window].probing {
                        if let Err(error) = self.next_type(window) {
                            self.complete_selection(primary, Err(error));
                        }
                    } else {
                        self.complete_selection(
                            primary,
                            Err(ClipboardReadError::UnsupportedContent),
                        );
                    }
                } else if event.property == self.atoms.GPUI_SELECTION {
                    if let Err(error) = self.receive(window, true) {
                        self.complete_selection(primary, Err(error));
                    }
                }
            }
            Event::PropertyNotify(event) => {
                if event.state == Property::DELETE {
                    if let Some(mut outgoing) = self.outgoing.remove(&(event.window, event.atom)) {
                        if Instant::now() < outgoing.deadline {
                            let end = outgoing.bytes.len().min(outgoing.offset + CHUNK);
                            self.wire.put(
                                event.window,
                                event.atom,
                                outgoing.target,
                                8,
                                &outgoing.bytes[outgoing.offset..end],
                            )?;
                            if end != outgoing.offset {
                                outgoing.offset = end;
                                self.outgoing.insert((event.window, event.atom), outgoing);
                            }
                        }
                    } else {
                        return Ok(false);
                    }
                } else if event.atom == self.atoms.GPUI_SELECTION
                    && self
                        .incoming
                        .get(&event.window)
                        .is_some_and(|incoming| incoming.incr && !incoming.draining)
                {
                    let primary = self.incoming[&event.window].primary;
                    if let Err(error) = self.receive(event.window, false) {
                        self.complete_selection(primary, Err(error));
                    }
                } else {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn serve(&mut self, request: &SelectionRequestEvent) -> anyhow::Result<()> {
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };
        let Some(index) = self.index(request.selection) else {
            return self.wire.notify(request, NONE);
        };
        let Some(snapshot) = self.selections[index].snapshot.as_ref() else {
            return self.wire.notify(request, NONE);
        };
        if request.target == self.atoms.TARGETS {
            let bytes: Vec<_> = std::iter::once(self.atoms.TARGETS)
                .chain(snapshot.iter().map(|(atom, _)| *atom))
                .flat_map(u32::to_ne_bytes)
                .collect();
            self.wire.put(
                request.requestor,
                property,
                AtomEnum::ATOM.into(),
                32,
                &bytes,
            )?;
        } else if let Some((target, bytes)) =
            snapshot.iter().find(|(atom, _)| *atom == request.target)
        {
            let actual_type = if *target == self.wire.atom("TEXT")? {
                if std::str::from_utf8(bytes).is_ok() {
                    self.atoms.UTF8_STRING
                } else if snapshot
                    .iter()
                    .any(|(atom, latin)| *atom == u32::from(AtomEnum::STRING) && latin == bytes)
                {
                    AtomEnum::STRING.into()
                } else {
                    return self.wire.notify(request, NONE);
                }
            } else {
                *target
            };
            if self.outgoing.contains_key(&(request.requestor, property)) {
                return self.wire.notify(request, NONE);
            }
            if bytes.len() > INCR_THRESHOLD {
                if self.outgoing.len() >= MAX_TRANSFERS {
                    return self.wire.notify(request, NONE);
                }
                self.wire.watch_property(request.requestor)?;
                self.wire.put(
                    request.requestor,
                    property,
                    self.atoms.INCR,
                    32,
                    &(bytes.len() as u32).to_ne_bytes(),
                )?;
                self.outgoing.insert(
                    (request.requestor, property),
                    Outgoing {
                        target: actual_type,
                        bytes: bytes.clone(),
                        offset: 0,
                        deadline: Instant::now() + TRANSFER_TIMEOUT,
                    },
                );
            } else {
                self.wire
                    .put(request.requestor, property, actual_type, 8, bytes)?;
            }
        } else {
            return self.wire.notify(request, NONE);
        }
        self.wire.notify(request, property)
    }
    fn receive(&mut self, window: Window, initial: bool) -> Result<(), ClipboardReadError> {
        let incoming = self.incoming.get_mut(&window).unwrap();
        incoming.waiting = Some(initial);
        if Instant::now() >= incoming.deadline {
            return Err(ClipboardReadError::TimedOut);
        }
        let selection = &self.selections[incoming.primary as usize];
        if selection.generation != incoming.generation || selection.owner != incoming.owner {
            return Err(ClipboardReadError::Cancelled);
        }
        let property = match self
            .wire
            .property(window, self.atoms.GPUI_SELECTION, incoming.offset)
        {
            Ok(property) => property,
            Err(error) if pending(&error) => return Ok(()),
            Err(error) => return Err(wire_error(error)),
        };
        incoming.waiting = None;
        if Instant::now() >= incoming.deadline {
            return Err(ClipboardReadError::TimedOut);
        }
        if initial && property.type_ == self.atoms.INCR {
            if property.format != 32 || property.bytes.len() != 4 {
                return Err(ClipboardReadError::UnsupportedContent);
            }
            if u32::from_ne_bytes(property.bytes[..4].try_into().unwrap()) as usize > BYTE_LIMIT {
                return Err(ClipboardReadError::TooLarge);
            }
            incoming.incr = true;
            self.wire
                .delete(window, self.atoms.GPUI_SELECTION)
                .map_err(transfer::transport)?;
            return Ok(());
        }
        let expected_format = if incoming.target == self.atoms.TARGETS {
            32
        } else {
            8
        };
        let expected_type = if incoming.target == self.atoms.TARGETS {
            AtomEnum::ATOM.into()
        } else {
            incoming.target
        };
        let text = incoming.mime.as_deref() == Some("TEXT");
        let supported = if text {
            property.type_ == self.atoms.UTF8_STRING
                || property.type_ == u32::from(AtomEnum::STRING)
        } else {
            property.type_ == expected_type
        };
        if property.format != expected_format
            || !supported
            || incoming
                .actual_type
                .is_some_and(|actual| actual != property.type_)
        {
            return Err(ClipboardReadError::UnsupportedContent);
        }
        incoming.actual_type = Some(property.type_);
        let limit = if incoming.target == self.atoms.TARGETS {
            TYPE_LIMIT * 4
        } else {
            BYTE_LIMIT.saturating_sub(incoming.total)
        };
        if property.bytes.len() + incoming.chunk.len() > limit
            || property.remaining as usize
                > limit.saturating_sub(property.bytes.len() + incoming.chunk.len())
        {
            return Err(ClipboardReadError::TooLarge);
        }
        if incoming.target != self.atoms.TARGETS {
            incoming.permit.reserve(property.bytes.len())?;
        }
        let empty = property.bytes.is_empty();
        incoming.chunk.extend_from_slice(&property.bytes);
        incoming.offset += (property.bytes.len() / 4) as u32;
        if property.remaining != 0 {
            incoming.draining = true;
            return Ok(());
        }
        incoming.offset = 0;
        incoming.draining = false;
        self.wire
            .delete(window, self.atoms.GPUI_SELECTION)
            .map_err(transfer::transport)?;
        if incoming.incr && !empty {
            return Ok(());
        }
        incoming.incr = false;
        if incoming.target == self.atoms.TARGETS {
            incoming.available = incoming
                .chunk
                .chunks_exact(4)
                .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
                .collect();
            incoming.chunk.clear();
            incoming.naming = true;
            return self.resolve_types(window);
        } else {
            if incoming.probing {
                incoming.types.clear();
            }
            incoming.total += incoming.chunk.len();
            let mime = incoming.mime.take().unwrap();
            let mime = if mime == "TEXT" {
                if incoming.actual_type == Some(AtomEnum::STRING.into()) {
                    "STRING".into()
                } else {
                    "UTF8_STRING".into()
                }
            } else {
                mime
            };
            incoming.representations.push(MimeRepresentation::new(
                mime,
                std::mem::take(&mut incoming.chunk),
            ));
        }
        self.advance_type(window)
    }
    fn resolve_types(&mut self, window: Window) -> Result<(), ClipboardReadError> {
        let incoming = self.incoming.get_mut(&window).unwrap();
        if Instant::now() >= incoming.deadline {
            return Err(ClipboardReadError::TimedOut);
        }
        while incoming.names.len() < incoming.available.len() {
            let atom = incoming.available[incoming.names.len()];
            let name = match self.wire.name(atom) {
                Ok(name) => name,
                Err(error) if pending(&error) => return Ok(()),
                Err(error) => return Err(wire_error(error)),
            };
            incoming.names.push((atom, name));
        }
        let preferred = preferred_mime_types();
        if let Some((atom, name)) = preferred
            .iter()
            .find_map(|name| incoming.names.iter().find(|(_, offered)| offered == name))
        {
            incoming.types.push_back((*atom, name.clone()));
        }
        for (atom, name) in &incoming.names {
            if name.contains('/')
                && name.len() <= 4096
                && !preferred.contains(name)
                && !name.starts_with("pid/")
                && !incoming.types.iter().any(|(_, selected)| selected == name)
            {
                incoming.types.push_back((*atom, name.clone()));
            }
        }
        incoming.naming = false;
        self.advance_type(window)
    }
    fn advance_type(&mut self, window: Window) -> Result<(), ClipboardReadError> {
        let incoming = self.incoming.get_mut(&window).unwrap();
        if let Some((target, mime)) = incoming.types.pop_front() {
            incoming.target = target;
            incoming.actual_type = None;
            incoming.mime = Some(mime);
            let primary = incoming.primary;
            self.wire
                .convert(
                    window,
                    self.selection_atom(primary),
                    target,
                    self.atoms.GPUI_SELECTION,
                )
                .map_err(transfer::transport)?;
        } else {
            let primary = incoming.primary;
            if incoming.representations.is_empty() {
                self.complete_selection(primary, Err(ClipboardReadError::UnsupportedContent));
            } else {
                let generation = incoming.generation;
                let deadline = incoming.deadline;
                let incoming = self.incoming.remove(&window).unwrap();
                let slot = Arc::new(Mutex::new(Some(DecodeInput {
                    representations: incoming.representations,
                    offered_types: incoming
                        .names
                        .into_iter()
                        .map(|(_, name)| name)
                        .filter(|name| !name.is_empty())
                        .collect(),
                    permit: incoming.permit,
                })));
                self.wire.destroy(window);
                let selection = &mut self.selections[primary as usize];
                selection.pending = None;
                selection.decoding = true;
                selection.decode_deadline = Some(deadline);
                selection.queued_decode = Some(slot.clone());
                self.decode_jobs.retain(DecodeJob::live);
                self.decode_jobs.push(DecodeJob {
                    primary,
                    generation,
                    cancel: selection.cancel.clone(),
                    deadline,
                    slot,
                });
            }
        }
        Ok(())
    }
    fn desired_types(&self) -> anyhow::Result<Vec<(Atom, String)>> {
        let names = preferred_mime_types();
        names
            .into_iter()
            .map(|name| self.wire.atom(&name).map(|atom| (atom, name)))
            .collect()
    }
    fn next_type(&mut self, window: Window) -> Result<(), ClipboardReadError> {
        let incoming = self.incoming.get_mut(&window).unwrap();
        let Some((target, mime)) = incoming.types.pop_front() else {
            return Err(ClipboardReadError::UnsupportedContent);
        };
        incoming.target = target;
        incoming.actual_type = None;
        incoming.mime = Some(mime);
        let primary = incoming.primary;
        self.wire
            .convert(
                window,
                self.selection_atom(primary),
                target,
                self.atoms.GPUI_SELECTION,
            )
            .map_err(transfer::transport)
    }
    pub fn active(&self) -> bool {
        !self.initialized
            || self.initial_owners.iter().any(|pending| *pending)
            || !self.queued_events.is_empty()
            || self.queued_sets.iter().any(Option::is_some)
            || self.clear_pending.is_some()
            || self
                .selections
                .iter()
                .any(|selection| selection.requested.is_some())
            || !self.incoming.is_empty()
            || !self.outgoing.is_empty()
            || self.handover.is_some()
    }
    pub fn tick(&mut self) {
        if !self.initialized {
            if Instant::now() >= self.init_deadline {
                self.initialized = true;
                self.failed = true;
                self.initial_owners = [false, false];
                self.queued_sets = [None, None];
                self.queued_events.clear();
                for primary in [false, true] {
                    self.complete_selection(primary, Err(ClipboardReadError::TimedOut));
                }
                return;
            }
            if let Err(error) = self.initialize() {
                if !pending(&error) {
                    log::error!("X11 clipboard initialization: {error}");
                }
            }
            if !self.initialized {
                return;
            }
        }
        for primary in [false, true] {
            if self.initial_owners[primary as usize] {
                if let Err(error) = self.initial_owner(primary) {
                    if !pending(&error) {
                        self.initial_owners[primary as usize] = false;
                        self.complete_selection(primary, Err(wire_error(error)));
                    }
                }
            }
            if let Some((deadline, _)) = self.queued_sets[primary as usize] {
                if Instant::now() >= deadline {
                    self.queued_sets[primary as usize] = None;
                } else if let Err(error) = self.try_set(primary) {
                    if !pending(&error) {
                        self.queued_sets[primary as usize] = None;
                    }
                }
            }
            if self.selections[primary as usize]
                .requested
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.complete_selection(primary, Err(ClipboardReadError::TimedOut));
            } else {
                self.drive_read(primary);
            }
        }
        if let Some(deadline) = self.clear_pending {
            if Instant::now() >= deadline {
                self.clear_pending = None;
            } else {
                let _ = self.try_clear();
            }
        }
        let events = std::mem::take(&mut self.queued_events);
        for (deadline, event) in events {
            if Instant::now() < deadline {
                if let Err(error) = self.try_event(&event) {
                    if pending(&error) {
                        self.queued_events.push_back((deadline, event));
                    }
                }
            }
        }
        if self.handover.is_some() && !self.handover_sent {
            if let Err(error) = self.setup_handover() {
                if !pending(&error) {
                    self.finish_handover();
                }
            }
        }

        if self
            .handover
            .as_ref()
            .is_some_and(|(deadline, _)| Instant::now() >= *deadline)
        {
            self.finish_handover();
        }
        self.outgoing
            .retain(|_, transfer| Instant::now() < transfer.deadline);
        let windows: Vec<_> = self.incoming.keys().copied().collect();
        for window in windows {
            let Some(incoming) = self.incoming.get(&window) else {
                continue;
            };
            let primary = incoming.primary;
            if Instant::now() >= incoming.deadline {
                self.complete_selection(primary, Err(ClipboardReadError::TimedOut));
            } else if incoming.naming {
                if let Err(error) = self.resolve_types(window) {
                    self.complete_selection(primary, Err(error));
                }
            } else if (incoming.draining || incoming.waiting.is_some())
                && let Err(error) = self.receive(window, incoming.waiting.unwrap_or(false))
            {
                self.complete_selection(primary, Err(error));
            }
        }
    }
}
impl<W: SelectionWire> Drop for Clipboard<W> {
    fn drop(&mut self) {
        for window in self.incoming.keys() {
            self.wire.destroy(*window);
        }
        self.wire.destroy(self.window);
    }
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
