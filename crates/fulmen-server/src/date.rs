//! `Date` header caching (`docs/DESIGN.md` §25.2).
//!
//! Formatting an IMF-fixdate costs a timestamp decomposition per call;
//! since the header only has one-second resolution, a single shared value
//! is refreshed by a background task once per second and cloned per
//! response (`HeaderValue` clones are refcount bumps).

use std::sync::{PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use http::HeaderValue;

const WEEKDAYS: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
const MONTHS: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];
const LEN: usize = 29; // "Mon, 02 Jan 2006 15:04:05 GMT"

/// Shared, cached `Date` header value.
#[derive(Debug)]
pub(crate) struct DateCache {
    value: RwLock<HeaderValue>,
}

impl DateCache {
    pub(crate) fn new() -> Self {
        Self {
            value: RwLock::new(render()),
        }
    }

    /// Current cached value. Returned [`HeaderValue`] clones are cheap.
    pub(crate) fn get(&self) -> HeaderValue {
        self.value
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Re-render the cached value from the wall clock.
    pub(crate) fn refresh(&self) {
        *self.value.write().unwrap_or_else(PoisonError::into_inner) = render();
    }

    /// Current value as bytes, for code paths that write a raw response.
    pub(crate) fn bytes(&self) -> [u8; LEN] {
        let mut out = [0u8; LEN];
        out.copy_from_slice(self.get().as_bytes());
        out
    }
}

fn render() -> HeaderValue {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut buf = [0u8; LEN];
    format_http_date(secs, &mut buf);
    // The buffer is always exactly one IMF-fixdate, which is valid ASCII.
    match HeaderValue::from_bytes(&buf) {
        Ok(value) => value,
        Err(_) => HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"),
    }
}

/// Write `secs` (Unix time) as an IMF-fixdate into `out`.
///
/// `Sun, 06 Nov 1994 08:49:37 GMT` — day/month decomposition via Howard
/// Hinnant's `civil_from_days`, no library needed.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "every operand is range-checked by construction: day<32, month<12, h/m/s<60, year in 0..=9999"
)]
fn format_http_date(secs: u64, out: &mut [u8; LEN]) {
    let secs = i64::try_from(secs).unwrap_or(i64::MAX).max(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let weekday = (days + 4).rem_euclid(7) as usize; // 1970-01-01 was Thursday

    // civil_from_days (Hinnant, public domain algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);

    out[0..3].copy_from_slice(WEEKDAYS[weekday]);
    out[3] = b',';
    out[4] = b' ';
    two(&mut out[5..7], day as u8);
    out[7] = b' ';
    out[8..11].copy_from_slice(MONTHS[(month - 1) as usize]);
    out[11] = b' ';
    four(&mut out[12..16], year.clamp(0, 9999) as u16);
    out[16] = b' ';
    two(&mut out[17..19], (tod / 3600) as u8);
    out[19] = b':';
    two(&mut out[20..22], (tod % 3600 / 60) as u8);
    out[22] = b':';
    two(&mut out[23..25], (tod % 60) as u8);
    out[25..29].copy_from_slice(b" GMT");
}

fn two(out: &mut [u8], v: u8) {
    out[0] = b'0' + v / 10;
    out[1] = b'0' + v % 10;
}

fn four(out: &mut [u8], v: u16) {
    out[0] = b'0' + (v / 1000) as u8;
    out[1] = b'0' + (v / 100 % 10) as u8;
    out[2] = b'0' + (v / 10 % 10) as u8;
    out[3] = b'0' + (v % 10) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(secs: u64) -> String {
        let mut buf = [0u8; LEN];
        format_http_date(secs, &mut buf);
        String::from_utf8(buf.to_vec()).unwrap()
    }

    #[test]
    fn known_dates() {
        assert_eq!(format(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(format(1_000_000_000), "Sun, 09 Sep 2001 01:46:40 GMT");
        assert_eq!(format(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
        assert_eq!(format(4_102_444_799), "Thu, 31 Dec 2099 23:59:59 GMT");
        // Leap day and year boundaries.
        assert_eq!(format(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(format(946_684_800), "Sat, 01 Jan 2000 00:00:00 GMT");
    }

    #[test]
    fn cache_returns_valid_header() {
        let cache = DateCache::new();
        let value = cache.get();
        assert_eq!(value.as_bytes().len(), LEN);
        cache.refresh();
        assert_eq!(cache.bytes().len(), LEN);
    }
}
