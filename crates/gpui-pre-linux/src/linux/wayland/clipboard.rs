use std::{fs::File, os::fd::{AsRawFd, BorrowedFd, OwnedFd, FromRawFd, IntoRawFd}, sync::{Arc, atomic::Ordering}, time::Instant};
use filedescriptor::Pipe;
use gpui::{ClipboardItem, ClipboardReadError, MimePayload, MimeRepresentation};
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;
use crate::linux::transfer::{self, Cancel, BYTE_LIMIT, TYPE_LIMIT, TRANSFER_TIMEOUT};

pub(crate) const FILE_LIST_MIME_TYPE: &str = transfer::URI_LIST;

pub(crate) trait ReceiveData: Clone {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
    fn destroy_offer(&self);
}
impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) { self.receive(mime_type, fd); }
    fn destroy_offer(&self) { self.destroy(); }
}
impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) { self.receive(mime_type, fd); }
    fn destroy_offer(&self) { self.destroy(); }
}
struct OfferLife<T: ReceiveData>(T);
impl<T: ReceiveData> Drop for OfferLife<T> { fn drop(&mut self) { self.0.destroy_offer(); } }

#[derive(Clone)]
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    pub mime_types: Vec<String>,
    _life: Arc<OfferLife<T>>,
}
impl<T: ReceiveData> DataOffer<T> {
    pub fn new(inner: T) -> Self { Self { _life: Arc::new(OfferLife(inner.clone())), inner, mime_types: Vec::new() } }
    pub fn add_mime_type(&mut self, mime_type: String) {
        if self.mime_types.len() < TYPE_LIMIT && !self.mime_types.contains(&mime_type) { self.mime_types.push(mime_type); }
    }
    pub fn receive(&self, mime: &str) -> Result<File, ClipboardReadError> {
        let pipe = Pipe::new().map_err(transfer::transport)?;
        self.inner.receive_data(mime.to_owned(), unsafe { BorrowedFd::borrow_raw(pipe.write.as_raw_fd()) });
        drop(pipe.write);
        Ok(unsafe { File::from_raw_fd(pipe.read.into_raw_fd()) })
    }
}

pub(crate) struct ReadRequest<T: ReceiveData> {
    pub generation: u64,
    pub cancel: Cancel,
    pub deadline: Instant,
    pub files: Vec<(String, File)>,
    pub offer: DataOffer<T>,
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
        transfer::decode(representations)
    }
}

pub(crate) struct Selection<T: ReceiveData> {
    pub generation: u64,
    pub contents: Option<ClipboardItem>,
    pub offer: Option<DataOffer<T>>,
    pub cached: Option<ClipboardItem>,
    pub cancel: Cancel,
    pub marker: String,
}
impl<T: ReceiveData> Default for Selection<T> {
    fn default() -> Self { Self { generation: 0, contents: None, offer: None, cached: None, cancel: transfer::cancelled(), marker: String::new() } }
}
impl<T: ReceiveData> Selection<T> {
    fn advance(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancel = transfer::cancelled();
        self.generation = self.generation.wrapping_add(1);
        self.cached = None;
    }
    pub fn set(&mut self, item: ClipboardItem) -> u64 {
        self.advance(); self.offer = None;
        self.marker = format!("pid/{}/{}/{}", std::process::id(), self.generation, self as *const _ as usize);
        self.contents = Some(item);
        self.generation
    }
    pub fn set_offer(&mut self, offer: Option<DataOffer<T>>) {
        if offer.as_ref().is_some_and(|offer| offer.mime_types.contains(&self.marker)) { self.offer = offer; return; }
        self.advance(); self.contents = None; self.offer = offer;
    }
    pub fn ready(&self) -> Option<ClipboardItem> { self.contents.clone().or_else(|| self.cached.clone()) }
    pub fn prepare(&self, connection: &Connection) -> Result<Option<ReadRequest<T>>, ClipboardReadError> {
        let Some(offer) = self.offer.clone() else { return Ok(None); };
        let types = transfer::selected_types(&offer.mime_types, true);
        if types.is_empty() { return Err(ClipboardReadError::UnsupportedContent); }
        let files = types.into_iter().map(|mime| offer.receive(&mime).map(|file| (mime, file))).collect::<Result<Vec<_>, _>>()?;
        connection.flush().map_err(transfer::transport)?;
        Ok(Some(ReadRequest { generation: self.generation, cancel: self.cancel.clone(), deadline: Instant::now() + TRANSFER_TIMEOUT, files, offer }))
    }
}

pub(crate) struct Clipboard {
    pub connection: Connection,
    pub selection: Selection<WlDataOffer>,
    pub primary: Selection<ZwpPrimarySelectionOfferV1>,
}
impl Clipboard {
    pub fn new(connection: Connection) -> Self { Self { connection, selection: Selection::default(), primary: Selection::default() } }
    pub fn read(&self) -> Option<ClipboardItem> { self.selection.ready() }
    pub fn read_primary(&self) -> Option<ClipboardItem> { self.primary.ready() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataSourceKind { Clipboard, Primary, Drag }

pub(crate) struct SourceData {
    pub kind: DataSourceKind,
    pub generation: u64,
    pub payload: MimePayload,
    pub cancel: Cancel,
}
impl SourceData {
    pub fn send(&self, mime: &str, fd: OwnedFd, executor: &gpui::BackgroundExecutor) {
        let Some(bytes) = self.payload.get(mime) else { return; };
        let cancel = self.cancel.clone();
        executor.spawn(async move {
            if let Err(error) = transfer::write(File::from(fd), bytes, &cancel, Instant::now() + TRANSFER_TIMEOUT) {
                log::debug!("native transfer send: {error}");
            }
        }).detach();
    }
}
