use gpui::{
    ClipboardEntry, ClipboardItem, ClipboardReadError, ExternalPaths, Image, ImageFormat,
    MimePayload, MimeRepresentation, SelectionIntent, hash,
};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
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
static TRANSFERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub(crate) struct TransferPermit;
impl TransferPermit {
    pub fn acquire() -> Result<Self, ClipboardReadError> {
        TRANSFERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 16).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| ClipboardReadError::Unavailable)
    }
}
impl Drop for TransferPermit {
    fn drop(&mut self) {
        TRANSFERS.fetch_sub(1, Ordering::Release);
    }
}

pub(crate) fn cancelled() -> Cancel {
    Arc::new(AtomicBool::new(false))
}

pub(crate) fn check(cancel: &Cancel, deadline: Instant) -> Result<(), ClipboardReadError> {
    if cancel.load(Ordering::Acquire) {
        return Err(ClipboardReadError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(ClipboardReadError::TimedOut);
    }
    Ok(())
}

pub(crate) fn transport(error: impl std::fmt::Display) -> ClipboardReadError {
    ClipboardReadError::Transport(error.to_string())
}

fn nonblocking(file: &File) -> Result<(), ClipboardReadError> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(transport(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn wait(
    file: &File,
    events: i16,
    cancel: &Cancel,
    deadline: Instant,
) -> Result<(), ClipboardReadError> {
    loop {
        check(cancel, deadline)?;
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(25) as i32;
        let mut pollfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis.max(1)) };
        if ready > 0 {
            return Ok(());
        }
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(transport(std::io::Error::last_os_error()));
        }
    }
}

pub(crate) fn read(
    mut file: File,
    cancel: &Cancel,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<u8>, ClipboardReadError> {
    nonblocking(&file)?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        wait(&file, libc::POLLIN, cancel, deadline)?;
        match file.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(len) => {
                if len > limit.saturating_sub(bytes.len()) {
                    return Err(ClipboardReadError::TooLarge);
                }
                bytes.extend_from_slice(&chunk[..len]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                ()
            }
            Err(error) => return Err(transport(error)),
        }
    }
}

pub(crate) fn write(
    mut file: File,
    bytes: Arc<[u8]>,
    cancel: &Cancel,
    deadline: Instant,
) -> Result<(), ClipboardReadError> {
    if bytes.len() > BYTE_LIMIT {
        return Err(ClipboardReadError::TooLarge);
    }
    nonblocking(&file)?;
    let mut offset = 0;
    while offset < bytes.len() {
        wait(&file, libc::POLLOUT, cancel, deadline)?;
        match file.write(&bytes[offset..bytes.len().min(offset + 8192)]) {
            Ok(0) => return Err(transport("clipboard pipe accepted no bytes")),
            Ok(len) => offset += len,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                ()
            }
            Err(error) => return Err(transport(error)),
        }
    }
    Ok(())
}

pub(crate) fn file_payload(
    paths: impl IntoIterator<Item = std::path::PathBuf>,
    intent: SelectionIntent,
) -> Result<MimePayload, ClipboardReadError> {
    let mut uris = String::new();
    let mut gnome = match intent {
        SelectionIntent::Copy => "copy\n".to_owned(),
        SelectionIntent::Cut => "cut\n".to_owned(),
    };
    let mut plain = String::new();
    for (index, path) in paths.into_iter().enumerate() {
        if index >= PATH_LIMIT {
            return Err(ClipboardReadError::TooLarge);
        }
        let uri = Url::from_file_path(&path).map_err(|_| ClipboardReadError::UnsupportedContent)?;
        uris.push_str(uri.as_str());
        uris.push_str("\r\n");
        gnome.push_str(uri.as_str());
        gnome.push('\n');
        plain.push_str(&path.to_string_lossy());
        plain.push('\n');
        if uris.len() > BYTE_LIMIT || gnome.len() > BYTE_LIMIT || plain.len() > BYTE_LIMIT {
            return Err(ClipboardReadError::TooLarge);
        }
    }
    let mut representations = vec![
        MimeRepresentation::new(URI_LIST, uris.into_bytes()),
        MimeRepresentation::new(GNOME_FILES, gnome.into_bytes()),
    ];
    let plain: Arc<[u8]> = plain.into_bytes().into();
    representations.extend(TEXT_TYPES.map(|mime| MimeRepresentation::new(mime, plain.clone())));
    Ok(MimePayload::new(representations, intent))
}

