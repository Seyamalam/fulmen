//! Text parsers for lax-mode coercion and the string-carried types
//! (`datetime`, `UUID`, `Email`, ...). Everything here is dependency-free
//! — there is no `chrono`/`uuid`/`url` crate — so each parser documents the
//! exact grammar subset it accepts.
//!
//! All parsers are also the *canonicalizers*: they return the normalized
//! string form the validators emit, so `fulmen-py` materialization can call
//! `datetime.fromisoformat`, `UUID(text)` etc. on the output directly —
//! or reuse these functions across the boundary. None of them allocate on
//! the happy path except to produce the canonical output.

use std::fmt::Write;

/// Parse the lax boolean words: `true|1|on|yes|y|t` and
/// `false|0|off|no|n|f`, ASCII case-insensitive, surrounding whitespace
/// tolerated. Anything else is `None` — `"2"` is not a bool.
pub fn parse_bool_text(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "y" | "t" => Some(true),
        "false" | "0" | "off" | "no" | "n" | "f" => Some(false),
        _ => None,
    }
}

/// Parse a lax float: `str::parse::<f64>` plus the infinity words
/// (`inf`, `+inf`, `-inf`, `infinity`, `-infinity`, `nan`, case
/// insensitive, whitespace tolerated). Non-finite results are accepted —
/// the `float` descriptor documents that `NaN`/`±inf` pass through.
pub fn parse_float_text(text: &str) -> Option<f64> {
    let t = text.trim();
    match t.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Some(f64::INFINITY),
        "-inf" | "-infinity" => return Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Some(f64::NAN),
        _ => {}
    }
    t.parse().ok()
}

// ---- calendar helpers -----------------------------------------------------

