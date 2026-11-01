//! Small shared helpers: home resolution, `{home}` templates, atomic
//! writes, RFC 3339 timestamps.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The operator's state root: `~/.local/state/aco`.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/aco")
}

pub fn home() -> Result<PathBuf> {
    std::env::home_dir().ok_or_else(|| Error::Usage("cannot resolve home directory".into()))
}

/// Expand an a4aa `{home}`-relative template.
pub fn expand_home(template: &str, home: &Path) -> PathBuf {
    match template.strip_prefix("{home}") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(template),
    }
}

/// RFC 3339 UTC timestamp for the current moment, without pulling a date
/// library: seconds since epoch → civil date via Howard Hinnant's
/// algorithm, formatted with millisecond precision dropped.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Date part of [`now_rfc3339`], for store-directory layout.
pub fn today() -> String {
    now_rfc3339()[..10].to_string()
}

/// Epoch milliseconds → RFC 3339 UTC with millisecond precision — the
/// timestamp shape session stores record per event.
pub fn epoch_ms_rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// RFC 3339 UTC (`…Z`) → epoch milliseconds — the inverse of
/// [`epoch_ms_rfc3339`] for stores that stamp events in milliseconds.
pub fn rfc3339_to_epoch_ms(ts: &str) -> Option<u64> {
    let ts = ts.strip_suffix('Z')?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    let (time, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = time.split(':');
    let (h, min, s) = (
        t.next()?.parse::<u64>().ok()?,
        t.next()?.parse::<u64>().ok()?,
        t.next()?.parse::<u64>().ok()?,
    );
    let frac = format!("{:0<3}", &frac[..frac.len().min(3)]);
    let millis = frac[..3].parse::<u64>().ok()?;
    let days = days_from_civil(y, m as u32, day as u32);
    Some(((days * 86_400 + (h * 3600 + min * 60 + s) as i64) * 1000 + millis as i64) as u64)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Write `contents` to `path` atomically (temp file in the same directory,
/// then rename). Creates parent directories.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::io(parent))?;
    }
    let tmp = path.with_extension(format!(
        "{}.aco-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    fs::write(&tmp, contents).map_err(Error::io(&tmp))?;
    fs::rename(&tmp, path).map_err(Error::io(&tmp))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_format() {
        let ts = now_rfc3339();
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn civil_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19000), (2022, 1, 8));
    }

    #[test]
    fn home_template() {
        let h = Path::new("/home/x");
        assert_eq!(
            expand_home("{home}/.codex/sessions", h),
            PathBuf::from("/home/x/.codex/sessions")
        );
    }
}
