// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Async `WinSock` `AF_UNIX` sockets for tokio on Windows.
//!
//! Neither tokio nor std compiles `AF_UNIX` support on Windows, but libkrun's
//! Windows port proxies guest vsock connections to host `WinSock` `AF_UNIX`
//! sockets. The supervisor must therefore listen on those sockets: socket2
//! encodes the `sun_path`, the sockets themselves come from plain `WinSock`
//! calls, and each socket is bridged onto tokio through dedicated pump
//! threads and channels, because tokio's IOCP driver cannot register these
//! handles.
//!
//! Every stream costs two OS threads (one per direction) and every listener
//! one, which suits the handful of control and relay connections a sandbox
//! uses but not high connection counts.

#![allow(unsafe_code)]

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker, ready};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, FIONBIO, INVALID_SOCKET, POLLRDNORM, POLLWRNORM, SD_SEND, SOCK_STREAM, SOCKADDR,
    SOCKET, SOCKET_ERROR, WSADATA, WSAEWOULDBLOCK, WSAGetLastError, WSAPOLL_EVENT_FLAGS, WSAPOLLFD,
    WSAPoll, WSAStartup, accept, bind, closesocket, connect as wsa_connect, ioctlsocket, listen,
    recv, send, shutdown, socket,
};

/// Chunks handed between the socket pumps and the async side.
const CHUNK_SIZE: usize = 16 * 1024;
/// `CHUNK_SIZE` as the length type of `recv`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
const CHUNK_LEN: i32 = CHUNK_SIZE as i32;
/// Buffered chunks per direction before `poll_write` parks.
const CHANNEL_CAPACITY: usize = 4;
/// How long a pump waits in `WSAPoll` before re-checking whether its owner
/// closed. Readiness wakes a pump immediately; this only bounds how long the
/// threads of a dropped socket linger.
const POLL_INTERVAL_MS: i32 = 100;
/// How long the write pump of a dropped stream keeps trying to deliver data
/// that was already queued when the peer stops reading.
const DROP_LINGER: Duration = Duration::from_secs(5);

/// `WinSock` functions report failures through `WSAGetLastError`; reading the
/// thread's last error instead can surface a stale code from an unrelated
/// earlier failure and mask the real one.
fn wsa_last_error() -> io::Error {
    // SAFETY: Plain error-code query with no preconditions.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn would_block(error: &io::Error) -> bool {
    error.raw_os_error() == Some(WSAEWOULDBLOCK)
}

/// `WSAEPROVIDERFAILEDINIT` often means a provider DLL could not be loaded,
/// which commit exhaustion causes; record the memory state alongside the
/// failure so the log distinguishes that from catalog corruption.
fn log_windows_memory() {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: `MEMORYSTATUSEX` is a plain in/out struct; `dwLength` is set
    // before the call as the API requires.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = u32::try_from(size_of::<MEMORYSTATUSEX>()).unwrap_or(u32::MAX);
    // SAFETY: The struct pointer is valid for the documented in/out length.
    if unsafe { GlobalMemoryStatusEx(&raw mut status) } != 0 {
        tracing::error!(
            memory_load_percent = status.dwMemoryLoad,
            available_physical_mib = status.ullAvailPhys / (1024 * 1024),
            available_commit_mib = status.ullAvailPageFile / (1024 * 1024),
            total_commit_mib = status.ullTotalPageFile / (1024 * 1024),
            "winsock-unix: system memory state at failure"
        );
    }
}

