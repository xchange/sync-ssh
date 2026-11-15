//! Main terminal view component for GPUI.
//!
//! This module provides [`TerminalView`], the primary component for embedding terminals
//! in GPUI applications. It manages:
//!
//! - **I/O Streams**: Accepts arbitrary [`Read`]/[`Write`]
//!   streams, allowing integration with any PTY implementation
//! - **Event Handling**: Keyboard and mouse input, with configurable callbacks
//! - **Rendering**: Efficient canvas-based rendering via [`TerminalRenderer`]
//! - **Configuration**: Font, colors, dimensions, and padding via [`TerminalConfig`]
//!
//! # Architecture
//!
//! The terminal uses a push-based async I/O architecture:
//!
//! 1. A background thread reads bytes from the PTY stdout in 4KB chunks
//! 2. Bytes are sent through a [flume](https://docs.rs/flume) channel to an async task
//! 3. The async task processes bytes through the VTE parser and calls `cx.notify()`
//! 4. GPUI repaints the terminal with the updated grid
//!
//! This approach ensures the terminal only wakes when data arrives, avoiding polling.
//!
//! # Thread Safety
//!
//! - [`TerminalView`] itself is not `Send` (it contains GPUI handles)
//! - The stdin writer is wrapped in `Arc<parking_lot::Mutex<>>` for thread-safe writes
//! - Callbacks ([`ResizeCallback`], [`KeyHandler`]) must be `Send + Sync`
//!
//! # Example
//!
//! ```ignore
//! use gpui::{Context, Edges, px};
//! use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};
//!
//! // In a GPUI window context:
//! let terminal = cx.new(|cx| {
//!     TerminalView::new(pty_writer, pty_reader, TerminalConfig::default(), cx)
//!         .with_resize_callback(move |cols, rows| {
//!             // Notify PTY of new dimensions
//!         })
//!         .with_exit_callback(|_, cx| {
//!             cx.quit();
//!         })
//! });
//!
//! // Focus the terminal to receive keyboard input
//! terminal.read(cx).focus_handle().focus(window);
//! ```

use crate::colors::ColorPalette;
use crate::event::{GpuiEventProxy, TerminalEvent};
use crate::input::keystroke_to_bytes;
use crate::render::TerminalRenderer;
use crate::search::{SearchOptions, SearchSummary, search_grid};
use crate::terminal::TerminalState;
use alacritty_terminal::{
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point as GridPoint},
    selection::{Selection, SelectionType},
    term::TermMode,
};
use gpui::{Edges, *};
use std::cell::Cell;
use std::io::{Read, Write};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub use alacritty_terminal::vte::ansi::CursorShape;

/// An input notification deliberately contains no typed text.
#[derive(Clone, Copy)]
pub struct InputPulse;

/// The transport rejected some input; no typed bytes are exposed.
#[derive(Clone, Copy)]
pub struct InputWriteError;

/// Emitted as soon as the output reader finishes, including in background tabs.
#[derive(Clone, Copy)]
pub struct TerminalExited;

#[derive(Clone, Copy)]
pub struct SearchChanged;

actions!(syncssh_terminal, [TerminalTab, TerminalBackTab]);
struct TerminalKeyBindings;
impl Global for TerminalKeyBindings {}

#[derive(Clone, Copy, Default)]
struct Geometry {
    bounds: Bounds<Pixels>,
    cell: Size<Pixels>,
}

/// Configuration for terminal creation and runtime updates.
///
/// This struct defines the terminal's appearance and behavior, including
/// grid dimensions, font settings, scrollback buffer, and color scheme.
///
/// # Default Values
///
/// | Field | Default |
/// |-------|---------|
/// | `cols` | 80 |
/// | `rows` | 24 |
/// | `font_family` | "monospace" |
/// | `font_fallbacks` | [] |
/// | `font_size` | 14px |
/// | `scrollback` | 10000 |
/// | `line_height_multiplier` | 1.2 |
/// | `padding` | 0px all sides |
/// | `colors` | Default palette |
///
/// # Example
///
/// ```ignore
/// use gpui::{Edges, px};
/// use gpui_terminal::{ColorPalette, TerminalConfig};
///
/// let config = TerminalConfig {
///     cols: 120,
///     rows: 40,
///     font_family: "JetBrains Mono".into(),
///     font_fallbacks: vec!["monospace".into()],
///     font_size: px(13.0),
///     scrollback: 50000,
///     line_height_multiplier: 1.1,
///     padding: Edges::all(px(10.0)),
///     colors: ColorPalette::builder()
///         .background(0x1a, 0x1a, 0x1a)
///         .foreground(0xe0, 0xe0, 0xe0)
///         .build(),
/// };
/// ```
///
/// # Runtime Updates
///
/// Configuration can be updated at runtime via [`TerminalView::update_config`].
/// This is useful for implementing features like dynamic font sizing:
///
/// ```ignore
/// terminal.update(cx, |terminal, cx| {
///     let mut config = terminal.config().clone();
///     config.font_size += px(1.0);
///     terminal.update_config(config, cx);
/// });
/// ```
#[derive(Clone, Debug)]
pub struct TerminalConfig {
    /// Number of columns (character width) in the terminal
    pub cols: usize,

    /// Number of rows (lines) in the terminal
    pub rows: usize,

    /// Font family name (e.g., "Fira Code", "JetBrains Mono")
    pub font_family: String,

    /// Ordered fallback families for characters absent from the primary font.
    pub font_fallbacks: Vec<String>,

    /// Font size in pixels
    pub font_size: Pixels,

