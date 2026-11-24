//! RDP (Remote Desktop Protocol) backend.
//!
//! RDP is a bitmap-graphics protocol, unlike SSH/serial/local which are
//! character streams. This backend runs the blocking `rdp-rs` client on a
//! dedicated OS thread, decodes incoming bitmap regions, and forwards them
//! to the UI as `BackendEvent::RdpBitmap` so a dedicated rendering panel can
//! composite them onto a full-screen RGBA buffer.

use std::{
    net::TcpStream,
    sync::{
        Arc,
        mpsc::{self, Receiver},
        Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow};
use rdp::core::client::Connector;
use rdp::core::event::{KeyboardEvent, PointerButton, PointerEvent, RdpEvent};

use crate::terminal::{
    BackendCommand, BackendEvent, BackendTx, GuardedBackendEventSender, RdpInputEvent,
};

/// Default remote desktop resolution advertised to the RDP server.
pub const DEFAULT_WIDTH: u16 = 1280;
/// Default remote desktop resolution advertised to the RDP server.
pub const DEFAULT_HEIGHT: u16 = 720;

/// Start an RDP session on a dedicated OS thread.
///
/// `width`/`height` set the initial virtual desktop resolution advertised to
/// the server. The returned `BackendTx::Rdp` lets the UI send commands
/// (e.g. `Close`) to the session.
pub fn spawn_rdp(
    tab_id: String,
    host: String,
    port: u16,
    domain: String,
    username: String,
    password: String,
    width: u16,
    height: u16,
    events: GuardedBackendEventSender,
) -> BackendTx {
    let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
    thread::Builder::new()
        .name(format!("ashell-rdp-{tab_id}"))
        .spawn(move || {
            if let Err(err) = run_rdp(
                tab_id.clone(),
                host,
                port,
                domain,
                username,
                password,
                width,
                height,
                cmd_rx,
                events.clone(),
            ) {
                let _ = events.send(BackendEvent::Closed {
                    tab_id,
                    reason: format!("{err:#}"),
                });
            }
        })
        .expect("spawn rdp thread");
    BackendTx::Rdp(cmd_tx)
}

/// Establish the RDP connection and run the blocking read/update loop.
fn run_rdp(
    tab_id: String,
    host: String,
    port: u16,
    domain: String,
    username: String,
    password: String,
    width: u16,
    height: u16,
    commands: Receiver<BackendCommand>,
    events: GuardedBackendEventSender,
) -> Result<()> {
    let _ = events.send(BackendEvent::RdpStatus {
        tab_id: tab_id.clone(),
        text: format!("connecting rdp {}:{}...", host, port),
    });

    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr)?;
    tcp.set_nodelay(true)?;
    // The blocking `client.read()` in the loop below must periodically time
    // out so the loop can service queued input commands. Without a read
    // timeout, a quiet desktop (server sending nothing) parks the thread in
    // `read()` forever and mouse/keyboard events are never dispatched.
    tcp.set_read_timeout(Some(Duration::from_millis(50)))?;

    // Certificate checks are off by default so self-signed RD hosts work.
    let mut connector = Connector::new()
        .screen(width, height)
        .credentials(domain, username.clone(), password)
        .name("ashell".to_string())
        .check_certificate(false);

    let mut client = connector
        .connect(tcp)
        .map_err(|e| anyhow!("rdp connect failed: {e:?}"))?;

    let _ = events.send(BackendEvent::RdpStatus {
        tab_id: tab_id.clone(),
        text: format!("connected rdp {username}@{host}"),
    });
    let _ = events.send(BackendEvent::Connected {
        tab_id: tab_id.clone(),
    });

    let exit_reason;
    let mut is_shutdown = false;
    // Host clipboard text cached here so the backend can answer the server's
    // clipboard format-data requests without blocking on the GPUI main thread.
    let mut clipboard_text: Option<String> = None;
    let send_queue: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

    // Advertise clipboard capabilities (CLIPRDR) once connected.
    let _ = client.write_clipboard(cliprdr_caps());
    let _ = client.write_clipboard(cliprdr_monitor_ready());

    loop {
        // Dispatch pending commands without blocking the read loop.
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                BackendCommand::Close => {
                    is_shutdown = true;
                }
                BackendCommand::RdpInput(event) => {
                    match event {
                        RdpInputEvent::Pointer(p) => {
                            let button = match p.button {
                                crate::terminal::RdpPointerButton::Left => PointerButton::Left,
                                crate::terminal::RdpPointerButton::Right => PointerButton::Right,
                                crate::terminal::RdpPointerButton::Middle => PointerButton::Middle,
                                crate::terminal::RdpPointerButton::None => PointerButton::None,
                            };
                            let _ = client.write(RdpEvent::Pointer(PointerEvent {
                                x: p.x,
                                y: p.y,
                                button,
                                down: p.down,
                            }));
                        }
                        RdpInputEvent::Key(k) => {
                            let _ = client.write(RdpEvent::Key(KeyboardEvent {
                                code: k.code,
                                down: k.down,
                            }));
                        }
                    }
                }
                BackendCommand::RdpSetClipboard(text) => {
                    clipboard_text = Some(text);
                }
                _ => {}
            }
        }
        if is_shutdown {
            exit_reason = "rdp session closed".to_string();
            break;
        }

        match client.read(|event| {
            handle_rdp_event(
                &tab_id,
                event,
                &events,
                &send_queue,
                &clipboard_text,
                width,
                height,
            )
        }) {
            Ok(()) => {}
            Err(e) => {
                // A read timeout is not an error: it's how the loop
                // escapes the blocking read to service queued input commands
                // while the desktop is otherwise quiet.
                if is_timeout_error(&e) {
                    continue;
                }
                if is_disconnect_error(&e) {
                    exit_reason = "rdp server closed the connection".to_string();
                } else {
                    exit_reason = format!("rdp error: {e:?}");
                }
                break;
            }
        }

        // Flush queued CLIPRDR responses back to the server.
        if let Ok(mut q) = send_queue.lock() {
            for msg in q.drain(..) {
                let _ = client.write_clipboard(msg);
            }
        }
    }

    let _ = client.shutdown();
    let _ = events.send(BackendEvent::Closed {
        tab_id,
        reason: exit_reason,
    });
    Ok(())
}

