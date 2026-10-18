use crate::linux::transfer::{self, BYTE_LIMIT, Cancel, TRANSFER_TIMEOUT, TYPE_LIMIT};
use filedescriptor::Pipe;
use gpui::{ClipboardItem, ClipboardReadError, MimePayload, MimeRepresentation};
use std::{
    fs::File,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

pub(crate) trait ReceiveData: Clone {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
    fn destroy_offer(&self);
}
impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
    fn destroy_offer(&self) {
        self.destroy();
    }
}
impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
    fn destroy_offer(&self) {
        self.destroy();
    }
}
struct OfferLife<T: ReceiveData>(T);
impl<T: ReceiveData> Drop for OfferLife<T> {
    fn drop(&mut self) {
        self.0.destroy_offer();
    }
}

#[derive(Clone)]
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    pub mime_types: Vec<String>,
    _life: Arc<OfferLife<T>>,
}
impl<T: ReceiveData> DataOffer<T> {
    pub fn new(inner: T) -> Self {
        Self {
            _life: Arc::new(OfferLife(inner.clone())),
            inner,
            mime_types: Vec::new(),
        }
    }
    pub fn add_mime_type(&mut self, mime_type: String) {
        if mime_type.len() <= 4096
            && self.mime_types.len() < TYPE_LIMIT
            && !self.mime_types.contains(&mime_type)
        {
            self.mime_types.push(mime_type);
        }
    }
    pub fn receive(&self, mime: &str) -> Result<File, ClipboardReadError> {
        let pipe = Pipe::new().map_err(transfer::transport)?;
        self.inner.receive_data(mime.to_owned(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        drop(pipe.write);
        Ok(unsafe { File::from_raw_fd(pipe.read.into_raw_fd()) })
    }
}

pub(crate) struct ReadRequest<T: ReceiveData> {
    pub generation: u64,
    pub cancel: Cancel,
    pub deadline: Instant,
    pub files: Vec<(String, File)>,
    pub _offer: DataOffer<T>,
    pub _permit: transfer::TransferPermit,
}
impl<T: ReceiveData> ReadRequest<T> {
    pub fn run(self) -> Result<ClipboardItem, ClipboardReadError> {
        let mut representations = Vec::new();
        let mut left = BYTE_LIMIT;
        for (mime, file) in self.files {
            let bytes = transfer::read(file, &self.cancel, self.deadline, left)?;
            left -= bytes.len();
            representations.push(MimeRepresentation::new(mime, bytes));
        }
        transfer::check(&self.cancel, self.deadline)?;
        transfer::decode_offered(representations, self._offer.mime_types)
    }
}

pub(crate) struct Selection<T: ReceiveData> {
    pub generation: u64,
    pub contents: Option<ClipboardItem>,
    pub offer: Option<DataOffer<T>>,
    pub cached: Option<ClipboardItem>,
    pub cancel: Cancel,
    pub marker: String,
    own_prefix: String,
    latest_source: Option<(u64, ClipboardItem)>,
    last_source: u64,
    pub receiving: bool,
    waiters:
        Vec<futures::channel::oneshot::Sender<Result<Option<ClipboardItem>, ClipboardReadError>>>,
}
impl<T: ReceiveData> Default for Selection<T> {
    fn default() -> Self {
        Self {
            generation: 0,
            contents: None,
            offer: None,
            cached: None,
            cancel: transfer::cancelled(),
            marker: String::new(),
            own_prefix: String::new(),
            latest_source: None,
            last_source: 0,
            receiving: false,
            waiters: Vec::new(),
        }
    }
}
impl<T: ReceiveData> Selection<T> {
    fn advance(&mut self) {
        self.receiving = false;
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(Err(ClipboardReadError::Cancelled));
        }
        self.cancel.store(true, Ordering::Release);
        self.cancel = transfer::cancelled();
        self.generation = self.generation.wrapping_add(1);
        self.cached = None;
    }
    pub fn set(&mut self, item: ClipboardItem) -> u64 {
        self.advance();
        self.offer = None;
        if self.own_prefix.is_empty() {
            self.own_prefix = format!("pid/{}/{}/", std::process::id(), self as *const _ as usize);
        }
        self.marker = format!("{}{}", self.own_prefix, self.generation);
        self.last_source = self.generation;
        self.latest_source = Some((self.last_source, item.clone()));
        self.contents = Some(item);
        self.generation
    }
    pub fn set_offer(&mut self, offer: Option<DataOffer<T>>) {
        if let Some(generation) = offer.as_ref().and_then(|offer| {
            offer.mime_types.iter().find_map(|mime| {
                (!self.own_prefix.is_empty())
                    .then(|| mime.strip_prefix(&self.own_prefix))
                    .flatten()
                    .and_then(|suffix| suffix.parse::<u64>().ok())
                    .filter(|generation| *generation <= self.last_source)
            })
        }) {
            if let Some((source, item)) = &self.latest_source
                && *source == generation
            {
                let item = item.clone();
                if self.contents.is_none() {
                    // A queued foreign predecessor can invalidate a receive, but cannot
                    // erase the immutable snapshot of the source just accepted.
                    self.advance();
                }
                self.contents = Some(item);
                self.offer = offer;
            }
            return;
        }
        self.advance();
        self.contents = None;
        self.offer = offer;
    }
    pub fn cancel_source(&mut self, source: &SourceData) -> bool {
        source.cancel.store(true, Ordering::Release);
        if self.latest_source.as_ref().map(|(id, _)| *id) != Some(source.generation) {
            return false;
        }
        self.latest_source = None;
        if self.contents.is_none() {
            return false;
        }
        self.set_offer(None);
        true
    }
    pub fn ready(&self) -> Option<ClipboardItem> {
        self.contents.clone().or_else(|| self.cached.clone())
    }
    pub fn subscribe(
        &mut self,
    ) -> futures::channel::oneshot::Receiver<Result<Option<ClipboardItem>, ClipboardReadError>>
    {
        let (sender, receiver) = futures::channel::oneshot::channel();
        if let Some(item) = self.ready() {
            let _ = sender.send(Ok(Some(item)));
        } else if self.offer.is_none() {
            let _ = sender.send(Ok(None));
        } else if self.waiters.len() < 64 {
            self.waiters.push(sender);
        } else {
            let _ = sender.send(Err(ClipboardReadError::Unavailable));
        }
        receiver
    }
    pub fn complete_read(
        &mut self,
        generation: u64,
        result: Result<ClipboardItem, ClipboardReadError>,
    ) -> bool {
        if self.generation != generation {
            return false;
        }
        self.receiving = false;
        if let Ok(item) = &result {
            self.cached = Some(item.clone());
        }
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(result.clone().map(Some));
        }
        true
    }
    pub fn prepare(
        &mut self,
        connection: &Connection,
    ) -> Result<Option<ReadRequest<T>>, ClipboardReadError> {
        if self.receiving || self.ready().is_some() {
            return Ok(None);
        }
        let Some(offer) = self.offer.clone() else {
            return Ok(None);
        };
        let types = transfer::selected_types(&offer.mime_types, true);
        if types.is_empty() {
            return Err(ClipboardReadError::UnsupportedContent);
        }
        let permit = transfer::TransferPermit::acquire()?;
        let files = types
            .into_iter()
            .map(|mime| offer.receive(&mime).map(|file| (mime, file)))
            .collect::<Result<Vec<_>, _>>()?;
        connection.flush().map_err(transfer::transport)?;
        self.receiving = true;
        Ok(Some(ReadRequest {
            generation: self.generation,
            cancel: self.cancel.clone(),
            deadline: Instant::now() + TRANSFER_TIMEOUT,
            files,
            _offer: offer,
            _permit: permit,
        }))
    }
}

