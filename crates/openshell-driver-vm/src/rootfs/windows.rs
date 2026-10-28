// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! ext4kit-backed stand-ins for `mke2fs -d`, `debugfs` and `e2fsck` on
//! Windows.
//!
//! Windows hosts have no e2fsprogs. ext4kit builds the same ext4 images
//! (`create`), applies the guest-side metadata and account changes the
//! driver would otherwise make with `debugfs` (`edit`), and repairs a
//! preserved guest disk a killed VM left unclean (`recover`). Every image
//! the driver touches on Windows is ext4kit-created, so the edit actions
//! accept it, and no helper VM is involved.

use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use super::{
    ROOTFS_VARIANT_MARKER, SANDBOX, SANDBOX_GUEST_INIT_PATH, SANDBOX_SUPERVISOR_PATH,
    SANDBOX_UMOCI_PATH, SANDBOX_VM_INIT_PATH, UMOCI, VM_INIT, parse_sandbox_guest_user_ids,
    sandbox_guest_user_ids, temporary_injection_path,
};

/// Overrides the ext4kit executable used for image operations; the default
/// is resolved from PATH, like e2fsprogs on Unix.
const EXT4KIT_ENV: &str = "OPENSHELL_VM_EXT4KIT";
const EXT4KIT_GUIDANCE: &str = "put ext4kit on PATH or set OPENSHELL_VM_EXT4KIT, and retry";
const MIB: u64 = 1024 * 1024;

/// Explain a failed ext4kit spawn. Windows reports `ERROR_ACCESS_DENIED`
/// when the resolved program is a directory (e.g. `OPENSHELL_VM_EXT4KIT`
/// pointing at the ext4kit checkout), which the generic guidance would
/// misdiagnose as "not installed".
fn ext4kit_spawn_failure(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "{error}. the resolved ext4kit is not an executable; point \
             OPENSHELL_VM_EXT4KIT at ext4kit.exe, not its directory"
        )
    } else {
        format!("{error}. {EXT4KIT_GUIDANCE}")
    }
}

fn ext4kit_command() -> Command {
    // Preflight and image operations must execute the same selected file
    // with the same inherited environment, including when PATH is
    // restricted.
    std::env::var_os(EXT4KIT_ENV).map_or_else(|| Command::new("ext4kit"), Command::new)
}

/// `ext4kit create`: `adds` are `(host path, absolute image directory)`
/// pairs; ext4kit places each host entry below that directory by name.
fn run_ext4kit(
    output: &Path,
    from: Option<&str>,
    size_mib: Option<u64>,
    adds: &[(PathBuf, &str)],
) -> Result<(), String> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    if output.exists() {
        fs::remove_file(output).map_err(|e| format!("remove {}: {e}", output.display()))?;
    }
    let mut command = ext4kit_command();
    command.arg("create").arg("--root-owner").arg("0:0");
    if let Some(from) = from {
        command.arg("--from").arg(from);
    }
    if let Some(size_mib) = size_mib {
        command.arg("--size").arg(size_mib.to_string());
    }
    for (source, destination) in adds {
        command
            .arg("--add")
            .arg(format!("{}:{destination}", source.display()));
    }
    command.arg(output);
    let result = command.output();
    let failure = match result {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => format!(
            "ext4kit failed with status {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => ext4kit_spawn_failure(&error),
    };
    let _ = fs::remove_file(output);
    Err(format!("create ext4 image {}: {failure}", output.display()))
}

fn edit_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn ext4kit_edit_output(image_path: &Path, action: &[OsString]) -> Result<Output, String> {
    let action_label = action
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    ext4kit_command()
        .arg("edit")
        .arg(image_path)
        .args(action)
        .output()
        .map_err(|error| {
            format!(
                "run ext4kit edit {action_label} on {}: {}",
                image_path.display(),
                ext4kit_spawn_failure(&error)
            )
        })
}

fn ext4kit_edit(image_path: &Path, action: &[OsString]) -> Result<(), String> {
    let output = ext4kit_edit_output(image_path, action)?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "ext4kit edit {} failed for {} with status {}\nstdout: {}\nstderr: {}",
        action
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
        image_path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// `ext4kit edit test`: exit 0 holds, exit 1 does not, anything else failed.
fn ext4kit_test(image_path: &Path, predicate: &str, guest_path: &str) -> Result<bool, String> {
    let action = edit_args(&["test", predicate, guest_path]);
    let output = ext4kit_edit_output(image_path, &action)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "ext4kit edit test {predicate} {guest_path} failed for {} with status {}\nstdout: {}\nstderr: {}",
            image_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

/// An absolute guest path made of normal components only, like the paths
/// `debugfs_quote_absolute_path` accepts on Unix.
fn validate_guest_path(guest_path: &str) -> Result<(), String> {
    let relative = guest_path.trim_start_matches('/');
    if !guest_path.starts_with('/')
        || relative.is_empty()
        || Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("invalid guest path '{guest_path}'"));
    }
    Ok(())
}

