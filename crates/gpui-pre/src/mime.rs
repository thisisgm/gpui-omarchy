use std::sync::Arc;

use crate::{AnyWindowHandle, ClipboardReadError, Modifiers, Pixels, Point};

/// A single immutable representation of a clipboard selection or native drag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MimeRepresentation {
    mime_type: Arc<str>,
    bytes: Arc<[u8]>,
}

impl MimeRepresentation {
    /// Captures bytes once, independently of later clipboard or selection changes.
    pub fn new(mime_type: impl Into<Arc<str>>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            mime_type: mime_type.into(),
            bytes: bytes.into(),
        }
    }

    /// The exact advertised type.
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// The bytes served only for this type.
    pub fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }
}

/// File-selection intent, independent of the negotiated transport action.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SelectionIntent {
    /// Copy leaves the originals in place.
    #[default]
    Copy,
    /// Cut asks the application backend to move its validated snapshot.
    Cut,
}

/// Immutable MIME representations supplied by an application or foreign owner.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MimePayload {
    representations: Arc<[MimeRepresentation]>,
    intent: SelectionIntent,
    offered_types: Arc<[String]>,
}

impl MimePayload {
    /// Captures distinct MIME types, preserving the first representation of each type.
    pub fn new(
        representations: impl IntoIterator<Item = MimeRepresentation>,
        intent: SelectionIntent,
    ) -> Self {
        let mut unique: Vec<MimeRepresentation> = Vec::new();
        for representation in representations {
            if !representation.mime_type.is_empty()
                && !unique
                    .iter()
                    .any(|other| other.mime_type == representation.mime_type)
            {
                unique.push(representation);
            }
        }
        let offered_types = unique
            .iter()
            .map(|representation| representation.mime_type().to_owned())
            .collect::<Vec<_>>()
            .into();
        Self {
            representations: unique.into(),
            intent,
            offered_types,
        }
    }

    /// Records bounded native offered names, including alternatives whose bytes were not fetched.
    pub fn with_offered_types(mut self, names: impl IntoIterator<Item = String>) -> Self {
        let mut offered = Vec::new();
        for name in names {
            if offered.len() >= 64 {
                break;
            }
            if !name.is_empty() && name.len() <= 4096 && !offered.contains(&name) {
                offered.push(name);
            }
        }
        self.offered_types = offered.into();
        self
    }

    /// Bounded native offered names; these names alone do not validate application data.
    pub fn offered_types(&self) -> &[String] {
        &self.offered_types
    }

    /// All representations, in caller preference order.
    pub fn representations(&self) -> &[MimeRepresentation] {
        &self.representations
    }

    /// The immutable bytes associated with a requested type.
    pub fn get(&self, mime_type: &str) -> Option<Arc<[u8]>> {
        self.representations
            .iter()
            .find(|entry| entry.mime_type() == mime_type)
            .map(|entry| entry.bytes.clone())
    }

    /// The selection's semantic intent.
    pub fn intent(&self) -> SelectionIntent {
        self.intent
    }
}

/// An action understood by the native data-transfer protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDragAction {
    /// Copy transport.
    Copy,
    /// Move transport; the GUI never removes the source files.
    Move,
}

/// Actions permitted by the lift, and its preferred transport action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDragActions {
    /// Whether the destination may select Copy.
    pub copy: bool,
    /// Whether the destination may select Move.
    pub move_: bool,
    /// Must be one of the permitted actions.
    pub preferred: NativeDragAction,
}

impl Default for NativeDragActions {
    fn default() -> Self {
        Self {
            copy: true,
            move_: false,
            preferred: NativeDragAction::Copy,
        }
    }
}

/// An application-supplied native drag, including opaque application MIME types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDragPayload {
    /// Bytes captured from the backend's resolved selection.
    pub data: MimePayload,
    /// Copy or Move capabilities for this gesture.
    pub actions: NativeDragActions,
    /// Application identity echoed in source completion events.
    pub source_id: u64,
}

/// A native destination event. Return false from the destination handler to reject it.
#[derive(Clone, Debug)]
pub struct NativeDropEvent {
    /// The receiving window.
    pub window: AnyWindowHandle,
    /// The latest pointer position.
    pub position: Point<Pixels>,
    /// Whether this event submits the drop, rather than entering the destination.
    pub dropped: bool,
    /// Every type the source advertised, including types that were not read.
    pub offered_types: Arc<[String]>,
    /// Successfully read representations; custom tokens remain opaque bytes.
    pub data: MimePayload,
    /// The negotiated native action, absent for a rejected or unfinished negotiation.
    pub action: Option<NativeDragAction>,
    /// Local modifier state for application policy.
    pub modifiers: Modifiers,
}

/// Lifecycle notification from an outbound native drag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDragPhase {
    /// The destination selected a transport action.
    Action,
    /// The pointer was released at the destination; not proof of completion.
    Dropped,
    /// The destination finished the native transfer.
    Finished,
    /// The native session was cancelled.
    Cancelled,
}

/// Native completion carries identity and action without authorizing any file operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDragEvent {
    /// Identity supplied at lift.
    pub source_id: u64,
    /// The last selected action.
    pub action: Option<NativeDragAction>,
    /// Session lifecycle phase.
    pub phase: NativeDragPhase,
}

/// Selection replacement, clear or read failure, used to retire cut marks.
#[derive(Clone, Debug)]
pub struct ClipboardChange {
    /// True for PRIMARY, false for CLIPBOARD.
    pub primary: bool,
    /// Monotonically changing selection generation.
    pub generation: u64,
    /// True while this application owns the selection.
    pub owned: bool,
    /// An asynchronous receive failure, if any.
    pub error: Option<ClipboardReadError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn two_mime_requests_return_their_own_immutable_bytes() {
        let payload = MimePayload::new(
            [
                MimeRepresentation::new("text/uri-list", &b"file:///a\r\n"[..]),
                MimeRepresentation::new("x-special/gnome-copied-files", &b"cut\nfile:///a\n"[..]),
                MimeRepresentation::new("application/x-test-token", &b"token"[..]),
            ],
            SelectionIntent::Cut,
        );
        assert_eq!(&*payload.get("text/uri-list").unwrap(), b"file:///a\r\n");
        assert_eq!(
            &*payload.get("x-special/gnome-copied-files").unwrap(),
            b"cut\nfile:///a\n"
        );
        assert!(payload.get("text/plain").is_none());
        let cloned = payload.clone();
        assert_eq!(payload, cloned);
        assert!(Arc::ptr_eq(
            &payload.get("application/x-test-token").unwrap(),
            &cloned.get("application/x-test-token").unwrap()
        ));
    }
}

/// Offer-time destination policy, evaluated before receiving data or processing Drop.
#[derive(Clone, Debug)]
pub struct NativeDragOffer {
    /// The destination window.
    pub window: AnyWindowHandle,
    /// The pointer position at Enter.
    pub position: Point<Pixels>,
    /// Types announced by the source; their bytes have not been validated yet.
    pub offered_types: Arc<[String]>,
    /// Current modifiers, for application transport policy.
    pub modifiers: Modifiers,
}
