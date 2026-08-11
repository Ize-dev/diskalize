//! Diskalize core.
//!
//! Both binaries link this library: `diskalize.exe` is the GUI client and
//! `diskalize-service.exe` the privileged indexer. Keeping everything in one
//! crate means the index layout, the snapshot format and the change-apply logic
//! exist exactly once, so the two halves cannot drift apart.

pub mod app;
pub mod client;
pub mod content;
pub mod fmt;
pub mod i18n;
pub mod index;
pub mod ipc;
pub mod media;
pub mod pdf;
pub mod scan;
pub mod search;
pub mod service;
pub mod settings;
pub mod shell;
pub mod single;
pub mod snapshot;
pub mod store;
pub mod tray;
pub mod ui;
pub mod win;
pub mod winshell;

/// Entry point of the GUI binary.
pub fn gui_main() -> eframe::Result<()> {
    // Release builds abort on panic and have no console, so a crash would
    // otherwise leave nothing behind but an exit code.
    std::panic::set_hook(Box::new(|info| {
        let text = format!(
            "{info}\n\nthread: {:?}\n",
            std::thread::current().name().unwrap_or("unnamed")
        );
        let _ = std::fs::write(std::env::temp_dir().join("diskalize_panic.txt"), text);
    }));

    // A path argument comes from the Explorer context menu ("%1" / "%V").
    // It is passed through as-is: UNC roots and network namespace items do not
    // always answer `Path::exists`, and silently dropping the argument used to
    // make Diskalize scan C: instead of what was right-clicked.
    let initial = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(|p| p.trim_matches('"').to_string())
        .filter(|p| !p.is_empty());

    // Autostart passes `--tray`: come up in the notification area, without
    // throwing a window in the user's face at login.
    let start_hidden = std::env::args().any(|a| a == "--tray");

    // `--windows N` opens N extra windows immediately. They are viewports in
    // this process, so this is also how their cost can be measured.
    let extra_windows: u32 = std::env::args()
        .skip_while(|a| a != "--windows")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(8);

    // `--new-instance` and the "allow multiple" setting both skip the guard.
    let forced = std::env::args().any(|a| a == "--new-instance");
    let multi = forced || settings::Settings::load().multi_instance;
    // Whoever holds the mutex owns the tray icon, the global hotkey and the
    // named pipe. Extra windows are guests: a second notification icon for the
    // same program is noise, and two processes fighting over one hotkey is
    // worse than noise.
    let mut primary = true;
    let ipc_rx = if multi {
        primary = single::is_first();
        std::sync::mpsc::channel().1
    } else {
        match single::acquire(initial.as_deref()) {
            single::Instance::Secondary => return Ok(()),
            single::Instance::Primary(rx) => rx,
        }
    };

    // Nothing is drawn until the user asks. The icon it waits behind is the
    // same one the running app uses, so it simply hands over.
    let mut initial = initial;
    if start_hidden {
        match wait_in_tray(&ipc_rx) {
            Wake::Quit => return Ok(()),
            Wake::Show(path) => initial = path.or(initial),
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([880.0, 560.0])
            .with_title("Diskalize")
            .with_app_id("diskalize"),
        ..Default::default()
    };

    eframe::run_native(
        "Diskalize",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, initial, ipc_rx, primary, extra_windows)))),
    )
}

/// What ended the wait.
enum Wake {
    /// Build the window. Carries a path if a second launch supplied one.
    Show(Option<String>),
    Quit,
}

/// Sits in the notification area until someone asks for a window.
///
/// Autostart used to build the window and then try to hide it, which never
/// worked cleanly: eframe shows the window itself after the first painted
/// frame, and winit rewrites the window styles while doing so, so every attempt
/// to suppress or disguise it left a frame or two on screen. Not creating the
/// window is the only approach with nothing to race against — and it costs
/// about 12 MB to sit here instead of 190.
///
/// Three things can end the wait: the tray icon, the global hotkey, and a
/// second launch handing over a path. The last one matters because the
/// handover pipe is already listening at this point, and its message would
/// otherwise sit unread in a channel nobody is watching.
fn wait_in_tray(ipc: &std::sync::mpsc::Receiver<String>) -> Wake {
    use std::sync::mpsc::RecvTimeoutError;

    let cfg = settings::Settings::load();
    // The real window does not exist yet, so there is nothing to repaint; this
    // context only satisfies the tray's signature.
    let tray = tray::Tray::new(
        "Diskalize",
        egui::Context::default(),
        cfg.hotkey_enabled
            .then_some((cfg.hotkey_mods, cfg.hotkey_vk)),
    );
    let Some(tray) = tray else {
        // No notification icon means no way back, so show the window rather
        // than become a process nobody can reach.
        return Wake::Show(None);
    };
    loop {
        // Whichever arrives first; polling both beats pulling in a select.
        if let Ok(path) = ipc.try_recv() {
            let path = path.trim().to_string();
            return Wake::Show((!path.is_empty()).then_some(path));
        }
        match tray.events.recv_timeout(std::time::Duration::from_millis(120)) {
            Ok(tray::TrayEvent::Show) | Ok(tray::TrayEvent::Search) => return Wake::Show(None),
            Ok(tray::TrayEvent::Quit) => return Wake::Quit,
            Err(RecvTimeoutError::Timeout) => {}
            // The tray thread is gone; carry on with a window rather than
            // leaving a process with no interface at all.
            Err(RecvTimeoutError::Disconnected) => return Wake::Show(None),
        }
    }
}
