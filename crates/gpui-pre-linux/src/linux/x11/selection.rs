use crate::linux::transfer::{self, BYTE_LIMIT, TRANSFER_TIMEOUT, TYPE_LIMIT};
use futures::channel::oneshot;
use gpui::{ClipboardChange, ClipboardItem, ClipboardReadError, MimePayload, MimeRepresentation};
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
    time::Instant,
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        Event,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::{
            self, Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
            Property, SelectionNotifyEvent, SelectionRequestEvent, Window,
        },
    },
    wrapper::ConnectionExt as _,
    xcb_ffi::XCBConnection,
};

const CHUNK: usize = 64 * 1024;
const INCR_THRESHOLD: usize = 256 * 1024;
const MAX_TRANSFERS: usize = 16;
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
    fn atom(&self, name: &str) -> anyhow::Result<Atom>;
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

pub(crate) struct Server {
    connection: Rc<XCBConnection>,
    root: Window,
}
impl SelectionWire for Server {
    fn atom(&self, name: &str) -> anyhow::Result<Atom> {
        Ok(self
            .connection
            .intern_atom(false, name.as_bytes())?
            .reply()?
            .atom)
    }
    fn window(&self) -> anyhow::Result<Window> {
        let window = self.connection.generate_id()?;
        self.connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                self.root,
                0,
                0,
                1,
                1,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()?;
        Ok(window)
    }
    fn destroy(&self, window: Window) {
        let _ = self.connection.destroy_window(window);
        let _ = self.connection.flush();
    }
    fn owner(&self, selection: Atom) -> anyhow::Result<Window> {
        Ok(self
            .connection
            .get_selection_owner(selection)?
            .reply()?
            .owner)
    }
    fn own(&self, owner: Window, selection: Atom) -> anyhow::Result<()> {
        self.connection
            .set_selection_owner(owner, selection, CURRENT_TIME)?;
        self.connection.flush()?;
        Ok(())
    }
    fn convert(
        &self,
        window: Window,
        selection: Atom,
        target: Atom,
        property: Atom,
    ) -> anyhow::Result<()> {
        self.connection
            .convert_selection(window, selection, target, property, CURRENT_TIME)?;
        self.connection.flush()?;
        Ok(())
    }
    fn property(
        &self,
        window: Window,
        property: Atom,
        offset: u32,
    ) -> anyhow::Result<PropertyData> {
        let reply = self
            .connection
            .get_property(
                false,
                window,
                property,
                AtomEnum::ANY,
                offset,
                (CHUNK / 4) as u32,
            )?
            .reply()?;
        Ok(PropertyData {
            type_: reply.type_,
            format: reply.format,
            bytes: reply.value,
            remaining: reply.bytes_after,
        })
    }
    fn delete(&self, window: Window, property: Atom) -> anyhow::Result<()> {
        self.connection.delete_property(window, property)?;
        self.connection.flush()?;
        Ok(())
    }
    fn put(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        format: u8,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        if format == 32 {
            let words: Vec<_> = bytes
                .chunks_exact(4)
                .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
                .collect();
            self.connection.change_property32(
                PropMode::REPLACE,
                window,
                property,
                type_,
                &words,
            )?;
        } else {
            self.connection
                .change_property8(PropMode::REPLACE, window, property, type_, bytes)?;
        }
        self.connection.flush()?;
        Ok(())
    }
    fn notify(&self, request: &SelectionRequestEvent, property: Atom) -> anyhow::Result<()> {
        let event = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        self.connection
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)?;
        self.connection.flush()?;
        Ok(())
    }
    fn watch_property(&self, window: Window) -> anyhow::Result<()> {
        self.connection.change_window_attributes(
            window,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        Ok(())
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
    mime: Option<String>,
    types: VecDeque<(Atom, String)>,
    representations: Vec<MimeRepresentation>,
    chunk: Vec<u8>,
    total: usize,
    incr: bool,
    offset: u32,
    draining: bool,
}

pub(crate) struct Clipboard<W: SelectionWire = Server> {
    wire: W,
    window: Window,
    atoms: Atoms,
    selections: [Selection; 2],
    outgoing: HashMap<(Window, Atom), Outgoing>,
    incoming: HashMap<Window, Incoming>,
    pub changes: Vec<ClipboardChange>,
    pub decode_jobs: Vec<(bool, u64, Vec<MimeRepresentation>)>,
    handover: Option<(Instant, Vec<oneshot::Sender<()>>)>,
}
impl Clipboard<Server> {
    pub fn new(connection: Rc<XCBConnection>, root: Window) -> anyhow::Result<Self> {
        let atoms = Atoms::new(&*connection)?.reply()?;
        connection.xfixes_query_version(5, 0)?.reply()?;
        let server = Server { connection, root };
        let window = server.window()?;
        for selection in [atoms.CLIPBOARD, atoms.PRIMARY] {
            server
                .connection
                .xfixes_select_selection_input(
                    window,
                    selection,
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )?
                .check()?;
        }
        let mut selections = [Selection::default(), Selection::default()];
        for (index, selection) in [atoms.CLIPBOARD, atoms.PRIMARY].into_iter().enumerate() {
            selections[index].owner = server.owner(selection)?;
        }
        Ok(Self {
            wire: server,
            window,
            atoms,
            selections,
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            changes: Vec::new(),
            decode_jobs: Vec::new(),
            handover: None,
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
    pub fn clear_if_owned(&mut self) -> Result<bool, ClipboardReadError> {
        if self
            .wire
            .owner(self.atoms.CLIPBOARD)
            .map_err(transfer::transport)?
            != self.window
        {
            return Ok(false);
        }
        self.wire
            .own(NONE, self.atoms.CLIPBOARD)
            .map_err(transfer::transport)?;
        self.replaced(false, NONE);
        Ok(true)
    }
    pub fn set(&mut self, primary: bool, item: ClipboardItem) -> Result<(), ClipboardReadError> {
        let payload = transfer::snapshot(&item)?;
        let representations = self.intern(&payload)?;
        self.wire
            .own(self.window, self.selection_atom(primary))
            .map_err(transfer::transport)?;
        self.replaced(primary, self.window);
        let selection = &mut self.selections[primary as usize];
        selection.item = Some(item);
        selection.snapshot = Some(representations);
        Ok(())
    }
    fn intern(&self, payload: &MimePayload) -> Result<Vec<(Atom, Arc<[u8]>)>, ClipboardReadError> {
        payload
            .representations()
            .iter()
            .map(|representation| {
                self.wire
                    .atom(representation.mime_type())
                    .map(|atom| (atom, representation.bytes().clone()))
                    .map_err(transfer::transport)
            })
            .collect()
    }
    fn replaced(&mut self, primary: bool, owner: Window) {
        let selection = &mut self.selections[primary as usize];
        selection.decoding = false;
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
        if let Some(item) = self.cached(primary) {
            let _ = sender.send(Ok(Some(item)));
            return receiver;
        }
        if self.selections[primary as usize].owner == NONE {
            let _ = sender.send(Ok(None));
            return receiver;
        }
        self.selections[primary as usize].waiters.push(sender);
        if self.selections[primary as usize].pending.is_none()
            && !self.selections[primary as usize].decoding
            && let Err(error) = self.begin(primary)
        {
            self.complete_selection(primary, Err(error));
        }
        receiver
    }
    fn begin(&mut self, primary: bool) -> Result<(), ClipboardReadError> {
        if self.incoming.len() >= MAX_TRANSFERS {
            return Err(ClipboardReadError::Unavailable);
        }
        let window = self.wire.window().map_err(transfer::transport)?;
        let selection = &self.selections[primary as usize];
        let incoming = Incoming {
            primary,
            generation: selection.generation,
            owner: selection.owner,
            deadline: Instant::now() + TRANSFER_TIMEOUT,
            target: self.atoms.TARGETS,
            mime: None,
            types: VecDeque::new(),
            representations: Vec::new(),
            chunk: Vec::new(),
            total: 0,
            incr: false,
            offset: 0,
            draining: false,
        };
        self.incoming.insert(window, incoming);
        self.selections[primary as usize].pending = Some(window);
        self.wire
            .convert(
                window,
                self.selection_atom(primary),
                self.atoms.TARGETS,
                self.atoms.GPUI_SELECTION,
            )
            .map_err(transfer::transport)
    }
    fn complete_selection(&mut self, primary: bool, result: ReadResult) {
        let selection = &mut self.selections[primary as usize];
        selection.decoding = false;
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
    pub fn finish_decode(
        &mut self,
        primary: bool,
        generation: u64,
        result: Result<ClipboardItem, ClipboardReadError>,
    ) {
        if self.selections[primary as usize].generation == generation {
            self.complete_selection(primary, result.map(Some));
        }
    }
    pub fn begin_handover(&mut self) -> anyhow::Result<Option<oneshot::Receiver<()>>> {
        if self.wire.owner(self.atoms.CLIPBOARD)? != self.window {
            return Ok(None);
        }
        let manager = self.wire.atom("CLIPBOARD_MANAGER")?;
        if self.wire.owner(manager)? == NONE {
            return Ok(None);
        }
        let (sender, receiver) = oneshot::channel();
        if let Some((_, waiters)) = self.handover.as_mut() {
            if waiters.len() < MAX_TRANSFERS {
                waiters.push(sender);
            }
            return Ok(Some(receiver));
        }
        let target = self.wire.atom("SAVE_TARGETS")?;
        self.wire.convert(self.window, manager, target, NONE)?;
        self.handover = Some((Instant::now() + TRANSFER_TIMEOUT, vec![sender]));
        Ok(Some(receiver))
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
                    if event.owner != self.selections[index].owner {
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
                    self.complete_selection(primary, Err(ClipboardReadError::UnsupportedContent));
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
                        target: *target,
                        bytes: bytes.clone(),
                        offset: 0,
                        deadline: Instant::now() + TRANSFER_TIMEOUT,
                    },
                );
            } else {
                self.wire
                    .put(request.requestor, property, *target, 8, bytes)?;
            }
        } else {
            return self.wire.notify(request, NONE);
        }
        self.wire.notify(request, property)
    }
    fn receive(&mut self, window: Window, initial: bool) -> Result<(), ClipboardReadError> {
        let incoming = self.incoming.get_mut(&window).unwrap();
        if Instant::now() >= incoming.deadline {
            return Err(ClipboardReadError::TimedOut);
        }
        let selection = &self.selections[incoming.primary as usize];
        if selection.generation != incoming.generation || selection.owner != incoming.owner {
            return Err(ClipboardReadError::Cancelled);
        }
        let property = self
            .wire
            .property(window, self.atoms.GPUI_SELECTION, incoming.offset)
            .map_err(transfer::transport)?;
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
        if property.format != expected_format || property.type_ != expected_type {
            return Err(ClipboardReadError::UnsupportedContent);
        }
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
            let mut available = Vec::new();
            for bytes in incoming.chunk.chunks_exact(4) {
                let atom = u32::from_ne_bytes(bytes.try_into().unwrap());
                available.push(atom);
            }
            incoming.chunk.clear();
            let mut desired = vec![
                transfer::GNOME_FILES.to_owned(),
                transfer::URI_LIST.to_owned(),
            ];
            use strum::IntoEnumIterator;
            desired.extend(gpui::ImageFormat::iter().map(|format| format.mime_type().to_owned()));
            desired.extend(transfer::TEXT_TYPES.map(str::to_owned));
            desired.push("STRING".to_owned());
            for mime in desired {
                let atom = self.wire.atom(&mime).map_err(transfer::transport)?;
                if available.contains(&atom) {
                    incoming.types.push_back((atom, mime));
                    break;
                }
            }
        } else {
            incoming.total += incoming.chunk.len();
            incoming.representations.push(MimeRepresentation::new(
                incoming.mime.take().unwrap(),
                std::mem::take(&mut incoming.chunk),
            ));
        }
        if let Some((target, mime)) = incoming.types.pop_front() {
            incoming.target = target;
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
                let representations = std::mem::take(&mut incoming.representations);
                self.incoming.remove(&window);
                self.wire.destroy(window);
                let selection = &mut self.selections[primary as usize];
                selection.pending = None;
                selection.decoding = true;
                self.decode_jobs
                    .push((primary, generation, representations));
            }
        }
        Ok(())
    }
    pub fn active(&self) -> bool {
        !self.incoming.is_empty() || !self.outgoing.is_empty() || self.handover.is_some()
    }
    pub fn tick(&mut self) {
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
            } else if incoming.draining
                && let Err(error) = self.receive(window, false)
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
