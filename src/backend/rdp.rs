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

    // Diagnostics: unconditionally record that the RDP thread started, plus
    // every candidate log path, so a missing file is immediately attributable
    // to "the backend never ran" rather than to a cwd/env issue.
    let startup_line = format!(
        "[rdp] backend start host={host}:{port} pid={} cwd={:?} exe={:?}\n",
        std::process::id(),
        std::env::current_dir(),
        std::env::current_exe(),
    );
    for p in ["rdp_clip_debug.log", "ashell/rdp_clip_debug.log"] {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            use std::io::Write;
            let _ = f.write_all(startup_line.as_bytes());
        }
    }

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

    // CLIPRDR initialization: as the client we are NOT the initiator. IronRDP's
    // `Cliprdr::start` returns an empty vec in client mode and FreeRDP's client
    // likewise sends nothing up front — `CB_MONITOR_READY` is a server-only PDU,
    // so the client must never send it. We wait for the server's Capabilities +
    // Monitor Ready, then reply with our Capabilities + Format List (see the
    // monitor-ready branch in `handle_rdp_event`).
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
                    if std::env::var_os("RDP_CLIP_DEBUG").map_or(true, |v| v != "0") {
                        eprintln!("[ashell] RdpSetClipboard len={}", text.len());
                    }
                    clipboard_text = Some(text.clone());
                    // Advertise the host clipboard to the server so the remote
                    // gets the text pushed into its clipboard, not only when it
                    // pastes. Without a CB_FORMAT_LIST_REQUEST the server has no
                    // way to know the host side changed, so A->B stays stale.
                    if let Ok(mut q) = send_queue.lock() {
                        q.push(cliprdr_format_list_request());
                    }
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
            if cliprdr_is_format_data_request(&data) {
                // Host -> remote: the server wants our clipboard text.
                if let Some(text) = clipboard {
                    if let Ok(mut q) = send_queue.lock() {
                        q.push(cliprdr_format_data_response(text));
                    }
                }
            } else if cliprdr_is_format_list(&data) {
                // The server announced a clipboard change (the remote copied
                // something, or is about to paste and enumerated its formats).
                // Windows expects a CB_FORMAT_LIST_RESPONSE back or it never
                // sends the CB_FORMAT_DATA_REQUEST that carries the pasted
                // payload, so both directions stall. Acknowledge first, then
                // ask for CF_UNICODETEXT to mirror the remote clipboard.
                if let Ok(mut q) = send_queue.lock() {
                    q.push(cliprdr_format_list_response());
                    q.push(cliprdr_format_data_request(13));
                }
            } else if cliprdr_is_format_data_response(&data) {
                // Remote -> host: the server answered our format-data request.
                // Decode the UTF-16LE text and hand it to the UI, which writes
                // it into the host system clipboard.
                if let Some(text) = cliprdr_decode_text_response(&data) {
                    let _ = events.send(BackendEvent::RemoteClipboard(text));
                }
            } else if cliprdr_is_monitor_ready(&data) {
                // The server signaled the CLIPRDR channel is up. Match IronRDP's
                // client `initiate_copy` in the Initialization state (lib.rs
                // 961-972): reply with Capabilities + Temporary Directory +
                // Format List as a batch. The Temporary Directory is part of the
                // Microsoft initialization handshake rdpclip expects before it
                // will accept the client's format list.
                if let Ok(mut q) = send_queue.lock() {
                    q.push(cliprdr_caps());
                    q.push(cliprdr_temp_directory());
                    q.push(cliprdr_format_list_request());
                }
            } else {
                // Diagnostic catch-all: log any cliprdr payload we don't
                // recognize so we can see every PDU the server sends.
                let msg_type = if data.len() >= 2 {
                    u16::from_le_bytes([data[0], data[1]])
                } else {
                    0
                };
                eprintln!(
                    "[rdp-clip] UNHANDLED cliprdr msgType=0x{:04x} len={}",
                    msg_type,
                    data.len()
                );
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
    // generalFlags MUST advertise CB_USE_LONG_FORMAT_NAMES (0x2) because the
    // CB_FORMAT_LIST entries below use the long-format-name form (formatId +
    // a length-prefixed / null-terminated UTF-16 name). 0x10 is CAN_LOCK_CLIPDATA,
    // not long names — the server already advertises 0x3e (which includes 0x2).
    data.extend_from_slice(&0x00000002u32.to_le_bytes()); // generalFlags = long format names
    let mut msg = cliprdr_header(0x0007, 0, data.len() as u32); // CB_CLIP_CAPS, msgFlags = 0
    msg.extend_from_slice(&data);
    msg
}

