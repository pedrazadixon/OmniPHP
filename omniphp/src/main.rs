// Release builds are a GUI app with no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod core;
mod single_instance;
mod ui;

use std::sync::{Arc, Mutex};

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::core::engine::Engine;
use crate::core::paths::Paths;
use crate::core::startup::STARTUP_FLAG;
use crate::ui::app::{AppView, SharedEngine};
use crate::ui::tray::TrayCommand;

fn main() {
    // The Windows startup entry passes a flag; anything else is a headless command.
    let args: Vec<String> = std::env::args().skip(1).filter(|arg| arg != STARTUP_FLAG).collect();
    if !args.is_empty() {
        std::process::exit(cli::run(&args));
    }

    let fatal = |error: anyhow::Error| ui::window::fatal(&format!("OmniPHP cannot start.\n\n{error:#}"));
    let paths = match Paths::discover() {
        Ok(paths) => paths,
        Err(error) => return fatal(error),
    };
    let instance = single_instance::key_for(paths.root());
    if !single_instance::acquire(&instance) {
        return;
    }

    let engine: SharedEngine = match Engine::load(paths) {
        Ok(engine) => Arc::new(Mutex::new(engine)),
        Err(error) => return fatal(error),
    };
    let (start_minimized, theme) = {
        let engine = ui::app::lock(&engine);
        (engine.start_minimized(), engine.theme())
    };

    application().with_assets(assets::Assets).run(move |cx| {
        init(cx);
        ui::app::apply_theme(theme, None, cx);
        // The app lives in the tray: closing the window must not end it.
        cx.set_quit_mode(QuitMode::Explicit);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1100.), px(720.)),
                cx,
            ))),
            window_min_size: Some(size(px(820.), px(480.))),
            show: !start_minimized,
            ..TitleBar::window_options()
        };
        let (window, view) = open_window(options, cx, |window, cx| {
            window.set_window_title("OmniPHP");
            window.on_window_should_close(cx, |window, _| {
                ui::window::hide(window);
                false
            });
            cx.new(|cx| AppView::new(engine.clone(), window, cx))
        })
        .expect("failed to open the main window");

        let on_tray_command = move |command: TrayCommand, cx: &mut App| match command {
            TrayCommand::StartAll => view.update(cx, |view, cx| view.start_all(cx)),
            TrayCommand::StopAll => view.update(cx, |view, cx| view.stop_all(cx)),
            // Shows a waiting screen, stops the services and then exits.
            TrayCommand::Quit => view.update(cx, |view, cx| view.quit(cx)),
            TrayCommand::Open => {}
        };
        match ui::tray::install(window, on_tray_command, cx) {
            Ok(commands) => single_instance::listen(&instance, move || {
                let _ = commands.try_send(TrayCommand::Open);
            }),
            Err(error) => eprintln!("tray icon unavailable: {error:#}"),
        }
    });
}
