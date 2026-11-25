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
/// directory. Gated by the `RDP_CLIP_DEBUG` env var so normal builds stay
/// silent. Windows GUI apps drop stderr, so a real file is the reliable sink.
#[macro_export]
macro_rules! clip_log {
    ($($arg:tt)*) => {{
        if std::env::var_os("RDP_CLIP_DEBUG").is_some() {
            use std::io::Write;
            let line = format!("[rdp-clip] {}\n", format_args!($($arg)*));
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true).append(true).open("rdp_clip_debug.log")
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }};
}
#[macro_use]
pub mod nla;
pub mod core;
pub mod codec;
