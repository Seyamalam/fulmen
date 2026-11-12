//! `decode_form` (application/x-www-form-urlencoded) must never panic
//! on arbitrary input (ROADMAP M2.19). Leading bytes steer [`Limits`];
//! input is lossy-decoded so the full byte space reaches the parser.

#![no_main]

use fulmen_schema::{Limits, decode_form_with_limits};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let at = |i: usize| data.get(i).copied().unwrap_or(0);
    let limits = Limits {
        max_items: usize::from(at(0)) * 64,
        max_str_bytes: usize::from(at(1)) * 16,
        ..Limits::default()
    };
    let payload = &data[data.len().min(2)..];
    let text = String::from_utf8_lossy(payload);
    let _ = decode_form_with_limits(&text, &limits);
    let _ = decode_form_with_limits(&text, &Limits::default());
});