    /// Maximum number of scrollback lines to keep in history
    pub scrollback: usize,

    /// Multiplier for line height to accommodate tall glyphs (e.g., nerd fonts)
    /// Default is 1.2 (20% extra height)
    pub line_height_multiplier: f32,

    /// Padding around the terminal content (top, right, bottom, left)
    /// The padding area renders with the terminal's background color
    pub padding: Edges<Pixels>,

    /// Color palette for terminal colors (16 ANSI colors, 256 extended colors,
    /// foreground, background, and cursor colors)
    pub colors: ColorPalette,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            font_family: "monospace".into(),
            font_fallbacks: Vec::new(),
            font_size: px(14.0),
            scrollback: 10000,
            line_height_multiplier: 1.2,
            padding: Edges::all(px(0.0)),
            colors: ColorPalette::default(),
        }
    }
}

/// Callback type for PTY resize notifications.
///
/// This callback is invoked when the terminal grid dimensions change,
/// typically due to window resizing. The callback receives the new
/// column and row counts.
///
/// # Arguments
///
/// * `cols` - New number of columns (characters wide)
/// * `rows` - New number of rows (lines tall)
///
/// # Thread Safety
///
/// This callback must be `Send + Sync` as it may be called from the render thread.
///
/// # Example
///
/// ```ignore
/// use portable_pty::PtySize;
///
/// let pty = Arc::new(Mutex::new(pty_master));
/// let pty_clone = pty.clone();
///
/// terminal.with_resize_callback(move |cols, rows| {
///     pty_clone.lock().resize(PtySize {
///         cols: cols as u16,
///         rows: rows as u16,
///         pixel_width: 0,
///         pixel_height: 0,
///     }).ok();
/// });
/// ```
pub type ResizeCallback = Box<dyn Fn(usize, usize) + Send + Sync>;

/// Callback type for key event interception.
///
/// This callback is invoked before the terminal processes a key event,
/// allowing you to intercept and handle specific key combinations.
///
/// # Arguments
///
/// * `event` - The key down event from GPUI
///
/// # Returns
///
/// * `true` - Consume the event (terminal will not process it)
/// * `false` - Let the terminal handle the event normally
///
/// # Thread Safety
///
/// This callback must be `Send + Sync`.
///
/// # Example
///
/// ```ignore
/// terminal.with_key_handler(|event| {
///     let keystroke = &event.keystroke;
///
///     // Intercept Ctrl++ for font size increase
///     if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
///         // Handle font size increase
///         return true; // Consume the event
///     }
///
///     // Intercept Ctrl+- for font size decrease
///     if keystroke.modifiers.control && keystroke.key == "-" {
///         // Handle font size decrease
///         return true;
///     }
///
///     false // Let terminal handle all other keys
/// });
/// ```
pub type KeyHandler = Box<dyn Fn(&KeyDownEvent) -> bool + Send + Sync>;

/// Callback for terminal bell events.
///
/// This callback is invoked when the terminal bell is triggered (BEL character,
/// ASCII 0x07), allowing you to play a sound or show a visual indicator.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_bell_callback(|window, cx| {
///     // Option 1: Visual bell (flash the window or show an indicator)
///     // Option 2: Play a sound
///     // Option 3: Notify the user via system notification
/// });
/// ```
pub type BellCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// Callback for terminal title changes.
///
/// This callback is invoked when the terminal title changes via escape sequences
/// (OSC 0, OSC 2), allowing you to update the window or tab title.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `title` - The new title string
///
/// # Example
///
/// ```ignore
/// terminal.with_title_callback(|window, cx, title| {
///     // Update the window title
///     // Or update a tab label in a tabbed interface
///     println!("Terminal title changed to: {}", title);
/// });
/// ```
pub type TitleCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for clipboard store requests.
///
/// This callback is invoked when the terminal wants to store data to the clipboard
/// via OSC 52 escape sequence. Applications like tmux and vim can use this to
/// copy text to the system clipboard.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `text` - The text to store in the clipboard
///
/// # Example
///
/// ```ignore
/// use gpui_terminal::Clipboard;
///
/// terminal.with_clipboard_store_callback(|window, cx, text| {
///     if let Ok(mut clipboard) = Clipboard::new() {
///         clipboard.copy(text).ok();
///     }
/// });
/// ```
pub type ClipboardStoreCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for terminal exit events.
///
/// This callback is invoked when the terminal process exits (e.g., shell exits,
/// process terminates). This is detected when the PTY reader reaches EOF.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_exit_callback(|window, cx| {
///     // Option 1: Quit the application
///     cx.quit();
///
///     // Option 2: Close this terminal tab/pane
///     // terminal_manager.close_terminal(terminal_id);
///
///     // Option 3: Show an exit message
///     // show_notification("Terminal exited");
/// });
/// ```
pub type ExitCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// The main terminal view component for GPUI applications.
///
/// `TerminalView` is a GPUI entity that implements the [`Render`] trait,
/// providing a complete terminal emulator that can be embedded in any GPUI application.
///
/// # Responsibilities
///
/// - **Terminal State**: Manages the grid, cursor, and colors via [`TerminalState`]
/// - **I/O Streams**: Reads from PTY stdout and writes to PTY stdin
/// - **Event Handling**: Processes keyboard, mouse, and resize events
/// - **Rendering**: Paints text, backgrounds, and cursor via [`TerminalRenderer`]
/// - **Callbacks**: Dispatches events to user-provided callbacks
///
/// # Creating a Terminal
///
/// Use [`TerminalView::new`] within a GPUI entity context:
///
/// ```ignore
/// let terminal = cx.new(|cx| {
///     TerminalView::new(writer, reader, config, cx)
///         .with_resize_callback(resize_callback)
///         .with_exit_callback(|_, cx| cx.quit())
/// });
/// ```
///
/// # Focus
///
/// The terminal must be focused to receive keyboard input:
///
/// ```ignore
/// terminal.read(cx).focus_handle().focus(window);
/// ```
///
/// # Callbacks
///
/// Configure behavior through builder methods:
///
/// - [`with_resize_callback`](Self::with_resize_callback) - PTY size changes
/// - [`with_exit_callback`](Self::with_exit_callback) - Process exit
/// - [`with_key_handler`](Self::with_key_handler) - Key event interception
/// - [`with_bell_callback`](Self::with_bell_callback) - Terminal bell
/// - [`with_title_callback`](Self::with_title_callback) - Title changes
/// - [`with_clipboard_store_callback`](Self::with_clipboard_store_callback) - Clipboard writes
///
/// # Thread Safety
///
/// `TerminalView` is not `Send` as it contains GPUI handles. The stdin writer
/// is internally wrapped in `Arc<parking_lot::Mutex<>>` for safe concurrent access.
pub struct TerminalView {
    /// The terminal state managing the grid and VTE parser
    state: TerminalState,
    geometry: Rc<Cell<Geometry>>,
    selecting: bool,
    marked: String,
    scroll_remainder: f32,
    input_enabled: bool,
    exited: bool,
    search_options: Option<SearchOptions>,
    search_summary: SearchSummary,
    search_task: Option<Task<()>>,
    content_revision: u64,
    search_generation: u64,
    search_reveal: bool,
    default_cursor_shape: CursorShape,
    default_cursor_blink: bool,
    cursor_visible: bool,
    cursor_blink_task: Option<Task<()>>,

