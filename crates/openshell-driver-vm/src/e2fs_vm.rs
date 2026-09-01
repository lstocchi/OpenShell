// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Windows-only e2fsprogs execution via a libkrun helper microVM.
//!
//! On Windows, e2fsprogs tools (`mke2fs`, `debugfs`) are unavailable natively.
//! This module boots a lightweight libkrun VM with:
//! - The helper rootfs as `/dev/vda` (read-only)
//! - The target image as `/dev/vdb` (read-write, virtio-blk)
//! - Source/inject directories shared via virtio-fs
//!
//! The VM runs the e2fsprogs command and exits. The output image is written
//! directly via virtio-blk for maximum throughput.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::ffi::{CString, c_char};
use tracing::{debug, info, warn};


use crate::runtime::configured_runtime_dir;

const DEBIAN_IMAGE_URL: &str =
    "https://cdimage.debian.org/images/cloud/trixie/latest/debian-13-nocloud-amd64.qcow2";
const VM_DISK_NAME: &str = "debian-13-nocloud.qcow2";
static BUILD_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

/// Arguments for the `--internal-e2fs-helper` subprocess.
struct E2fsHelperArgs {
    runtime_dir: PathBuf,
    helper_rootfs: PathBuf,
    data_disk: PathBuf,
    virtiofs_shares: Vec<(String, PathBuf)>,
    exec_command: String,
    console_output: PathBuf,
}

/// Spawn the `--internal-e2fs-helper` subprocess and wait for it to complete.
fn run_e2fs_helper(runtime_dir: &Path, disk_path: &Path, source_dir: &Path, args: Vec<CString>) -> Result<(), String> {
    let self_exe = std::env::current_exe()
        .map_err(|e| format!("resolve current executable path: {e}"))?;

    info!("running e2fs helper with self_exe: {:?}", self_exe);

    let mut cmd = Command::new(&self_exe);
    cmd.arg("--internal-e2fs-helper");
    cmd.arg("--e2fs-runtime-dir").arg(runtime_dir);
    cmd.arg("--e2fs-helper-rootfs").arg(source_dir);
    cmd.arg("--e2fs-data-disk").arg(disk_path);

    for arg in args.iter() {
        cmd.arg("--e2fs-args").arg(arg.to_string_lossy().as_ref());
    }

    info!("running e2fs helper with cmd: {:?}", cmd);

    let output = cmd.output().map_err(|e| {
        format!(
            "spawn e2fs helper subprocess {}: {e}",
            self_exe.display()
        )
    })?;

    info!("e2fs helper output: {:?}", output);

    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "e2fs helper VM exited with status {}",
        output.status,
    ))
}

pub fn exec_vm(runtime_dir: &Path, disk_path: &Path, source_dir: &Path, args: Vec<CString>) -> Result<(), String> {
    let krun = crate::ffi::libkrun(runtime_dir)?;
    let ctx = unsafe { (krun.krun_create_ctx)() };
    if ctx < 0 {
        return Err(format!("krun_create_ctx failed: {ctx}"));
    }
    let ctx_id = ctx as u32;

    let ret = unsafe { (krun.krun_set_vm_config)(ctx_id, 1, 1024) };
    if ret < 0 {
        return Err(format!("krun_set_vm_config failed: {ret}"));
    }

    let data_path = CString::new(disk_path.to_string_lossy().as_bytes().to_vec())
        .map_err(|e| format!("invalid data disk path: {e}"))?;

    let label_cstr = CString::new("root").unwrap();
    let label_ptr = label_cstr.as_ptr();

    let ret = unsafe { (krun.krun_add_disk2)(ctx_id, label_ptr, data_path.as_ptr(), 1, false) };

    if ret < 0 {
        return Err(format!("krun_add_disk2 failed: {ret}"));
    }

    let device_cstr = CString::new("/dev/vda1").unwrap();
    let device_ptr = device_cstr.as_ptr();

    let fstype_cstr = CString::new("ext4").unwrap();
    let fstype_ptr = fstype_cstr.as_ptr();

    let ret = unsafe { (krun.krun_set_root_disk_remount)(ctx_id, device_ptr, fstype_ptr, std::ptr::null()) };
    if ret < 0 {
        return Err(format!("krun_set_root_disk_remount failed: {ret}"));
    }

    let tag_cstr = CString::new("krunai").unwrap();
    let tag_ptr = tag_cstr.as_ptr();

    let path_cstr =
        CString::new(source_dir.to_str().unwrap()).unwrap();
    let path_ptr = path_cstr.as_ptr();

    let ret = unsafe { (krun.krun_add_virtiofs)(ctx_id, tag_ptr, path_ptr) };
    if ret < 0 {
        return Err(format!("krun_add_virtiofs failed: {ret}"));
    }

    /* 
    let mut ports = Vec::new();
    for (host_port, guest_port) in vmcfg.mapped_ports.iter() {
        let map = format!("{}:{}", host_port, guest_port);
        ports.push(std::ffi::CString::new(map).unwrap());
    }
    let mut ps: Vec<*const c_char> = Vec::new();
    for port in ports.iter() {
        ps.push(port.as_ptr());
    }
    ps.push(std::ptr::null()); 
    */

    //let hostname = CString::new(format!("HOSTNAME={}", vmcfg.name)).unwrap();
    let cmd = "/bin/sh";
    let home = CString::new("HOME=/root").unwrap();
    let entry = CString::new(format!("ENTRY={cmd}")).unwrap();

    let mut env: Vec<*const c_char> = Vec::new();
    //env.push(hostname.as_ptr());
    env.push(home.as_ptr());
    env.push(entry.as_ptr());
    env.push(std::ptr::null());

    let mut argv: Vec<*const c_char> = Vec::new();
    for a in args.iter() {
        argv.push(a.as_ptr());
    }
    argv.push(std::ptr::null());

    let c_cmd = CString::new(cmd).unwrap();
    let ret = unsafe { (krun.krun_set_exec)(ctx_id, c_cmd.as_ptr(), argv.as_ptr(), env.as_ptr()) };
    if ret < 0 {
        return Err(format!("krun_set_exec failed: {ret}"));
    }

    let result = unsafe { (krun.krun_start_enter)(ctx_id) };
    if result < 0 {
        return Err(format!("krun_start_enter failed: {result}"));
    }
    info!("VM command executed with result: {result}");
    Ok(())
}

