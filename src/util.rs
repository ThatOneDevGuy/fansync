use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::time::Duration;

pub static VERBOSE: AtomicU8 = AtomicU8::new(0);

pub fn verbose() -> u8 {
    VERBOSE.load(Relaxed)
}

/// Writes one timestamped line to stderr.
#[macro_export]
macro_rules! log {
    ($($a:tt)*) => { $crate::util::log_line(format_args!($($a)*)) };
}

/// Like `log!`, but only with -v.
#[macro_export]
macro_rules! vlog {
    ($($a:tt)*) => { if $crate::util::verbose() >= 1 { $crate::util::log_line(format_args!($($a)*)) } };
}

/// Like `log!`, but only with -vv.
#[macro_export]
macro_rules! dlog {
    ($($a:tt)*) => { if $crate::util::verbose() >= 2 { $crate::util::log_line(format_args!($($a)*)) } };
}

pub fn log_line(args: fmt::Arguments) {
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        libc::localtime_r(&now, &mut tm);
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{:02}:{:02}:{:02} {}", tm.tm_hour, tm.tm_min, tm.tm_sec, args);
}

/// Bytes as a short human-readable size, e.g. "1.5 GiB".
pub fn size(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

pub fn dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if d.as_millis() >= 10_000 {
        format!("{s}s")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

/// A path relative to the top of a transfer, for messages. Like rsync,
/// control characters and bytes that aren't UTF-8 are shown as \#ooo.
pub fn show(rel: &[u8]) -> String {
    if rel.is_empty() {
        return ".".into();
    }
    let mut out = String::with_capacity(rel.len());
    for chunk in rel.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c.is_control() {
                let mut b = [0u8; 4];
                for byte in c.encode_utf8(&mut b).bytes() {
                    out.push_str(&format!("\\#{byte:03o}"));
                }
            } else {
                out.push(c);
            }
        }
        for byte in chunk.invalid() {
            out.push_str(&format!("\\#{byte:03o}"));
        }
    }
    out
}

/// Parses sizes like "64M", "1.5G", "4096" or "16KiB".
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("invalid size {s:?}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(format!("invalid size {s:?}")),
    };
    Ok((n * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert_eq!(super::parse_size("64M").unwrap(), 64 << 20);
        assert_eq!(super::parse_size("1.5G").unwrap(), 3 << 29);
        assert_eq!(super::parse_size("4096").unwrap(), 4096);
        assert_eq!(super::parse_size("16KiB").unwrap(), 16384);
        assert!(super::parse_size("x").is_err());
        assert_eq!(super::size(1536), "1.5 KiB");
        assert_eq!(super::show(b"a\nb\xff"), "a\\#012b\\#377");
    }
}