/// `WinSock` must be started before any socket call; libkrun does the same
/// in the processes it runs in. Ref-counted by Windows, so calling it again
/// next to libstd's own lazy initialization (or racing another caller) is
/// harmless. Only success is remembered: a failure such as transient commit
/// exhaustion is retried on the next call.
fn ensure_wsa_startup() -> io::Result<()> {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.load(Ordering::Acquire) {
        return Ok(());
    }
    // SAFETY: `WSADATA` is a plain out-struct and 0x0202 requests
    // WinSock 2.2, the version every modern provider implements.
    let mut data: WSADATA = unsafe { std::mem::zeroed() };
    // `WSAStartup` returns its error code directly; `WSAGetLastError` is
    // not valid after it.
    match unsafe { WSAStartup(0x0202, &raw mut data) } {
        0 => {
            STARTED.store(true, Ordering::Release);
            Ok(())
        }
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

/// A socket handle shared by its owner and its pump threads.
///
/// The Windows `AF_UNIX` provider does not wake a blocked `recv` or `send` on
/// `shutdown`, and waking it with `closesocket` would let the pump's next
/// call land on a recycled handle value owned by an unrelated socket.
/// Instead the socket is non-blocking, pumps wait in `WSAPoll` with a timeout
/// and watch `closing`, and the handle is closed only when the last
/// reference drops, after every pump has exited.
struct SharedSocket {
    handle: SOCKET,
    closing: AtomicBool,
}

impl SharedSocket {
    /// Take ownership of `handle` and switch it to non-blocking mode. The
    /// handle is closed on error.
    fn new(handle: SOCKET) -> io::Result<Arc<Self>> {
        let shared = Arc::new(Self {
            handle,
            closing: AtomicBool::new(false),
        });
        let mut non_blocking: u32 = 1;
        // SAFETY: The handle is valid and `FIONBIO` takes a `u32` argument.
        if unsafe { ioctlsocket(handle, FIONBIO, &raw mut non_blocking) } == SOCKET_ERROR {
            return Err(wsa_last_error());
        }
        Ok(shared)
    }

    /// Ask the pumps to exit; the handle closes once the last one has.
    fn close(&self) {
        self.closing.store(true, Ordering::Release);
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Wait until `events` (or a hang-up or error) is signaled. Returns
    /// `Ok(false)` once `stop` reports that the pump should give up.
    fn wait(
        &self,
        events: WSAPOLL_EVENT_FLAGS,
        mut stop: impl FnMut() -> bool,
    ) -> io::Result<bool> {
        loop {
            if stop() {
                return Ok(false);
            }
            let mut poll_fd = WSAPOLLFD {
                fd: self.handle,
                events,
                revents: 0,
            };
            // SAFETY: One valid `WSAPOLLFD` for the duration of the call.
            match unsafe { WSAPoll(&raw mut poll_fd, 1, POLL_INTERVAL_MS) } {
                SOCKET_ERROR => return Err(wsa_last_error()),
                0 => {}
                // Readiness, hang-up and error all make the next call return.
                _ => return Ok(true),
            }
        }
    }
}

impl Drop for SharedSocket {
    fn drop(&mut self) {
        // SAFETY: The last reference is gone, so no pump uses the handle and
        // it is closed exactly once.
        unsafe { closesocket(self.handle) };
    }
}

fn spawn_pump(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map(drop)
}

/// `WinSock` `AF_UNIX` listener accepted asynchronously.
///
/// A pump thread accepts connections into a channel, so `accept` is
/// cancel-safe: a connection accepted after the caller stopped waiting is
/// handed to the next `accept` call instead of being dropped.
pub struct UnixListener {
    socket: Arc<SharedSocket>,
    incoming: tokio::sync::Mutex<mpsc::Receiver<io::Result<UnixStream>>>,
    path: PathBuf,
}

impl std::fmt::Debug for UnixListener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixListener")
            .field("socket", &self.socket.handle)
            .field("path", &self.path)
            .finish()
    }
}

impl UnixListener {
    /// Bind and listen on `path`. socket2 only encodes the UTF-8 `sun_path`;
    /// the socket itself is created with plain `socket(AF_UNIX, ...)`, the
    /// way libkrun's `WinSock` `AF_UNIX` support does. socket2's own creation
    /// path goes through `WSASocketW(.., WSA_FLAG_OVERLAPPED)`, which the
    /// Windows `AF_UNIX` provider rejects with `WSAEPROVIDERFAILEDINIT`.
    pub fn bind(path: &Path) -> io::Result<Self> {
        if let Err(error) = ensure_wsa_startup() {
            tracing::error!(%error, "winsock-unix: WSAStartup failed");
            return Err(error);
        }
        let address = socket2::SockAddr::unix(path)?;
        // SAFETY: Plain creation call; `SharedSocket` owns the handle from
        // here on and closes it on every error path below.
        let handle = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        if handle == INVALID_SOCKET {
            let error = wsa_last_error();
            tracing::error!(
                error = %error,
                path = %path.display(),
                "winsock-unix: socket(AF_UNIX) failed"
            );
            log_windows_memory();
            return Err(error);
        }
        let socket = SharedSocket::new(handle)?;
        // SAFETY: The address points at the encoded `sun_path` for the
        // length reported by socket2.
        if unsafe {
            bind(
                socket.handle,
                address.as_ptr().cast::<SOCKADDR>(),
                address.len(),
            )
        } == SOCKET_ERROR
        {
            let error = wsa_last_error();
            tracing::error!(
                error = %error,
                path = %path.display(),
                "winsock-unix: bind failed"
            );
            return Err(error);
        }
        // SAFETY: The handle is valid and the backlog is a plain integer.
        if unsafe { listen(socket.handle, 64) } == SOCKET_ERROR {
            let error = wsa_last_error();
            tracing::error!(
                error = %error,
                path = %path.display(),
                "winsock-unix: listen failed"
            );
            return Err(error);
        }
        let (incoming_tx, incoming) = mpsc::channel(1);
        spawn_accept_pump(Arc::clone(&socket), incoming_tx)?;
        tracing::debug!(path = %path.display(), "winsock-unix: listener bound");
        Ok(Self {
            socket,
            incoming: tokio::sync::Mutex::new(incoming),
            path: path.to_path_buf(),
        })
    }

    /// Accept one connection.
    pub async fn accept(&self) -> io::Result<UnixStream> {
        self.incoming.lock().await.recv().await.unwrap_or_else(|| {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "winsock unix accept pump has exited",
            ))
        })
    }
}

