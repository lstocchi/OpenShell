// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! e2fsprogs-backed ext4 image operations for Unix hosts: `mke2fs -d` builds
//! images, `debugfs` applies guest-side metadata and account changes, and
//! `e2fsck` repairs a preserved guest disk a killed VM left unclean.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{sandbox_guest_user_ids, temporary_injection_path};

pub fn write_rootfs_image_file(
    image_path: &Path,
    guest_path: &str,
    contents: &[u8],
) -> Result<(), String> {
    ensure_rootfs_image_parent_dirs(image_path, guest_path);

    let tmp_path = temporary_injection_path(image_path);
    fs::write(&tmp_path, contents).map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
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
    let _ = run_debugfs(image_path, &format!("rm {quoted_guest_path}"));
    let result = run_debugfs(
        image_path,
        &format!("write {quoted_tmp_path} {quoted_guest_path}"),
    );
    let _ = fs::remove_file(&tmp_path);
    result
}

pub fn remove_rootfs_image_file(image_path: &Path, guest_path: &str) -> Result<(), String> {
    let Some(quoted_guest_path) = debugfs_quote_absolute_path(guest_path) else {
        return Err(format!("invalid debugfs guest path '{guest_path}'"));
    };
    run_debugfs(image_path, &format!("rm {quoted_guest_path}"))
}

pub fn set_rootfs_image_file_mode(
    image_path: &Path,
    guest_path: &str,
    mode: u32,
) -> Result<(), String> {
    let regular_file_mode = 0o100_000 | (mode & 0o7777);
    let Some(quoted_guest_path) = debugfs_quote_absolute_path(guest_path) else {
        return Err(format!("invalid debugfs guest path '{guest_path}'"));
    };
    run_debugfs(
        image_path,
        &format!("set_inode_field {quoted_guest_path} mode 0{regular_file_mode:o}"),
    )
}

