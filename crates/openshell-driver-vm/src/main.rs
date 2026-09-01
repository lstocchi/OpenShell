// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use futures::Stream;
use miette::{IntoDiagnostic, Result};
use openshell_core::VERSION;
use openshell_core::proto::compute::v1::compute_driver_server::ComputeDriverServer;
#[cfg(target_os = "macos")]
use openshell_driver_vm::{VM_RUNTIME_DIR_ENV, configured_runtime_dir};
use openshell_driver_vm::{VmBackend, VmDriver, VmDriverConfig, VmLaunchConfig, procguard, run_vm};
#[cfg(windows)]
use std::future::Future;
use std::io;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "openshell-driver-vm")]
#[command(version = VERSION)]
#[allow(clippy::struct_excessive_bools)]
struct Args {
    #[arg(long, hide = true, default_value_t = false)]
    internal_run_vm: bool,

    #[arg(long = "vm-root-disk", hide = true, alias = "vm-rootfs")]
    vm_root_disk: Option<PathBuf>,

    #[arg(long = "vm-overlay-disk", hide = true)]
    vm_overlay_disk: Option<PathBuf>,

    #[arg(long = "vm-image-disk", hide = true)]
    vm_image_disk: Option<PathBuf>,

    #[arg(long = "vm-kernel-image", hide = true)]
    vm_kernel_image: Option<PathBuf>,

    #[arg(long, hide = true)]
    vm_exec: Option<String>,

    #[arg(long, hide = true, default_value = "/")]
    vm_workdir: String,

    #[arg(long, hide = true)]
    vm_env: Vec<String>,

    #[arg(long, hide = true)]
    vm_console_output: Option<PathBuf>,

    #[arg(long, hide = true, default_value_t = 2)]
    vm_vcpus: u8,

    #[arg(long, hide = true, default_value_t = 2048)]
    vm_mem_mib: u32,

    #[arg(long, hide = true, default_value_t = 1)]
    vm_krun_log_level: u32,

    #[arg(long, env = "OPENSHELL_COMPUTE_DRIVER_BIND")]
    bind_address: Option<SocketAddr>,

    #[arg(long, env = "OPENSHELL_COMPUTE_DRIVER_SOCKET")]
    bind_socket: Option<PathBuf>,

    #[arg(long, hide = true)]
    expected_peer_pid: Option<u32>,