impl Drop for UnixListener {
    fn drop(&mut self) {
        // The accept pump exits within one poll interval and the handle
        // closes with it; dropping `incoming` also fails a pending handoff.
        self.socket.close();
        // WinSock leaves the socket file behind like Unix does. Removing it
        // here rather than when the handle closes keeps a quick re-bind of
        // the same path from losing its new file to the old listener.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn spawn_accept_pump(
    listener: Arc<SharedSocket>,
    incoming: mpsc::Sender<io::Result<UnixStream>>,
) -> io::Result<()> {
    spawn_pump("winsock-unix-accept", move || {
        loop {
            let result = match listener.wait(POLLRDNORM, || listener.is_closing()) {
                Ok(true) => {
                    // SAFETY: The handle stays open while this pump holds a
                    // reference; the output pointers are null because the
                    // peer address of an AF_UNIX stream carries no useful
                    // data.
                    let accepted = unsafe {
                        accept(listener.handle, std::ptr::null_mut(), std::ptr::null_mut())
                    };
                    if accepted == INVALID_SOCKET {
                        let error = wsa_last_error();
                        if would_block(&error) {
                            continue;
                        }
                        Err(error)
                    } else {
                        UnixStream::new(accepted)
                    }
                }
                Ok(false) => return,
                Err(error) => Err(error),
            };
            // Blocks until `accept` takes it, like a backlog entry; fails
            // once the listener drops.
            if incoming.blocking_send(result).is_err() {
                return;
            }
        }
    })
}

/// Dial the `AF_UNIX` socket at `path` and bridge it onto tokio. The blocking
/// `connect` runs on the blocking pool; local `AF_UNIX` connects return
/// immediately.
pub async fn connect(path: &Path) -> io::Result<UnixStream> {
    ensure_wsa_startup()?;
    let address = socket2::SockAddr::unix(path)?;
    tokio::task::spawn_blocking(move || {
        // SAFETY: Plain creation call; the handle is closed on error and
        // owned by the returned stream otherwise.
        let handle = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        if handle == INVALID_SOCKET {
            return Err(wsa_last_error());
        }
        // SAFETY: The address points at the encoded `sun_path` for the
        // length reported by socket2.
        if unsafe { wsa_connect(handle, address.as_ptr().cast::<SOCKADDR>(), address.len()) }
            == SOCKET_ERROR
        {
            let error = wsa_last_error();
            // SAFETY: The handle was created above and is closed exactly once.
            unsafe { closesocket(handle) };
            return Err(error);
        }
        UnixStream::new(handle)
    })
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))?
}

