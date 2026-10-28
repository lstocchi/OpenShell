// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal runtime-loaded bindings for the libkrun C API used by the VM driver.
//!
//! The platform backends live in sibling modules and mirror each other: `unix`
//! binds the 1.x context API (`krun_create_ctx`/`krun_set_exec`) while
//! `windows` binds the 2.0 object API that the WHP port of libkrun exposes.
//! Constants and the symbol-loading helper are shared here.

#![allow(unsafe_code)]

use std::path::Path;

use libloading::Library;

#[cfg(not(windows))]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub use unix::{LibKrun, KRUN_LOG_TARGET_DEFAULT, libkrun};
#[cfg(windows)]
pub use windows::{KrunBytes, KrunStr, KRUN_DISK_FORMAT_RAW, krun_init_binary_path, libkrun};

pub const KRUN_LOG_LEVEL_OFF: u32 = 0;
pub const KRUN_LOG_LEVEL_ERROR: u32 = 1;
pub const KRUN_LOG_LEVEL_WARN: u32 = 2;
pub const KRUN_LOG_LEVEL_INFO: u32 = 3;
pub const KRUN_LOG_LEVEL_DEBUG: u32 = 4;
pub const KRUN_LOG_LEVEL_TRACE: u32 = 5;
pub const KRUN_LOG_STYLE_AUTO: u32 = 0;
pub const KRUN_LOG_OPTION_NO_ENV: u32 = 1;

fn load_symbol<T: Copy>(library: &Library, name: &str, path: &Path) -> Result<T, String> {
    let mut symbol = name.as_bytes().to_vec();
    symbol.push(0);
    unsafe {
        library
            .get::<T>(&symbol)
            .map(|symbol| *symbol)
            .map_err(|e| format!("load symbol {name} from {}: {e}", path.display()))
    }
}
