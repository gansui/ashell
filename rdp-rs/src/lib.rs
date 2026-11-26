#![allow(mismatched_lifetime_syntaxes)]

extern crate byteorder;
extern crate indexmap;
extern crate yasna;
extern crate native_tls;
extern crate md4;
extern crate hmac;
extern crate md5;
extern crate rand;
extern crate num_bigint;
extern crate x509_parser;
extern crate num_enum;
#[cfg(feature = "mstsc-rs")]
extern crate minifb;
#[cfg(feature = "mstsc-rs")]
extern crate winapi;
#[cfg(feature = "mstsc-rs")]
extern crate hex;
#[cfg(feature = "mstsc-rs")]
extern crate clap;


#[macro_use]
pub mod model;

/// Append a cliprdr diagnostic line to `rdp_clip_debug.log` in the working
/// directory. ON by default so it triggers even without the env var (Windows
/// GUI launch often drops the inherited environment). Set RDP_CLIP_DEBUG=0
/// to silence it.
#[macro_export]
macro_rules! clip_log {
    ($($arg:tt)*) => {{
        if std::env::var_os("RDP_CLIP_DEBUG").map_or(true, |v| v != "0") {
            use std::io::Write;
            let line = format!("[rdp-clip] {}\n", format_args!($($arg)*));
            for p in ["rdp_clip_debug.log", "ashell/rdp_clip_debug.log"] {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true).append(true).open(p)
                {
                    let _ = f.write_all(line.as_bytes());
                }
            }
        }
    }};
}
#[macro_use]
pub mod nla;
pub mod core;
pub mod codec;