/// Messages queued to the write pump.
enum ToWriter {
    Data(Vec<u8>),
    /// Answered once all data queued ahead of it is sent.
    Flush(oneshot::Sender<()>),
    /// Send the queued data, close the socket's send side, then answer.
    Shutdown(oneshot::Sender<()>),
}

/// State the write pump shares with the async side.
#[derive(Default)]
struct WriterState {
    /// Waker parked in `poll_write` on a full channel.
    waker: Mutex<Option<Waker>>,
    /// The failure that stopped the pump, reported to later writes.
    error: Mutex<Option<io::Error>>,
}

impl WriterState {
    fn wake(&self) {
        let waker = self
            .waker
            .lock()
            .expect("write waker lock not poisoned")
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The error for operations that find the pump gone. `io::Error` is not
    /// `Clone`, so the recorded failure is rebuilt.
    fn exit_error(&self) -> io::Error {
        let recorded = self.error.lock().expect("write error lock not poisoned");
        let Some(error) = recorded.as_ref() else {
            return io::Error::new(
                io::ErrorKind::BrokenPipe,
                "winsock unix write pump has exited",
            );
        };
        error.raw_os_error().map_or_else(
            || io::Error::new(error.kind(), error.to_string()),
            io::Error::from_raw_os_error,
        )
    }
}

/// A connected `WinSock` `AF_UNIX` socket bridged onto tokio.
///
/// Two pump threads move bytes between the socket and channels; the async
/// methods only move data between memory buffers and register wakers.
/// `poll_flush` and `poll_shutdown` complete once the pump has handed the
/// queued data to the socket. Dropping the stream stops reading and lets the
/// write pump deliver what was already queued, giving up after
/// [`DROP_LINGER`] if the peer stops reading.
pub struct UnixStream {
    socket: Arc<SharedSocket>,
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    rx_buf: Vec<u8>,
    rx_pos: usize,
    tx: Option<mpsc::Sender<ToWriter>>,
    writer: Arc<WriterState>,
    /// Data was queued since the last flush request.
    unflushed: bool,
    flush_reply: Option<oneshot::Receiver<()>>,
    shutdown_reply: Option<oneshot::Receiver<()>>,
}

impl UnixStream {
    /// Bridge an already-connected socket, taking ownership of `handle`.
    fn new(handle: SOCKET) -> io::Result<Self> {
        let socket = SharedSocket::new(handle)?;
        let (data_tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (tx, writer_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let writer = Arc::new(WriterState::default());
        spawn_read_pump(Arc::clone(&socket), data_tx)?;
        if let Err(error) = spawn_write_pump(Arc::clone(&socket), writer_rx, Arc::clone(&writer)) {
            socket.close();
            return Err(error);
        }
        Ok(Self {
            socket,
            rx,
            rx_buf: Vec::new(),
            rx_pos: 0,
            tx: Some(tx),
            writer,
            unflushed: false,
            flush_reply: None,
            shutdown_reply: None,
        })
    }

    /// Queue `message` for the write pump, parking on a full channel.
    fn poll_queue(&self, cx: &Context<'_>, message: ToWriter) -> Poll<io::Result<()>> {
        let Some(tx) = self.tx.as_ref() else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "winsock unix stream is already shut down",
            )));
        };
        let message = match tx.try_send(message) {
            Ok(()) => return Poll::Ready(Ok(())),
            Err(mpsc::error::TrySendError::Full(message)) => message,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Poll::Ready(Err(self.writer.exit_error()));
            }
        };
        *self
            .writer
            .waker
            .lock()
            .expect("write waker lock not poisoned") = Some(cx.waker().clone());
        // Retry after parking: if the pump drained the channel between the
        // failed send and parking, it found no waker to wake.
        match tx.try_send(message) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(self.writer.exit_error())),
        }
    }
}

