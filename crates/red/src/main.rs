//! The RED application binary. Initialises logging, spawns the `red-service`
//! backend thread, opens the GPUI window, and mounts the root view onto the
//! service's command channel and event stream.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod config;
mod connect;
#[cfg(feature = "dev-stats")]
mod dev_stats;
mod editor;
mod icons;
mod keymap;
mod menu;
mod palette;
mod result;
mod schema;
mod secrets;
mod settings;
mod settings_ui;
mod settings_watch;
mod shell;
mod sql;
mod theme;

use gpui::{prelude::*, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_platform::application;

use crate::app::AppState;
use crate::assets::Assets;

// Dev builds count every allocation (see `dev_stats`); normal builds keep the
// system allocator with zero overhead.
#[cfg(feature = "dev-stats")]
#[global_allocator]
static GLOBAL: dev_stats::Counting = dev_stats::Counting;

// ⌘Q quits the app. We render a seamless titlebar with no native app menu, so
// the standard macOS quit shortcut has to be bound and handled ourselves.
gpui::actions!(red, [Quit]);

// The dev perf HUD's toggle action (⌥⌘P), bound only under the feature.
#[cfg(feature = "dev-stats")]
gpui::actions!(red, [ToggleDevStats]);

fn main() {
    init_tracing();

    application().with_assets(Assets).run(|cx: &mut App| {
        cx.set_global(crate::theme::one_dark());
        if let Err(err) = Assets::load_fonts(cx) {
            tracing::warn!("failed to load vendored fonts: {err}");
        }
        // Install every key binding (Flint component keymaps + RED's globals and
        // app-chrome bindings) from the central keymap.
        keymap::bind_all(cx);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        // Populate the global menu bar (the macOS bar at the top of the screen).
        // Items reference the same action structs `keymap` binds, so their
        // shortcuts render and stay in sync automatically.
        cx.set_menus(menu::build_menus());
        // Dev-only: ⌥⌘P toggles the perf HUD overlay.
        #[cfg(feature = "dev-stats")]
        cx.bind_keys([gpui::KeyBinding::new("cmd-alt-p", ToggleDevStats, None)]);

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

    // Route panics on any thread (the GPUI main thread included) through tracing,
    // so a crash lands in the same log as everything else rather than bare stderr.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
    }));
}