fn is_leap_year(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn ndigits(text: &str, at: usize, n: usize) -> Option<u64> {
    let s = text.get(at..at + n)?;
    if s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

fn byte_at(text: &str, at: usize, want: u8) -> Option<()> {
    match text.as_bytes().get(at) {
        Some(&b) if b == want => Some(()),
        _ => None,
    }
}

/// A parsed wall-clock time with optional UTC offset. `fraction` is the
/// raw digit text (any length); precision handling is left to the
/// materializer (`time.fromisoformat` truncates beyond microseconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeParts {
    /// Hour `0..=23` (`24:00` is rejected — Python `time` has no 24h form).
    pub hour: u64,
    /// Minute `0..=59`.
    pub minute: u64,
    /// Second `0..=59` (leap second `60` is rejected — Python rejects it).
    pub second: u64,
    /// Fraction-of-second digits as written, when present.
    pub fraction: Option<Box<str>>,
    /// UTC offset in signed minutes (`+02:30` → `150`); `None` for naive
    /// time; `Z` becomes `Some(0)`.
    pub offset_minutes: Option<i32>,
}

/// Parse `HH:MM[:SS[.fraction]][Z | ±HH:MM]` starting at `text[0]`.
/// Returns the parts and the number of bytes consumed. Seconds are
/// optional per ISO 8601; `HH` alone is not (Python `time` needs minutes).
fn parse_time_of_day(text: &str) -> Option<(TimeParts, usize)> {
    let hour = ndigits(text, 0, 2)?;
    byte_at(text, 2, b':')?;
    let minute = ndigits(text, 3, 2)?;
    let mut pos = 5;
    let mut second = 0;
    let mut fraction = None;
    if text.as_bytes().get(pos) == Some(&b':') {
        second = ndigits(text, pos + 1, 2)?;
        pos += 3;
        if text.as_bytes().get(pos) == Some(&b'.') {
            let start = pos + 1;
            let mut end = start;
            while text.as_bytes().get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                return None;
            }
            fraction = Some(text[start..end].into());
            pos = end;
        }
    }
    let mut offset_minutes = None;
    match text.as_bytes().get(pos) {
        Some(b'z' | b'Z') => {
            offset_minutes = Some(0);
            pos += 1;
        }
        Some(b'+' | b'-') => {
            let neg = text.as_bytes()[pos] == b'-';
            let oh = ndigits(text, pos + 1, 2)?;
            byte_at(text, pos + 3, b':')?;
            let om = ndigits(text, pos + 4, 2)?;
            pos += 6;
            // oh, om are two digits each → ≤ 6039, always fits i32.
            let v = i32::try_from(oh * 60 + om).unwrap_or(i32::MAX);
            offset_minutes = Some(if neg { -v } else { v });
        }
        _ => {}
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if offset_minutes.is_some_and(|m| m.abs() > 23 * 60 + 59) {
        return None;
    }
    Some((
        TimeParts {
            hour,
            minute,
            second,
            fraction,
            offset_minutes,
        },
        pos,
    ))
}

/// Render the canonical `HH:MM:SS[.frac]` + offset tail.
fn render_time(t: &TimeParts, out: &mut String) {
    let _ = write!(out, "{:02}:{:02}:{:02}", t.hour, t.minute, t.second);
    if let Some(f) = &t.fraction {
        out.push('.');
        out.push_str(f);
    }
    if let Some(off) = t.offset_minutes {
        if off == 0 {
            out.push('Z');
        } else {
            let a = off.unsigned_abs();
            out.push(if off < 0 { '-' } else { '+' });
            let _ = write!(out, "{:02}:{:02}", a / 60, a % 60);
        }
    }
}

/// Parse an ISO 8601 date `YYYY-MM-DD` (year `0000..=9999`, real calendar
/// days including leap years). The canonical form is the input itself.
pub fn parse_iso_date(text: &str) -> Option<String> {
    if text.len() != 10 {
        return None;
    }
    let year = ndigits(text, 0, 4)?;
    byte_at(text, 4, b'-')?;
    let month = ndigits(text, 5, 2)?;
    byte_at(text, 7, b'-')?;
    let day = ndigits(text, 8, 2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(text.to_owned())
}

/// Parse an ISO 8601 local time `HH:MM[:SS[.frac]][±HH:MM|Z]`. Canonical
/// form: seconds always present, fraction kept as written, `z`→`Z`,
/// zero offsets normalized to `Z`, others to `±HH:MM`.
pub fn parse_iso_time(text: &str) -> Option<String> {
    let (t, used) = parse_time_of_day(text)?;
    if used != text.len() {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 4);
    render_time(&t, &mut out);
    Some(out)
}

/// Parse an RFC 3339 timestamp: `YYYY-MM-DD[Tt ]HH:MM:SS[.frac][Z|±HH:MM]`.
///
/// The accepted subset: the `T` separator is required but may be lowercase
/// or a space (all `datetime.fromisoformat`-compatible); seconds are
/// required (strict RFC 3339); `24:00` and leap second `60` are rejected
/// because Python's `datetime` rejects them; offsets need the colon
/// (`+02:00`, not `+0200`). A *naive* timestamp (no offset) is accepted
/// and stays naive, matching `fromisoformat`. Canonical form has `T`
/// uppercase and zero offsets rendered `Z`.
pub fn parse_rfc3339(text: &str) -> Option<String> {
    if text.len() < 19 {
        // Shortest legal: `YYYY-MM-DDTHH:MM:SS` (naive).
        return None;
    }
    let year = ndigits(text, 0, 4)?;
    byte_at(text, 4, b'-')?;
    let month = ndigits(text, 5, 2)?;
    byte_at(text, 7, b'-')?;
    let day = ndigits(text, 8, 2)?;
    match text.as_bytes()[10] {
        b't' | b'T' | b' ' => {}
        _ => return None,
    }
    let (t, used) = parse_time_of_day(&text[11..])?;
    if 11 + used != text.len() {
        return None;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 2);
    let _ = write!(out, "{year:04}-{month:02}-{day:02}T");
    render_time(&t, &mut out);
    Some(out)
}

/// The pieces of an ISO 8601 duration, reduced to the `timedelta`
/// constructor's units. Sub-microsecond input is truncated (Python
/// `timedelta` cannot hold it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationParts {
    /// Whether a leading `-` was present.
    pub negative: bool,
    /// Whole days (`P…D` plus `P…W`×7).
    pub days: u64,
    /// Whole seconds below a day (`T…H…M…S` reduced).
    pub seconds: u64,
    /// Microseconds below a second.
    pub micros: u64,
}

impl DurationParts {
    /// Canonical ISO 8601 form `-P{d}DT{h}H{m}M{s[.ffffff]}S`. Every
    /// component is always present, so the shape is fixed — the
    /// `fulmen-py` materializer can scan it trivially, and
    /// `timedelta(days=…, seconds=…, microseconds=…)` reconstructs the
    /// value exactly.
    pub fn to_canonical(&self) -> String {
        let mut out = String::with_capacity(32);
        if self.negative {
            out.push('-');
        }
        out.push('P');
        out.push_str(&self.days.to_string());
        out.push_str("DT");
        let h = self.seconds / 3600;
        let m = self.seconds % 3600 / 60;
        let s = self.seconds % 60;
        out.push_str(&h.to_string());
        out.push('H');
        out.push_str(&m.to_string());
        out.push('M');
        if self.micros == 0 {
            out.push_str(&s.to_string());
        } else {
            let _ = write!(out, "{s}.{:06}", self.micros);
        }
        out.push('S');
        out
    }
}

/// One duration component's numeric payload: `D+` or `D+[.,]D+` (the
/// fraction kept as ≤19 digits — more precision cannot survive into
/// microseconds anyway). Returns `(whole, Option<(frac, frac_len)>)`.
fn component_number(text: &str) -> Option<(u64, Option<(u64, u8)>)> {
    let (int_s, frac_s) = match text.split_once(['.', ',']) {
        Some((a, b)) => (a, Some(b)),
        None => (text, None),
    };
    if int_s.is_empty() || !int_s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int_v: u64 = int_s.parse().ok()?;
    let frac = match frac_s {
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let f = &f[..f.len().min(19)];
            // len ≤ 19 — always fits u8.
            Some((f.parse().ok()?, u8::try_from(f.len()).unwrap_or(u8::MAX)))
        }
        None => None,
    };
    Some((int_v, frac))
}

