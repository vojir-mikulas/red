// SPDX-License-Identifier: GPL-3.0-or-later

//! The RED application binary. Opens the GPUI window and mounts the root view.
//! The Tokio backend (`red-service`) and database drivers (`red-driver`) exist
//! and are tested; wiring them into this UI is the next step.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod config;
mod connect;
mod shell;

use flint::prelude::*;
use flint::TextInput;
use gpui::{prelude::*, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_platform::application;

use crate::app::AppState;
use crate::assets::Assets;

fn main() {
    init_tracing();

    application().with_assets(Assets).run(|cx: &mut App| {
        cx.set_global(Theme::one_dark());
        if let Err(err) = Assets::load_fonts(cx) {
            eprintln!("warning: failed to load vendored fonts: {err}");
        }
        // The connection form's text fields need the editing key bindings.
        TextInput::bind_keys(cx);

        // Spawn the Tokio backend and hand its event stream to the root view.
        let mut service = red_service::spawn();
        let events = service.take_events().expect("service event stream");

        let bounds = Bounds::centered(None, gpui::size(gpui::px(1100.0), gpui::px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(gpui::size(gpui::px(720.), gpui::px(480.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("RED".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| AppState::new(cx, service, events)),
        )
        .expect("failed to open RED window");

        // Closing the last window quits, or GPUI's event loop lingers with no UI.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        cx.activate(true);
    });
}

/// Initialise `tracing` to stderr. Level is `RUST_LOG` or `info`.
fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
}
