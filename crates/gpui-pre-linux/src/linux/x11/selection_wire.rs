use super::selection::{CHUNK, PropertyData, SelectionWire};
use crate::linux::transfer::TRANSFER_TIMEOUT;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME,
    connection::{Connection, DiscardMode, RequestConnection, RequestKind},
    protocol::{
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::{
            self, Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
            SelectionRequestEvent, Window,
        },
    },
    wrapper::ConnectionExt as _,
    x11_utils::TryParse,
    xcb_ffi::XCBConnection,
};

#[derive(Debug)]
pub(super) struct Pending;
impl std::fmt::Display for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("X11 reply pending")
    }
}
impl std::error::Error for Pending {}
pub(super) fn pending(error: &anyhow::Error) -> bool {
    error.is::<Pending>()
}

type Work = Box<dyn FnOnce(&XCBConnection, &mut Vec<Reply>) + Send>;
type Completion = Box<dyn FnOnce(anyhow::Result<Vec<u8>>) + Send>;
struct Reply {
    sequence: u64,
    deadline: Instant,
    complete: Completion,
}
#[derive(Hash, PartialEq, Eq, Clone)]
enum Query {
    Atom(String),
    FixesVersion,
    Name(Atom),
    Owner(Atom),
    Window,
    Property(Window, Atom, u32),
}
enum Value {
    Atom(Atom),
    Name(String),
    Window(Window),
    Property(PropertyData),
}
type Answer = mpsc::Receiver<anyhow::Result<Value>>;

#[link(name = "xcb")]
unsafe extern "C" {
    fn xcb_poll_for_reply64(
        connection: *mut std::ffi::c_void,
        sequence: u64,
        reply: *mut *mut u8,
        error: *mut *mut u8,
    ) -> i32;
}
fn poll(connection: &XCBConnection, sequence: u64) -> Option<anyhow::Result<Vec<u8>>> {
    let mut reply = std::ptr::null_mut();
    let mut error = std::ptr::null_mut();
    if unsafe {
        xcb_poll_for_reply64(
            connection.get_raw_xcb_connection(),
            sequence,
            &mut reply,
            &mut error,
        )
    } == 0
    {
        return None;
    }
    let result = if !error.is_null() {
        Err(anyhow::anyhow!(
            "X11 selection request failed with code {}",
            unsafe { *error.add(1) }
        ))
    } else if reply.is_null() {
        Err(anyhow::anyhow!("X11 selection connection closed"))
    } else {
        let header = unsafe { std::slice::from_raw_parts(reply, 32) };
        let length = 32 + u32::from_ne_bytes(header[4..8].try_into().unwrap()) as usize * 4;
        Ok(unsafe { std::slice::from_raw_parts(reply, length) }.to_vec())
    };
    unsafe {
        libc::free(reply.cast());
        libc::free(error.cast());
    }
    Some(result)
}
fn remember<R: TryParse + Send + 'static>(
    cookie: x11rb::cookie::Cookie<'_, XCBConnection, R>,
    deadline: Instant,
    replies: &mut Vec<Reply>,
    sender: mpsc::SyncSender<anyhow::Result<Value>>,
    map: impl FnOnce(R) -> Value + Send + 'static,
) {
    let sequence = cookie.sequence_number();
    std::mem::forget(cookie);
    replies.push(Reply {
        sequence,
        deadline,
        complete: Box::new(move |bytes| {
            let result = bytes.and_then(|bytes| {
                R::try_parse(&bytes)
                    .map(|(reply, _)| map(reply))
                    .map_err(Into::into)
            });
            let _ = sender.try_send(result);
        }),
    });
}