/// Create the parent directories of `guest_path`, ignoring failures like
/// the debugfs `mkdir` sweep on Unix.
fn ensure_rootfs_image_parent_dirs(image_path: &Path, guest_path: &str) {
    let Some(parent) = Path::new(guest_path).parent() else {
        return;
    };
    let _ = ext4kit_edit(
        image_path,
        &edit_args(&["mkdir", "-p", &parent.to_string_lossy()]),
    );
}

pub fn write_rootfs_image_file(
    image_path: &Path,
    guest_path: &str,
    contents: &[u8],
) -> Result<(), String> {
    ensure_rootfs_image_parent_dirs(image_path, guest_path);

    let tmp_path = temporary_injection_path(image_path);
    fs::write(&tmp_path, contents).map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
    if let Err(error) = validate_guest_path(guest_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    // Remove first so the write lands on a fresh inode with the default
    // mode, like the debugfs rm+write sequence on Unix; callers set the
    // final mode right after.
    let _ = ext4kit_edit(image_path, &edit_args(&["rm", "-f", guest_path]));
    let result = ext4kit_edit(
        image_path,
        &[
            OsString::from("write"),
            tmp_path.as_os_str().to_os_string(),
            OsString::from(guest_path),
        ],
    );
    let _ = fs::remove_file(&tmp_path);
    result
}

pub fn remove_rootfs_image_file(image_path: &Path, guest_path: &str) -> Result<(), String> {
    validate_guest_path(guest_path)?;
    ext4kit_edit(image_path, &edit_args(&["rm", guest_path]))
}

pub fn set_rootfs_image_file_mode(
    image_path: &Path,
    guest_path: &str,
    mode: u32,
) -> Result<(), String> {
    validate_guest_path(guest_path)?;
    ext4kit_edit(
        image_path,
        &edit_args(&["chmod", &format!("{:o}", mode & 0o7777), guest_path]),
    )
}

/// Repair a preserved guest disk a killed VM left unclean: clear the error
/// state, drop the orphan list and recompute the free-space counters, like
/// `e2fsck -p -f` does on Unix. ext4kit images carry no journal, so there
/// is nothing to replay.
pub fn recover_rootfs_image(image_path: &Path) -> Result<(), String> {
    let output = ext4kit_command()
        .arg("recover")
        .arg(image_path)
        .output()
        .map_err(|error| {
            format!(
                "run ext4kit recover on {}: {}",
                image_path.display(),
                ext4kit_spawn_failure(&error)
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "ext4kit recover failed for {} with status {}\nstdout: {}\nstderr: {}",
        image_path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// Contents of the file at an absolute guest path, `None` when missing.
pub fn read_image_file(image_path: &Path, guest_path: &str) -> Result<Option<Vec<u8>>, String> {
    if ext4kit_test(image_path, "-f", guest_path)? {
        let action = edit_args(&["cat", guest_path]);
        let output = ext4kit_edit_output(image_path, &action)?;
        if !output.status.success() {
            return Err(format!(
                "ext4kit edit cat {guest_path} failed for {} with status {}\nstdout: {}\nstderr: {}",
                image_path.display(),
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        return Ok(Some(output.stdout));
    }

    if ext4kit_test(image_path, "-e", guest_path)? || ext4kit_test(image_path, "-L", guest_path)? {
        return Err(format!(
            "{guest_path} in rootfs image is not a readable regular file"
        ));
    }
    Ok(None)
}

/// Check, without following symlinks, whether the image has a directory at
/// `guest_path`, like `debugfs stat` on Unix.
pub fn ext4_image_has_directory(image_path: &Path, guest_path: &str) -> Result<bool, String> {
    if ext4kit_test(image_path, "-L", guest_path)? {
        return Ok(false);
    }
    ext4kit_test(image_path, "-d", guest_path)
}

/// Give the staged `/sandbox` tree to the sandbox account, like the debugfs
/// `set_inode_field` sweep on Unix. ext4kit's recursive chown also re-owns
/// symlinks themselves, which the Unix sweep leaves alone.
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

    ext4kit_edit(
        image_path,
        &edit_args(&["chown", "-R", &format!("{uid}:{gid}"), "/sandbox"]),
    )
}

fn replace_or_append_entry(
    contents: &str,
    replacement: &str,
    matches: impl Fn(&str) -> bool,
) -> String {
    let mut output = String::with_capacity(contents.len().max(replacement.len() + 1));
    let mut replaced = false;
    for existing in contents.lines() {
        if matches(existing) {
            if replaced {
                continue;
            }
            output.push_str(replacement);
            replaced = true;
        } else {
            output.push_str(existing);
        }
        output.push('\n');
    }
    if !replaced {
        output.push_str(replacement);
        output.push('\n');
    }
    output
}

fn prepare_sandbox_account(
    image_path: &Path,
    staging_dir: &Path,
    configured_identity: Option<(u32, u32)>,
) -> Result<(u32, u32), String> {
    let passwd_path = "/etc/passwd";
    let existing_passwd =
        match read_image_file(image_path, passwd_path)? {
            Some(bytes) => Some(String::from_utf8(bytes).map_err(|error| {
                format!("read {passwd_path} from rootfs image as UTF-8: {error}")
            })?),
            None => None,
        };
    let existing_identity = if configured_identity.is_none() {
        existing_passwd
            .as_deref()
            .map(|passwd| parse_sandbox_guest_user_ids(passwd, &image_path.display().to_string()))
            .transpose()?
            .flatten()
    } else {
        None
    };

    if configured_identity.is_none()
        && let Some(identity) = existing_identity
    {
        return Ok(identity);
    }

    let identity = configured_identity.unwrap_or_else(|| {
        let uid = super::DEFAULT_SANDBOX_UID;
        (uid, uid)
    });
    ext4kit_edit(image_path, &edit_args(&["mkdir", "-p", "/etc"]))?;

    let account_dir = staging_dir.join("sandbox-account");
    fs::create_dir_all(&account_dir)
        .map_err(|error| format!("create {}: {error}", account_dir.display()))?;
    let records = [
        ("group", "/etc/group", format!("sandbox:x:{}:", identity.1)),
        ("gshadow", "/etc/gshadow", "sandbox:!::".to_string()),
        (
            "passwd",
            "/etc/passwd",
            format!(
                "sandbox:x:{}:{}:OpenShell Sandbox:/sandbox:/bin/bash",
                identity.0, identity.1
            ),
        ),
        (
            "shadow",
            "/etc/shadow",
            "sandbox:!:20123:0:99999:7:::".to_string(),
        ),
    ];

    for (filename, guest_path, replacement) in records {
        let contents = read_image_file(image_path, guest_path)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let contents =
            replace_or_append_entry(&contents, &replacement, |line| line.starts_with("sandbox:"));
        let host_path = account_dir.join(filename);
        fs::write(&host_path, contents)
            .map_err(|error| format!("write {}: {error}", host_path.display()))?;
        ext4kit_edit(
            image_path,
            &[
                OsString::from("write"),
                host_path.as_os_str().to_os_string(),
                OsString::from(guest_path),
            ],
        )?;
    }

    Ok(identity)
}

fn require_guest_path(image_path: &Path, candidates: &[&str]) -> Result<(), String> {
    for candidate in candidates {
        if ext4kit_test(image_path, "-e", candidate)? {
            return Ok(());
        }
    }
    Err(format!(
        "rootfs image is missing required path (one of: {})",
        candidates.join(", ")
    ))
}

fn prepare_bootstrap_rootfs_image(
    image_path: &Path,
    staging_dir: &Path,
    configured_identity: Option<(u32, u32)>,
) -> Result<(), String> {
    ext4kit_edit(
        image_path,
        &edit_args(&["rm", "-f", "/opt/openshell/.initialized"]),
    )?;

    for path in [
        SANDBOX_GUEST_INIT_PATH,
        SANDBOX_SUPERVISOR_PATH,
        SANDBOX_VM_INIT_PATH,
        SANDBOX_UMOCI_PATH,
    ] {
        ext4kit_edit(image_path, &edit_args(&["chmod", "755", path]))?;
    }

    for path in ["/sandbox", "/image-cache", "/lower", "/overlay", "/newroot"] {
        ext4kit_edit(image_path, &edit_args(&["mkdir", "-p", path]))?;
        ext4kit_edit(image_path, &edit_args(&["chmod", "755", path]))?;
    }

    let (uid, gid) = prepare_sandbox_account(image_path, staging_dir, configured_identity)?;
    ext4kit_edit(
        image_path,
        &edit_args(&["chown", "-R", &format!("{uid}:{gid}"), "/sandbox"]),
    )?;

    for (path, what) in [
        (SANDBOX_GUEST_INIT_PATH, "guest init"),
        (SANDBOX_SUPERVISOR_PATH, "sandbox supervisor"),
        (SANDBOX_VM_INIT_PATH, "VM init helper"),
        (SANDBOX_UMOCI_PATH, "umoci"),
    ] {
        if !ext4kit_test(image_path, "-x", path)? {
            return Err(format!(
                "rootfs image is missing executable {what} at {path}"
            ));
        }
    }
    require_guest_path(image_path, &["/bin/bash"])?;
    require_guest_path(image_path, &["/bin/mount", "/usr/bin/mount"])?;
    require_guest_path(
        image_path,
        &[
            "/usr/sbin/chroot",
            "/usr/bin/chroot",
            "/sbin/chroot",
            "/bin/chroot",
        ],
    )?;
    require_guest_path(image_path, &["/bin/sed", "/usr/bin/sed"])?;
    Ok(())
}

/// ext4kit nests an added directory under its own name; add each child of
/// `dir` at `/` so the directory's contents become the image root.
fn root_children(dir: &Path) -> Result<Vec<(PathBuf, &'static str)>, String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .map(|entry| entry.map(|entry| (entry.path(), "/")))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read {}: {e}", dir.display()))?;
    entries.sort();
    Ok(entries)
}

/// `mke2fs -d source image` for an image file already sized by the caller.
pub(super) fn format_ext4_image_from_dir(source: &Path, image_path: &Path) -> Result<(), String> {
    let size = fs::metadata(image_path)
        .map_err(|e| format!("stat {}: {e}", image_path.display()))?
        .len();
    run_ext4kit(
        image_path,
        None,
        Some(size.div_ceil(MIB)),
        &root_children(source)?,
    )
}

/// Build and prepare the cached bootstrap rootfs from an ext4kit image source.
/// Registry callers pass `docker://<pinned-ref>`; local callers pass an
/// explicit `docker-daemon:` or `containers-storage:` source so ext4kit uses
/// the same engine that the driver inspected. All image fixups run through
/// `ext4kit edit` before this returns.
pub fn create_bootstrap_rootfs_image(
    image_source: &str,
    staging_dir: &Path,
    image_path: &Path,
    image_identity: &str,
    sandbox_identity: Option<(u32, u32)>,
) -> Result<(), String> {
    let payload = staging_dir.join("payload");
    if payload.exists() {
        fs::remove_dir_all(&payload).map_err(|e| format!("remove {}: {e}", payload.display()))?;
    }
    // A Windows checkout with core.autocrlf rewrites sources to CRLF, which
    // bash in the guest rejects.
    write_payload_file(
        &payload,
        SANDBOX_GUEST_INIT_PATH,
        include_str!("../../scripts/openshell-vm-sandbox-init.sh")
            .replace("\r\n", "\n")
            .as_bytes(),
    )?;
    for (guest_path, compressed, what) in [
        (SANDBOX_SUPERVISOR_PATH, SANDBOX, "openshell-sandbox"),
        (SANDBOX_VM_INIT_PATH, VM_INIT, "openshell-vm-init"),
        (SANDBOX_UMOCI_PATH, UMOCI, "umoci"),
    ] {
        if compressed.is_empty() {
            return Err(format!(
                "{what} is not embedded; build openshell-driver-vm with OPENSHELL_VM_RUNTIME_COMPRESSED_DIR set"
            ));
        }
        let bytes = zstd::decode_all(Cursor::new(compressed))
            .map_err(|e| format!("decompress {what}: {e}"))?;
        write_payload_file(&payload, guest_path, &bytes)?;
    }
    write_payload_file(&payload, "/opt/openshell/.rootfs-type", b"sandbox\n")?;
    write_payload_file(
        &payload,
        &format!("/{ROOTFS_VARIANT_MARKER}"),
        format!("{}:image:{image_identity}\n", env!("CARGO_PKG_VERSION")).as_bytes(),
    )?;
    run_ext4kit(
        image_path,
        Some(image_source),
        None,
        &root_children(&payload)?,
    )?;
    prepare_bootstrap_rootfs_image(image_path, staging_dir, sandbox_identity)
}

fn write_payload_file(payload: &Path, guest_path: &str, contents: &[u8]) -> Result<(), String> {
    let path = payload.join(relative_guest_path(guest_path)?);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))
}

fn relative_guest_path(guest_path: &str) -> Result<PathBuf, String> {
    validate_guest_path(guest_path)?;
    Ok(PathBuf::from(guest_path.trim_start_matches('/')))
}