/// Wait for the write pump's answer to a flush or shutdown request; a
/// dropped answer means the pump stopped on an error.
fn poll_reply(
    reply: &mut Option<oneshot::Receiver<()>>,
    writer: &WriterState,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    let Some(pending) = reply.as_mut() else {
        return Poll::Ready(Ok(()));
    };
    let result = ready!(Pin::new(pending).poll(cx));
    *reply = None;
    Poll::Ready(result.map_err(|_| writer.exit_error()))
}

impl AsyncRead for UnixStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.rx_pos == this.rx_buf.len() {
            match ready!(this.rx.poll_recv(cx)) {
                Some(Ok(chunk)) => {
                    this.rx_buf = chunk;
                    this.rx_pos = 0;
                }
                Some(Err(error)) => return Poll::Ready(Err(error)),
                // The read pump exited after EOF or a reported error.
                None => return Poll::Ready(Ok(())),
            }
        }
        let end = this.rx_buf.len();
        let count = (end - this.rx_pos).min(buf.remaining());
        buf.put_slice(&this.rx_buf[this.rx_pos..this.rx_pos + count]);
        this.rx_pos += count;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for UnixStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Bounding each message keeps the queued memory at
        // `CHANNEL_CAPACITY * CHUNK_SIZE` whatever the caller passes.
        let len = buf.len().min(CHUNK_SIZE);
        ready!(this.poll_queue(cx, ToWriter::Data(buf[..len].to_vec())))?;
        this.unflushed = true;
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.flush_reply.is_some() {
                return poll_reply(&mut this.flush_reply, &this.writer, cx);
            }
            if !this.unflushed || this.tx.is_none() {
                return Poll::Ready(Ok(()));
            }
            let (reply, pending) = oneshot::channel();
            ready!(this.poll_queue(cx, ToWriter::Flush(reply)))?;
            this.unflushed = false;
            this.flush_reply = Some(pending);
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.shutdown_reply.is_some() {
                return poll_reply(&mut this.shutdown_reply, &this.writer, cx);
            }
            if this.tx.is_none() {
                return Poll::Ready(Ok(()));
            }
            let (reply, pending) = oneshot::channel();
            ready!(this.poll_queue(cx, ToWriter::Shutdown(reply)))?;
            this.tx = None;
            this.unflushed = false;
            this.shutdown_reply = Some(pending);
        }
    }
}

impl Drop for UnixStream {
    fn drop(&mut self) {
        // The read pump exits within one poll interval; dropping `tx` lets
        // the write pump finish the queue and end the stream gracefully.
        self.socket.close();
    }
}

fn spawn_read_pump(
    socket: Arc<SharedSocket>,
    data_tx: mpsc::Sender<io::Result<Vec<u8>>>,
) -> io::Result<()> {
    spawn_pump("winsock-unix-read", move || {
        let mut buffer = vec![0_u8; CHUNK_SIZE];
        while !socket.is_closing() {
            // SAFETY: The handle stays open while this pump holds a
            // reference; the buffer is valid for the requested length.
            let received = unsafe { recv(socket.handle, buffer.as_mut_ptr(), CHUNK_LEN, 0) };
            // Only `SOCKET_ERROR` is negative.
            let chunk = match usize::try_from(received) {
                // Graceful EOF: dropping the sender yields EOF to `poll_read`.
                Ok(0) => return,
                Ok(received) => Ok(buffer[..received].to_vec()),
                Err(_) => {
                    let error = wsa_last_error();
                    if !would_block(&error) {
                        let _ = data_tx.blocking_send(Err(error));
                        return;
                    }
                    match socket.wait(POLLRDNORM, || socket.is_closing()) {
                        Ok(true) => continue,
                        Ok(false) => return,
                        Err(error) => Err(error),
                    }
                }
            };
            let failed = chunk.is_err();
            // Blocks until the async side consumes a slot, applying
            // backpressure to the socket; fails once the stream drops.
            if data_tx.blocking_send(chunk).is_err() || failed {
                return;
            }
        }
    })
}