pub(crate) fn snapshot(item: &ClipboardItem) -> Result<MimePayload, ClipboardReadError> {
    let mut representations = Vec::new();
    let mut intent = SelectionIntent::Copy;
    for entry in item.entries() {
        match entry {
            ClipboardEntry::Mime(payload) => {
                representations.extend_from_slice(payload.representations());
                intent = payload.intent();
            }
            ClipboardEntry::Image(image) => representations.push(MimeRepresentation::new(
                image.format.mime_type(),
                image.bytes.clone(),
            )),
            ClipboardEntry::ExternalPaths(paths) => representations.extend_from_slice(
                file_payload(paths.paths().iter().cloned(), intent)?.representations(),
            ),
            ClipboardEntry::String(_) => (),
        }
    }
    if let Some(text) = item.text() {
        let bytes: Arc<[u8]> = text.into_bytes().into();
        representations.extend(TEXT_TYPES.map(|mime| MimeRepresentation::new(mime, bytes.clone())));
    }
    let payload = MimePayload::new(representations, intent);
    if payload.representations().len() > TYPE_LIMIT
        || payload
            .representations()
            .iter()
            .any(|entry| entry.bytes().len() > BYTE_LIMIT)
    {
        return Err(ClipboardReadError::TooLarge);
    }
    Ok(payload)
}

pub(crate) fn paths(payload: &MimePayload) -> Result<ExternalPaths, ClipboardReadError> {
    let gnome = payload.get(GNOME_FILES);
    let bytes = gnome.clone().or_else(|| payload.get(URI_LIST));
    let Some(bytes) = bytes else {
        return Ok(ExternalPaths::default());
    };
    let text = std::str::from_utf8(&bytes).map_err(transport)?;
    let mut lines = text.lines();
    if gnome.is_some() && !matches!(lines.next(), Some("copy" | "cut")) {
        return Err(ClipboardReadError::UnsupportedContent);
    }
    let mut paths = ExternalPaths::default();
    for line in lines.filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let Ok(uri) = Url::parse(line) else {
            continue;
        };
        let Ok(path) = uri.to_file_path() else {
            continue;
        };
        if paths.0.len() >= PATH_LIMIT {
            return Err(ClipboardReadError::TooLarge);
        }
        paths.0.push(path);
    }
    Ok(paths)
}

pub(crate) fn decode(
    representations: Vec<MimeRepresentation>,
) -> Result<ClipboardItem, ClipboardReadError> {
    let intent = representations
        .iter()
        .find(|entry| entry.mime_type() == GNOME_FILES)
        .map(|entry| {
            if entry.bytes().starts_with(b"cut\n") {
                SelectionIntent::Cut
            } else {
                SelectionIntent::Copy
            }
        })
        .unwrap_or_default();
    let payload = MimePayload::new(representations, intent);
    let mut entries = vec![ClipboardEntry::Mime(payload.clone())];
    if payload.get(GNOME_FILES).is_some() || payload.get(URI_LIST).is_some() {
        entries.push(ClipboardEntry::ExternalPaths(paths(&payload)?));
    } else if let Some(bytes) = TEXT_TYPES
        .iter()
        .copied()
        .chain(["text/plain;charset=UTF-8", "TEXT"])
        .find_map(|mime| payload.get(mime))
    {
        let text = std::str::from_utf8(&bytes)
            .map_err(transport)?
            .replace("\r\n", "\n");
        entries.push(ClipboardEntry::String(text.into()));
    } else if let Some(bytes) = payload.get("STRING") {
        let text: String = bytes.iter().map(|byte| *byte as char).collect();
        entries.push(ClipboardEntry::String(text.into()));
    } else if let Some((format, bytes)) = ImageFormat::iter()
        .find_map(|format| payload.get(format.mime_type()).map(|bytes| (format, bytes)))
    {
        let bytes = bytes.to_vec();
        entries.push(ClipboardEntry::Image(Image {
            id: hash(&bytes),
            format,
            bytes,
        }));
    }
    Ok(ClipboardItem { entries })
}

pub(crate) fn selected_types(offered: &[String], arbitrary: bool) -> Vec<String> {
    let preferred = [GNOME_FILES, URI_LIST]
        .into_iter()
        .find(|mime| offered.iter().any(|offered| offered == mime));
    let preferred = preferred.or_else(|| {
        TEXT_TYPES
            .into_iter()
            .find(|mime| offered.iter().any(|offered| offered == mime))
    });
    let preferred = preferred.or_else(|| {
        ImageFormat::iter()
            .map(|format| format.mime_type())
            .find(|mime| offered.iter().any(|offered| offered == mime))
    });
    let mut types: Vec<_> = preferred.into_iter().map(str::to_owned).collect();
    if arbitrary {
        for mime in offered {
            if types.len() >= TYPE_LIMIT {
                break;
            }
            let standard = mime == GNOME_FILES
                || mime == URI_LIST
                || TEXT_TYPES.contains(&mime.as_str())
                || ImageFormat::iter().any(|format| format.mime_type() == mime);
            if !standard && !mime.starts_with("pid/") && !types.contains(mime) {
                types.push(mime.clone());
            }
        }
    }
    types
}

