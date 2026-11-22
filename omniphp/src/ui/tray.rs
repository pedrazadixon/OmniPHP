//! System tray icon. GPUI has no tray API, so this uses the `tray-icon` crate:
//! the icon is created on the main thread and GPUI's own message loop
//! dispatches its window messages. Tray callbacks only push a [`TrayCommand`]
//! into a channel; a foreground task applies them to the app.

use anyhow::Result;
use gpui_kit::{AnyWindowHandle, App, AsyncApp, Global, Task};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::window;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Open,
    StartAll,
    StopAll,
    Quit,
}

pub type CommandSender = async_channel::Sender<TrayCommand>;

/// Keeps the icon and its command task alive for the life of the app.
struct Tray {
    _icon: TrayIcon,
    _task: Task<()>,
}

impl Global for Tray {}

/// Creates the tray icon for `window`. Returns a sender other parts of the app
/// (for example the single-instance listener) can use to issue the same commands.
///
/// The tray shows the window itself. Every other command is passed to
/// `on_command`, including Quit, which `on_command` must carry out.
pub fn install(
    window: AnyWindowHandle,
    on_command: impl Fn(TrayCommand, &mut App) + 'static,
    cx: &mut App,
) -> Result<CommandSender> {
    let items = [
        (MenuItem::new("Open OmniPHP", true, None), TrayCommand::Open),
        (MenuItem::new("Start all services", true, None), TrayCommand::StartAll),
        (MenuItem::new("Stop all services", true, None), TrayCommand::StopAll),
        (MenuItem::new("Quit and stop services", true, None), TrayCommand::Quit),
    ];
    let menu = Menu::new();
    menu.append_items(&[
        &items[0].0,
        &PredefinedMenuItem::separator(),
        &items[1].0,
        &items[2].0,
        &PredefinedMenuItem::separator(),
        &items[3].0,
    ])?;

    let icon = TrayIconBuilder::new()
        .with_tooltip("OmniPHP")
        .with_icon(app_icon()?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;

    let (tx, rx) = async_channel::unbounded::<TrayCommand>();

    let commands: Vec<(MenuId, TrayCommand)> =
        items.iter().map(|(item, command)| (item.id().clone(), *command)).collect();
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some((_, command)) = commands.iter().find(|(id, _)| *id == event.id) {
            let _ = menu_tx.try_send(*command);
        }
    }));

    let click_tx = tx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            let _ = click_tx.try_send(TrayCommand::Open);
        }
    }));

    let task = cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok(command) = rx.recv().await {
            let _ = cx.update(|cx| match command {
                TrayCommand::Open => {
                    let _ = window.update(cx, |_, window, _| window::show(window));
                }
                // The window comes forward to show that services are being
                // stopped; `on_command` ends the app once they have.
                TrayCommand::Quit => {
                    let _ = window.update(cx, |_, window, _| window::show(window));
                    on_command(command, cx);
                }
                TrayCommand::StartAll | TrayCommand::StopAll => on_command(command, cx),
            });
        }
    });

    cx.set_global(Tray {
        _icon: icon,
        _task: task,
    });
    Ok(tx)
}

/// The application icon embedded in the executable (resource 1, see
/// `build.rs`), or a plain square if the exe was built without it.
fn app_icon() -> Result<Icon> {
    match Icon::from_resource(1, None) {
        Ok(icon) => Ok(icon),
        Err(_) => fallback_icon(),
    }
}

/// A 32px rounded square in the app accent colour.
fn fallback_icon() -> Result<Icon> {
    const SIZE: u32 = 32;
    const RADIUS: f32 = 7.0;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let max = SIZE as f32 - RADIUS;
            let dx = (RADIUS - fx).max(fx - max).max(0.0);
            let dy = (RADIUS - fy).max(fy - max).max(0.0);
            let inside = dx * dx + dy * dy <= RADIUS * RADIUS;
            rgba.extend_from_slice(&[0x25, 0x63, 0xeb, if inside { 0xff } else { 0x00 }]);
        }
    }
    Ok(Icon::from_rgba(rgba, SIZE, SIZE)?)
}