fn spawn_write_pump(
    socket: Arc<SharedSocket>,
    mut writer_rx: mpsc::Receiver<ToWriter>,
    writer: Arc<WriterState>,
) -> io::Result<()> {
    spawn_pump("winsock-unix-write", move || {
        if let Err(error) = run_write_pump(&socket, &mut writer_rx, &writer) {
            *writer.error.lock().expect("write error lock not poisoned") = Some(error);
        }
        // Close the channel before waking a parked writer so its retry sees
        // the pump gone instead of parking again.
        drop(writer_rx);
        writer.wake();
    })
}

fn run_write_pump(
    socket: &SharedSocket,
    writer_rx: &mut mpsc::Receiver<ToWriter>,
    writer: &WriterState,
) -> io::Result<()> {
    let mut linger_deadline = None;
    let mut give_up = || {
        socket.is_closing()
            && Instant::now()
                >= *linger_deadline.get_or_insert_with(|| Instant::now() + DROP_LINGER)
    };
    while let Some(message) = writer_rx.blocking_recv() {
        // Receiving freed a slot; let a writer parked on a full channel retry.
        writer.wake();
        match message {
            ToWriter::Data(data) => {
                if !send_all(socket, &data, &mut give_up)? {
                    return Ok(());
                }
            }
            ToWriter::Flush(reply) => {
                let _ = reply.send(());
            }
            ToWriter::Shutdown(reply) => {
                shutdown_send(socket)?;
                let _ = reply.send(());
                return Ok(());
            }
        }
    }
    // The stream dropped without a shutdown and its queued data is sent:
    // close the send side so the peer observes EOF after it.
    shutdown_send(socket)
}

/// Send all of `data`. Returns `Ok(false)` if `give_up` stopped the wait for
/// a writable socket.
fn send_all(
    socket: &SharedSocket,
    data: &[u8],
    give_up: &mut impl FnMut() -> bool,
) -> io::Result<bool> {
    let mut offset = 0;
    while offset < data.len() {
        let pending = &data[offset..];
        // SAFETY: The handle stays open while this pump holds a reference;
        // the slice is valid for the requested length, which `CHUNK_SIZE`
        // bounds well below `i32::MAX`.
        let len = i32::try_from(pending.len()).unwrap_or(i32::MAX);
        let sent = unsafe { send(socket.handle, pending.as_ptr(), len, 0) };
        // Only `SOCKET_ERROR` is negative.
        match usize::try_from(sent) {
            Err(_) => {
                let error = wsa_last_error();
                if !would_block(&error) {
                    return Err(error);
                }
                if !socket.wait(POLLWRNORM, &mut *give_up)? {
                    return Ok(false);
                }
            }
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "winsock unix send accepted no data",
                ));
            }
            Ok(sent) => offset += sent,
        }
    }
    Ok(true)
}