/// Format an ext4 image from a source directory using a helper VM.
///
/// The source directory is shared via virtio-fs (preserving symlinks).
/// The output image file must already be pre-allocated to the correct size
/// with the file handle closed before calling this function.
pub fn format_ext4_via_vm(source: &Path, image_path: &Path) -> Result<(), String> {
    info!("Formatting ext4 image via VM: {}; {}", source.display(), image_path.display());
    let runtime_dir = configured_runtime_dir()?;

    let source_dir = source.parent().unwrap_or_else(|| Path::new("."));

    let vm_disk = runtime_dir.join(VM_DISK_NAME);
    if !vm_disk.is_file() {
        let status = Command::new("curl")
            .arg("-L")
            .arg("-o")
            .arg(&vm_disk)
            .arg("--progress-bar")
            .arg(DEBIAN_IMAGE_URL)
            .status();
        match status {
            Ok(status) if status.success() => {}
            _ => {
                return Err(format!("failed to download Debian image"));
            }
        }
    }

    let console_output = temp_console_path(image_path);

    let image_name = image_path.file_name().unwrap().to_str().unwrap();

    info!("Formatting ext4 image via VM: {}; {}", source.display(), image_path.display());
    let cmd_args = vec![
            CString::new("-c").unwrap(),
            CString::new(
                format!("mkdir -p /krunai && mount -t virtiofs krunai /krunai && /sbin/mkfs.ext4 -q -F -t ext4 -E root_owner=0:0 -d /krunai/source /krunai/{image_name} && /sbin/reboot -f"),
            )
            .unwrap(),
        ];

/*     exec_vm(&runtime_dir, &vm_disk, &source_dir, cmd_args);
    info!("executing VM command");
    let file_created = source_dir.join(image_name);
    info!("checking if file {} exists", file_created.display());
    
    if !file_created.is_file() {
        return Err(format!("file {} not created", file_created.display()));
    }
    info!("copying image from {} to {}", file_created.display(), image_path.display());
    fs::copy(file_created, image_path).map_err(|e| format!("copy image {}: {e}", image_path.display()))?;
    info!("finished formatting ext4 image via VM: {}; {}", source.display(), image_path.display());
    return Ok(()); */

/*     let args = E2fsHelperArgs {
        runtime_dir,
        helper_rootfs,
        data_disk: image_path.to_path_buf(),
        virtiofs_shares: vec![("source".to_string(), source.to_path_buf())],
        exec_command,
        console_output: console_output.clone(),
    };*/

    let result = run_e2fs_helper(&runtime_dir, &vm_disk, &source_dir, cmd_args);
    info!("executing VM command");
    let file_created = source_dir.join(image_name);
    info!("checking if file {} exists", file_created.display());
    
    if !file_created.is_file() {
        return Err(format!("file {} not created", file_created.display()));
    }
    if file_created != image_path {
        info!("copying image from {} to {}", file_created.display(), image_path.display());
        fs::copy(&file_created, image_path)
            .map_err(|e| format!("copy image {}: {e}", image_path.display()))?;
    } else {
        info!("image {} created directly in target location, skipping copy", file_created.display());
    }
    result
}

