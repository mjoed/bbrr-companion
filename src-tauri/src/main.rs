// prevents additional console window on Windows in release, do not remove
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    bbrr_companion_lib::run()
}
