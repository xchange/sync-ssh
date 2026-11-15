//! ASCII-only encryption passwords; other text fields retain normal IME support.
use gpui::{prelude::*, *};
use gpui_component::{
    input::{Input, InputState},
    Sizable,
};
use std::ops::Range;

pub fn valid(text: &str) -> bool {
    text.len() <= 1024 && text.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

pub fn state(window: &mut Window, cx: &mut Context<InputState>) -> InputState {
    let state = InputState::new(window, cx)
        .masked(true)
        .validate(|text, _| valid(text));
    #[cfg(target_os = "windows")]
    windows_ime::watch(&state, window, cx);
    state
}

pub fn input(state: &Entity<InputState>, disabled: bool, small: bool) -> AnyElement {
    let native_state = state.clone();
    let keyboard_state = state.clone();
    div()
        .relative()
        .w_full()
        .capture_key_down(move |event, window, cx| {
            // AppKit dispatches printable keys before invoking the input method.
            // Consume ASCII here so an active CJK input source never starts preedit.
            #[cfg(target_os = "macos")]
            if !disabled
                && keyboard_state.focus_handle(cx).is_focused(window)
                && !event.keystroke.modifiers.platform
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.alt
            {
                if let Some(text) = event
                    .keystroke
                    .key_char
                    .as_ref()
                    .filter(|text| !text.is_empty() && valid(text))
                {
                    keyboard_state.update(cx, |state, cx| {
                        EntityInputHandler::replace_text_in_range(state, None, text, window, cx)
                    });
                    cx.stop_propagation();
                    window.prevent_default();
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (&keyboard_state, event, window, cx);
        })
        .child(
            Input::new(state)
                .disabled(disabled)
                .when(small, |input| input.small()),
        )
        .child(
            div().absolute().inset_0().child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let focus = native_state.focus_handle(cx);
                        window.handle_input(
                            &focus,
                            AsciiHandler {
                                inner: ElementInputHandler::new(bounds, native_state.clone()),
                                disabled,
                            },
                            cx,
                        );
                    },
                )
                .size_full(),
            ),
        )
        .into_any_element()
}

struct AsciiHandler {
    inner: ElementInputHandler<InputState>,
    disabled: bool,
}
impl InputHandler for AsciiHandler {
    fn selected_text_range(
        &mut self,
        ignore_disabled: bool,
        w: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        if self.disabled || !ignore_disabled {
            None
        } else {
            self.inner.selected_text_range(true, w, cx)
        }
    }
    fn marked_text_range(&mut self, _: &mut Window, _: &mut App) -> Option<Range<usize>> {
        None
    }
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        w: &mut Window,
        cx: &mut App,
    ) {
        if !self.disabled && valid(text) {
            self.inner.replace_text_in_range(range, text, w, cx);
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) {
    }
    fn unmark_text(&mut self, w: &mut Window, cx: &mut App) {
        self.inner.unmark_text(w, cx);
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<Bounds<Pixels>> {
        None
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }
    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

#[cfg(target_os = "windows")]
mod windows_ime {
    use super::*;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{cell::RefCell, collections::HashMap};
    use windows_sys::Win32::UI::{Input::Ime::ImmAssociateContext, WindowsAndMessaging::IsWindow};
    thread_local! { static DISABLED: RefCell<HashMap<isize, (EntityId, isize)>> = RefCell::default(); }
    pub(super) fn watch(state: &InputState, window: &mut Window, cx: &mut Context<InputState>) {
        let owner = cx.entity().entity_id();
        let focus = state.focus_handle(cx);
        cx.on_focus(&focus, window, move |_, window, _| {
            let Ok(handle) = window.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return;
            };
            let hwnd = handle.hwnd.get();
            DISABLED.with(|disabled| {
                let mut disabled = disabled.borrow_mut();
                if let Some(entry) = disabled.get_mut(&hwnd) {
                    entry.0 = owner;
                } else {
                    // Detach this window's IME context while a password owns focus.
                    let previous = unsafe { ImmAssociateContext(hwnd as _, std::ptr::null_mut()) };
                    disabled.insert(hwnd, (owner, previous as isize));
                }
            });
        })
        .detach();
        cx.on_blur(&focus, window, move |_, _, _| restore(owner))
            .detach();
        cx.on_release(move |_, _| restore(owner)).detach();
    }
    fn restore(owner: EntityId) {
        DISABLED.with(|disabled| {
            let mut disabled = disabled.borrow_mut();
            let handles: Vec<_> = disabled
                .iter()
                .filter(|(_, entry)| entry.0 == owner)
                .map(|(&hwnd, _)| hwnd)
                .collect();
            for hwnd in handles {
                let (_, previous) = disabled.remove(&hwnd).unwrap();
                unsafe {
                    if IsWindow(hwnd as _) != 0 {
                        ImmAssociateContext(hwnd as _, previous as _);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn passwords_accept_visible_ascii_and_reject_unicode_and_controls() {
        assert!(super::valid(
            "ASCII password 123 !@#$%^&*()_+-=~[]{}\\|;:'\",.<>/?"
        ));
        assert!(super::valid(""));
        for value in [
            "中文密码",
            "ＡＢＣ",
            "café",
            "abc\n",
            "abc\t",
            "abc\0",
            "abc\u{7f}",
        ] {
            assert!(!super::valid(value));
        }
        assert!(!super::valid(&"a".repeat(1025)));
    }
}
