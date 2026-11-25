//! Tray menu construction (right click).
//!
//! This is a deliberately minimal fallback. Everything the user normally
//! does — picking stations, playback controls, remote control, history —
//! lives in the tray window (see `ui.rs`), opened with a left click on the
//! tray icon or with *Open main window* here. The menu only needs to work
//! when the window backend is unavailable, so it carries just three items:
//!
//! ```text
//! Open main window
//! ---
//! Configuration…
//! ---
//! Quit
//! ```

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

pub const OPEN_MAIN: &str = "open-main";
pub const CONFIG: &str = "configuration";
pub const QUIT: &str = "quit";

pub fn build_menu() -> Menu {
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::with_id(OPEN_MAIN, "Open main window", true, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(CONFIG, "Configuration…", true, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(QUIT, "Quit", true, None));
    menu
}
