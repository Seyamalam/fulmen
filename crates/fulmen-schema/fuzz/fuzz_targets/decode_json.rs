//! `decode_json` must never panic on arbitrary bytes — the untrusted
//! request-body surface (ROADMAP M2.19). The first three input bytes
//! steer the structural [`Limits`] so bounded and unbounded walks both
//! get coverage, then the payload is decoded once under those limits
//! and once under the defaults.

#![no_main]

use fulmen_schema::{Limits, decode_json_with_limits};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let at = |i: usize| data.get(i).copied().unwrap_or(0);
    let limits = Limits {
        max_depth: u32::from(at(0)) + 1,
        max_items: usize::from(at(1)) * 64,
        max_str_bytes: usize::from(at(2)) * 16,
        ..Limits::default()
    };
    let payload = &data[data.len().min(3)..];
    let _ = decode_json_with_limits(payload, &limits);
    let _ = decode_json_with_limits(payload, &Limits::default());
});