pub(crate) struct Clipboard {
    pub connection: Connection,
    pub selection: Selection<WlDataOffer>,
    pub primary: Selection<ZwpPrimarySelectionOfferV1>,
}
impl Clipboard {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            selection: Selection::default(),
            primary: Selection::default(),
        }
    }
    pub fn read(&self) -> Option<ClipboardItem> {
        self.selection.ready()
    }
    pub fn read_primary(&self) -> Option<ClipboardItem> {
        self.primary.ready()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataSourceKind {
    Clipboard,
    Primary,
    Drag,
}

pub(crate) struct SourceData {
    pub kind: DataSourceKind,
    pub generation: u64,
    pub payload: MimePayload,
    pub cancel: Cancel,
}
impl SourceData {
    pub fn send(&self, mime: &str, fd: OwnedFd, executor: &gpui::BackgroundExecutor) {
        let Some((bytes, cancel, permit)) = self.prepare_send(mime) else {
            return;
        };
        executor
            .spawn(async move {
                let _permit = permit;
                if let Err(error) = transfer::write(
                    File::from(fd),
                    bytes,
                    &cancel,
                    Instant::now() + TRANSFER_TIMEOUT,
                ) {
                    log::debug!("native transfer send: {error}");
                }
            })
            .detach();
    }
    fn prepare_send(&self, mime: &str) -> Option<(Arc<[u8]>, Cancel, transfer::TransferPermit)> {
        if self.cancel.load(Ordering::Acquire) {
            return None;
        }
        Some((
            self.payload.get(mime)?,
            self.cancel.clone(),
            transfer::TransferPermit::acquire().ok()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};
    #[derive(Clone)]
    struct FakeOffer {
        destroyed: Rc<Cell<usize>>,
    }
    impl ReceiveData for FakeOffer {
        fn receive_data(&self, _: String, _: BorrowedFd<'_>) {}
        fn destroy_offer(&self) {
            self.destroyed.set(self.destroyed.get() + 1);
        }
    }
    #[test]
    fn offer_is_destroyed_exactly_once_after_pending_reader_releases_it() {
        let destroyed = Rc::new(Cell::new(0));
        let offer = DataOffer::new(FakeOffer {
            destroyed: destroyed.clone(),
        });
        let reader = offer.clone();
        drop(offer);
        assert_eq!(destroyed.get(), 0);
        drop(reader);
        assert_eq!(destroyed.get(), 1);
    }
    #[test]
    fn old_source_cancellation_does_not_clear_a_new_selection() {
        let mut selection: Selection<FakeOffer> = Selection::default();
        let old = selection.set(ClipboardItem::new_string("old".into()));
        let new = selection.set(ClipboardItem::new_string("new".into()));
        assert_ne!(old, new);
        if selection.generation == old {
            selection.set_offer(None);
        }
        assert_eq!(selection.ready().unwrap().text().unwrap(), "new");
    }
}

#[cfg(test)]
mod review_ownership {
    use super::*;
    #[derive(Clone)]
    pub(super) struct Offer;
    impl ReceiveData for Offer {
        fn receive_data(&self, _: String, _: BorrowedFd<'_>) {}
        fn destroy_offer(&self) {}
    }
    pub(super) fn echo(marker: String) -> DataOffer<Offer> {
        let mut offer = DataOffer::new(Offer);
        offer.add_mime_type(marker);
        offer
    }
    #[test]
    fn review_wayland_queued_own_selection_echo_cannot_clear_newer_owner() {
        let mut selection: Selection<Offer> = Selection::default();
        selection.set(ClipboardItem::new_string("old".into()));
        let old_marker = selection.marker.clone();
        let current = selection.set(ClipboardItem::new_string("new".into()));
        let current_marker = selection.marker.clone();
        selection.set_offer(Some(echo(old_marker)));
        selection.set_offer(Some(echo(current_marker)));
        assert_eq!(
            selection.generation, current,
            "queued old own-selection echo invalidated current generation"
        );
        assert_eq!(
            selection
                .contents
                .as_ref()
                .and_then(ClipboardItem::text)
                .as_deref(),
            Some("new")
        );
    }

    #[test]
    fn review_r2_queued_foreign_selection_before_latest_own_echo_restores_owner() {
        let mut selection: Selection<Offer> = Selection::default();
        selection.set(ClipboardItem::new_string("old own source".into()));
        selection.set_offer(Some(echo(selection.marker.clone())));
        selection.set(ClipboardItem::new_string("new own source".into()));
        let latest_marker = selection.marker.clone();
        // This foreign notification was queued before the compositor accepted the new source.
        selection.set_offer(Some(echo("UTF8_STRING".into())));
        selection.set_offer(Some(echo(latest_marker)));
        assert_eq!(
            selection.ready().and_then(|item| item.text()).as_deref(),
            Some("new own source"),
            "the latest accepted source must own the cache after its compositor echo"
        );
    }
}

#[cfg(test)]
mod source_identity_tests {
    use super::review_ownership::{Offer, echo};
    use super::*;
    use std::os::unix::net::UnixStream;
    use wayland_client::{
        Dispatch, Proxy, QueueHandle,
        protocol::{
            wl_data_device_manager::WlDataDeviceManager, wl_data_source::WlDataSource,
            wl_registry::WlRegistry,
        },
    };
    use wayland_protocols::wp::primary_selection::zv1::client::{
        zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
        zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
    };
    struct Harness;
    wayland_client::delegate_noop!(Harness: ignore WlRegistry);
    wayland_client::delegate_noop!(Harness: ignore WlDataDeviceManager);
    wayland_client::delegate_noop!(Harness: ignore ZwpPrimarySelectionDeviceManagerV1);
    impl Dispatch<WlDataSource, SourceData> for Harness {
        fn event(
            _: &mut Self,
            _: &WlDataSource,
            _: wayland_client::protocol::wl_data_source::Event,
            _: &SourceData,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<ZwpPrimarySelectionSourceV1, SourceData> for Harness {
        fn event(
            _: &mut Self,
            _: &ZwpPrimarySelectionSourceV1,
            _: wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::Event,
            _: &SourceData,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    fn send<P: Proxy>(
        source: &P,
        executor: &gpui::BackgroundExecutor,
        dispatcher: &gpui::TestDispatcher,
    ) -> Vec<u8> {
        let (reader, writer) = UnixStream::pair().unwrap();
        source
            .data::<SourceData>()
            .unwrap()
            .send("UTF8_STRING", writer.into(), executor);
        dispatcher.run_until_parked();
        transfer::read(
            File::from(OwnedFd::from(reader)),
            &transfer::cancelled(),
            Instant::now() + std::time::Duration::from_millis(100),
            1024,
        )
        .unwrap()
    }
    fn exercise<P: Proxy>(kind: DataSourceKind, make: impl Fn(SourceData) -> P) {
        let dispatcher = gpui::TestDispatcher::new(0);
        let executor = gpui::BackgroundExecutor::new(Arc::new(dispatcher.clone()));
        let mut selection: Selection<Offer> = Selection::default();
        let mut source = |text: &str| {
            let item = ClipboardItem::new_string(text.into());
            let payload = transfer::snapshot(&item).unwrap();
            let generation = selection.set(item);
            (
                make(SourceData {
                    kind,
                    generation,
                    payload,
                    cancel: transfer::cancelled(),
                }),
                selection.marker.clone(),
            )
        };
        let (old, old_marker) = source("old source");
        let (new, new_marker) = source("new source");
        assert_ne!(old.id(), new.id());
        assert_eq!(send(&old, &executor, &dispatcher), b"old source");
        selection.set_offer(Some(echo(old_marker.clone())));
        assert!(!selection.cancel_source(old.data().unwrap()));
        selection.set_offer(Some(echo(old_marker)));
        selection.set_offer(Some(echo("UTF8_STRING".into())));
        let foreign_generation = selection.generation;
        let foreign_cancel = selection.cancel.clone();
        selection.set_offer(Some(echo(new_marker.clone())));
        assert!(selection.generation > foreign_generation);
        assert!(foreign_cancel.load(Ordering::Acquire));
        assert_eq!(
            selection.ready().unwrap().text().as_deref(),
            Some("new source")
        );
        assert_eq!(send(&new, &executor, &dispatcher), b"new source");
        assert!(send(&old, &executor, &dispatcher).is_empty());
        let receive_cancel = selection.cancel.clone();
        assert!(selection.cancel_source(new.data().unwrap()));
        assert!(receive_cancel.load(Ordering::Acquire));
        assert!(selection.ready().is_none());
        selection.set_offer(Some(echo(new_marker)));
        assert!(
            selection.ready().is_none(),
            "cancelled source echo resurrected ownership"
        );
    }
    #[test]
    fn source_associated_send_cancel_and_queued_echoes_preserve_latest_owner() {
        let (socket, _server) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<Harness>();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let manager: WlDataDeviceManager = registry.bind(1, 3, &qh, ());
        exercise(DataSourceKind::Clipboard, |data| {
            manager.create_data_source(&qh, data)
        });
        let primary: ZwpPrimarySelectionDeviceManagerV1 = registry.bind(2, 1, &qh, ());
        exercise(DataSourceKind::Primary, |data| {
            primary.create_source(&qh, data)
        });
    }
}

#[cfg(test)]
mod pending_read_tests {
    use super::review_ownership::{Offer, echo};
    use super::*;
    #[test]
    fn review_wayland_paste_and_primary_wait_for_existing_prefetch() {
        for _primary in [false, true] {
            let mut selection: Selection<Offer> = Selection::default();
            selection.set_offer(Some(echo("UTF8_STRING".into())));
            let generation = selection.generation;
            selection.receiving = true;
            let mut first = selection.subscribe();
            let mut second = selection.subscribe();
            assert_eq!(first.try_recv().unwrap(), None);
            assert_eq!(second.try_recv().unwrap(), None);
            assert!(selection.complete_read(
                generation,
                Ok(ClipboardItem::new_string("prefetched".into()))
            ));
            for receiver in [first, second] {
                assert_eq!(
                    futures::executor::block_on(receiver)
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .text()
                        .as_deref(),
                    Some("prefetched")
                );
            }
            assert!(!selection.receiving);
            assert_eq!(
                selection.ready().unwrap().text().as_deref(),
                Some("prefetched")
            );
        }
    }
    #[test]
    fn review_wayland_replacement_cancels_waiters_without_publishing_old_read() {
        let mut selection: Selection<Offer> = Selection::default();
        selection.set_offer(Some(echo("UTF8_STRING".into())));
        let old = selection.generation;
        let receiver = selection.subscribe();
        selection.set_offer(None);
        assert_eq!(
            futures::executor::block_on(receiver).unwrap(),
            Err(ClipboardReadError::Cancelled)
        );
        assert!(!selection.complete_read(old, Ok(ClipboardItem::new_string("stale".into()))));
        assert!(selection.ready().is_none());
    }
}
