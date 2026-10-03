//! How many hardware HEVC encoder sessions this Mac runs at once: a stream uses one per tile and
//! one for the full frame. Ignored: it takes the media engine for a moment.
//!
//!   cargo test --release -p platform-mac --test session_limit -- --ignored --nocapture

use platform_mac::encoder::{Encoder, EncoderConfig};
use protocol::Codec;

#[test]
#[ignore = "benchmark"]
fn hardware_encoder_sessions() {
    let mut sessions = Vec::new();
    for (w, h) in [(3072u32, 320u32), (6144, 2560)] {
        sessions.clear();
        let cfg = EncoderConfig { width: w, height: h, fps: 120, bitrate_bps: 8_000_000, codec: Codec::Hevc };
        let limit = loop {
            match Encoder::new(&cfg, |_| {}) {
                Ok(e) if e.hardware() && e.codec() == Codec::Hevc => sessions.push(e),
                Ok(_) | Err(_) => break sessions.len(),
            }
            if sessions.len() >= 256 {
                break sessions.len();
            }
        };
        println!("{w}x{h}: {limit} hardware HEVC sessions at once");
    }
}
