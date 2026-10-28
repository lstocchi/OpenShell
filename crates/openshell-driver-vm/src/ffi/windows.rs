// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runtime-loaded bindings for the libkrun 2.0 object API used on Windows.
//!
//! The Windows (WHP) port of libkrun only exposes the 2.0 builder API, so the
//! launcher cannot reuse the 1.x `krun_create_ctx`/`krun_set_exec` bindings in
//! `unix.rs`. The guest init binary and its config are injected through a
//! virtiofs overlay exactly like `krun_init_config_apply` does, because that
//! helper resolves symbols through `dlsym` and is unavailable on Windows.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libloading::Library;

use super::load_symbol;

pub type KrunObject = *mut c_void;
pub type KrunError = *mut c_void;
pub type KrunResult = u64;

pub const KRUN_RESULT_SUCCESS: KrunResult = 0;
pub const KRUN_DISK_FORMAT_RAW: u32 = 0;

/// libkrun runtime DLL loaded by the launcher.
pub const LIBKRUN_DLL: &str = "libkrun.dll";
/// Kernel bundle `krun_payload_load_krunfw` loads by bare name.
pub const LIBKRUNFW_DLL: &str = "libkrunfw.dll";
/// Static Linux init binary `krun_init` would embed (`init=/init.krun`).
pub const KRUN_INIT_BINARY: &str = "krun-init";

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KrunStr {
    data: *const c_char,
    len: usize,
}

