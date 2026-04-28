//! Tauri shell entry point.
//!
//! M2.5.0 is just the bare app — Tauri v2 builder, single window, no plugins,
//! no commands. NyxID OAuth + WS bridge to charon-daemon land in M2.5.1.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running charon desktop");
}
