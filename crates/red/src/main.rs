// SPDX-License-Identifier: GPL-3.0-or-later

//! The RED application binary. Opens the GPUI window and mounts the root view.
//! The Tokio backend (`red-service`) and database drivers (`red-driver`) exist
//! and are tested; wiring them into this UI is the next step.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod config;
mod connect;
mod editor;
mod icons;
mod result;
mod schema;
mod shell;
mod sql;
mod theme;

use flint::{CodeEditor, TextInput};
use gpui::{prelude::*, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_platform::application;

use crate::app::AppState;
use crate::assets::Assets;

fn main() {
    init_tracing();

    application().with_assets(Assets).run(|cx: &mut App| {
        cx.set_global(crate::theme::one_dark());
        if let Err(err) = Assets::load_fonts(cx) {
            eprintln!("warning: failed to load vendored fonts: {err}");
        }
        // The connection form's text fields and the SQL editor need their
        // editing key bindings installed once at startup.
        TextInput::bind_keys(cx);
        CodeEditor::bind_keys(cx);

        // Spawn the Tokio backend and hand its event stream to the root view.
        let mut service = red_service::spawn();
        let events = service.take_events().expect("service event stream");

        let bounds = Bounds::centered(None, gpui::size(gpui::px(1100.0), gpui::px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(gpui::size(gpui::px(720.), gpui::px(480.))),
                titlebar: Some(titlebar_options()),
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

/// macOS: seamless titlebar — hide the native bar and inset the traffic lights
/// into our top strip (which doubles as the drag region). The top bar's left
/// inset clears them. Mirrors Nyx.
#[cfg(target_os = "macos")]
fn titlebar_options() -> TitlebarOptions {
    TitlebarOptions {
        title: None,
        appears_transparent: true,
        traffic_light_position: Some(gpui::point(gpui::px(13.), gpui::px(13.))),
    }
}

/// Non-macOS: keep the native caption bar so min/max/close work out of the box.
/// `traffic_light_position` is macOS-only and ignored here.
#[cfg(not(target_os = "macos"))]
fn titlebar_options() -> TitlebarOptions {
    TitlebarOptions {
        title: Some("RED".into()),
        appears_transparent: false,
        traffic_light_position: None,
    }
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