pub(crate) fn preferred_drag_type(types: &[String], arbitrary: bool) -> Option<&String> {
    types
        .iter()
        .find(|mime| mime.as_str() == GNOME_FILES)
        .or_else(|| types.iter().find(|mime| mime.as_str() == URI_LIST))
        .or_else(|| if arbitrary { types.first() } else { None })
}

pub(crate) fn notify_clipboard(
    common: &std::rc::Rc<std::cell::RefCell<super::LinuxCommon>>,
    event: gpui::ClipboardChange,
) {
    let executor = common.borrow().foreground_executor.clone();
    let common = common.clone();
    executor
        .spawn(async move {
            let callback = common.borrow_mut().callbacks.clipboard_change.take();
            if let Some(mut callback) = callback {
                callback(event);
                let mut common = common.borrow_mut();
                if common.callbacks.clipboard_change.is_none() {
                    common.callbacks.clipboard_change = Some(callback);
                }
            }
        })
        .detach();
}

pub(crate) fn notify_drag(
    common: &std::rc::Rc<std::cell::RefCell<super::LinuxCommon>>,
    event: gpui::NativeDragEvent,
) {
    let executor = common.borrow().foreground_executor.clone();
    let common = common.clone();
    executor
        .spawn(async move {
            let callback = common.borrow_mut().callbacks.native_drag.take();
            if let Some(mut callback) = callback {
                callback(event);
                let mut common = common.borrow_mut();
                if common.callbacks.native_drag.is_none() {
                    common.callbacks.native_drag = Some(callback);
                }
            }
        })
        .detach();
}