/// Build a `CB_TEMP_DIRECTORY` (0x0006): IronRDP's client `initiate_copy` sends
/// this together with capabilities and the format list in the Initialization
/// state. Microsoft's rdpclip may not accept a format list until the client has
/// declared its temp directory. Layout is the 8-byte header + a 520-byte
/// UTF-16LE null-terminated path buffer.
fn cliprdr_temp_directory() -> Vec<u8> {
    let mut data = vec![0u8; 520];
    let path = ".cliprdr";
    for (i, unit) in path.encode_utf16().enumerate() {
        data[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    let mut msg = cliprdr_header(0x0006, 0, data.len() as u32);
    msg.extend_from_slice(&data);
    msg
}

/// Return true when a cliprdr payload is a `CB_MONITOR_READY` (0x0001): the
/// server telling the client the CLIPRDR channel is initialized and the
/// client should now send its capabilities and format list.
fn cliprdr_is_monitor_ready(data: &[u8]) -> bool {
    data.len() >= 2 && u16::from_le_bytes([data[0], data[1]]) == 0x0001
}

/// Return true when a received cliprdr payload is a `CB_FORMAT_DATA_REQUEST`
/// (0x0004), meaning the remote wants the host clipboard data.
fn cliprdr_is_format_data_request(data: &[u8]) -> bool {
    data.len() >= 2 && u16::from_le_bytes([data[0], data[1]]) == 0x0004
}

/// Return true when a cliprdr payload is a `CB_FORMAT_LIST` (0x0002): the
/// server announcing it copied or cleared its clipboard.
fn cliprdr_is_format_list(data: &[u8]) -> bool {
    data.len() >= 2 && u16::from_le_bytes([data[0], data[1]]) == 0x0002
}

/// Return true when a cliprdr payload is a `CB_FORMAT_DATA_RESPONSE` (0x0005).
fn cliprdr_is_format_data_response(data: &[u8]) -> bool {
    data.len() >= 2 && u16::from_le_bytes([data[0], data[1]]) == 0x0005
}

/// Build a `CB_FORMAT_LIST_RESPONSE` (0x0003) acknowledging the server's
/// format-list announcement. Windows only proceeds to paste (CB_FORMAT_DATA_REQUEST)
/// after it sees this, so it is required for the host->remote direction too.
fn cliprdr_format_list_response() -> Vec<u8> {
    cliprdr_header(0x0003, 0x0001, 0) // msgFlags = CB_RESPONSE
}

/// Build a `CB_FORMAT_LIST` (0x0002) announcing the host clipboard now holds
/// CF_UNICODETEXT. Announcing (rather than `CB_FORMAT_LIST_REQUEST` 0x0001, which
/// reads the remote clipboard) is what pushes host text into the remote: the
/// server answers with a CB_FORMAT_LIST_RESPONSE and then a CB_FORMAT_DATA_REQUEST,
/// which the loop answers with the cached text. Must NOT carry CB_RESPONSE —
/// that flag marks a reply to a request, and announcing a change is a fresh
/// event, not a reply.
fn cliprdr_format_list_request() -> Vec<u8> {
    // Long-format-name + Unicode (no ASCII_NAMES flag) encoding. Both FreeRDP
    // (`cliprdr_packet_format_list_new`) and IronRDP (`FormatList::encode`) write
    // the format list as a bare sequence of entries — NO cFormats/pad header —
    // and set dataLen to the sum of the entries. Windows rdpclip parses it that
    // way (long-name pass: formatId u32, then the null-terminated name). The
    // 2-byte cFormats/pad MS-RDPECLIP header makes rdpclip misparse offset 0 as
    // formatId=1, so it never takes ownership of CF_UNICODETEXT and never issues
    // a format data request on paste. An empty name is the UTF-16 null [0,0], so
    // one CF_UNICODETEXT entry is 4 + 2 = 6 bytes and dataLen is 6.
    let mut data = Vec::with_capacity(6);
    data.extend_from_slice(&13u32.to_le_bytes()); // CF_UNICODETEXT formatId
    data.extend_from_slice(&0u16.to_le_bytes()); // formatName = UTF-16 null terminator
    let mut msg = cliprdr_header(0x0002, 0, data.len() as u32); // CB_FORMAT_LIST
    msg.extend_from_slice(&data);
    msg
}

/// Build a `CB_FORMAT_DATA_REQUEST` (0x0004) asking the server for the payload
/// of the given format id (CF_UNICODETEXT = 13).
fn cliprdr_format_data_request(format_id: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(4);
    data.extend_from_slice(&format_id.to_le_bytes());
    let mut msg = cliprdr_header(0x0004, 0, data.len() as u32);
    msg.extend_from_slice(&data);
    msg
}

/// Decode a `CB_FORMAT_DATA_RESPONSE` payload into UTF-16LE text. Layout after
/// the 8-byte header: a u32 `responseData` length, then the format data.
fn cliprdr_decode_text_response(data: &[u8]) -> Option<String> {
    let body = data.get(12..)?;
    let bytes = body.strip_suffix(&[0u8, 0u8]).unwrap_or(body);
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
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