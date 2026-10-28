//! Shipyard's desktop app: see the library.

// No console window on Windows (a future platform's: D14-2).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    shipyard_desktop::run();
}