/// Parse an ISO 8601 duration `P[nD][T[nH][nM][n[.f]S]]` or `P[nW]`, with
/// an optional leading `-`/`+` (an extension — ISO 8601 has no sign,
/// Python `timedelta` can be negative). A fraction is allowed on the
/// *last* component only. `P` alone, `PT`, a bare `T…`, and the `nW` form
/// combined with other units are rejected.
///
/// Reduction: weeks → 7 days; hours/minutes/seconds → `seconds`; fractions
/// → `micros` (truncated below 1 µs). `u64` overflow rejects.
pub fn parse_iso8601_duration(text: &str) -> Option<DurationParts> {
    let t = text.trim();
    let (negative, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let rest = rest.strip_prefix('P')?;
    // Weeks form must be alone: `P…W`.
    if let Some(w) = rest.strip_suffix('W') {
        if w.is_empty() || !w.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let days = w.parse::<u64>().ok()?.checked_mul(7)?;
        return Some(DurationParts {
            negative,
            days,
            seconds: 0,
            micros: 0,
        });
    }
    let (day_part, time_part) = match rest.find('T') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let mut days: u64 = 0;
    let mut seconds: u64 = 0;
    let mut micros: u64 = 0;
    let mut any = false;
    if !day_part.is_empty() {
        // `nD` — must be the whole day part, and a fraction forbids a
        // following time part.
        let len = day_part.find('D')?;
        let (v, frac) = component_number(&day_part[..len])?;
        if len + 1 != day_part.len() || (frac.is_some() && !time_part.is_empty()) {
            return None;
        }
        days = v;
        if let Some((f, fl)) = frac {
            let um = scale_frac(f, fl, 86_400_000_000);
            seconds += um / 1_000_000;
            micros += um % 1_000_000;
        }
        any = true;
    }
    let mut tp = time_part;
    for (unit, factor) in [(b'H', 3_600u64), (b'M', 60), (b'S', 1)] {
        let Some(i) = tp.find(unit as char) else {
            continue;
        };
        let (v, frac) = component_number(&tp[..i])?;
        seconds = seconds.checked_add(v.checked_mul(factor)?)?;
        tp = &tp[i + 1..];
        if let Some((f, fl)) = frac {
            if !tp.is_empty() {
                return None; // a fraction must sit on the last component
            }
            micros = micros.checked_add(scale_frac(f, fl, factor * 1_000_000))?;
        }
        any = true;
    }
    if !tp.is_empty() || !any {
        return None;
    }
    seconds = seconds.checked_add(micros / 1_000_000)?;
    micros %= 1_000_000;
    if seconds >= 86_400 {
        days = days.checked_add(seconds / 86_400)?;
        seconds %= 86_400;
    }
    Some(DurationParts {
        negative,
        days,
        seconds,
        micros,
    })
}

/// `frac / 10^frac_len × unit_micros`, as u64 micros (truncated below 1µs).
fn scale_frac(frac: u64, frac_len: u8, unit_micros: u64) -> u64 {
    let mut scale: u128 = u128::from(frac) * u128::from(unit_micros);
    for _ in 0..frac_len {
        scale /= 10;
    }
    u64::try_from(scale.min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

/// Parse a UUID: `8-4-4-4-12` hex (any case), `32` hex digits, `{…}`
/// braces, or `urn:uuid:…` — the forms `uuid.UUID(text)` accepts. Returns
/// the canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` lowercase form.
pub fn parse_uuid(text: &str) -> Option<String> {
    let t = text.trim();
    let t = t
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .unwrap_or(t);
    let t = t
        .strip_prefix("urn:uuid:")
        .or_else(|| t.strip_prefix("URN:UUID:"))
        .unwrap_or(t);
    let hex: &str = if t.len() == 36 {
        if ![8, 13, 18, 23]
            .iter()
            .all(|&i| t.as_bytes().get(i) == Some(&b'-'))
        {
            return None;
        }
        t
    } else if t.len() == 32 {
        t
    } else {
        return None;
    };
    if !hex.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return None;
    }
    let mut out = String::with_capacity(36);
    let mut i = 0usize;
    for c in hex.chars() {
        if c == '-' {
            continue;
        }
        if matches!(i, 8 | 12 | 16 | 20) {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
        i += 1;
    }
    if i != 32 {
        return None;
    }
    Some(out)
}

/// The pragmatic `Email` check (DESIGN.md §11.1): exactly one `@`,
/// non-empty local and domain parts, no whitespace or control characters.
/// Deliberately permissive — RFC 5321 quoting, IDN and length caps are
/// out of scope; tightening later does not change the IR.
pub fn check_email(text: &str) -> bool {
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return false;
    }
    !text
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
}

/// The pragmatic `Url` check: `<scheme>://<host>[…]` where `scheme` is
/// `[a-zA-Z][a-zA-Z0-9+.-]*` and `host` is a non-empty run up to `/`, `?`
/// or `#`. Userinfo, ports, paths, queries and fragments pass unchecked —
/// only `scheme://host` is verified. No whitespace or control characters.
/// Relative URLs and `scheme:`-without-authority forms are rejected.
pub fn check_url(text: &str) -> bool {
    if text
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return false;
    }
    let Some(colon) = text.find(':') else {
        return false;
    };
    let scheme = &text[..colon];
    if scheme.is_empty()
        || !scheme
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic())
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
    {
        return false;
    }
    let Some(rest) = text[colon + 1..].strip_prefix("//") else {
        return false;
    };
    let host_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    host_len > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_text() {
        assert_eq!(parse_bool_text("true"), Some(true));
        assert_eq!(parse_bool_text("FALSE"), Some(false));
        assert_eq!(parse_bool_text(" 1 "), Some(true));
        assert_eq!(parse_bool_text("off"), Some(false));
        assert_eq!(parse_bool_text("2"), None);
        assert_eq!(parse_bool_text(""), None);
    }

    #[test]
    fn float_text() {
        assert_eq!(parse_float_text("1.5"), Some(1.5));
        assert_eq!(parse_float_text("1e3"), Some(1000.0));
        assert_eq!(parse_float_text("inf"), Some(f64::INFINITY));
        assert_eq!(parse_float_text("-Infinity"), Some(f64::NEG_INFINITY));
        assert!(parse_float_text("nan").unwrap().is_nan());
        assert_eq!(parse_float_text("abc"), None);
    }

    #[test]
    fn dates() {
        assert_eq!(parse_iso_date("2024-02-29").unwrap(), "2024-02-29");
        assert!(parse_iso_date("2023-02-29").is_none()); // not a leap year
        assert!(parse_iso_date("2024-13-01").is_none());
        assert!(parse_iso_date("2024-00-10").is_none());
        assert!(parse_iso_date("2024-04-31").is_none());
        assert!(parse_iso_date("2024-1-1").is_none());
        assert!(parse_iso_date("10000-01-01").is_none());
        assert_eq!(parse_iso_date("0000-01-01").unwrap(), "0000-01-01");
    }

    #[test]
    fn times() {
        assert_eq!(parse_iso_time("23:59").unwrap(), "23:59:00");
        assert_eq!(parse_iso_time("23:59:58.123").unwrap(), "23:59:58.123");
        assert_eq!(parse_iso_time("23:59:58z").unwrap(), "23:59:58Z");
        assert_eq!(parse_iso_time("10:20:30+05:30").unwrap(), "10:20:30+05:30");
        assert_eq!(parse_iso_time("10:20:30-00:00").unwrap(), "10:20:30Z");
        assert!(parse_iso_time("24:00").is_none());
        assert!(parse_iso_time("23:59:60").is_none());
        assert!(parse_iso_time("23:59:58+24:00").is_none());
        assert!(parse_iso_time("12").is_none());
        assert!(parse_iso_time("12:30x").is_none());
    }

    #[test]
    fn datetimes() {
        assert_eq!(
            parse_rfc3339("2024-01-31T10:20:30Z").unwrap(),
            "2024-01-31T10:20:30Z"
        );
        assert_eq!(
            parse_rfc3339("2024-01-31t10:20:30.5z").unwrap(),
            "2024-01-31T10:20:30.5Z"
        );
        assert_eq!(
            parse_rfc3339("2024-01-31 10:20:30+02:00").unwrap(),
            "2024-01-31T10:20:30+02:00"
        );
        assert_eq!(
            parse_rfc3339("2024-01-31T10:20:30").unwrap(),
            "2024-01-31T10:20:30"
        );
        assert!(parse_rfc3339("2024-02-30T00:00:00Z").is_none());
        assert!(parse_rfc3339("2024-01-31T24:00:00Z").is_none());
        assert!(parse_rfc3339("2024-01-31").is_none());
        assert!(parse_rfc3339("2024-01-31T10:20:30+0200").is_none());
        assert!(parse_rfc3339("2024-01-31T10:20Z").is_none());
    }

    #[test]
    fn durations() {
        let d = parse_iso8601_duration("P3DT4H5M6S").unwrap();
        assert_eq!(
            (d.negative, d.days, d.seconds, d.micros),
            (false, 3, 4 * 3600 + 5 * 60 + 6, 0)
        );
        assert_eq!(d.to_canonical(), "P3DT4H5M6S");
        let d = parse_iso8601_duration("P2W").unwrap();
        assert_eq!((d.days, d.seconds), (14, 0));
        let d = parse_iso8601_duration("-PT0.5S").unwrap();
        assert_eq!((d.negative, d.seconds, d.micros), (true, 0, 500_000));
        let d = parse_iso8601_duration("PT1.5H").unwrap();
        assert_eq!((d.seconds, d.micros), (5400, 0)); // 1.5 h = 5400 s
        let d = parse_iso8601_duration("PT90M").unwrap();
        assert_eq!(d.seconds, 5400);
        let d = parse_iso8601_duration("PT1H30M").unwrap();
        assert_eq!(d.seconds, 5400);
        assert_eq!(d.to_canonical(), "P0DT1H30M0S");
        let d = parse_iso8601_duration("P1.5D").unwrap();
        assert_eq!((d.days, d.seconds), (1, 43_200));
        assert!(parse_iso8601_duration("P").is_none());
        assert!(parse_iso8601_duration("PT").is_none());
        assert!(parse_iso8601_duration("P3D2W").is_none());
        assert!(parse_iso8601_duration("P1.5D2W").is_none());
        assert!(parse_iso8601_duration("P1D2X").is_none());
        assert!(parse_iso8601_duration("T1H").is_none());
        assert!(parse_iso8601_duration("P1DT1.5H2M").is_none()); // frac not last
        assert!(parse_iso8601_duration("P1W2D").is_none()); // W must be alone
        assert!(parse_iso8601_duration("P1.5W").is_none());
    }

    #[test]
    fn uuids() {
        assert_eq!(
            parse_uuid("550E8400-E29B-41D4-A716-446655440000").unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            parse_uuid("550e8400e29b41d4a716446655440000").unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            parse_uuid("{550e8400-e29b-41d4-a716-446655440000}").unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            parse_uuid("urn:uuid:550e8400-e29b-41d4-a716-446655440000").unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert!(parse_uuid("550e8400-e29b-41d4-a716").is_none());
        assert!(parse_uuid("zz0e8400-e29b-41d4-a716-446655440000").is_none());
        assert!(parse_uuid("").is_none());
    }

    #[test]
    fn emails() {
        assert!(check_email("a@b.co"));
        assert!(check_email("user+tag@example.org"));
        assert!(!check_email("@b.co"));
        assert!(!check_email("a@"));
        assert!(!check_email("a@b@c"));
        assert!(!check_email("a b@c"));
        assert!(!check_email("a@b c"));
        assert!(!check_email("plainstring"));
    }

    #[test]
    fn urls() {
        assert!(check_url("https://example.com"));
        assert!(check_url("https://example.com:8080/p?q=1#f"));
        assert!(check_url("http://user:pw@h.co/x"));
        assert!(check_url("git+ssh://h.co/repo"));
        assert!(!check_url("example.com"));
        assert!(!check_url("https://"));
        assert!(!check_url("https:// "));
        assert!(!check_url("1ftp://h.co"));
        assert!(!check_url("ftp:h.co")); // no //
        assert!(!check_url("https://exa mple.com"));
    }
}
