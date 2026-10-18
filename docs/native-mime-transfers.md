# Native MIME transfers on Linux

`ClipboardItem::new_mime` and `ExternalDragPayload::Mime` carry immutable `MimeRepresentation` values. Each representation has its own exact type and `Arc<[u8]>` bytes. Cloning the payload shares those bytes. Type names supplied by an application are opaque to GPUI.

Text and image constructors remain available. An `ExternalPaths` clipboard entry advertises escaped local file URLs through `text/uri-list`, GNOME's `copy` file representation, and plain-text aliases. To publish Cut intent, supply both the URI and GNOME representations with `SelectionIntent::Cut`:

```rust
use gpui::{ClipboardItem, MimePayload, MimeRepresentation, SelectionIntent};

let item = ClipboardItem::new_mime(MimePayload::new([
    MimeRepresentation::new("text/uri-list", &b"file:///example/a%20b\r\n"[..]),
    MimeRepresentation::new("x-special/gnome-copied-files", &b"cut\nfile:///example/a%20b\n"[..]),
], SelectionIntent::Cut));
```

`SelectionIntent` describes the application snapshot. It does not rewrite arbitrary representation bytes. The application must supply consistent GNOME bytes and intent. GPUI does not copy, move, delete, inspect or stat the selected files. Selection validation, collision handling, operation tokens and source removal remain application responsibilities.

Use `App::read_from_clipboard_async` for a foreign selection. The synchronous API returns an owned item or a completed cache; it does not wait for a pending foreign transfer. Wayland starts a background read when a selection arrives. X11 starts one from XFixes ownership events and an explicit asynchronous read, using its existing connection. A foreign file selection prefers GNOME data for Cut intent, then a URI list with Copy intent. Text and image entries retain their normal decoded entry types alongside the selected MIME representation.

`App::on_clipboard_change` reports ownership changes, clear events and receive errors with a generation. Discard work associated with an earlier generation. `clear_clipboard_if_owned` refuses to clear a foreign selection. Applications that already have an external clipboard helper must bridge the helper's validated snapshot to this owner instead of having both claim the native selection.

For native drags, supply `NativeDragPayload` with representations, a source identity, allowed actions and a preference. Ctrl selects Copy when permitted, Shift selects Move when permitted, and otherwise the preference chooses the action at lift. Wayland advertises that chosen action. The legacy `Files` payload retains its Copy and Move mask. Wayland has no Link action bit; an application's semantic link marker belongs in its own MIME bytes and is handled by its backend.

`App::on_native_drag_offer` chooses destination actions at Enter, before receiving data or processing Drop. Its default permits only Copy. An application can permit and prefer Move for its own validated operation policy; ordinary foreign drops can keep the Copy default. Offered type names alone do not validate an application token.

`App::on_native_drop` exposes advertised types, successfully selected representations, latest position, modifiers and the selected transport action. It is called on entry and again with `dropped: true` when submitting. Return false to reject the destination. The callback determines native acceptance; existing `ExternalPaths` drop handlers still receive legacy file events, so applications should arrange one transfer authority. An opaque token never becomes an `ExternalPaths` entry. Without a MIME handler, a plain-text-only drag is rejected.

`App::on_native_drag` reports selected action, pointer release, native finish and cancellation for the supplied source identity. A finish is transport completion and never authorizes GPUI to remove source files. Starting a native Wayland drag requires the held left-button grab on the source surface. Pending incoming reads survive Drop and its following Leave, while a genuine Leave or a new destination cancels the previous generation.

Native pipe transfers have an absolute two-second deadline and a 64 MiB receive ceiling, with at most 100,000 decoded local paths. Known aliases are represented by the preferred MIME rather than fetched repeatedly. Additional opaque types share the receive ceiling. Sends are capped at 64 MiB per representation and run nonblocking. At most sixteen Wayland transfers are admitted concurrently. X11 bounds incoming and outgoing INCR state to sixteen sessions per direction, reads properties in 64 KiB slices and expires active transfers after two seconds. Its transfer timer is absent while idle; ownership uses XFixes events and never polling or a helper process. Before quit or a requested display-mode change, it offers SAVE_TARGETS to an existing clipboard manager on the same connection and waits asynchronously for acknowledgement or the bounded deadline.

Headless tests use in-process Wayland offer and X11 selection fakes, real anonymous pipes, and the same lifecycle helpers called by the platforms. These are deterministic protocol and transport tests. They do not establish compositor, clipboard-manager or file-manager interoperability, native frame time, or process memory cost.