    #[arg(
        long,
        env = "OPENSHELL_COMPUTE_DRIVER_ALLOW_UNAUTHENTICATED_TCP",
        default_value_t = false
    )]
    allow_unauthenticated_tcp: bool,

    #[arg(
        long,
        env = "OPENSHELL_COMPUTE_DRIVER_ALLOW_SAME_UID_PEER",
        default_value_t = false
    )]
    allow_same_uid_peer: bool,

    #[arg(long, env = "OPENSHELL_LOG_LEVEL", default_value = "info")]
    log_level: String,

    #[arg(long, env = "OPENSHELL_GRPC_ENDPOINT")]
    openshell_endpoint: Option<String>,

    #[arg(long, env = "OPENSHELL_SANDBOX_IMAGE", default_value = "")]
    default_image: String,

    #[arg(long, env = "OPENSHELL_VM_BOOTSTRAP_IMAGE", default_value = "")]
    bootstrap_image: String,

    #[arg(
        long,
        env = "OPENSHELL_VM_DRIVER_STATE_DIR",
        default_value = "target/openshell-vm-driver"
    )]
    state_dir: PathBuf,

    #[arg(long = "guest-tls-ca", env = "OPENSHELL_VM_TLS_CA")]
    guest_tls_ca: Option<PathBuf>,

    #[arg(long = "guest-tls-cert", env = "OPENSHELL_VM_TLS_CERT")]
    guest_tls_cert: Option<PathBuf>,

    #[arg(long = "guest-tls-key", env = "OPENSHELL_VM_TLS_KEY")]
    guest_tls_key: Option<PathBuf>,

    #[arg(long, env = "OPENSHELL_VM_KRUN_LOG_LEVEL", default_value_t = 1)]
    krun_log_level: u32,

    #[arg(long, env = "OPENSHELL_VM_DRIVER_VCPUS", default_value_t = 2)]
    vcpus: u8,

    #[arg(long, env = "OPENSHELL_VM_DRIVER_MEM_MIB", default_value_t = 2048)]
    mem_mib: u32,

    #[arg(long, env = "OPENSHELL_VM_OVERLAY_DISK_MIB", default_value_t = 4096)]
    overlay_disk_mib: u64,

    #[arg(long, env = "OPENSHELL_VM_GPU")]
    gpu: bool,

    #[arg(long, env = "OPENSHELL_VM_GPU_MEM_MIB", default_value_t = 8192)]
    gpu_mem_mib: u32,

    #[arg(long, env = "OPENSHELL_VM_GPU_VCPUS", default_value_t = 4)]
    gpu_vcpus: u8,

    #[arg(long, env = "OPENSHELL_VM_SANDBOX_UID")]
    sandbox_uid: Option<u32>,

    #[arg(long, env = "OPENSHELL_VM_SANDBOX_GID")]
    sandbox_gid: Option<u32>,

    #[arg(long, hide = true)]
    vm_backend: Option<String>,

    #[arg(long, hide = true)]
    vm_gpu_bdf: Option<String>,

    #[arg(long, hide = true)]
    vm_tap_device: Option<String>,

    #[arg(long, hide = true)]
    vm_guest_ip: Option<String>,

    #[arg(long, hide = true)]
    vm_host_ip: Option<String>,

    #[arg(long, hide = true)]
    vm_vsock_cid: Option<u32>,

    #[arg(long, hide = true)]
    vm_guest_mac: Option<String>,

    #[arg(long, hide = true)]
    vm_gateway_port: Option<u16>,

    // -- e2fs helper VM args (Windows only, used by --internal-e2fs-helper) --
    #[arg(long, hide = true, default_value_t = false)]
    internal_e2fs_helper: bool,

    #[arg(long = "e2fs-runtime-dir", hide = true)]
    e2fs_runtime_dir: Option<PathBuf>,

    #[arg(long = "e2fs-helper-rootfs", hide = true)]
    e2fs_helper_rootfs: Option<PathBuf>,

    #[arg(long = "e2fs-data-disk", hide = true)]
    e2fs_data_disk: Option<PathBuf>,

    #[arg(long = "e2fs-args", hide = true, allow_hyphen_values = true)]
    e2fs_args: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing::info!("Starting openshell-driver-vm");
    let args = Args::parse();
    if args.internal_run_vm {
        // We intentionally defer procguard arming until `run_vm()` so
        // that the only arm is the one that knows how to clean up
        // gvproxy. Racing two watchers against the same parent-death
        // event causes the bare arm's `exit(1)` to win, skipping the
        // gvproxy cleanup and leaking the helper. The risk window
        // before `run_vm` arms procguard is ~a few syscalls long
        // (`build_vm_launch_config`, `configured_runtime_dir`), which
        // is negligible next to the parent gRPC server's uptime.
        maybe_reexec_internal_vm_with_runtime_env()?;
        let config = build_vm_launch_config(&args).map_err(|err| miette::miette!("{err}"))?;
        // check if process has env variable OPENSHELL_KRUN_WORKER set
        for env in config.env.clone() {
            tracing::info!("env: {:?}", env);
        }
        run_vm(&config).map_err(|err| miette::miette!("{err}"))?;
        return Ok(());
    }

    if args.internal_e2fs_helper {
        run_internal_e2fs_helper(&args).map_err(|err| miette::miette!("{err}"))?;
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level)),
        )
        .init();

    let listen_mode = compute_driver_listen_mode(&args).map_err(|err| miette::miette!("{err}"))?;

    // Arm procguard so that if the gateway is killed (SIGKILL or crash)
    // we also die. Without this the driver is reparented to init and
    // keeps its per-sandbox VM launchers alive forever. Launchers have
    // their own procguards (armed in `run_vm`) which cascade cleanup of
    // gvproxy and the libkrun worker the moment this driver exits.
    if let Err(err) = procguard::die_with_parent() {
        tracing::warn!(
            error = %err,
            "procguard arm failed; gateway crashes may orphan this driver"
        );
    }

    let driver = VmDriver::new(VmDriverConfig {
        openshell_endpoint: args
            .openshell_endpoint
            .ok_or_else(|| miette::miette!("OPENSHELL_GRPC_ENDPOINT is required"))?,
        state_dir: args.state_dir.clone(),
        launcher_bin: None,
        default_image: args.default_image.clone(),
        bootstrap_image: args.bootstrap_image.clone(),
        log_level: args.log_level.clone(),
        krun_log_level: args.krun_log_level,
        vcpus: args.vcpus,
        mem_mib: args.mem_mib,
        overlay_disk_mib: args.overlay_disk_mib,
        guest_tls_ca: args.guest_tls_ca.clone(),
        guest_tls_cert: args.guest_tls_cert.clone(),
        guest_tls_key: args.guest_tls_key.clone(),
        gpu_enabled: args.gpu,
        gpu_mem_mib: args.gpu_mem_mib,
        gpu_vcpus: args.gpu_vcpus,
        sandbox_uid: args.sandbox_uid,
        sandbox_gid: args.sandbox_gid,
    })
    .await
    .map_err(|err| miette::miette!("{err}"))?;

    match listen_mode {
        #[cfg(unix)]
        ComputeDriverListenMode::Unix {
            socket_path,
            expected_peer_pid,
        } => {
            prepare_compute_driver_socket(&socket_path).map_err(|err| miette::miette!("{err}"))?;

            info!(socket = %socket_path.display(), "Starting vm compute driver");
            let listener = UnixListener::bind(&socket_path).into_diagnostic()?;
            restrict_socket_permissions(&socket_path).map_err(|err| miette::miette!("{err}"))?;
            let result = tonic::transport::Server::builder()
                .add_service(ComputeDriverServer::new(driver))
                .serve_with_incoming(AuthenticatedUnixIncoming::new(listener, expected_peer_pid))
                .await
                .into_diagnostic();
            let _ = std::fs::remove_file(&socket_path);
            result
        }
        #[cfg(windows)]
        ComputeDriverListenMode::NamedPipe {
            pipe_name,
            expected_peer_pid,
        } => {
            info!(pipe = %pipe_name, "Starting vm compute driver");
            let incoming = AuthenticatedNamedPipeIncoming::new(pipe_name, expected_peer_pid);
            tonic::transport::Server::builder()
                .add_service(ComputeDriverServer::new(driver))
                .serve_with_incoming(incoming)
                .await
                .into_diagnostic()
        }
        ComputeDriverListenMode::Tcp(bind_address) => {
            info!(address = %bind_address, "Starting unauthenticated dev vm compute driver");
            tonic::transport::Server::builder()
                .add_service(ComputeDriverServer::new(driver))
                .serve(bind_address)
                .await
                .into_diagnostic()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComputeDriverListenMode {
    #[cfg(unix)]
    Unix {
        socket_path: PathBuf,
        expected_peer_pid: Option<u32>,
    },
    #[cfg(windows)]
    NamedPipe {
        pipe_name: String,
        expected_peer_pid: Option<u32>,
    },
    Tcp(SocketAddr),
}

fn compute_driver_listen_mode(args: &Args) -> std::result::Result<ComputeDriverListenMode, String> {
    if let Some(socket_path) = args.bind_socket.clone() {
        #[cfg(unix)]
        {
            if args.expected_peer_pid.is_none() && !args.allow_same_uid_peer {
                return Err(
                    "--expected-peer-pid is required with --bind-socket; use --allow-same-uid-peer only for local development"
                        .to_string(),
                );
            }
            return Ok(ComputeDriverListenMode::Unix {
                socket_path,
                expected_peer_pid: args.expected_peer_pid,
            });
        }
        #[cfg(windows)]
        {
            // On Windows `--bind-socket` carries a named pipe address
            // (`\\.\pipe\...`, see `compute_driver_socket_path` in the
            // gateway) rather than a filesystem path. `--allow-same-uid-peer`
            // has no literal uid equivalent here; it's reused to mean
            // "skip the peer-pid check", matching the Unix dev escape hatch.
            let pipe_name = socket_path
                .to_str()
                .ok_or_else(|| "--bind-socket must be valid UTF-8 on Windows".to_string())?
                .to_string();
            if args.expected_peer_pid.is_none() && !args.allow_same_uid_peer {
                return Err(
                    "--expected-peer-pid is required with --bind-socket; use --allow-same-uid-peer only for local development"
                        .to_string(),
                );
            }
            return Ok(ComputeDriverListenMode::NamedPipe {
                pipe_name,
                expected_peer_pid: args.expected_peer_pid,
            });
        }
        #[cfg(not(any(unix, windows)))]
        {
            return Err("--bind-socket is not supported on this platform".to_string());
        }
    }

    if !args.allow_unauthenticated_tcp {
        return Err(
            "--bind-socket is required; unauthenticated TCP mode is disabled unless --allow-unauthenticated-tcp is set for local development"
                .to_string(),
        );
    }

    let Some(bind_address) = args.bind_address else {
        return Err("--bind-address is required with --allow-unauthenticated-tcp".to_string());
    };

    Ok(ComputeDriverListenMode::Tcp(bind_address))
}

#[cfg(unix)]
fn prepare_compute_driver_socket(socket_path: &Path) -> std::result::Result<(), String> {
    let Some(parent) = socket_path.parent() else {
        return Err(format!(
            "vm compute driver socket path '{}' has no parent directory",
            socket_path.display()
        ));
    };
    let expected_uid = current_euid();
    prepare_private_socket_dir(parent, expected_uid)?;
    remove_stale_socket(socket_path, expected_uid)
}

#[cfg(unix)]
fn current_euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[cfg(unix)]
fn prepare_private_socket_dir(
    socket_dir: &Path,
    expected_uid: u32,
) -> std::result::Result<(), String> {
    std::fs::create_dir_all(socket_dir)
        .map_err(|err| format!("create socket dir {}: {err}", socket_dir.display()))?;
    let metadata = std::fs::symlink_metadata(socket_dir)
        .map_err(|err| format!("stat socket dir {}: {err}", socket_dir.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "socket dir {} is a symlink; refusing to use it",
            socket_dir.display()
        ));
    }
    if !file_type.is_dir() {
        return Err(format!(
            "socket dir {} is not a directory",
            socket_dir.display()
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(format!(
            "socket dir {} is owned by uid {} but current euid is {}",
            socket_dir.display(),
            metadata.uid(),
            expected_uid
        ));
    }
    std::fs::set_permissions(socket_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("chmod socket dir {}: {err}", socket_dir.display()))
}

#[cfg(unix)]
fn remove_stale_socket(socket_path: &Path, expected_uid: u32) -> std::result::Result<(), String> {
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("stat socket {}: {err}", socket_path.display())),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "socket {} is a symlink; refusing to remove it",
            socket_path.display()
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(format!(
            "socket {} is owned by uid {} but current euid is {}",
            socket_path.display(),
            metadata.uid(),
            expected_uid
        ));
    }
    if !file_type.is_socket() {
        return Err(format!(
            "socket path {} exists but is not a Unix socket",
            socket_path.display()
        ));
    }
    std::fs::remove_file(socket_path)
        .map_err(|err| format!("remove stale socket {}: {err}", socket_path.display()))
}

#[cfg(unix)]
fn restrict_socket_permissions(socket_path: &Path) -> std::result::Result<(), String> {
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("chmod socket {}: {err}", socket_path.display()))
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerCredentials {
    uid: u32,
    pid: Option<i32>,
}

#[cfg(unix)]
fn peer_credentials(stream: &UnixStream) -> std::result::Result<PeerCredentials, String> {
    let credentials = stream
        .peer_cred()
        .map_err(|err| format!("read peer credentials: {err}"))?;
    Ok(PeerCredentials {
        uid: credentials.uid(),
        pid: credentials.pid(),
    })
}

#[cfg(unix)]
fn authorize_peer_credentials(
    peer: PeerCredentials,
    driver_uid: u32,
    gateway_pid: Option<u32>,
) -> std::result::Result<(), String> {
    if peer.uid != driver_uid {
        return Err(format!(
            "peer uid {} does not match current euid {}",
            peer.uid, driver_uid
        ));
    }
    let Some(gateway_pid) = gateway_pid else {
        return Ok(());
    };
    let Some(peer_process_id) = peer.pid.and_then(|pid| u32::try_from(pid).ok()) else {
        return Err(format!(
            "peer pid is unavailable; expected gateway pid {gateway_pid}"
        ));
    };
    if peer_process_id != gateway_pid {
        return Err(format!(
            "peer pid {peer_process_id} does not match expected gateway pid {gateway_pid}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
struct AuthenticatedUnixIncoming {
    listener: UnixListener,
    expected_uid: u32,
    expected_peer_pid: Option<u32>,
}

#[cfg(unix)]
impl AuthenticatedUnixIncoming {
    fn new(listener: UnixListener, expected_peer_pid: Option<u32>) -> Self {
        Self {
            listener,
            expected_uid: current_euid(),
            expected_peer_pid,
        }
    }
}

#[cfg(unix)]
impl Stream for AuthenticatedUnixIncoming {
    type Item = io::Result<UnixStream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _addr))) => {
                    let authorized = peer_credentials(&stream).and_then(|peer| {
                        authorize_peer_credentials(peer, this.expected_uid, this.expected_peer_pid)
                    });
                    match authorized {
                        Ok(()) => return Poll::Ready(Some(Ok(stream))),
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "rejected vm compute driver UDS client"
                            );
                        }
                    }
                }
                Poll::Ready(Err(err)) => return Poll::Ready(Some(Err(err))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Authorize a named pipe peer by process id — the closest Windows
/// equivalent of the Unix path's uid+pid check (there is no uid analog for
/// this purely host-local IPC channel, so pid is the whole story here).
#[cfg(windows)]
fn authorize_named_pipe_peer(
    peer_pid: u32,
    expected_peer_pid: Option<u32>,
) -> std::result::Result<(), String> {
    let Some(expected_peer_pid) = expected_peer_pid else {
        return Ok(());
    };
    if peer_pid != expected_peer_pid {
        return Err(format!(
            "peer pid {peer_pid} does not match expected gateway pid {expected_peer_pid}"
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn named_pipe_peer_pid(server: &NamedPipeServer) -> std::result::Result<u32, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    let handle = server.as_raw_handle() as isize as HANDLE;
    let mut pid: u32 = 0;
    // SAFETY: `handle` is a valid, open named pipe server handle owned by
    // `server` for the duration of this call; `pid` is a valid out-param.
    let ok = unsafe { GetNamedPipeClientProcessId(handle, &raw mut pid) };
    if ok == 0 {
        return Err(format!(
            "GetNamedPipeClientProcessId failed: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(pid)
}

/// Create a fresh named pipe instance and wait for a client to connect to
/// it. `first` must be `true` only for the very first instance of
/// `pipe_name` — this makes pipe creation fail immediately (instead of on
/// first connection) if another process is already listening on the same
/// name, mirroring how the Unix path refuses to bind over a live socket.
#[cfg(windows)]
async fn accept_named_pipe(pipe_name: String, first: bool) -> io::Result<NamedPipeServer> {
    let server = ServerOptions::new()
        .first_pipe_instance(first)
        .create(&pipe_name)?;
    server.connect().await?;
    Ok(server)
}

/// A `Stream` of authenticated named pipe connections, mirroring
/// `AuthenticatedUnixIncoming` for Windows. Named pipes require a fresh
/// server instance per connection (unlike a Unix `accept()` loop on one
/// listener fd), so this keeps a pending "accept next client" future
/// around and re-arms it immediately after each connection — including
/// rejected ones — so a subsequent client is never left unable to connect.
#[cfg(windows)]
struct AuthenticatedNamedPipeIncoming {
    pipe_name: String,
    expected_peer_pid: Option<u32>,
    accept: Pin<Box<dyn Future<Output = io::Result<NamedPipeServer>> + Send>>,
}

#[cfg(windows)]
impl AuthenticatedNamedPipeIncoming {
    fn new(pipe_name: String, expected_peer_pid: Option<u32>) -> Self {
        Self {
            accept: Box::pin(accept_named_pipe(pipe_name.clone(), true)),
            pipe_name,
            expected_peer_pid,
        }
    }
}

#[cfg(windows)]
impl Stream for AuthenticatedNamedPipeIncoming {
    type Item = io::Result<ConnectedNamedPipe>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.accept.as_mut().poll(cx) {
                Poll::Ready(Ok(server)) => {
                    // Immediately queue the next instance so a subsequent
                    // client (e.g. a gRPC channel reconnect) can connect
                    // while this one is handed off to tonic.
                    this.accept = Box::pin(accept_named_pipe(this.pipe_name.clone(), false));

                    let authorized = named_pipe_peer_pid(&server)
                        .and_then(|peer_pid| authorize_named_pipe_peer(peer_pid, this.expected_peer_pid));
                    match authorized {
                        Ok(()) => return Poll::Ready(Some(Ok(ConnectedNamedPipe(server)))),
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "rejected vm compute driver named pipe client"
                            );
                        }
                    }
                }
                Poll::Ready(Err(err)) => {
                    this.accept = Box::pin(accept_named_pipe(this.pipe_name.clone(), false));
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct ConnectedNamedPipe(NamedPipeServer);

#[cfg(windows)]
impl tonic::transport::server::Connected for ConnectedNamedPipe {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

#[cfg(windows)]
impl tokio::io::AsyncRead for ConnectedNamedPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl tokio::io::AsyncWrite for ConnectedNamedPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

fn run_internal_e2fs_helper(args: &Args) -> std::result::Result<(), String> {
    let runtime_dir = args
        .e2fs_runtime_dir
        .as_ref()
        .ok_or_else(|| "--e2fs-runtime-dir is required in e2fs helper mode".to_string())?;
    let helper_rootfs = args
        .e2fs_helper_rootfs
        .as_ref()
        .ok_or_else(|| "--e2fs-helper-rootfs is required in e2fs helper mode".to_string())?;
    let data_disk = args
        .e2fs_data_disk
        .as_ref()
        .ok_or_else(|| "--e2fs-data-disk is required in e2fs helper mode".to_string())?;

    let mut arg_values = Vec::new();
    for arg in &args.e2fs_args {
        arg_values.push(std::ffi::CString::new(arg.as_str()).unwrap());
    }

    #[cfg(target_os = "windows")]
    {
        openshell_driver_vm::e2fs_vm::exec_vm(runtime_dir, data_disk, helper_rootfs, arg_values)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (runtime_dir, helper_rootfs, data_disk, console_output, exec_command, virtiofs_shares);
        Err("--internal-e2fs-helper is only supported on Windows".to_string())
    }
}

fn build_vm_launch_config(args: &Args) -> std::result::Result<VmLaunchConfig, String> {
    let root_disk = args
        .vm_root_disk
        .clone()
        .ok_or_else(|| "--vm-root-disk is required in internal VM mode".to_string())?;
    let overlay_disk = args
        .vm_overlay_disk
        .clone()
        .ok_or_else(|| "--vm-overlay-disk is required in internal VM mode".to_string())?;
    let image_disk = args.vm_image_disk.clone();
    let exec_path = args
        .vm_exec
        .clone()
        .ok_or_else(|| "--vm-exec is required in internal VM mode".to_string())?;
    let console_output = args
        .vm_console_output
        .clone()
        .ok_or_else(|| "--vm-console-output is required in internal VM mode".to_string())?;

    let backend = match args.vm_backend.as_deref() {
        Some("qemu") => VmBackend::Qemu,
        Some("libkrun") | None => VmBackend::Libkrun,
        Some(other) => return Err(format!("unknown VM backend: {other}")),
    };

    Ok(VmLaunchConfig {
        root_disk,
        overlay_disk,
        image_disk,
        kernel_image: args.vm_kernel_image.clone(),
        vcpus: args.vm_vcpus,
        mem_mib: args.vm_mem_mib,
        exec_path,
        args: Vec::new(),
        env: args.vm_env.clone(),
        workdir: args.vm_workdir.clone(),
        log_level: args.vm_krun_log_level,
        console_output,
        backend,
        gpu_bdf: args.vm_gpu_bdf.clone(),
        tap_device: args.vm_tap_device.clone(),
        guest_ip: args.vm_guest_ip.clone(),
        host_ip: args.vm_host_ip.clone(),
        vsock_cid: args.vm_vsock_cid,
        guest_mac: args.vm_guest_mac.clone(),
        gateway_port: args.vm_gateway_port,
    })
}

#[cfg(target_os = "macos")]
fn maybe_reexec_internal_vm_with_runtime_env() -> Result<()> {
    use std::os::unix::process::CommandExt as _;

    const REEXEC_ENV: &str = "__OPENSHELL_DRIVER_VM_REEXEC";

    if std::env::var_os(REEXEC_ENV).is_some() {
        return Ok(());
    }

    let runtime_dir = configured_runtime_dir().map_err(|err| miette::miette!("{err}"))?;
    let runtime_str = runtime_dir.to_string_lossy();
    let needs_reexec = std::env::var_os("DYLD_LIBRARY_PATH")
        .is_none_or(|value| !value.to_string_lossy().contains(runtime_str.as_ref()));
    if !needs_reexec {
        return Ok(());
    }

    let mut dyld_paths = vec![runtime_dir.clone()];
    if let Some(existing) = std::env::var_os("DYLD_LIBRARY_PATH") {
        dyld_paths.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(&dyld_paths)
        .map_err(|err| miette::miette!("join DYLD_LIBRARY_PATH: {err}"))?;
    let exe = std::env::current_exe().into_diagnostic()?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Use execvp() so the current process is *replaced* by the re-exec'd
    // binary — no wrapper process sits between the compute driver and
    // the actually-running VM launcher. That avoids two problems:
    //   1. An extra process level that survives SIGKILL of the driver
    //      (the wrapper was reparenting the re-exec'd child to init).
    //   2. Signal forwarding: with a wrapper, a SIGTERM to the wrapper
    //      doesn't reach the child unless we hand-roll forwarding.
    // After exec, the child inherits our PID and our procguard arming.
    let err = std::process::Command::new(exe)
        .args(&args)
        .env("DYLD_LIBRARY_PATH", &joined)
        .env(VM_RUNTIME_DIR_ENV, runtime_dir)
        .env(REEXEC_ENV, "1")
        .exec();
    // `exec()` only returns on failure.
    Err(miette::miette!("failed to re-exec with runtime env: {err}"))
}

#[cfg(not(target_os = "macos"))]
// Signature must match the macOS variant which can fail.
#[allow(clippy::unnecessary_wraps)]
fn maybe_reexec_internal_vm_with_runtime_env() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Args, ComputeDriverListenMode, PeerCredentials, authorize_peer_credentials,
        compute_driver_listen_mode,
    };
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn peer_authorization_accepts_matching_uid_and_pid() {
        authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: Some(42),
            },
            1000,
            Some(42),
        )
        .unwrap();
    }

    #[test]
    fn peer_authorization_rejects_wrong_pid() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: Some(7),
            },
            1000,
            Some(42),
        )
        .expect_err("wrong pid should be rejected");
        assert!(err.contains("does not match expected gateway pid"));
    }

    #[test]
    fn peer_authorization_rejects_wrong_uid() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1001,
                pid: Some(42),
            },
            1000,
            Some(42),
        )
        .expect_err("wrong uid should be rejected");
        assert!(err.contains("does not match current euid"));
    }

    #[test]
    fn peer_authorization_rejects_missing_pid_when_expected() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: None,
            },
            1000,
            Some(42),
        )
        .expect_err("missing pid should be rejected");
        assert!(err.contains("peer pid is unavailable"));
    }

    #[test]
    fn peer_authorization_accepts_matching_uid_without_expected_pid() {
        authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: None,
            },
            1000,
            None,
        )
        .unwrap();
    }

    #[test]
    fn listen_mode_rejects_default_tcp() {
        let args = Args::parse_from(["openshell-driver-vm"]);
        let err = compute_driver_listen_mode(&args).expect_err("default TCP should be disabled");
        assert!(err.contains("--bind-socket is required"));
    }

    #[test]
    fn listen_mode_rejects_bind_address_without_tcp_opt_in() {
        let args = Args::parse_from(["openshell-driver-vm", "--bind-address", "127.0.0.1:50061"]);
        let err =
            compute_driver_listen_mode(&args).expect_err("TCP bind should require explicit opt-in");
        assert!(err.contains("--allow-unauthenticated-tcp"));
    }

    #[test]
    fn listen_mode_requires_bind_address_with_tcp_opt_in() {
        let args = Args::parse_from(["openshell-driver-vm", "--allow-unauthenticated-tcp"]);
        let err =
            compute_driver_listen_mode(&args).expect_err("TCP opt-in should require an address");
        assert!(err.contains("--bind-address is required"));
    }

    #[test]
    fn listen_mode_accepts_explicit_unauthenticated_tcp() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--allow-unauthenticated-tcp",
            "--bind-address",
            "127.0.0.1:50061",
        ]);
        assert_eq!(
            compute_driver_listen_mode(&args).unwrap(),
            ComputeDriverListenMode::Tcp("127.0.0.1:50061".parse().unwrap())
        );
    }

    #[test]
    fn listen_mode_requires_expected_peer_pid_for_uds() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--bind-socket",
            "/tmp/compute-driver.sock",
        ]);
        let err = compute_driver_listen_mode(&args)
            .expect_err("UDS should require gateway peer pid by default");
        assert!(err.contains("--expected-peer-pid is required"));
    }

    #[test]
    fn listen_mode_accepts_uds_with_expected_peer_pid() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--bind-socket",
            "/tmp/compute-driver.sock",
            "--expected-peer-pid",
            "42",
        ]);
        assert_eq!(
            compute_driver_listen_mode(&args).unwrap(),
            ComputeDriverListenMode::Unix {
                socket_path: PathBuf::from("/tmp/compute-driver.sock"),
                expected_peer_pid: Some(42),
            }
        );
    }

    #[test]
    fn listen_mode_accepts_explicit_same_uid_uds_dev_mode() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--bind-socket",
            "/tmp/compute-driver.sock",
            "--allow-same-uid-peer",
        ]);
        assert_eq!(
            compute_driver_listen_mode(&args).unwrap(),
            ComputeDriverListenMode::Unix {
                socket_path: PathBuf::from("/tmp/compute-driver.sock"),
                expected_peer_pid: None,
            }
        );
    }
}