pub(crate) fn allow_drop(
    common: &std::rc::Rc<std::cell::RefCell<super::LinuxCommon>>,
    event: gpui::NativeDropEvent,
    has_paths: bool,
) -> bool {
    let callback = common.borrow_mut().callbacks.native_drop.take();
    let Some(mut callback) = callback else {
        return has_paths;
    };
    let accepted = callback(event);
    let mut common = common.borrow_mut();
    if common.callbacks.native_drop.is_none() {
        common.callbacks.native_drop = Some(callback);
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::{fd::OwnedFd, unix::net::UnixStream},
        thread,
    };

    fn pipe() -> (File, File) {
        let (read, write) = UnixStream::pair().unwrap();
        (
            File::from(OwnedFd::from(read)),
            File::from(OwnedFd::from(write)),
        )
    }
    #[test]
    fn source_that_stops_writing_hits_absolute_deadline() {
        let (file, _writer) = pipe();
        let start = Instant::now();
        assert_eq!(
            read(file, &cancelled(), start + Duration::from_millis(40), 1024),
            Err(ClipboardReadError::TimedOut)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn continuously_writing_oversized_source_is_bounded() {
        let (file, mut writer) = pipe();
        let source = thread::spawn(move || while writer.write_all(&[1; 256]).is_ok() {});
        assert_eq!(
            read(file, &cancelled(), Instant::now() + TRANSFER_TIMEOUT, 1024),
            Err(ClipboardReadError::TooLarge)
        );
        source.join().unwrap();
    }
    #[test]
    fn drip_source_cannot_extend_deadline() {
        let (file, mut writer) = pipe();
        let source = thread::spawn(move || {
            while writer.write_all(b"a").is_ok() {
                thread::sleep(Duration::from_millis(5));
            }
        });
        let start = Instant::now();
        assert_eq!(
            read(file, &cancelled(), start + Duration::from_millis(50), 1024),
            Err(ClipboardReadError::TimedOut)
        );
        source.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn stalled_send_does_not_block_past_deadline() {
        let (_reader, file) = pipe();
        let bytes = vec![0; 1024 * 1024].into();
        assert_eq!(
            write(
                file,
                bytes,
                &cancelled(),
                Instant::now() + Duration::from_millis(40)
            ),
            Err(ClipboardReadError::TimedOut)
        );
    }
    #[test]
    fn cancelled_generation_stops_read_and_write() {
        let (file, _writer) = pipe();
        let cancel = cancelled();
        cancel.store(true, Ordering::Release);
        assert_eq!(
            read(file, &cancel, Instant::now() + TRANSFER_TIMEOUT, 1024),
            Err(ClipboardReadError::Cancelled)
        );
        let (_reader, file) = pipe();
        assert_eq!(
            write(
                file,
                Arc::from(&b"a"[..]),
                &cancel,
                Instant::now() + TRANSFER_TIMEOUT
            ),
            Err(ClipboardReadError::Cancelled)
        );
    }
    #[test]
    fn file_selection_serves_distinct_mime_bytes_and_escaped_uris() {
        let payload = file_payload(
            ["/fixture/a b#c".into(), "/fixture/second".into()],
            SelectionIntent::Cut,
        )
        .unwrap();
        assert_eq!(
            &*payload.get(URI_LIST).unwrap(),
            b"file:///fixture/a%20b%23c\r\nfile:///fixture/second\r\n"
        );
        assert_eq!(
            &*payload.get(GNOME_FILES).unwrap(),
            b"cut\nfile:///fixture/a%20b%23c\nfile:///fixture/second\n"
        );
        let item = decode(payload.representations().to_vec()).unwrap();
        assert_eq!(item.mime_payload().unwrap().intent(), SelectionIntent::Cut);
        assert_eq!(
            paths(item.mime_payload().unwrap()).unwrap().paths().len(),
            2
        );
    }
    #[test]
    fn uri_only_is_copy_and_custom_tokens_are_never_paths() {
        let item = decode(vec![MimeRepresentation::new(
            URI_LIST,
            &b"# comment\r\nfile:///a\r\n"[..],
        )])
        .unwrap();
        assert_eq!(item.mime_payload().unwrap().intent(), SelectionIntent::Copy);
        let item = decode(vec![MimeRepresentation::new(
            "application/x-test-token",
            &b"single-use-token"[..],
        )])
        .unwrap();
        assert!(
            paths(item.mime_payload().unwrap())
                .unwrap()
                .paths()
                .is_empty()
        );
        assert_eq!(
            &*item
                .mime_payload()
                .unwrap()
                .get("application/x-test-token")
                .unwrap(),
            b"single-use-token"
        );
    }
    #[test]
    fn old_source_snapshot_is_independent_of_replacement_before_send() {
        let old = snapshot(&ClipboardItem::new_mime(
            file_payload(["/old".into()], SelectionIntent::Cut).unwrap(),
        ))
        .unwrap();
        let replacement = snapshot(&ClipboardItem::new_string("replacement".into())).unwrap();
        assert_eq!(&*old.get(GNOME_FILES).unwrap(), b"cut\nfile:///old\n");
        assert!(replacement.get(GNOME_FILES).is_none());
        let cloned = old.clone();
        assert_eq!(old, cloned);
        assert!(Arc::ptr_eq(
            &old.get(URI_LIST).unwrap(),
            &cloned.get(URI_LIST).unwrap()
        ));
    }
    #[test]
    fn normal_text_and_images_retain_their_entry_types() {
        let item = ClipboardItem::new_string("a\r\nb".into());
        assert_eq!(
            decode(snapshot(&item).unwrap().representations().to_vec())
                .unwrap()
                .text()
                .unwrap(),
            "a\nb"
        );
        let item = ClipboardItem::new_image(&Image {
            id: 1,
            format: ImageFormat::Png,
            bytes: vec![1, 2, 3],
        });
        assert!(decode(snapshot(&item).unwrap().representations().to_vec()).unwrap().entries().iter().any(|entry| matches!(entry, ClipboardEntry::Image(image) if image.bytes == vec![1, 2, 3])));
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;
    #[test]
    fn native_transfer_serialization_range() {
        let paths: Vec<_> = (0..1000)
            .map(|index| std::path::PathBuf::from(format!("/fixture/file {index}#.txt")))
            .collect();
        let mut minimum = u128::MAX;
        let mut maximum = 0;
        let mut bytes = 0;
        for _ in 0..25 {
            let start = Instant::now();
            let payload = std::hint::black_box(
                file_payload(paths.iter().cloned(), SelectionIntent::Cut).unwrap(),
            );
            let elapsed = start.elapsed().as_micros();
            minimum = minimum.min(elapsed);
            maximum = maximum.max(elapsed);
            assert_eq!(
                std::str::from_utf8(&payload.get(URI_LIST).unwrap())
                    .unwrap()
                    .lines()
                    .count(),
                1000
            );
            bytes = payload
                .representations()
                .iter()
                .map(|representation| representation.bytes().len())
                .sum::<usize>();
        }
        println!(
            "mime_1000_us={minimum}..{maximum}; samples=25; advertised_bytes={bytes}; synthetic paths; no filesystem access"
        );
    }
}