pub(crate) struct Server {
    work: mpsc::SyncSender<Work>,
    event_wakeup: Option<calloop::channel::Channel<()>>,
    queries: RefCell<HashMap<Query, Answer>>,
    atoms: RefCell<HashMap<String, Atom>>,
    root: Window,
    fixes_ready: Cell<bool>,
}
impl Server {
    pub fn new(connection: Arc<XCBConnection>, root: Window) -> anyhow::Result<Self> {
        let (work, receiver) = mpsc::sync_channel::<Work>(128);
        let (wake, event_wakeup) = calloop::channel::sync_channel(1);
        std::thread::Builder::new()
            .name("x11-selection".into())
            .spawn(move || {
                let mut replies = Vec::<Reply>::new();
                loop {
                    let work = if replies.is_empty() {
                        receiver
                            .recv()
                            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    } else {
                        receiver.recv_timeout(Duration::from_millis(1))
                    };
                    match work {
                        Ok(work) => {
                            work(&connection, &mut replies);
                            // Only this worker flushes selection requests; the UI never waits on socket writes.
                            if connection.flush().is_err() {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    // Flush and reply polling can both pull events off the shared socket.
                    // Wake the foreground drain even when no reply was ready. The bounded
                    // channel coalesces wakes without blocking or relying on socket readiness.
                    let mut index = 0;
                    while index < replies.len() {
                        let result = if Instant::now() >= replies[index].deadline {
                            connection.discard_reply(
                                replies[index].sequence,
                                RequestKind::HasResponse,
                                DiscardMode::DiscardReplyAndError,
                            );
                            Some(Err(gpui::ClipboardReadError::TimedOut.into()))
                        } else {
                            poll(&connection, replies[index].sequence)
                        };
                        if let Some(result) = result {
                            (replies.swap_remove(index).complete)(result);
                        } else {
                            index += 1;
                        }
                    }
                    let _ = wake.try_send(());
                }
                for reply in replies {
                    connection.discard_reply(
                        reply.sequence,
                        RequestKind::HasResponse,
                        DiscardMode::DiscardReplyAndError,
                    );
                }
            })?;
        Ok(Self {
            work,
            event_wakeup: Some(event_wakeup),
            queries: RefCell::new(HashMap::new()),
            atoms: RefCell::new(HashMap::new()),
            root,
            fixes_ready: Cell::new(false),
        })
    }
    pub fn register_event_wakeup(
        &mut self,
        handle: &calloop::LoopHandle<'static, ()>,
        mut drain: impl FnMut() + 'static,
    ) -> anyhow::Result<calloop::RegistrationToken> {
        let channel = self
            .event_wakeup
            .take()
            .expect("X11 event wakeup registered once");
        handle
            .insert_source(channel, move |event, _, _| {
                if let calloop::channel::Event::Msg(()) = event {
                    drain();
                }
            })
            .map_err(|error| anyhow::anyhow!("Failed to register X11 selection wakeup: {error}"))
    }
    fn enqueue(
        &self,
        work: impl FnOnce(&XCBConnection, &mut Vec<Reply>) + Send + 'static,
    ) -> anyhow::Result<()> {
        self.work
            .try_send(Box::new(work))
            .map_err(|_| gpui::ClipboardReadError::Unavailable.into())
    }
    fn query(
        &self,
        key: Query,
        work: impl FnOnce(
            &XCBConnection,
            &mut Vec<Reply>,
            mpsc::SyncSender<anyhow::Result<Value>>,
            Instant,
        ) + Send
        + 'static,
    ) -> anyhow::Result<Value> {
        let mut queries = self.queries.borrow_mut();
        if let Some(receiver) = queries.get(&key) {
            match receiver.try_recv() {
                Ok(result) => {
                    queries.remove(&key);
                    return result;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    queries.remove(&key);
                    anyhow::bail!("X11 selection worker stopped");
                }
                Err(mpsc::TryRecvError::Empty) => return Err(Pending.into()),
            }
        }
        if queries.len() >= 128 {
            return Err(gpui::ClipboardReadError::Unavailable.into());
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let deadline = Instant::now() + TRANSFER_TIMEOUT;
        self.enqueue(move |connection, replies| {
            if Instant::now() < deadline {
                work(connection, replies, sender, deadline);
            }
        })?;
        queries.insert(key, receiver);
        Err(Pending.into())
    }
}
impl SelectionWire for Server {
    fn atom(&self, name: &str) -> anyhow::Result<Atom> {
        if let Some(atom) = self.atoms.borrow().get(name) {
            return Ok(*atom);
        }
        let name_owned = name.to_owned();
        let Value::Atom(atom) = self.query(
            Query::Atom(name_owned.clone()),
            move |connection, replies, sender, deadline| match connection
                .intern_atom(false, name_owned.as_bytes())
            {
                Ok(cookie) => remember(cookie, deadline, replies, sender, |reply| {
                    Value::Atom(reply.atom)
                }),
                Err(error) => {
                    let _ = sender.try_send(Err(error.into()));
                }
            },
        )?
        else {
            unreachable!()
        };
        if self.atoms.borrow().len() >= 4096 {
            self.atoms.borrow_mut().clear();
        }
        self.atoms.borrow_mut().insert(name.into(), atom);
        Ok(atom)
    }
    fn name(&self, atom: Atom) -> anyhow::Result<String> {
        let Value::Name(name) = self.query(
            Query::Name(atom),
            move |connection, replies, sender, deadline| match connection.get_atom_name(atom) {
                Ok(cookie) => remember(cookie, deadline, replies, sender, |reply| {
                    Value::Name(if reply.name.len() <= 4096 {
                        String::from_utf8(reply.name).unwrap_or_default()
                    } else {
                        String::new()
                    })
                }),
                Err(error) => {
                    let _ = sender.try_send(Err(error.into()));
                }
            },
        )?
        else {
            unreachable!()
        };
        Ok(name)
    }
    fn window(&self) -> anyhow::Result<Window> {
        let root = self.root;
        let Value::Window(window) =
            self.query(Query::Window, move |connection, _, sender, _| {
                let result = (|| {
                    let window = connection.generate_id()?;
                    connection
                        .create_window(
                            COPY_DEPTH_FROM_PARENT,
                            window,
                            root,
                            0,
                            0,
                            1,
                            1,
                            0,
                            xproto::WindowClass::INPUT_OUTPUT,
                            0,
                            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                        )?
                        .ignore_error();
                    Ok(Value::Window(window))
                })();
                let _ = sender.try_send(result);
            })?
        else {
            unreachable!()
        };
        Ok(window)
    }
    fn destroy(&self, window: Window) {
        self.queries
            .borrow_mut()
            .retain(|key, _| !matches!(key, Query::Property(w, _, _) if *w == window));
        let _ = self.enqueue(move |connection, _| {
            let _ = connection
                .destroy_window(window)
                .map(|cookie| cookie.ignore_error());
        });
    }
    fn watch_selection(&self, window: Window, selection: Atom) -> anyhow::Result<()> {
        if !self.fixes_ready.get() {
            let Value::Atom(version) = self.query(
                Query::FixesVersion,
                |connection, replies, sender, deadline| match connection.xfixes_query_version(5, 0)
                {
                    Ok(cookie) => remember(cookie, deadline, replies, sender, |reply| {
                        Value::Atom(reply.major_version)
                    }),
                    Err(error) => {
                        let _ = sender.try_send(Err(error.into()));
                    }
                },
            )?
            else {
                unreachable!()
            };
            anyhow::ensure!(version >= 1, "XFixes selection tracking is unavailable");
            self.fixes_ready.set(true);
        }
        self.enqueue(move |connection, _| {
            let _ = connection
                .xfixes_select_selection_input(
                    window,
                    selection,
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )
                .map(|cookie| cookie.ignore_error());
        })
    }
    fn owner(&self, selection: Atom) -> anyhow::Result<Window> {
        let Value::Window(window) = self.query(
            Query::Owner(selection),
            move |connection, replies, sender, deadline| match connection
                .get_selection_owner(selection)
            {
                Ok(cookie) => remember(cookie, deadline, replies, sender, |reply| {
                    Value::Window(reply.owner)
                }),
                Err(error) => {
                    let _ = sender.try_send(Err(error.into()));
                }
            },
        )?
        else {
            unreachable!()
        };
        Ok(window)
    }
    fn own(&self, owner: Window, selection: Atom) -> anyhow::Result<()> {
        self.enqueue(move |connection, _| {
            let _ = connection
                .set_selection_owner(owner, selection, CURRENT_TIME)
                .map(|cookie| cookie.ignore_error());
        })
    }
    fn convert(
        &self,
        window: Window,
        selection: Atom,
        target: Atom,
        property: Atom,
    ) -> anyhow::Result<()> {
        self.enqueue(move |connection, _| {
            let _ = connection
                .convert_selection(window, selection, target, property, CURRENT_TIME)
                .map(|cookie| cookie.ignore_error());
        })
    }
    fn property(
        &self,
        window: Window,
        property: Atom,
        offset: u32,
    ) -> anyhow::Result<PropertyData> {
        let Value::Property(data) = self.query(
            Query::Property(window, property, offset),
            move |connection, replies, sender, deadline| match connection.get_property(
                false,
                window,
                property,
                AtomEnum::ANY,
                offset,
                (CHUNK / 4) as u32,
            ) {
                Ok(cookie) => remember(cookie, deadline, replies, sender, |reply| {
                    Value::Property(PropertyData {
                        type_: reply.type_,
                        format: reply.format,
                        bytes: reply.value,
                        remaining: reply.bytes_after,
                    })
                }),
                Err(error) => {
                    let _ = sender.try_send(Err(error.into()));
                }
            },
        )?
        else {
            unreachable!()
        };
        Ok(data)
    }
    fn delete(&self, window: Window, property: Atom) -> anyhow::Result<()> {
        self.enqueue(move |connection, _| {
            let _ = connection
                .delete_property(window, property)
                .map(|cookie| cookie.ignore_error());
        })
    }
    fn put(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        format: u8,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let bytes = bytes.to_vec();
        self.enqueue(move |connection, _| {
            if format == 32 {
                let words: Vec<_> = bytes
                    .chunks_exact(4)
                    .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
                    .collect();
                let _ = connection
                    .change_property32(PropMode::REPLACE, window, property, type_, &words)
                    .map(|cookie| cookie.ignore_error());
            } else {
                let _ = connection
                    .change_property8(PropMode::REPLACE, window, property, type_, &bytes)
                    .map(|cookie| cookie.ignore_error());
            }
        })
    }
    fn notify(&self, request: &SelectionRequestEvent, property: Atom) -> anyhow::Result<()> {
        let request = *request;
        self.enqueue(move |connection, _| {
            let event = xproto::SelectionNotifyEvent {
                response_type: xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: request.time,
                requestor: request.requestor,
                selection: request.selection,
                target: request.target,
                property,
            };
            let _ = connection
                .send_event(false, request.requestor, EventMask::NO_EVENT, event)
                .map(|cookie| cookie.ignore_error());
        })
    }
    fn watch_property(&self, window: Window) -> anyhow::Result<()> {
        self.enqueue(move |connection, _| {
            let _ = connection
                .change_window_attributes(
                    window,
                    &xproto::ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE),
                )
                .map(|cookie| cookie.ignore_error());
        })
    }
}

#[cfg(test)]
mod review_event_wakeup {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, IntoRawFd},
        os::unix::net::UnixStream,
    };

    #[link(name = "xcb")]
    unsafe extern "C" {
        fn xcb_connect_to_fd(fd: i32, auth: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }

    #[test]
    fn review_r2_reply_worker_preserves_selection_event_wakeup() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let fake = std::thread::spawn(move || {
            let mut hello = [0u8; 12];
            peer.read_exact(&mut hello).unwrap();
            assert_eq!(hello[0], b'l');
            let name = u16::from_ne_bytes(hello[6..8].try_into().unwrap()) as usize;
            let data = u16::from_ne_bytes(hello[8..10].try_into().unwrap()) as usize;
            let mut auth = vec![0; name.next_multiple_of(4) + data.next_multiple_of(4)];
            peer.read_exact(&mut auth).unwrap();
            let mut setup = [0u8; 40];
            setup[0] = 1;
            setup[2..4].copy_from_slice(&11u16.to_ne_bytes());
            setup[6..8].copy_from_slice(&8u16.to_ne_bytes());
            setup[12..16].copy_from_slice(&0x20_0000u32.to_ne_bytes());
            setup[16..20].copy_from_slice(&0x1f_ffffu32.to_ne_bytes());
            setup[26..28].copy_from_slice(&65535u16.to_ne_bytes());
            setup[32] = 32;
            setup[33] = 32;
            setup[34] = 8;
            setup[35] = 255;
            peer.write_all(&setup).unwrap();
            let mut sequence = 0u16;
            loop {
                let mut header = [0u8; 4];
                if peer.read_exact(&mut header).is_err() {
                    break;
                }
                sequence += 1;
                let length = u16::from_ne_bytes(header[2..4].try_into().unwrap()) as usize * 4;
                let mut body = vec![0; length - 4];
                peer.read_exact(&mut body).unwrap();
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&sequence.to_ne_bytes());
                if header[0] == 16 {
                    reply[8..12].copy_from_slice(&42u32.to_ne_bytes());
                    let mut event = [0u8; 32];
                    event[0] = 31;
                    event[2..4].copy_from_slice(&sequence.to_ne_bytes());
                    event[8..12].copy_from_slice(&50u32.to_ne_bytes());
                    event[12..16].copy_from_slice(&1u32.to_ne_bytes());
                    event[16..20].copy_from_slice(&12u32.to_ne_bytes());
                    event[20..24].copy_from_slice(&6u32.to_ne_bytes());
                    peer.write_all(&[event, reply].concat()).unwrap();
                } else {
                    assert_eq!(header[0], 98, "unexpected fake-server opcode");
                    peer.write_all(&reply).unwrap();
                }
            }
        });
        let fd = socket.as_raw_fd();
        let raw = unsafe { xcb_connect_to_fd(socket.into_raw_fd(), std::ptr::null_mut()) };
        let connection =
            Arc::new(unsafe { XCBConnection::from_raw_xcb_connection(raw, true).unwrap() });
        let mut wire = Server::new(connection.clone(), 50).unwrap();
        let mut event_loop = calloop::EventLoop::try_new().unwrap();
        let events = std::rc::Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        let drain_connection = connection.clone();
        wire.register_event_wakeup(&event_loop.handle(), move || {
            while let Some(event) = drain_connection.poll_for_event().unwrap() {
                observed.borrow_mut().push(event);
            }
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match wire.atom("REVIEW_WAKEUP") {
                Ok(atom) => {
                    assert_eq!(atom, 42);
                    break;
                }
                Err(error) if pending(&error) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("fake transport failed: {error}"),
            }
        }
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
        while events.borrow().is_empty() && Instant::now() < deadline {
            event_loop
                .dispatch(Some(Duration::from_millis(20)), &mut ())
                .unwrap();
        }
        assert!(
            events
                .borrow()
                .iter()
                .any(|event| matches!(event, x11rb::protocol::Event::SelectionNotify(_))),
            "reply worker buffered SelectionNotify without waking the UI event source (fd readiness={ready})"
        );
        drop(event_loop);
        drop(wire);
        drop(connection);
        fake.join().unwrap();
    }
}
