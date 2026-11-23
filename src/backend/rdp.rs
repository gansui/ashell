//! RDP (Remote Desktop Protocol) backend.
//!
//! RDP is a bitmap-graphics protocol, unlike SSH/serial/local which are
//! character streams. This backend runs the blocking `rdp-rs` client on a
//! dedicated OS thread, decodes incoming bitmap regions, and forwards them
//! to the UI as `BackendEvent::RdpBitmap` so a dedicated rendering panel can
//! composite them onto a full-screen RGBA buffer.

use std::{
    net::TcpStream,
    sync::mpsc::{self, Receiver},
    thread,
};

use anyhow::{Result, anyhow};
use rdp::core::client::Connector;
use rdp::core::event::{KeyboardEvent, PointerButton, PointerEvent, RdpEvent};

use crate::terminal::{
    BackendCommand, BackendEvent, BackendTx, GuardedBackendEventSender, RdpInputEvent,
};

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
                _ => {}
            }
        }
        if is_shutdown {
            exit_reason = "rdp session closed".to_string();
            break;
        }

        match client.read(|event| handle_rdp_event(&tab_id, event, &events)) {
            Ok(()) => {}
            Err(e) => {
                if is_disconnect_error(&e) {
                    exit_reason = "rdp server closed the connection".to_string();
                } else {
                    exit_reason = format!("rdp error: {e:?}");
                }
                break;
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
fn handle_rdp_event(tab_id: &str, event: RdpEvent, events: &GuardedBackendEventSender) {
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
                        rgba,
                    });
                }
                Err(e) => {
                    tracing::warn!("[rdp] failed to decompress bitmap on {tab_id}: {e:?}");
                }
            }
        }
        // Pointer / Key events are input from the client to the server; they
        // are sent through `RdpClient::write`, not received here.
        _ => {}
    }
}

/// Returns true when the rdp-rs error is a server-requested disconnect.
fn is_disconnect_error(error: &rdp::model::error::Error) -> bool {
    matches!(
        error,
        rdp::model::error::Error::RdpError(e)
            if e.kind() == rdp::model::error::RdpErrorKind::Disconnect
    )
}