/// Decode a single RDP event into ashell backend events.
fn handle_rdp_event(
    tab_id: &str,
    event: RdpEvent,
    events: &GuardedBackendEventSender,
    send_queue: &Arc<Mutex<Vec<Vec<u8>>>>,
    clipboard: &Option<String>,
    desktop_width: u16,
    desktop_height: u16,
) {
    match event {
        RdpEvent::Bitmap(bitmap) => {
            let x = bitmap.dest_left;
            let y = bitmap.dest_top;
            let width = bitmap.width;
            let height = bitmap.height;
            match bitmap.decompress() {
                Ok(rgba) => {
                    let _ = events.send(BackendEvent::RdpBitmap {
                        tab_id: tab_id.to_string(),
                        x,
                        y,
                        width,
                        height,
                        desktop_width,
                        desktop_height,
                        rgba,
                    });
                }
                Err(e) => {
                    tracing::warn!("[rdp] failed to decompress bitmap on {tab_id}: {e:?}");
                }
            }
        }
        RdpEvent::Cliprdr(data) => {
            // A clipboard virtual-channel payload from the server. If it is a
            // format-data request, answer with the host clipboard text.
            if cliprdr_is_format_data_request(&data) {
                if let Some(text) = clipboard {
                    if let Ok(mut q) = send_queue.lock() {
                        q.push(cliprdr_format_data_response(text));
                    }
                }
            }
        }
        // Pointer / Key events are input from the client to the server; they
        // are sent through `RdpClient::write`, not received here.
        _ => {}
    }
}

/// Returns true when the rdp-rs error is a read timeout. A timeout is not
/// fatal: `set_read_timeout` is what lets the loop below service queued input
/// commands while the desktop is otherwise quiet.
fn is_timeout_error(error: &rdp::model::error::Error) -> bool {
    matches!(error, rdp::model::error::Error::Io(e)
        if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock))
}

/// Returns true when the rdp-rs error is a server-requested disconnect.
fn is_disconnect_error(error: &rdp::model::error::Error) -> bool {
    matches!(
        error,
        rdp::model::error::Error::RdpError(e)
            if e.kind() == rdp::model::error::RdpErrorKind::Disconnect
    )
}

// --- CLIPRDR (MS-RDPECLIP) helpers ---

/// Build a CLIPRDR message header (msgType u16, msgFlags u16, dataLen u32).
fn cliprdr_header(msg_type: u16, flags: u16, data_len: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data_len as usize);
    v.extend_from_slice(&msg_type.to_le_bytes());
    v.extend_from_slice(&flags.to_le_bytes());
    v.extend_from_slice(&data_len.to_le_bytes());
    v
}

/// Build a `CB_CLIP_CAPS` (0x0007) advertising the General capability set.
fn cliprdr_caps() -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&1u16.to_le_bytes()); // cCapabilitiesSets
    data.extend_from_slice(&0u16.to_le_bytes()); // pad
    // General capability set (12 bytes including type and length).
    data.extend_from_slice(&0x0001u16.to_le_bytes()); // capabilitySetType = General
    data.extend_from_slice(&12u16.to_le_bytes()); // lengthCapability
    data.extend_from_slice(&0x0002u32.to_le_bytes()); // version = CB_CAPS_VERSION_2
    data.extend_from_slice(&0x00000002u32.to_le_bytes()); // generalFlags = long format names
    let mut msg = cliprdr_header(0x0007, 0x0001, data.len() as u32); // msgFlags = CB_RESPONSE
    msg.extend_from_slice(&data);
    msg
}

/// Build a `CB_MONITOR_READY` (0x0001) telling the server the client is ready.
fn cliprdr_monitor_ready() -> Vec<u8> {
    let mut msg = cliprdr_header(0x0001, 0, 4);
    msg.extend_from_slice(&0u32.to_le_bytes()); // shareId
    msg
}

/// Return true when a received cliprdr payload is a `CB_FORMAT_DATA_REQUEST`
/// (0x0004), meaning the remote wants the host clipboard data.
fn cliprdr_is_format_data_request(data: &[u8]) -> bool {
    data.len() >= 2 && u16::from_le_bytes([data[0], data[1]]) == 0x0004
}

/// Build a `CB_FORMAT_DATA_RESPONSE` (0x0005) carrying the host clipboard text
/// encoded as UTF-16LE (CF_UNICODETEXT).
fn cliprdr_format_data_response(text: &str) -> Vec<u8> {
    let mut text_data = Vec::with_capacity(text.len() * 2 + 2);
    for unit in text.encode_utf16() {
        text_data.extend_from_slice(&unit.to_le_bytes());
    }
    text_data.extend_from_slice(&0u16.to_le_bytes()); // null terminator
    // The response body starts with a u32 length of the format data.
    let mut data = Vec::with_capacity(4 + text_data.len());
    data.extend_from_slice(&(text_data.len() as u32).to_le_bytes());
    data.extend_from_slice(&text_data);
    let mut msg = cliprdr_header(0x0005, 0x0001, data.len() as u32); // msgFlags = CB_RESPONSE
    msg.extend_from_slice(&data);
    msg
}