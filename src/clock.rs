use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds. Deadlines use the wall clock so that time spent
/// asleep counts toward them.
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

pub fn now_iso() -> String {
    iso(now_ms())
}

/// UTC timestamp with millisecond precision, e.g. `2026-10-10T18:49:35.474Z`.
pub fn iso(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        ms.rem_euclid(1000)
    )
}

// Howard Hinnant's days-to-civil algorithm for the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

unsafe extern "C" {
    fn tzset();
}

/// Local time for commit subjects, e.g. `2026-10-10 13:49:35`.
pub fn local_stamp(ms: i64) -> String {
    // The type comes from localtime_r: naming libc::time_t is deprecated on musl.
    let seconds = ms.div_euclid(1000) as _;
    // SAFETY: an all-zero `tm` is a valid value for localtime_r to overwrite.
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes only into `parts`. tzset reads TZ, and this
    // process never modifies its own environment, so there is no data race.
    let converted = unsafe {
        tzset();
        !libc::localtime_r(&seconds, &mut parts).is_null()
    };

    if !converted {
        return iso(ms)[..19].replace('T', " ");
    }

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        i64::from(parts.tm_year) + 1900,
        parts.tm_mon + 1,
        parts.tm_mday,
        parts.tm_hour,
        parts.tm_min,
        parts.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats_utc_with_milliseconds() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(951_827_696_789), "2000-02-29T12:34:56.789Z");
        assert_eq!(iso(1_791_658_175_474), "2026-10-10T18:49:35.474Z");
        assert_eq!(iso(-1000), "1969-12-31T23:59:59.000Z");
    }

    #[test]
    fn local_stamp_has_commit_subject_shape() {
        let stamp = local_stamp(now_ms());
        let bytes = stamp.as_bytes();
        assert_eq!(stamp.len(), 19, "{stamp}");
        for (index, byte) in bytes.iter().enumerate() {
            match index {
                4 | 7 => assert_eq!(*byte, b'-'),
                10 => assert_eq!(*byte, b' '),
                13 | 16 => assert_eq!(*byte, b':'),
                _ => assert!(byte.is_ascii_digit(), "{stamp}"),
            }
        }
    }
}
