//! The RED application binary. Initialises logging, spawns the `red-service`
//! backend thread, opens the GPUI window, and mounts the root view onto the
//! service's command channel and event stream.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod assistant;
mod changelog;
mod cli;
mod columns_panel;
mod connect;
mod conversations;
mod decode;
#[cfg(feature = "dev-stats")]
mod dev_stats;
mod editor;
mod env;
mod er;
mod filter;
mod find;
mod history;
mod icons;
mod import;
mod inspector;
mod keymap;
mod keymap_config;
mod kvbrowse;
mod kvconsole;
mod kvkeyspace;
mod kvmonitor;
mod kvpubsub;
mod local_state;
mod markdown;
mod menu;
mod palette;
mod plan;
mod queries;
mod recent_keys;
mod redis_analysis;
mod result;
mod sample;
mod schema;
mod settings;
mod settings_ui;
mod settings_watch;
mod shell;
mod sql;
mod theme;
mod window_chrome;

// Connection-list persistence and OS-keychain access were extracted into the
// UI-free `red-config` crate so a headless CLI can read the same
// `connections.toml` and keychain. Re-exported under their original paths so
// every existing `crate::config::*` / `crate::secrets::*` call site is unchanged.
mod config {
    pub use red_config::config::*;
}
mod secrets {
    pub use red_config::secrets::*;
}

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
    // Headless CLI: when argv names a verb (`red query|exec|test|connections`)
    // this runs the command and exits before any GPUI init. A bare `red` returns
    // `None` and falls through to the desktop app below.
    if let Some(code) = cli::run() {
        std::process::exit(i32::from(code));
    }

    init_tracing();

    // macOS GUI launches inherit a minimal PATH that omits Homebrew / Node, which
    // would break spawning the ACP agent (`npx …`). Patch it before any thread or
    // subprocess exists. No-op off macOS.
    env::augment_path_for_gui_launch();

    // Windows self-update can't delete the running exe, so it leaves the previous
    // binary as `<exe>.old` and relies on the next launch to reap it. Best-effort:
    // a still-locked or absent file just means there's nothing to clean up.
    #[cfg(target_os = "windows")]
    if let Ok(exe) = std::env::current_exe() {
        if let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) {
            let _ = std::fs::remove_file(dir.join(format!("{name}.old")));
        }
    }

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
        // Dev-only ⌥⌘P (perf HUD) is bound inside `keymap::apply` so a keymap
        // reload's clear doesn't drop it; the action stays declared below.

        // Spawn the Tokio backend and hand its event stream to the root view.
        let mut service = red_service::spawn();
        let events = service.take_events().expect("service event stream");

        let bounds = Bounds::centered(None, gpui::size(gpui::px(1100.0), gpui::px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(gpui::size(gpui::px(720.), gpui::px(480.))),
                titlebar: Some(titlebar_options()),
                // The Wayland app_id (and X11 WM_CLASS) GNOME matches against a
                // `.desktop` file to pick the alt-tab / taskbar icon. Must equal
                // our desktop file's basename (`red.desktop`, `Icon=red`) and its
                // `StartupWMClass`, or the running window shows no icon.
                app_id: Some("red".into()),
                // On Linux (GNOME/Wayland in particular) the compositor draws no
                // titlebar, so we ask for client-side decorations and paint our
                // own controls + resize borders (see `window_chrome`). macOS and
                // Windows keep their native frame (the default, `Server`).
                #[cfg(target_os = "linux")]
                window_decorations: Some(gpui::WindowDecorations::Client),
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

/// macOS: seamless titlebar: hide the native bar and inset the traffic lights
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
        title: Some("Red".into()),
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
