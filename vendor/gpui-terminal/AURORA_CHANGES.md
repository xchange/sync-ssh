Vendored from gpui-terminal 0.1.0, by Leonard Seibold.
Upstream: https://github.com/zortax/gpui-terminal
Original MIT / Apache-2.0 licenses are included.

Changes inherited from Aurora Terminal (maintained by SyncSSH):
- Native GPUI input handler for text and IME composition; shell control keys stay terminal keys.
- Terminal-specific Tab bindings preserve shell completion inside GPUI Component Root.
- Explicit input gate disables keyboard, paste, IME and mouse transport writes while a modal is open.
- Drag, double-click word, and triple-click line selection, explicit clipboard copy and bracketed paste.
- Scrollback rendering, bounded output channel and configurable history.
- SGR/legacy/UTF8 mouse reporting, drag motion, wheel and alternate-screen scrolling.
- Clipboard-mutating upstream tests are ignored unless explicitly run in an isolated desktop.
- Device status responses returned to the PTY instead of silently dropped.
- Correct handling of display offset, hidden cursor, inverse cells, wide spacers and combining marks.
- Content-free input event and cursor geometry for optional decorative effects.
- Background literal/regex search over grid snapshots, including scrollback and soft wraps, with explicit case matching, bounded result counts, independent highlights and navigation.
- Immediate terminal-exit events for hidden tabs; terminal input remains disabled after the output stream ends.
- Live appearance updates retain the terminal session: font metrics trigger resize, history limits resize existing scrollback, and application cursor-shape escapes override the configured default. Cursor blinking uses a bounded timer only while the focused terminal is rendered.
- Ordered per-character font fallback for terminal output, bold/italic styles and IME composition. Native font cascades are supplemented by cached glyph-coverage checks so custom and embedded fallback fonts also work on GPUI backends without full cascade support; cell dimensions remain based on the primary font.
- SyncSSH does not register OSC52 clipboard callbacks, so remote output cannot read or overwrite the clipboard.

This is an application-maintained patch, not a claim that upstream provides these features.
