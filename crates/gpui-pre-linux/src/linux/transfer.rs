use std::{
    fs::File,
    io::{Read, Write},
    os::fd::AsRawFd,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};
use gpui::{ClipboardEntry, ClipboardItem, ClipboardReadError, ExternalPaths, Image, ImageFormat, MimePayload, MimeRepresentation, SelectionIntent, hash};
use strum::IntoEnumIterator;
use url::Url;

pub(crate) const URI_LIST: &str = "text/uri-list";
pub(crate) const GNOME_FILES: &str = "x-special/gnome-copied-files";
pub(crate) const TEXT_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
pub(crate) const BYTE_LIMIT: usize = 64 * 1024 * 1024;
pub(crate) const PATH_LIMIT: usize = 100_000;
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const TYPE_LIMIT: usize = 64;
pub(crate) type Cancel = Arc<AtomicBool>;

pub(crate) fn cancelled() -> Cancel { Arc::new(AtomicBool::new(false)) }

pub(crate) fn check(cancel: &Cancel, deadline: Instant) -> Result<(), ClipboardReadError> {
    if cancel.load(Ordering::Acquire) { return Err(ClipboardReadError::Cancelled); }
    if Instant::now() >= deadline { return Err(ClipboardReadError::TimedOut); }
    Ok(())
}

pub(crate) fn transport(error: impl std::fmt::Display) -> ClipboardReadError { ClipboardReadError::Transport(error.to_string()) }

fn nonblocking(file: &File) -> Result<(), ClipboardReadError> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(transport(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn wait(file: &File, events: i16, cancel: &Cancel, deadline: Instant) -> Result<(), ClipboardReadError> {
    loop {
        check(cancel, deadline)?;
        let millis = deadline.saturating_duration_since(Instant::now()).as_millis().min(25) as i32;
        let mut pollfd = libc::pollfd { fd: file.as_raw_fd(), events, revents: 0 };
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis.max(1)) };
        if ready > 0 { return Ok(()); }
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(transport(std::io::Error::last_os_error()));
        }
    }
}

pub(crate) fn read(mut file: File, cancel: &Cancel, deadline: Instant, limit: usize) -> Result<Vec<u8>, ClipboardReadError> {
    nonblocking(&file)?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        wait(&file, libc::POLLIN, cancel, deadline)?;
        match file.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(len) => {
                if len > limit.saturating_sub(bytes.len()) { return Err(ClipboardReadError::TooLarge); }
                bytes.extend_from_slice(&chunk[..len]);
            }
            Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => (),
            Err(error) => return Err(transport(error)),
        }
    }
}

pub(crate) fn write(mut file: File, bytes: Arc<[u8]>, cancel: &Cancel, deadline: Instant) -> Result<(), ClipboardReadError> {
    if bytes.len() > BYTE_LIMIT { return Err(ClipboardReadError::TooLarge); }
    nonblocking(&file)?;
    let mut offset = 0;
    while offset < bytes.len() {
        wait(&file, libc::POLLOUT, cancel, deadline)?;
        match file.write(&bytes[offset..bytes.len().min(offset + 8192)]) {
            Ok(0) => return Err(transport("clipboard pipe accepted no bytes")),
            Ok(len) => offset += len,
            Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => (),
            Err(error) => return Err(transport(error)),
        }
    }
    Ok(())
}

pub(crate) fn file_payload(paths: impl IntoIterator<Item = std::path::PathBuf>, intent: SelectionIntent) -> Result<MimePayload, ClipboardReadError> {
    let mut uris = String::new();
    let mut gnome = match intent { SelectionIntent::Copy => "copy\n".to_owned(), SelectionIntent::Cut => "cut\n".to_owned() };
    let mut plain = String::new();
    for (index, path) in paths.into_iter().enumerate() {
        if index >= PATH_LIMIT { return Err(ClipboardReadError::TooLarge); }
        let uri = Url::from_file_path(&path).map_err(|_| ClipboardReadError::UnsupportedContent)?;
        uris.push_str(uri.as_str()); uris.push_str("\r\n");
        gnome.push_str(uri.as_str()); gnome.push('\n');
        plain.push_str(&path.to_string_lossy()); plain.push('\n');
        if uris.len() + gnome.len() + plain.len() * TEXT_TYPES.len() > BYTE_LIMIT { return Err(ClipboardReadError::TooLarge); }
    }
    let mut representations = vec![MimeRepresentation::new(URI_LIST, uris.into_bytes()), MimeRepresentation::new(GNOME_FILES, gnome.into_bytes())];
    let plain: Arc<[u8]> = plain.into_bytes().into();
    representations.extend(TEXT_TYPES.map(|mime| MimeRepresentation::new(mime, plain.clone())));
    Ok(MimePayload::new(representations, intent))
}

