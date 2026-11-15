//! Shared chrome palette, kept on the GPUI application thread.
use crate::settings::{ThemeChoice, UiPalette};
use gpui::{prelude::*, *};

#[derive(Clone, Copy)]
pub struct Appearance(pub ThemeChoice);
impl Global for Appearance {}

pub fn palette(cx: &App) -> UiPalette {
    cx.try_global::<Appearance>()
        .map(|theme| theme.0)
        .unwrap_or_default()
        .palette()
}

pub fn apply(theme: ThemeChoice, window: &mut Window, cx: &mut App) {
    cx.set_global(Appearance(theme));
    gpui_component::Theme::change(
        if theme == ThemeChoice::Light {
            gpui_component::ThemeMode::Light
        } else {
            gpui_component::ThemeMode::Dark
        },
        Some(window),
        cx,
    );
}

impl UiPalette {
    pub fn color(self, color: u32) -> Rgba {
        rgb(self.map_color(color))
    }

    pub fn text(self, value: impl Into<SharedString>, color: u32, size: f32) -> Div {
        div()
            .text_size(px(size))
            .text_color(self.color(color))
            .child(value.into())
    }
}
