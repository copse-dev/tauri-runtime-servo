// Copyright 2020-2026 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

/// Convenient type alias of Result type for the Servo runtime.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the Servo runtime.
#[non_exhaustive]
#[derive(thiserror::Error, Debug)]
pub enum Error {
  #[error("Servo error: {0}")]
  Servo(String),
  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),
  #[error(transparent)]
  HttpError(#[from] http::Error),
  #[error("Failed to get the window handle: {0}")]
  WindowHandle(#[from] raw_window_handle::HandleError),
}