pub(crate) fn snapshot(item: &ClipboardItem) -> Result<MimePayload, ClipboardReadError> {
    let mut representations = Vec::new();
    let mut intent = SelectionIntent::Copy;
    for entry in item.entries() {
        match entry {
            ClipboardEntry::Mime(payload) => { representations.extend_from_slice(payload.representations()); intent = payload.intent(); }
            ClipboardEntry::Image(image) => representations.push(MimeRepresentation::new(image.format.mime_type(), image.bytes.clone())),
            ClipboardEntry::ExternalPaths(paths) => representations.extend_from_slice(file_payload(paths.paths().iter().cloned(), intent)?.representations()),
            ClipboardEntry::String(_) => (),
        }
    }
    if let Some(text) = item.text() {
        let bytes: Arc<[u8]> = text.into_bytes().into();
        representations.extend(TEXT_TYPES.map(|mime| MimeRepresentation::new(mime, bytes.clone())));
    }
    let payload = MimePayload::new(representations, intent);
    if payload.representations().len() > TYPE_LIMIT || payload.representations().iter().map(|entry| entry.bytes().len()).sum::<usize>() > BYTE_LIMIT { return Err(ClipboardReadError::TooLarge); }
    Ok(payload)
}

pub(crate) fn paths(payload: &MimePayload) -> Result<ExternalPaths, ClipboardReadError> {
    let gnome = payload.get(GNOME_FILES);
    let bytes = gnome.clone().or_else(|| payload.get(URI_LIST));
    let Some(bytes) = bytes else { return Ok(ExternalPaths::default()); };
    let text = std::str::from_utf8(&bytes).map_err(transport)?;
    let mut lines = text.lines();
    if gnome.is_some() && !matches!(lines.next(), Some("copy" | "cut")) { return Err(ClipboardReadError::UnsupportedContent); }
    let mut paths = ExternalPaths::default();
    for line in lines.filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let Ok(uri) = Url::parse(line) else { continue; };
        let Ok(path) = uri.to_file_path() else { continue; };
        if paths.0.len() >= PATH_LIMIT { return Err(ClipboardReadError::TooLarge); }
        paths.0.push(path);
    }
    Ok(paths)
}

pub(crate) fn decode(representations: Vec<MimeRepresentation>) -> Result<ClipboardItem, ClipboardReadError> {
    let intent = representations.iter().find(|entry| entry.mime_type() == GNOME_FILES).map(|entry| {
        if entry.bytes().starts_with(b"cut\n") { SelectionIntent::Cut } else { SelectionIntent::Copy }
    }).unwrap_or_default();
    let payload = MimePayload::new(representations, intent);
    let mut entries = vec![ClipboardEntry::Mime(payload.clone())];
    if payload.get(GNOME_FILES).is_some() || payload.get(URI_LIST).is_some() {
        entries.push(ClipboardEntry::ExternalPaths(paths(&payload)?));
    } else if let Some(bytes) = TEXT_TYPES.iter().find_map(|mime| payload.get(mime)) {
        let text = std::str::from_utf8(&bytes).map_err(transport)?.replace("\r\n", "\n");
        entries.push(ClipboardEntry::String(text.into()));
    } else if let Some((format, bytes)) = ImageFormat::iter().find_map(|format| payload.get(format.mime_type()).map(|bytes| (format, bytes))) {
        let bytes = bytes.to_vec();
        entries.push(ClipboardEntry::Image(Image { id: hash(&bytes), format, bytes }));
    }
    Ok(ClipboardItem { entries })
}

pub(crate) fn selected_types(offered: &[String], arbitrary: bool) -> Vec<String> {
    let file = offered.iter().any(|mime| mime == GNOME_FILES || mime == URI_LIST);
    offered.iter().filter(|mime| {
        (file && (mime.as_str() == GNOME_FILES || mime.as_str() == URI_LIST)) ||
        (arbitrary && !mime.starts_with("pid/")) || TEXT_TYPES.contains(&mime.as_str()) || ImageFormat::iter().any(|format| format.mime_type() == mime.as_str())
    }).take(TYPE_LIMIT).cloned().collect()
}

pub(crate) fn notify_clipboard(common: &std::rc::Rc<std::cell::RefCell<super::LinuxCommon>>, event: gpui::ClipboardChange) {
    let executor = common.borrow().foreground_executor.clone();
    let common = common.clone();
    executor.spawn(async move {
        let callback = common.borrow_mut().callbacks.clipboard_change.take();
        if let Some(mut callback) = callback {
            callback(event);
            let mut common = common.borrow_mut();
            if common.callbacks.clipboard_change.is_none() { common.callbacks.clipboard_change = Some(callback); }
        }
    }).detach();
}

pub(crate) fn notify_drag(common: &std::rc::Rc<std::cell::RefCell<super::LinuxCommon>>, event: gpui::NativeDragEvent) {
    let executor = common.borrow().foreground_executor.clone();
    let common = common.clone();
    executor.spawn(async move {
        let callback = common.borrow_mut().callbacks.native_drag.take();
        if let Some(mut callback) = callback {
            callback(event);
            let mut common = common.borrow_mut();
            if common.callbacks.native_drag.is_none() { common.callbacks.native_drag = Some(callback); }
        }
    }).detach();
}

pub(crate) fn allow_drop(common: &std::rc::Rc<std::cell::RefCell<super::LinuxCommon>>, event: gpui::NativeDropEvent, has_paths: bool) -> bool {
    let callback = common.borrow_mut().callbacks.native_drop.take();
    let Some(mut callback) = callback else { return has_paths; };
    let accepted = callback(event);
    let mut common = common.borrow_mut();
    if common.callbacks.native_drop.is_none() { common.callbacks.native_drop = Some(callback); }
    accepted
}