fn shutdown_send(socket: &SharedSocket) -> io::Result<()> {
    // SAFETY: The handle stays open while this pump holds a reference; only
    // the send side is closed so the peer observes EOF after the data.
    if unsafe { shutdown(socket.handle, SD_SEND) } == SOCKET_ERROR {
        return Err(wsa_last_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// Windows limits `sun_path` to 108 bytes, so keep fixture paths short.
    fn socket_path() -> (PathBuf, PathBuf) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!(
            "wsu-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("create test dir");
        let path = directory.join("s.sock");
        (directory, path)
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len)
            .map(|index| u8::try_from(index % 251).expect("below 251"))
            .collect()
    }

    #[test]
    fn plain_af_unix_socket_creation_succeeds() {
        ensure_wsa_startup().expect("WSAStartup");
        // SAFETY: Plain creation call; the handle is closed before returning.
        let socket = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        assert_ne!(
            socket,
            INVALID_SOCKET,
            "socket(AF_UNIX) failed: {}",
            wsa_last_error()
        );
        // SAFETY: The handle was just created and is closed exactly once.
        unsafe { closesocket(socket) };
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_round_trips_large_payloads_both_ways() {
        let (directory, path) = socket_path();
        let listener = UnixListener::bind(&path).expect("bind");
        let request = payload(1024 * 1024 + 7);
        let response = payload(300 * 1024);

        let server_response = response.clone();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let mut received = Vec::new();
            stream
                .read_to_end(&mut received)
                .await
                .expect("server read");
            stream
                .write_all(&server_response)
                .await
                .expect("server write");
            stream.shutdown().await.expect("server shutdown");
            received
        });

        let mut client = connect(&path).await.expect("connect");
        client.write_all(&request).await.expect("client write");
        client.flush().await.expect("client flush");
        client.shutdown().await.expect("client shutdown");
        let mut received = Vec::new();
        client
            .read_to_end(&mut received)
            .await
            .expect("client read");

        assert!(
            server.await.expect("server task") == request,
            "request corrupted"
        );
        assert!(received == response, "response corrupted");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_stream_delivers_queued_data() {
        let (directory, path) = socket_path();
        let listener = UnixListener::bind(&path).expect("bind");
        let request = payload(256 * 1024);

        let mut client = connect(&path).await.expect("connect");
        let mut server = listener.accept().await.expect("accept");
        // The payload exceeds the queue plus the socket buffer, so the server
        // must already be reading for `write_all` to return.
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            server
                .read_to_end(&mut received)
                .await
                .expect("server read");
            received
        });
        client.write_all(&request).await.expect("client write");
        drop(client);

        let received = reader.await.expect("reader task");
        assert!(received == request, "queued data lost on drop");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_stream_releases_the_handle() {
        let (directory, path) = socket_path();
        let listener = UnixListener::bind(&path).expect("bind");
        let client = connect(&path).await.expect("connect");
        let _server = listener.accept().await.expect("accept");

        let socket = Arc::clone(&client.socket);
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&socket) > 1 {
            assert!(Instant::now() < deadline, "pump threads did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::remove_dir_all(&directory).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_accept_keeps_the_connection() {
        let (directory, path) = socket_path();
        let listener = UnixListener::bind(&path).expect("bind");
        for _ in 0..3 {
            let accepted = tokio::time::timeout(Duration::from_millis(20), listener.accept()).await;
            assert!(accepted.is_err(), "accept returned without a client");
        }
        let mut client = connect(&path).await.expect("connect");
        let mut server = listener.accept().await.expect("accept");
        client.write_all(b"ping").await.expect("client write");
        let mut received = [0_u8; 4];
        server.read_exact(&mut received).await.expect("server read");
        assert_eq!(&received, b"ping");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listener_drop_removes_the_socket_file_and_allows_rebind() {
        let (directory, path) = socket_path();
        drop(UnixListener::bind(&path).expect("bind"));
        assert!(!path.exists(), "socket file left behind");
        let listener = UnixListener::bind(&path).expect("re-bind");
        let mut client = connect(&path).await.expect("connect");
        let mut server = listener.accept().await.expect("accept");
        server.write_all(b"pong").await.expect("server write");
        let mut received = [0_u8; 4];
        client.read_exact(&mut received).await.expect("client read");
        assert_eq!(&received, b"pong");
        std::fs::remove_dir_all(&directory).ok();
    }
}
