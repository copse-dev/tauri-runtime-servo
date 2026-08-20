// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// The Servo-backed Tauri runtime, used in place of the default Wry runtime.
type ServoRuntime = tauri_runtime_servo::Servo<tauri::EventLoopMessage>;

#[tauri::command]
fn greet(name: &str) -> String {
  format!("Hello {name}, you have been greeted from Rust via Servo!")
}

fn main() {
  tauri::Builder::<ServoRuntime>::new()
    .invoke_system(tauri_runtime_servo::INVOKE_SYSTEM_SCRIPT)
    .invoke_handler(tauri::generate_handler![greet])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
