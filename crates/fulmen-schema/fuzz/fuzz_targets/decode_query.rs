//! Lax text-source parsers must never panic on arbitrary input — the
//! untrusted query-string / Cookie-header surface (ROADMAP M2.19).
//! Leading bytes steer [`Limits`]; input is lossy-decoded so the full
//! byte space reaches the percent-decoder, and each segment is also run
//! through `validate_text` lax coercion against a union validator.

#![no_main]

use std::sync::OnceLock;

use fulmen_schema::{
    Limits, Source, TypeDesc, Validator, compile, decode_cookie_with_limits,
    decode_query_with_limits, validate_text,
};
use libfuzzer_sys::fuzz_target;

/// A validator covering the lax coercion arms — numeric, boolean and
/// the canonicalizing parsers (uuid/decimal/datetime).
fn text_validator() -> &'static Validator {
    static V: OnceLock<Validator> = OnceLock::new();
    V.get_or_init(|| {
        compile(&TypeDesc::union(vec![
            TypeDesc::int(),
            TypeDesc::float(),
            TypeDesc::bool(),
            TypeDesc::uuid(),
            TypeDesc::decimal(),
            TypeDesc::datetime(),
            TypeDesc::str(),
        ]))
        .expect("static desc compiles")
    })
}

fuzz_target!(|data: &[u8]| {
    let at = |i: usize| data.get(i).copied().unwrap_or(0);
    let limits = Limits {
        max_items: usize::from(at(0)) * 64,
        max_str_bytes: usize::from(at(1)) * 16,
        ..Limits::default()
    };
    let payload = &data[data.len().min(2)..];
    let text = String::from_utf8_lossy(payload);
    let _ = decode_query_with_limits(&text, &limits);
    let _ = decode_cookie_with_limits(&text, &limits);
    // Individual pair values feed the lax text validator too — split on
    // the query grammar's separators so coercions see realistic tokens.
    let v = text_validator();
    for seg in text.split(['&', '=', ';']) {
        let _ = validate_text(seg, v, Source::Query);
    }
});