    /// The renderer for drawing terminal content
    renderer: TerminalRenderer,

    /// Focus handle for keyboard event handling
    focus_handle: FocusHandle,

    /// Writer for sending input to the terminal process
    stdin_writer: Arc<parking_lot::Mutex<Box<dyn Write + Send>>>,

    /// Receiver for terminal events from the event proxy
    event_rx: mpsc::Receiver<TerminalEvent>,

    /// Configuration used to create this terminal
    config: TerminalConfig,

    /// Async task that reads bytes and notifies the view (push-based)
    #[allow(dead_code)]
    _reader_task: Task<()>,

    /// Callback to notify the PTY about size changes
    resize_callback: Option<Arc<ResizeCallback>>,

    /// Optional callback to intercept key events before terminal processing
    key_handler: Option<Arc<KeyHandler>>,

    /// Callback for terminal bell events
    bell_callback: Option<BellCallback>,

    /// Callback for terminal title changes
    title_callback: Option<TitleCallback>,

    /// Callback for clipboard store requests
    clipboard_store_callback: Option<ClipboardStoreCallback>,

    /// Callback for terminal exit events
    exit_callback: Option<ExitCallback>,
}

impl TerminalView {
    /// Create a new terminal with provided I/O streams.
    ///
    /// This method initializes a new terminal emulator with the given stdin writer
    /// and stdout reader. It spawns a background task to read from stdout and
    /// process incoming bytes through the VTE parser.
    ///
    /// # Arguments
    ///
    /// * `stdin_writer` - Writer for sending input bytes to the terminal process
    /// * `stdout_reader` - Reader for receiving output bytes from the terminal process
    /// * `config` - Terminal configuration (dimensions, font, etc.)
    /// * `cx` - GPUI context for this view
    ///
    /// # Returns
    ///
    /// A new `TerminalView` instance ready to be rendered.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // In a GPUI window context:
    /// let terminal = cx.new(|cx| {
    ///     TerminalView::new(stdin_writer, stdout_reader, TerminalConfig::default(), cx)
    /// });
    /// ```
    pub fn new<W, R>(
        stdin_writer: W,
        stdout_reader: R,
        config: TerminalConfig,
        cx: &mut Context<Self>,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        if !cx.has_global::<TerminalKeyBindings>() {
            cx.bind_keys([
                KeyBinding::new("tab", TerminalTab, Some("SyncSSH")),
                KeyBinding::new("shift-tab", TerminalBackTab, Some("SyncSSH")),
            ]);
            cx.set_global(TerminalKeyBindings);
        }
        // Create event channel for terminal events
        let (event_tx, event_rx) = mpsc::channel();

        // Clone event_tx for the reader task to send Exit event when PTY closes
        let exit_event_tx = event_tx.clone();

        // Create event proxy for alacritty
        let event_proxy = GpuiEventProxy::new(event_tx);

        // Create terminal state
        let mut state = TerminalState::new_with_scrollback(
            config.cols,
            config.rows,
            config.scrollback,
            event_proxy,
        );
        state.update_preferences(
            config.scrollback,
            alacritty_terminal::vte::ansi::CursorStyle {
                shape: CursorShape::Underline,
                blinking: false,
            },
        );

        // Create renderer with font settings and color palette
        let mut renderer = TerminalRenderer::new(
            config.font_family.clone(),
            config.font_size,
            config.line_height_multiplier,
            config.colors.clone(),
        );
        renderer.font_fallbacks = Some(FontFallbacks::from_fonts(config.font_fallbacks.clone()));

        // Create focus handle
        let focus_handle = cx.focus_handle();

        // Wrap stdin writer in Arc<Mutex> for thread-safe access
        let stdin_writer = Arc::new(parking_lot::Mutex::new(
            Box::new(stdin_writer) as Box<dyn Write + Send>
        ));

        // Create async channel for bytes (push-based notification)
        // Using flume instead of smol::channel because flume is executor-agnostic
        // and properly wakes GPUI's async executor when data arrives
        let (bytes_tx, bytes_rx) = flume::bounded::<Vec<u8>>(256);

        // Spawn background thread to read from stdout
        // This thread sends bytes through the async channel
        thread::spawn(move || {
            Self::read_stdout_blocking(stdout_reader, bytes_tx);
        });

        // Spawn async task that awaits on the channel and notifies the view
        // This is push-based: the task blocks until bytes arrive, then immediately notifies
        let reader_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                // Wait for bytes from the background reader (blocks until data arrives)
                match bytes_rx.recv_async().await {
                    Ok(bytes) => {
                        // Process bytes and notify the view
                        let result = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                            view.state.process_bytes(&bytes);
                            view.content_changed(cx);
                            cx.notify();
                        });
                        if result.is_err() {
                            // View was dropped, exit
                            break;
                        }
                    }
                    Err(_) => {
                        // Channel closed - PTY has finished, send Exit event
                        let _ = exit_event_tx.send(TerminalEvent::Exit);
                        // Notify view to process the Exit event
                        let _ = this.update(cx, |view, cx: &mut Context<Self>| {
                            view.exited = true;
                            view.input_enabled = false;
                            view.marked.clear();
                            cx.emit(TerminalExited);
                            cx.notify();
                        });
                        break;
                    }
                }
            }
        });

        Self {
            state,
            geometry: Rc::new(Cell::new(Geometry::default())),
            selecting: false,
            marked: String::new(),
            scroll_remainder: 0.0,
            input_enabled: true,
            exited: false,
            search_options: None,
            search_summary: SearchSummary::default(),
            search_task: None,
            content_revision: 0,
            search_generation: 0,
            search_reveal: false,
            default_cursor_shape: CursorShape::Underline,
            default_cursor_blink: false,
            cursor_visible: true,
            cursor_blink_task: None,
            renderer,
            focus_handle,
            stdin_writer,
            event_rx,
            config,
            _reader_task: reader_task,
            resize_callback: None,
            key_handler: None,
            bell_callback: None,
            title_callback: None,
            clipboard_store_callback: None,
            exit_callback: None,
        }
    }

    /// Set a callback to be invoked when the terminal is resized.
    ///
    /// This callback should resize the underlying PTY to match the new dimensions.
    /// The callback receives (cols, rows) as arguments.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with (cols, rows) on resize
    pub fn with_resize_callback(
        mut self,
        callback: impl Fn(usize, usize) + Send + Sync + 'static,
    ) -> Self {
        self.resize_callback = Some(Arc::new(Box::new(callback)));
        self
    }

    /// Set a callback to intercept key events before terminal processing.
    ///
    /// The callback receives the key event and should return `true` to consume
    /// the event (prevent the terminal from processing it), or `false` to allow
    /// normal terminal processing.
    ///
    /// # Arguments
    ///
    /// * `handler` - A function that receives key events and returns whether to consume them
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_key_handler(|event| {
    ///     // Handle Ctrl++ to increase font size
    ///     if event.keystroke.modifiers.control && event.keystroke.key == "+" {
    ///         // Handle the event
    ///         return true; // Consume the event
    ///     }
    ///     false // Let terminal handle it
    /// })
    /// ```
    pub fn with_key_handler(
        mut self,
        handler: impl Fn(&KeyDownEvent) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.key_handler = Some(Arc::new(Box::new(handler)));
        self
    }

    /// Set a callback to be invoked when the terminal bell is triggered.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to play a sound or show a visual indicator.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the bell is triggered
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_bell_callback(|window, cx| {
    ///     // Play a sound or flash the screen
    /// })
    /// ```
    pub fn with_bell_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.bell_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal title changes.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the new title string.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the new title
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_title_callback(|window, cx, title| {
    ///     // Update window title or tab title
    /// })
    /// ```
    pub fn with_title_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.title_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal wants to store data to the clipboard.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the text to store. This is typically triggered by OSC 52 escape sequences.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the text to store
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_clipboard_store_callback(|window, cx, text| {
    ///     // Store text to system clipboard
    /// })
    /// ```
    pub fn with_clipboard_store_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.clipboard_store_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal process exits.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to close the terminal view or show an exit message.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the process exits
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_exit_callback(|window, cx| {
    ///     // Close the terminal tab or show exit message
    /// })
    /// ```
    pub fn with_exit_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.exit_callback = Some(Box::new(callback));
        self
    }

    /// Background thread that reads from stdout.
    ///
    /// This function runs in a background thread, continuously reading bytes
    /// from the stdout reader and sending them through the async channel.
    /// The async channel allows the main async task to be woken up immediately
    /// when data arrives (push-based).
    fn read_stdout_blocking<R: Read + Send + 'static>(
        mut stdout_reader: R,
        bytes_tx: flume::Sender<Vec<u8>>,
    ) {
        let mut buffer = [0u8; 4096];

        loop {
            match stdout_reader.read(&mut buffer) {
                Ok(0) => {
                    // EOF - channel will be dropped, signaling completion
                    break;
                }
                Ok(n) => {
                    // Send bytes to the async task
                    let bytes = buffer[..n].to_vec();
                    if bytes_tx.send(bytes).is_err() {
                        break; // Channel closed
                    }
                }
                Err(_) => {
                    // Read error
                    break;
                }
            }
        }
    }

    /// Handle keyboard input events.
    ///
    /// Converts GPUI keystrokes to terminal escape sequences and writes them
    /// to the stdin writer. If a key handler is set and returns true, the event
    /// is consumed and not sent to the terminal.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.input_enabled {
            cx.stop_propagation();
            return;
        }
        if self
            .key_handler
            .as_ref()
            .is_some_and(|handler| handler(event))
        {
            return;
        }
        let key = &event.keystroke;
        let shortcut = if cfg!(target_os = "macos") {
            key.modifiers.platform
        } else {
            key.modifiers.control && key.modifiers.shift
        };
        if shortcut && key.key.eq_ignore_ascii_case("c") {
            if let Some(text) = self.state.with_term(|term| term.selection_to_string()) {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.stop_propagation();
            return;
        }
        if shortcut && key.key.eq_ignore_ascii_case("v") {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.paste(&text, cx);
            }
            cx.stop_propagation();
            return;
        }
        if key.modifiers.platform {
            return;
        }
        // Text, including keyboard layouts and composition, is committed by the platform IME.
        if !key.modifiers.control
            && !key.modifiers.alt
            && (key.key.chars().count() == 1 || key.key == "space")
        {
            return;
        }
        if !self.marked.is_empty() {
            return;
        }
        if let Some(bytes) = keystroke_to_bytes(key, self.state.mode()) {
            self.send_input(&bytes, cx);
            cx.stop_propagation();
        }
    }

    /// Gate every user-originating transport write, including native IME commits.
    pub fn set_input_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.input_enabled = enabled && !self.exited;
        if !self.input_enabled {
            self.marked.clear();
            self.selecting = false;
        }
        cx.notify();
    }

    fn send_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if !self.input_enabled {
            return;
        }
        self.state.with_term_mut(|term| {
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        });
        let mut writer = self.stdin_writer.lock();
        let result = writer.write_all(bytes).and_then(|_| writer.flush());
        drop(writer);
        if result.is_ok() {
            cx.emit(InputPulse);
        } else {
            cx.emit(InputWriteError);
        }
        cx.notify();
    }

    pub fn paste(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.state.mode().contains(TermMode::BRACKETED_PASTE) {
            let cleaned = text.replace('\x1b', "");
            self.send_input(format!("\x1b[200~{cleaned}\x1b[201~").as_bytes(), cx);
        } else {
            self.send_input(
                text.replace("\r\n", "\r").replace('\n', "\r").as_bytes(),
                cx,
            );
        }
    }

    fn point_for_position(&self, position: Point<Pixels>) -> GridPoint {
        let g = self.geometry.get();
        let width = f32::from(g.cell.width).max(1.0);
        let height = f32::from(g.cell.height).max(1.0);
        let col = (f32::from(position.x - g.bounds.origin.x - self.config.padding.left) / width)
            .floor()
            .max(0.) as usize;
        let row = (f32::from(position.y - g.bounds.origin.y - self.config.padding.top) / height)
            .floor()
            .max(0.) as usize;
        self.state.with_term(|term| {
            GridPoint::new(
                Line(row.min(term.screen_lines() - 1) as i32 - term.grid().display_offset() as i32),
                Column(col.min(term.columns() - 1)),
            )
        })
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_enabled {
            return;
        }
        window.focus(&self.focus_handle);
        if !event.modifiers.shift && self.state.mode().intersects(TermMode::MOUSE_MODE) {
            self.report_mouse(
                event.position,
                match event.button {
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    _ => 0,
                },
                false,
                event.modifiers,
            );
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let point = self.point_for_position(event.position);
        let kind = if event.click_count >= 3 {
            SelectionType::Lines
        } else if event.click_count == 2 {
            SelectionType::Semantic
        } else {
            SelectionType::Simple
        };
        self.state.with_term_mut(|term| {
            term.selection = Some(Selection::new(
                kind,
                point,
                alacritty_terminal::index::Side::Left,
            ))
        });
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        if !self.selecting
            && !event.modifiers.shift
            && self.state.mode().intersects(TermMode::MOUSE_MODE)
        {
            self.report_mouse(
                event.position,
                match event.button {
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    _ => 0,
                },
                true,
                event.modifiers,
            );
        }
        self.selecting = false;
    }

    fn report_mouse(
        &mut self,
        position: Point<Pixels>,
        button: u8,
        released: bool,
        modifiers: Modifiers,
    ) {
        if !self.input_enabled {
            return;
        }
        let mut point = self.point_for_position(position);
        point.line.0 += self.state.with_term(|term| term.grid().display_offset()) as i32;
        let flags =
            crate::mouse::encode_modifiers(modifiers.shift, modifiers.alt, modifiers.control);
        let bytes = crate::mouse::encode_report(
            button | flags,
            released,
            point.column.0 + 1,
            point.line.0.max(0) as usize + 1,
            self.state.mode(),
        );
        let mut writer = self.stdin_writer.lock();
        let _ = writer.write_all(&bytes);
        let _ = writer.flush();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = self.state.mode();
        if !self.selecting
            && !event.modifiers.shift
            && (mode.contains(TermMode::MOUSE_MOTION)
                || (mode.contains(TermMode::MOUSE_DRAG) && event.pressed_button.is_some()))
        {
            let button = match event.pressed_button {
                Some(MouseButton::Left) => 0,
                Some(MouseButton::Middle) => 1,
                Some(MouseButton::Right) => 2,
                _ => 3,
            };
            self.report_mouse(event.position, button + 32, false, event.modifiers);
        }
        if self.selecting && event.dragging() {
            let point = self.point_for_position(event.position);
            self.state.with_term_mut(|term| {
                if let Some(selection) = &mut term.selection {
                    selection.update(point, alacritty_terminal::index::Side::Right);
                }
            });
            cx.notify();
        }
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_enabled {
            return;
        }
        let height = self.geometry.get().cell.height.max(px(1.));
        self.scroll_remainder += f32::from(event.delta.pixel_delta(height).y) / f32::from(height);
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines != 0 {
            let mode = self.state.mode();
            if !event.modifiers.shift && mode.intersects(TermMode::MOUSE_MODE) {
                for _ in 0..lines.unsigned_abs().min(16) {
                    self.report_mouse(
                        event.position,
                        if lines > 0 { 64 } else { 65 },
                        false,
                        event.modifiers,
                    );
                }
            } else if !event.modifiers.shift
                && mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            {
                let arrow = if lines > 0 { "up" } else { "down" };
                if let Some(bytes) = keystroke_to_bytes(&Keystroke::parse(arrow).unwrap(), mode) {
                    let mut writer = self.stdin_writer.lock();
                    for _ in 0..lines.unsigned_abs().min(16) {
                        let _ = writer.write_all(&bytes);
                    }
                    let _ = writer.flush();
                }
            } else {
                self.state
                    .with_term_mut(|term| term.scroll_display(Scroll::Delta(lines)));
                cx.notify();
            }
        }
    }

    /// Absolute window coordinates of the live cursor, for IME and decorative effects.
    pub fn cursor_bounds(&self) -> Bounds<Pixels> {
        let geometry = self.geometry.get();
        self.state.with_term(|term| {
            let cursor = term.grid().cursor.point;
            Bounds::new(
                point(
                    geometry.bounds.origin.x
                        + self.config.padding.left
                        + geometry.cell.width * cursor.column.0 as f32,
                    geometry.bounds.origin.y
                        + self.config.padding.top
                        + geometry.cell.height
                            * (cursor.line.0 + term.grid().display_offset() as i32) as f32,
                ),
                geometry.cell,
            )
        })
    }

    /// Process pending terminal events.
    ///
    /// This method drains all available events from the event receiver
    /// and handles them appropriately. Note: bytes are processed in the
    /// async reader task, not here.
    fn process_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Process terminal events (from alacritty event proxy)
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                TerminalEvent::PtyWrite(data) => {
                    let mut writer = self.stdin_writer.lock();
                    let _ = writer.write_all(data.as_bytes());
                    let _ = writer.flush();
                }
                TerminalEvent::Wakeup => {
                    // Terminal has new content - already handled by async task
                }
                TerminalEvent::Bell => {
                    if let Some(ref callback) = self.bell_callback {
                        callback(window, cx);
                    }
                }
                TerminalEvent::Title(title) => {
                    if let Some(ref callback) = self.title_callback {
                        callback(window, cx, &title);
                    }
                }
                TerminalEvent::ClipboardStore(text) => {
                    if let Some(ref callback) = self.clipboard_store_callback {
                        callback(window, cx, &text);
                    }
                }
                TerminalEvent::ClipboardLoad => {
                    // Terminal wants to load data from clipboard
                    // TODO: Implement clipboard integration
                }
                TerminalEvent::Exit => {
                    if let Some(ref callback) = self.exit_callback {
                        callback(window, cx);
                    }
                }
            }
        }
    }

    /// Get the current terminal dimensions.
    ///
    /// # Returns
    ///
    /// A tuple of (columns, rows).
    pub fn dimensions(&self) -> (usize, usize) {
        (self.state.cols(), self.state.rows())
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    pub fn search_summary(&self) -> &SearchSummary {
        &self.search_summary
    }

    pub fn set_search(&mut self, options: SearchOptions, cx: &mut Context<Self>) {
        if self.search_options.as_ref() == Some(&options) {
            return;
        }
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_options = Some(options);
        self.search_summary = SearchSummary::default();
        self.renderer.search_matches = Arc::default();
        self.renderer.search_current = None;
        self.search_reveal = true;
        self.queue_search(cx);
        cx.emit(SearchChanged);
        cx.notify();
    }

    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_options = None;
        self.search_summary = SearchSummary::default();
        self.renderer.search_matches = Arc::default();
        self.renderer.search_current = None;
        cx.emit(SearchChanged);
        cx.notify();
    }

    pub fn navigate_search(&mut self, backwards: bool, cx: &mut Context<Self>) {
        let count = self.renderer.search_matches.len();
        if count == 0 {
            return;
        }
        let current = self.renderer.search_current.unwrap_or(count - 1);
        let current = if backwards {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        };
        self.renderer.search_current = Some(current);
        self.search_summary.current = current + 1;
        let point = *self.renderer.search_matches[current].start();
        self.state.with_term_mut(|term| term.scroll_to_point(point));
        cx.emit(SearchChanged);
        cx.notify();
    }

    fn content_changed(&mut self, cx: &mut Context<Self>) {
        self.content_revision = self.content_revision.wrapping_add(1);
        if self.search_options.is_some() {
            // Never paint positions from an older grid after scrolling/reflow.
            self.renderer.search_matches = Arc::default();
            self.queue_search(cx);
        }
    }

    fn queue_search(&mut self, cx: &mut Context<Self>) {
        let Some(options) = self.search_options.as_ref() else {
            return;
        };
        if options.query.is_empty() || self.search_summary.error.is_some() {
            return;
        }
        self.search_summary.searching = true;
        if self.search_task.is_some() {
            return;
        }
        cx.emit(SearchChanged);
        self.search_task = Some(cx.spawn(async move |this, cx| {
            // One worker per view, with a short coalescing delay for typing and
            // output bursts. Updating the query never spawns competing scans.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(140))
                .await;
            let Ok(Some((grid, options, revision, generation))) = this.update(cx, |view, _| {
                let options = view
                    .search_options
                    .clone()
                    .filter(|o| !o.query.is_empty())?;
                Some((
                    view.state.with_term(|term| term.grid().clone()),
                    options,
                    view.content_revision,
                    view.search_generation,
                ))
            }) else {
                let _ = this.update(cx, |view, _| view.search_task = None);
                return;
            };
            let result = cx
                .background_spawn(async move { search_grid(grid, &options) })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.search_task = None;
                if view.search_options.is_none() {
                    return;
                }
                if view.content_revision != revision || view.search_generation != generation {
                    view.queue_search(cx);
                    return;
                }
                let count = result.matches.len();
                let current = if count == 0 {
                    None
                } else {
                    Some(
                        view.renderer
                            .search_current
                            .unwrap_or(count - 1)
                            .min(count - 1),
                    )
                };
                view.renderer.search_matches = Arc::new(result.matches);
                view.renderer.search_current = current;
                view.search_summary = SearchSummary {
                    matches: count,
                    current: current.map_or(0, |i| i + 1),
                    searching: false,
                    limited: result.limited,
                    error: result.error,
                };
                if view.search_reveal {
                    view.search_reveal = false;
                    if let Some(index) = current {
                        let point = *view.renderer.search_matches[index].start();
                        view.state.with_term_mut(|term| term.scroll_to_point(point));
                    }
                }
                cx.emit(SearchChanged);
                cx.notify();
            });
        }));
    }

    /// Resize the terminal to new dimensions.
    ///
    /// This method should be called when the terminal view size changes.
    /// It updates the internal grid and notifies the terminal process of the new size.
    ///
    /// # Arguments
    ///
    /// * `cols` - New number of columns
    /// * `rows` - New number of rows
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.resize(cols, rows);
    }

    /// Get the current terminal configuration.
    ///
    /// # Returns
    ///
    /// A reference to the current configuration.
    pub fn config(&self) -> &TerminalConfig {
        &self.config
    }

    /// Get the focus handle for this terminal view.
    ///
    /// # Returns
    ///
    /// A reference to the focus handle.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Update the terminal configuration.
    ///
    /// This method updates the terminal's configuration, including font settings,
    /// padding, and color palette. Changes take effect on the next render.
    ///
    /// # Arguments
    ///
    /// * `config` - The new configuration to apply
    /// * `cx` - The context for triggering a repaint
    pub fn update_config(&mut self, config: TerminalConfig, cx: &mut Context<Self>) {
        // Update renderer with new font settings and palette
        self.renderer.font_family = config.font_family.clone();
        self.renderer.font_fallbacks =
            Some(FontFallbacks::from_fonts(config.font_fallbacks.clone()));
        self.renderer.font_size = config.font_size;
        self.renderer.line_height_multiplier = config.line_height_multiplier;
        self.renderer.palette = config.colors.clone();

        if self.config.scrollback != config.scrollback {
            self.state.update_preferences(
                config.scrollback,
                alacritty_terminal::vte::ansi::CursorStyle {
                    shape: self.default_cursor_shape,
                    blinking: self.default_cursor_blink,
                },
            );
            self.content_changed(cx);
        }

        // Store the new config
        self.config = config;

        // Trigger a repaint - cell dimensions will be recalculated via measure_cell()
        cx.notify();
    }

    /// Set the default cursor; DECSCUSR from terminal applications can override it.
    pub fn set_cursor_style(&mut self, shape: CursorShape, blinking: bool, cx: &mut Context<Self>) {
        self.default_cursor_shape = shape;
        self.default_cursor_blink = blinking;
        self.cursor_visible = true;
        self.state.update_preferences(
            self.config.scrollback,
            alacritty_terminal::vte::ansi::CursorStyle { shape, blinking },
        );
        cx.notify();
    }

    /// Calculate terminal dimensions from pixel bounds and cell size.
    ///
    /// Helper method to determine how many columns and rows fit in the given bounds.
    #[allow(dead_code)]
    fn calculate_dimensions(&self, bounds: Bounds<Pixels>) -> (usize, usize) {
        let width_f32: f32 = bounds.size.width.into();
        let height_f32: f32 = bounds.size.height.into();
        let cell_width_f32: f32 = self.renderer.cell_width.into();
        let cell_height_f32: f32 = self.renderer.cell_height.into();

        let cols = ((width_f32 / cell_width_f32) as usize).max(1);
        let rows = ((height_f32 / cell_height_f32) as usize).max(1);
        (cols, rows)
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Process any pending events
        self.process_events(window, cx);

        let should_blink = self.input_enabled
            && !self.exited
            && self.focus_handle.is_focused(window)
            && self.state.with_term(|term| {
                term.cursor_style().blinking
                    && term.mode().contains(TermMode::SHOW_CURSOR)
                    && term.grid().display_offset() == 0
            });
        if should_blink && self.cursor_blink_task.is_none() {
            self.cursor_blink_task = Some(cx.spawn_in(window, async move |this, cx| {
                Timer::after(Duration::from_millis(530)).await;
                let _ = this.update_in(cx, |this, _, cx| {
                    this.cursor_blink_task = None;
                    this.cursor_visible = !this.cursor_visible;
                    cx.notify();
                });
            }));
        } else if !should_blink {
            self.cursor_blink_task = None;
            self.cursor_visible = true;
        }
        self.renderer.cursor_visible = self.cursor_visible;

        // Get terminal state and renderer for rendering
        let state_arc = self.state.term_arc();
        let renderer = self.renderer.clone();
        let resize_callback = self.resize_callback.clone();
        let padding = self.config.padding;
        let geometry = self.geometry.clone();
        let entity = cx.entity();
        let focus = self.focus_handle.clone();
        let marked = self.marked.clone();
        let input_enabled = self.input_enabled;

        div()
            .size_full()
            .bg(self.config.colors.background())
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .key_context("SyncSSH")
            .on_action(cx.listener(|this, _: &TerminalTab, _, cx| this.send_input(b"\t", cx)))
            .on_action(
                cx.listener(|this, _: &TerminalBackTab, _, cx| this.send_input(b"\x1b[Z", cx)),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        use alacritty_terminal::grid::Dimensions;

                        // Measure actual cell dimensions from the font
                        let mut measured_renderer = renderer.clone();
                        measured_renderer.measure_cell(window);
                        geometry.set(Geometry {
                            bounds,
                            cell: size(measured_renderer.cell_width, measured_renderer.cell_height),
                        });
                        if input_enabled {
                            window.handle_input(
                                &focus,
                                ElementInputHandler::new(bounds, entity.clone()),
                                cx,
                            );
                        }

                        // Calculate available space after padding
                        let available_width: f32 =
                            (bounds.size.width - padding.left - padding.right).into();
                        let available_height: f32 =
                            (bounds.size.height - padding.top - padding.bottom).into();
                        let cell_width_f32: f32 = measured_renderer.cell_width.into();
                        let cell_height_f32: f32 = measured_renderer.cell_height.into();

                        let cols = ((available_width / cell_width_f32) as usize).max(1);
                        let rows = ((available_height / cell_height_f32) as usize).max(1);

                        // Helper struct implementing Dimensions for resize
                        struct TermSize {
                            cols: usize,
                            rows: usize,
                        }
                        impl Dimensions for TermSize {
                            fn total_lines(&self) -> usize {
                                self.rows
                            }
                            fn screen_lines(&self) -> usize {
                                self.rows
                            }
                            fn columns(&self) -> usize {
                                self.cols
                            }
                            fn last_column(&self) -> alacritty_terminal::index::Column {
                                alacritty_terminal::index::Column(self.cols.saturating_sub(1))
                            }
                            fn bottommost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(self.rows as i32 - 1)
                            }
                            fn topmost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(0)
                            }
                        }

                        // Resize terminal if dimensions changed
                        let mut term = state_arc.lock();
                        let current_cols = term.columns();
                        let current_rows = term.screen_lines();
                        if cols != current_cols || rows != current_rows {
                            // Notify the PTY about the resize
                            if let Some(ref callback) = resize_callback {
                                callback(cols, rows);
                            }
                            term.resize(TermSize { cols, rows });
                            measured_renderer.search_matches = Arc::default();
                            let resized = entity.downgrade();
                            cx.defer(move |cx| {
                                let _ = resized.update(cx, |view, cx| view.content_changed(cx));
                            });
                        }

                        // Paint the terminal with measured dimensions
                        measured_renderer.paint(bounds, padding, &term, window, cx);
                        if !marked.is_empty() {
                            let p = term.grid().cursor.point;
                            let origin = point(
                                bounds.origin.x
                                    + padding.left
                                    + measured_renderer.cell_width * p.column.0 as f32,
                                bounds.origin.y
                                    + padding.top
                                    + measured_renderer.cell_height * p.line.0 as f32,
                            );
                            let text: SharedString = marked.clone().into();
                            let run = TextRun {
                                len: marked.len(),
                                font: measured_renderer.font_for_text(
                                    &text,
                                    false,
                                    false,
                                    window.text_system(),
                                ),
                                color: measured_renderer.palette.foreground(),
                                background_color: Some(measured_renderer.palette.background()),
                                underline: Some(UnderlineStyle {
                                    color: None,
                                    thickness: px(1.),
                                    wavy: false,
                                }),
                                strikethrough: None,
                            };
                            let line = window.text_system().shape_line(
                                text,
                                measured_renderer.font_size,
                                &[run],
                                None,
                            );
                            let _ = line.paint(origin, measured_renderer.cell_height, window, cx);
                        }
                    },
                )
                .size_full(),
            )
    }
}

// Tests are omitted due to macro expansion issues with the test attribute
// in this configuration. Integration tests can be added separately.

impl EventEmitter<InputPulse> for TerminalView {}
impl EventEmitter<InputWriteError> for TerminalView {}
impl EventEmitter<TerminalExited> for TerminalView {}
impl EventEmitter<SearchChanged> for TerminalView {}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let units: Vec<u16> = self.marked.encode_utf16().collect();
        let start = range.start.min(units.len());
        let end = range.end.min(units.len()).max(start);
        *actual = Some(start..end);
        Some(String::from_utf16_lossy(&units[start..end]))
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked.is_empty()).then(|| 0..self.marked.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked.clear();
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked.clear();
        self.send_input(text.as_bytes(), cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_enabled {
            return;
        }
        self.marked = text.to_owned();
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(self.cursor_bounds())
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}