/// Replay the ext4 journal and repair automatically correctable filesystem
/// state before the driver mutates a preserved guest disk offline.
pub fn recover_rootfs_image(image_path: &Path) -> Result<(), String> {
    let mut command = e2fs_command("e2fsck")?;
    let path = PathBuf::from(command.get_program());
    let output = command.arg("-p").arg("-f").arg(image_path).output();
    match output {
        Ok(output) if matches!(output.status.code(), Some(0..=2)) => Ok(()),
        Ok(output) => Err(format!(
            "{} failed with status {}\nstdout: {}\nstderr: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
        Err(error) => Err(format!("run {}: {error}", path.display())),
    }
}

pub(super) fn format_ext4_image_from_dir(source: &Path, image_path: &Path) -> Result<(), String> {
    let path = openshell_core::e2fsprogs::resolve(&["mke2fs", "mkfs.ext4"])?;
    let output = Command::new(&path)
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-E")
        .arg("root_owner=0:0")
        .arg("-d")
        .arg(source)
        .arg(image_path)
        .output()
        .map_err(|error| format!("run {}: {error}", path.display()))?;
    if output.status.success() {
        return Ok(());
    }
    // A selected formatter failure must retain its diagnostics. Retrying with
    // another installation can overwrite a partial image and hide the cause.
    Err(format!(
        "failed to create ext4 rootfs image from {}: {} failed with status {}\nstdout: {}\nstderr: {}",
        source.display(),
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn ensure_rootfs_image_parent_dirs(image_path: &Path, guest_path: &str) {
    let Some(parent) = Path::new(guest_path).parent() else {
        return;
    };
    let mut current = String::new();
    for component in parent.components() {
        let part = component.as_os_str().to_string_lossy();
        if part == "/" || part.is_empty() {
            continue;
        }
        current.push('/');
        current.push_str(&part);
        let _ = run_debugfs(image_path, &format!("mkdir {current}"));
    }
}

pub fn normalize_sandbox_owner_in_rootfs_image(
    source: &Path,
    image_path: &Path,
) -> Result<(), String> {
    let sandbox_dir = source.join("sandbox");
    if !sandbox_dir.exists() {
        return Ok(());
    }

    let Some((uid, gid)) = sandbox_guest_user_ids(source)? else {
        return Ok(());
    };

    let mut commands = Vec::new();
    if !collect_sandbox_owner_commands(&sandbox_dir, "/sandbox", uid, gid, &mut commands)? {
        return Ok(());
    }
    if commands.is_empty() {
        return Ok(());
    }

    run_debugfs_batch(image_path, &commands)
}

fn collect_sandbox_owner_commands(
    source_path: &Path,
    guest_path: &str,
    uid: u32,
    gid: u32,
    commands: &mut Vec<String>,
) -> Result<bool, String> {
    let metadata = fs::symlink_metadata(source_path).map_err(|e| {
        format!(
            "stat {} for rootfs ownership normalization: {e}",
            source_path.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }

    let Some(quoted_guest_path) = debugfs_quote_absolute_path(guest_path) else {
        return Ok(false);
    };
    commands.push(format!("set_inode_field {quoted_guest_path} uid {uid}"));
    commands.push(format!("set_inode_field {quoted_guest_path} gid {gid}"));

    if !metadata.is_dir() {
        return Ok(true);
    }

    let mut entries = fs::read_dir(source_path)
        .map_err(|e| {
            format!(
                "read {} for rootfs ownership normalization: {e}",
                source_path.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            format!(
                "read {} entry for rootfs ownership normalization: {e}",
                source_path.display()
            )
        })?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Ok(false);
        };
        let child_guest_path = format!("{guest_path}/{file_name}");
        if !collect_sandbox_owner_commands(&entry.path(), &child_guest_path, uid, gid, commands)? {
            return Ok(false);
        }
    }

    Ok(true)
}

fn debugfs_quote_absolute_path(path: &str) -> Option<String> {
    if path.is_empty() || !path.starts_with('/') {
        return None;
    }

    debugfs_quote_argument(path)
}

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

/// Check, without mounting, whether an ext4 image has a directory at
/// `guest_path`. Symlinks are not followed.
pub fn ext4_image_has_directory(image_path: &Path, guest_path: &str) -> Result<bool, String> {
    let quoted_path = debugfs_quote_absolute_path(guest_path)
        .ok_or_else(|| format!("invalid debugfs guest path '{guest_path}'"))?;
    let command = format!("stat {quoted_path}");
    let output = e2fs_command("debugfs")?
        .arg("-R")
        .arg(&command)
        .arg(image_path)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            // debugfs exits 0 whether or not the path exists; the answer
            // is only in its output.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stdout.contains("Type: directory") {
                return Ok(true);
            }
            if stdout.contains("Type: ") || stderr.contains("File not found") {
                return Ok(false);
            }
            Err(format!(
                "debugfs command '{command}' produced unrecognized output for {}\nstdout: {stdout}\nstderr: {stderr}",
                image_path.display()
            ))
        }
        Ok(output) => Err(format!(
            "debugfs command '{command}' failed for {}: debugfs failed with status {}\nstdout: {}\nstderr: {}. Install e2fsprogs (debugfs) and retry",
            image_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
        Err(error) => Err(format!(
            "debugfs command '{command}' failed for {}: {error}. Install e2fsprogs (debugfs) and retry",
            image_path.display()
        )),
    }
}

/// `debugfs cat`: contents of the file at an absolute guest path. A missing
/// file fails; ext4kit on Windows instead reports it as absent.
pub fn read_image_file(image_path: &Path, guest_path: &str) -> Result<Option<Vec<u8>>, String> {
    let quoted_path = debugfs_quote_absolute_path(guest_path)
        .ok_or_else(|| format!("invalid debugfs guest path '{guest_path}'"))?;
    let command = format!("cat {quoted_path}");
    let output = e2fs_command("debugfs")?
        .arg("-R")
        .arg(&command)
        .arg(image_path)
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(Some(output.stdout)),
        Ok(output) => Err(format!(
            "debugfs command '{command}' failed for {}: debugfs failed with status {}\nstdout: {}\nstderr: {}. Install e2fsprogs (debugfs) and retry",
            image_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
        Err(error) => Err(format!(
            "debugfs command '{command}' failed for {}: {error}. Install e2fsprogs (debugfs) and retry",
            image_path.display()
        )),
    }
}

fn run_debugfs_batch(image_path: &Path, commands: &[String]) -> Result<(), String> {
    let command_path = temporary_injection_path(image_path);
    let mut contents = commands.join("\n");
    contents.push('\n');
    fs::write(&command_path, contents)
        .map_err(|e| format!("write {}: {e}", command_path.display()))?;

    let result = run_debugfs_batch_file(image_path, &command_path);
    let _ = fs::remove_file(&command_path);
    result
}

fn run_debugfs_batch_file(image_path: &Path, command_path: &Path) -> Result<(), String> {
    let output = e2fs_command("debugfs")?
        .arg("-w")
        .arg("-f")
        .arg(command_path)
        .arg(image_path)
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "debugfs batch {} failed for {}: debugfs failed with status {}\nstdout: {}\nstderr: {}. Install e2fsprogs (debugfs) and retry",
            command_path.display(),
            image_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
        Err(error) => Err(format!(
            "debugfs batch {} failed for {}: {error}. Install e2fsprogs (debugfs) and retry",
            command_path.display(),
            image_path.display()
        )),
    }
}

fn run_debugfs(image_path: &Path, command: &str) -> Result<(), String> {
    let output = e2fs_command("debugfs")?
        .arg("-w")
        .arg("-R")
        .arg(command)
        .arg(image_path)
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "debugfs command '{command}' failed for {}: debugfs failed with status {}\nstdout: {}\nstderr: {}. Install e2fsprogs (debugfs) and retry",
            image_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
        Err(error) => Err(format!(
            "debugfs command '{command}' failed for {}: {error}. Install e2fsprogs (debugfs) and retry",
            image_path.display()
        )),
    }
}

fn e2fs_command(tool: &str) -> Result<Command, String> {
    // Preflight and image operations must execute the same selected file with
    // the same inherited environment, including when PATH is restricted.
    Ok(Command::new(openshell_core::e2fsprogs::resolve(&[tool])?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rootfs::{create_ext4_image_from_dir_with_size, unique_temp_dir};

    use std::fs::File;
    use std::path::PathBuf;

    #[test]
    fn recover_rootfs_image_accepts_clean_ext4_image() {
        if e2fs_command("e2fsck").is_err() {
            return;
        }

        let dir = unique_temp_dir();
        let source = dir.join("source");
        let image = dir.join("overlay.ext4");
        fs::create_dir_all(source.join("upper")).expect("create source upperdir");
        fs::create_dir_all(source.join("work")).expect("create source workdir");
        create_ext4_image_from_dir_with_size(&source, &image, 64 * 1024 * 1024)
            .expect("create ext4 image");

        recover_rootfs_image(&image).expect("recover clean ext4 image");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ext4_image_has_directory_distinguishes_directories_files_and_missing_paths() {
        if e2fs_command("debugfs").is_err() {
            return;
        }

        let dir = unique_temp_dir();
        let source = dir.join("source");
        let image = dir.join("prepared.ext4");
        fs::create_dir_all(source.join("image-rootfs/bin")).expect("create image rootfs");
        fs::write(source.join("regular"), "file\n").expect("write regular file");
        create_ext4_image_from_dir_with_size(&source, &image, 64 * 1024 * 1024)
            .expect("create ext4 image");

        assert!(ext4_image_has_directory(&image, "/image-rootfs").expect("stat directory"));
        assert!(!ext4_image_has_directory(&image, "/regular").expect("stat regular file"));
        assert!(!ext4_image_has_directory(&image, "/missing").expect("stat missing path"));
        assert!(ext4_image_has_directory(&image, "relative").is_err());

        let not_ext4 = dir.join("not-ext4.img");
        fs::write(&not_ext4, vec![0_u8; 64 * 1024]).expect("write non-ext4 image");
        assert!(ext4_image_has_directory(&not_ext4, "/image-rootfs").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_sandbox_owner_commands_quotes_guest_paths() {
        let dir = unique_temp_dir();
        let sandbox_dir = dir.join("sandbox");
        fs::create_dir_all(sandbox_dir.join("dir with space")).expect("create sandbox tree");
        fs::write(sandbox_dir.join("dir with space/file.txt"), "hello\n").expect("write file");

        let mut commands = Vec::new();
        assert!(
            collect_sandbox_owner_commands(&sandbox_dir, "/sandbox", 998, 997, &mut commands)
                .expect("collect commands")
        );

        assert!(commands.contains(&"set_inode_field \"/sandbox\" uid 998".to_string()));
        assert!(commands.contains(&"set_inode_field \"/sandbox\" gid 997".to_string()));
        assert!(
            commands.contains(
                &"set_inode_field \"/sandbox/dir with space/file.txt\" uid 998".to_string()
            )
        );
        assert!(
            commands.contains(
                &"set_inode_field \"/sandbox/dir with space/file.txt\" gid 997".to_string()
            )
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debugfs_quote_argument_quotes_source_paths_with_spaces() {
        assert_eq!(
            debugfs_quote_argument("/tmp/openshell state/.openshell-rootfs-inject-123-0"),
            Some("\"/tmp/openshell state/.openshell-rootfs-inject-123-0\"".to_string())
        );
        assert_eq!(
            debugfs_quote_argument("/tmp/path/with\\backslash/and\"quote"),
            Some("\"/tmp/path/with\\\\backslash/and\\\"quote\"".to_string())
        );
        assert_eq!(debugfs_quote_argument("/tmp/bad\npath"), None);
    }

    fn run_tool_fixture_test(test: &str, directory: &Path) {
        // Isolate PATH in a child test process. Other tests may prepare images
        // concurrently and must never observe this deliberately broken tool.
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", test, "--nocapture"])
            .env("PATH", directory)
            .env("OPENSHELL_ROOTFS_TOOL_TEST_ROOT", directory)
            .output()
            .expect("isolated tool test");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn formatter_preserves_selected_tool_failure_without_retry() {
        use std::os::unix::fs::PermissionsExt as _;

        if let Some(directory) = std::env::var_os("OPENSHELL_ROOTFS_TOOL_TEST_ROOT") {
            let directory = PathBuf::from(directory);
            let image = directory.join("rootfs.ext4");
            File::create(&image)
                .expect("image")
                .set_len(16 * 1024 * 1024)
                .expect("image size");
            let source = directory.join("source");
            fs::create_dir(&source).expect("source directory");
            let error = format_ext4_image_from_dir(&source, &image)
                .expect_err("the selected formatter failure must not select another installation");
            assert!(error.contains("selected-formatter-failed"), "{error}");
            assert!(error.contains("42"), "{error}");
            return;
        }
        let directory = tempfile::tempdir().expect("tool installation");
        for (name, body) in [
            ("mke2fs", "echo selected-formatter-failed >&2\nexit 42"),
            ("mkfs.ext4", "exit 0"),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("tool fixture");
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable");
        }
        run_tool_fixture_test(
            "rootfs::unix::tests::formatter_preserves_selected_tool_failure_without_retry",
            directory.path(),
        );
    }

    #[test]
    fn recovery_preserves_selected_path_and_execution_errors() {
        use std::os::unix::fs::PermissionsExt as _;

        if let Some(directory) = std::env::var_os("OPENSHELL_ROOTFS_TOOL_TEST_ROOT") {
            let directory = PathBuf::from(directory);
            let path = directory.join("e2fsck");
            for (script, expected) in [
                (
                    "#!/nonexistent/e2fsprogs-interpreter\n",
                    "No such file or directory",
                ),
                (
                    "#!/bin/sh\necho recovery-failed >&2\nexit 4\n",
                    "recovery-failed",
                ),
            ] {
                fs::write(&path, script).expect("recovery tool");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable");
                let error = recover_rootfs_image(&directory.join("overlay.ext4"))
                    .expect_err("recovery failure");
                assert!(
                    error.contains(path.canonicalize().unwrap().to_str().unwrap()),
                    "{error}"
                );
                assert!(error.contains(expected), "{error}");
            }
            return;
        }
        let directory = tempfile::tempdir().expect("tool installation");
        run_tool_fixture_test(
            "rootfs::unix::tests::recovery_preserves_selected_path_and_execution_errors",
            directory.path(),
        );
    }
}