impl KrunStr {
    pub fn new(value: &str) -> Self {
        Self {
            data: value.as_ptr().cast(),
            len: value.len(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KrunBytes {
    data: *const u8,
    len: usize,
}

impl KrunBytes {
    pub fn new(value: &[u8]) -> Self {
        Self {
            data: value.as_ptr(),
            len: value.len(),
        }
    }
}

type FnInitLog = unsafe extern "C" fn(u64, u32, u32, u32, *mut KrunError) -> KrunResult;
type FnNew = unsafe extern "C" fn() -> KrunObject;
type FnNewErr = unsafe extern "C" fn(*mut KrunError) -> KrunObject;
type FnAdd = unsafe extern "C" fn(KrunObject, KrunObject);
type FnFsOverlayAddDir =
    unsafe extern "C" fn(KrunObject, KrunStr, u32, *mut KrunError) -> KrunResult;
type FnFsOverlayAddFile =
    unsafe extern "C" fn(KrunObject, KrunStr, KrunBytes, u32, bool, *mut KrunError) -> KrunResult;
type FnPayloadAppendCmdline = unsafe extern "C" fn(KrunObject, KrunStr);
type FnFsDeviceNew = unsafe extern "C" fn(KrunStr, KrunStr, *mut KrunError) -> KrunObject;
type FnFsDeviceNewNull = unsafe extern "C" fn(KrunStr, *mut KrunError) -> KrunObject;
type FnFsDeviceSetOverlay = unsafe extern "C" fn(KrunObject, KrunObject);
type FnBlockDeviceNew = unsafe extern "C" fn(KrunStr, KrunStr, u32, *mut KrunError) -> KrunObject;
type FnBlockDeviceSetReadOnly = unsafe extern "C" fn(KrunObject, bool);
type FnVsockDeviceNew = unsafe extern "C" fn(u64, u32, *mut KrunError) -> KrunObject;
type FnVsockAddUnixPort = unsafe extern "C" fn(KrunObject, u32, KrunStr, bool);
type FnBuilderU8 = unsafe extern "C" fn(*mut KrunObject, u8, *mut KrunError) -> KrunResult;
type FnBuilderU32 = unsafe extern "C" fn(*mut KrunObject, u32, *mut KrunError) -> KrunResult;
type FnBuilderBool = unsafe extern "C" fn(*mut KrunObject, bool, *mut KrunError) -> KrunResult;
type FnBuilderObject = unsafe extern "C" fn(*mut KrunObject, KrunObject);
type FnConsoleAddDefault = unsafe extern "C" fn(
    KrunObject,
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *mut KrunError,
) -> KrunResult;
type FnConsoleBuild = unsafe extern "C" fn(KrunObject, *mut KrunError) -> KrunObject;
type FnBuilderBuild = unsafe extern "C" fn(*mut KrunObject, *mut KrunError) -> KrunObject;
type FnVmmRun = unsafe extern "C" fn(KrunObject);
type FnErrorResult = unsafe extern "C" fn(KrunError) -> KrunResult;
type FnErrorDestroy = unsafe extern "C" fn(KrunError);
type FnResultName = unsafe extern "C" fn(KrunResult) -> *const c_char;

// Field names mirror the libkrun C API symbol names (`krun_*`).
#[allow(clippy::struct_field_names)]
pub struct LibKrun {
    pub krun_init_log: FnInitLog,
    pub krun_fs_overlay_new: FnNew,
    pub krun_fs_overlay_add_dir: FnFsOverlayAddDir,
    pub krun_fs_overlay_add_file: FnFsOverlayAddFile,
    pub krun_payload_load_krunfw: FnNewErr,
    pub krun_payload_append_cmdline: FnPayloadAppendCmdline,
    pub krun_pci_device_manager_new: FnNew,
    pub krun_pci_device_manager_add: FnAdd,
    pub krun_mmio_device_manager_new: FnNew,
    pub krun_mmio_device_manager_add: FnAdd,
    pub krun_fs_device_new: FnFsDeviceNew,
    pub krun_fs_device_new_read_only: FnFsDeviceNew,
    pub krun_fs_device_new_null: FnFsDeviceNewNull,
    pub krun_fs_device_set_overlay: FnFsDeviceSetOverlay,
    pub krun_block_device_new: FnBlockDeviceNew,
    pub krun_block_device_set_read_only: FnBlockDeviceSetReadOnly,
    pub krun_vsock_device_new: FnVsockDeviceNew,
    pub krun_vsock_device_add_unix_port: FnVsockAddUnixPort,
    pub krun_rng_device_new: FnNewErr,
    pub krun_console_device_builder: FnNew,
    pub krun_console_builder_add_default_console: FnConsoleAddDefault,
    pub krun_console_builder_build: FnConsoleBuild,
    pub krun_vmm_builder_new: FnNew,
    pub krun_vmm_builder_vcpus: FnBuilderU8,
    pub krun_vmm_builder_ram_mib: FnBuilderU32,
    pub krun_vmm_builder_payload: FnBuilderObject,
    pub krun_vmm_builder_devices: FnBuilderObject,
    pub krun_vmm_builder_acpi: FnBuilderBool,
    pub krun_vmm_builder_build: FnBuilderBuild,
    pub krun_vmm_run: FnVmmRun,
    pub krun_error_result: FnErrorResult,
    pub krun_error_destroy: FnErrorDestroy,
    pub krun_result_name_cstr: FnResultName,
}

static LIBKRUN: OnceLock<LibKrun> = OnceLock::new();

pub fn libkrun(runtime_dir: &Path) -> Result<&'static LibKrun, String> {
    if let Some(lib) = LIBKRUN.get() {
        return Ok(lib);
    }
    let loaded = LibKrun::load(runtime_dir)?;
    let _ = LIBKRUN.set(loaded);
    Ok(LIBKRUN.get().expect("libkrun should be initialized"))
}

pub fn krun_init_binary_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(KRUN_INIT_BINARY)
}

impl LibKrun {
    fn load(runtime_dir: &Path) -> Result<Self, String> {
        // `krun_payload_load_krunfw` calls LoadLibrary("libkrunfw.dll"). A
        // module that is already loaded matches by base name, so pinning the
        // runtime copy first keeps the search path from picking another one.
        let krunfw_path = runtime_dir.join(LIBKRUNFW_DLL);
        Box::leak(Box::new(unsafe {
            Library::new(&krunfw_path)
                .map_err(|e| format!("load libkrunfw from {}: {e}", krunfw_path.display()))?
        }));

        let path = runtime_dir.join(LIBKRUN_DLL);
        let library = Box::leak(Box::new(unsafe {
            Library::new(&path).map_err(|e| format!("load libkrun from {}: {e}", path.display()))?
        }));
        macro_rules! load {
            ($name:ident) => {
                load_symbol(library, stringify!($name), &path)?
            };
        }
        Ok(Self {
            krun_init_log: load!(krun_init_log),
            krun_fs_overlay_new: load!(krun_fs_overlay_new),
            krun_fs_overlay_add_dir: load!(krun_fs_overlay_add_dir),
            krun_fs_overlay_add_file: load!(krun_fs_overlay_add_file),
            krun_payload_load_krunfw: load!(krun_payload_load_krunfw),
            krun_payload_append_cmdline: load!(krun_payload_append_cmdline),
            krun_pci_device_manager_new: load!(krun_pci_device_manager_new),
            krun_pci_device_manager_add: load!(krun_pci_device_manager_add),
            krun_mmio_device_manager_new: load!(krun_mmio_device_manager_new),
            krun_mmio_device_manager_add: load!(krun_mmio_device_manager_add),
            krun_fs_device_new: load!(krun_fs_device_new),
            krun_fs_device_new_read_only: load!(krun_fs_device_new_read_only),
            krun_fs_device_new_null: load!(krun_fs_device_new_null),
            krun_fs_device_set_overlay: load!(krun_fs_device_set_overlay),
            krun_block_device_new: load!(krun_block_device_new),
            krun_block_device_set_read_only: load!(krun_block_device_set_read_only),
            krun_vsock_device_new: load!(krun_vsock_device_new),
            krun_vsock_device_add_unix_port: load!(krun_vsock_device_add_unix_port),
            krun_rng_device_new: load!(krun_rng_device_new),
            krun_console_device_builder: load!(krun_console_device_builder),
            krun_console_builder_add_default_console: load!(
                krun_console_builder_add_default_console
            ),
            krun_console_builder_build: load!(krun_console_builder_build),
            krun_vmm_builder_new: load!(krun_vmm_builder_new),
            krun_vmm_builder_vcpus: load!(krun_vmm_builder_vcpus),
            krun_vmm_builder_ram_mib: load!(krun_vmm_builder_ram_mib),
            krun_vmm_builder_payload: load!(krun_vmm_builder_payload),
            krun_vmm_builder_devices: load!(krun_vmm_builder_devices),
            krun_vmm_builder_acpi: load!(krun_vmm_builder_acpi),
            krun_vmm_builder_build: load!(krun_vmm_builder_build),
            krun_vmm_run: load!(krun_vmm_run),
            krun_error_result: load!(krun_error_result),
            krun_error_destroy: load!(krun_error_destroy),
            krun_result_name_cstr: load!(krun_result_name_cstr),
        })
    }

    /// Consume an error out-parameter, turning it into a message.
    pub fn check_error(&self, error: KrunError, context: &str) -> Result<(), String> {
        if error.is_null() {
            return Ok(());
        }
        let result = unsafe { (self.krun_error_result)(error) };
        let name = self.result_name(result);
        unsafe { (self.krun_error_destroy)(error) };
        Err(format!("{context}: {name}"))
    }

    pub fn check_result(
        &self,
        result: KrunResult,
        error: KrunError,
        context: &str,
    ) -> Result<(), String> {
        self.check_error(error, context)?;
        if result == KRUN_RESULT_SUCCESS {
            Ok(())
        } else {
            Err(format!("{context}: {}", self.result_name(result)))
        }
    }

    pub fn check_object(
        &self,
        object: KrunObject,
        error: KrunError,
        context: &str,
    ) -> Result<KrunObject, String> {
        self.check_error(error, context)?;
        if object.is_null() {
            Err(format!("{context}: libkrun returned no object"))
        } else {
            Ok(object)
        }
    }

    fn result_name(&self, result: KrunResult) -> String {
        let name = unsafe { (self.krun_result_name_cstr)(result) };
        if name.is_null() {
            format!("libkrun result {result:#x}")
        } else {
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        }
    }
}