/// Run a single debugfs command against an ext4 image via helper VM.
pub fn run_debugfs_via_vm(image_path: &Path, command: &str) -> Result<(), String> {
    let runtime_dir = configured_runtime_dir()?;

    let source_dir = image_path.parent().unwrap_or_else(|| Path::new("."));

    let vm_disk = runtime_dir.join(VM_DISK_NAME);
    if !vm_disk.is_file() {
        let status = Command::new("curl")
            .arg("-L")
            .arg("-o")
            .arg(&vm_disk)
            .arg("--progress-bar")
            .arg(DEBIAN_IMAGE_URL)
            .status();
        match status {
            Ok(status) if status.success() => {}
            _ => {
                return Err(format!("failed to download Debian image"));
            }
        }
    }

    let image_name = image_path.file_name().unwrap().to_str().unwrap();

    let cmd_args = vec![
            CString::new("-c").unwrap(),
            CString::new(
                format!("mkdir -p /krunai && mount -t virtiofs krunai /krunai && debugfs -w -R {command} /krunai/{image_name} && /sbin/reboot -f"),
            )
            .unwrap(),
        ];

    //exec_vm(&runtime_dir, &vm_disk, &source_dir, cmd_args)
    run_e2fs_helper(&runtime_dir, &vm_disk, &source_dir, cmd_args)
}

/// Run a debugfs batch file against an ext4 image via helper VM.
pub fn run_debugfs_batch_file_via_vm(
    image_path: &Path,
    command_path: &Path,
) -> Result<(), String> {
    info!("Running debugfs batch file via VM: {}; {}", command_path.display(), image_path.display());
    let runtime_dir = configured_runtime_dir()?;

    let source_dir = image_path.parent().unwrap_or_else(|| Path::new("."));

    let vm_disk = runtime_dir.join(VM_DISK_NAME);
    if !vm_disk.is_file() {
        let status = Command::new("curl")
            .arg("-L")
            .arg("-o")
            .arg(&vm_disk)
            .arg("--progress-bar")
            .arg(DEBIAN_IMAGE_URL)
            .status();
        match status {
            Ok(status) if status.success() => {}
            _ => {
                return Err(format!("failed to download Debian image"));
            }
        }
    }

    let image_name = image_path.file_name().unwrap().to_str().unwrap();
    let command_name = command_path.file_name().unwrap().to_str().unwrap();

    let cmd_args = vec![
            CString::new("-c").unwrap(),
            CString::new(
                format!("mkdir -p /krunai && mount -t virtiofs krunai /krunai && debugfs -w -f /krunai/{command_name} /krunai/{image_name} && /sbin/reboot -f"),
            )
            .unwrap(),
        ];

    //exec_vm(&runtime_dir, &vm_disk, &source_dir, cmd_args)
    run_e2fs_helper(&runtime_dir, &vm_disk, &source_dir, cmd_args)
}

fn temporary_injection_path(image_path: &Path) -> PathBuf {
    let parent = image_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".openshell-rootfs-inject-{}",
        std::process::id()
    ))
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn debugfs_quote_absolute_path(path: &str) -> Option<String> {
    if path.is_empty() || !path.starts_with("C:\\") {
        return None;
    }

    debugfs_quote_argument(path)
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn debugfs_quote_argument(argument: &str) -> Option<String> {
    if argument.is_empty() {
        return None;
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    for ch in argument.chars() {
        match ch {
            '\0' | '\n' | '\r' => return None,
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    Some(quoted)
}

/// Write a file into an ext4 image via debugfs in a helper VM.
///
/// The payload content is placed in a virtio-fs shared inject directory,
/// and debugfs writes it into the image at the specified guest path.
pub fn write_file_via_vm(
    image_path: &Path,
    guest_path: &str,
    contents: &[u8],
) -> Result<(), String> {
    info!("Writing file via VM: {}; {}", image_path.display(), guest_path);
    
    let tmp_path = temporary_injection_path(image_path);
    fs::write(&tmp_path, contents)
        .map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
    let Some(quoted_guest_path) = debugfs_quote_absolute_path(guest_path) else {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("invalid debugfs guest path '{guest_path}'"));
    };
    let Some(quoted_tmp_path) = debugfs_quote_argument(&tmp_path.to_string_lossy()) else {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!(
            "invalid debugfs injection path '{}'",
            tmp_path.display()
        ));
    };

    let _ = run_debugfs_via_vm(image_path, &format!("rm {quoted_guest_path}"));
    let result = run_debugfs_via_vm(image_path, &format!("write {quoted_tmp_path} {quoted_guest_path}"));
    let _ = fs::remove_file(&tmp_path);
    result
}

fn temp_inject_dir(image_path: &Path) -> Result<PathBuf, String> {
    let parent = image_path.parent().unwrap_or_else(|| Path::new("."));
    let dir = parent.join(format!(
        ".openshell-e2fs-inject-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir)
        .map_err(|e| format!("create inject dir {}: {e}", dir.display()))?;
    Ok(dir)
}

fn temp_console_path(image_path: &Path) -> PathBuf {
    let parent = image_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".openshell-e2fs-console-{}.log",
        std::process::id()
    ))
}
