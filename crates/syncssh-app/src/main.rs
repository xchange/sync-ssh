#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod appearance;
mod ascii_password;
mod backup_settings;
mod fonts;
mod local_vault;
mod settings;
mod sftp_pane;
mod terminal_pane;
mod tunnel_pane;
mod ui;

use gpui::*;
use gpui_component::{Root, Theme, ThemeMode};

fn main() {
    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(|cx| {
            if let Err(error) = fonts::register(cx.text_system()) {
                eprintln!("Unable to load bundled terminal fonts: {error}");
                cx.quit();
                return;
            }
            gpui_component::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.bind_keys([
                KeyBinding::new("cmd-q", ui::Quit, None),
                KeyBinding::new("cmd-b", ui::ToggleSidebar, None),
                KeyBinding::new("ctrl-shift-b", ui::ToggleSidebar, None),
                KeyBinding::new("cmd-,", ui::OpenSettings, None),
                KeyBinding::new("ctrl-,", ui::OpenSettings, None),
                KeyBinding::new("tab", ui::ModalNext, Some("SyncSSHModal")),
                KeyBinding::new("shift-tab", ui::ModalPrevious, Some("SyncSSHModal")),
                KeyBinding::new("escape", ui::ModalClose, Some("SyncSSHModal")),
                KeyBinding::new("cmd-t", ui::NewTab, None),
                KeyBinding::new("ctrl-shift-t", ui::NewTab, None),
                KeyBinding::new("cmd-d", ui::SplitVertical, Some("SyncSSHPane")),
                KeyBinding::new("cmd-shift-d", ui::SplitHorizontal, Some("SyncSSHPane")),
                KeyBinding::new("ctrl-shift-d", ui::SplitVertical, Some("SyncSSHPane")),
                KeyBinding::new("ctrl-alt-shift-d", ui::SplitHorizontal, Some("SyncSSHPane")),
                KeyBinding::new("cmd-shift-w", ui::ClosePane, Some("SyncSSHPane")),
                KeyBinding::new("ctrl-shift-w", ui::ClosePane, Some("SyncSSHPane")),
                KeyBinding::new("cmd-shift-l", ui::LockVault, None),
                KeyBinding::new("ctrl-shift-l", ui::LockVault, None),
                KeyBinding::new(
                    "cmd-f",
                    terminal_pane::ToggleTerminalSearch,
                    Some("SyncSSHPane"),
                ),
                KeyBinding::new(
                    "ctrl-shift-f",
                    terminal_pane::ToggleTerminalSearch,
                    Some("SyncSSHPane"),
                ),
                KeyBinding::new(
                    "enter",
                    terminal_pane::SearchNext,
                    Some("SyncSSHSearch > Input"),
                ),
                KeyBinding::new(
                    "shift-enter",
                    terminal_pane::SearchPrevious,
                    Some("SyncSSHSearch > Input"),
                ),
                KeyBinding::new(
                    "escape",
                    terminal_pane::SearchClose,
                    Some("SyncSSHSearch > Input"),
                ),
            ]);
            cx.on_action(|_: &ui::Quit, cx| cx.quit());
            let result = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1320.), px(850.)),
                        cx,
                    ))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("SyncSSH".into()),
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(760.), px(520.))),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| ui::SyncSSH::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            );
            if let Err(error) = result {
                eprintln!("Unable to open SyncSSH: {error}");
                cx.quit();
            }
            cx.activate(true);
        });
}
