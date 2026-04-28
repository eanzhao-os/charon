// Prevents the extra Windows console window when run from explorer.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    charon_desktop_lib::run();
}
