//! Display formatting helpers.

/// Binary size with adaptive precision, e.g. "1,4 GB" style but dot-separated.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0usize;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    let prec = if v < 10.0 {
        2
    } else if v < 100.0 {
        1
    } else {
        0
    };
    format!("{v:.prec$} {}", UNITS[u])
}

/// Thousands-separated integer.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(*b as char);
    }
    out
}

pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0,0 %".into();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p >= 10.0 {
        format!("{p:.1} %").replace('.', ",")
    } else {
        format!("{p:.2} %").replace('.', ",")
    }
}

pub fn duration(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.2} s", ms as f64 / 1000.0)
    } else {
        format!("{}:{:02} min", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Unix seconds -> "YYYY-MM-DD HH:MM" (UTC-based civil date, no chrono dependency).
pub fn timestamp(secs: u32) -> String {
    if secs == 0 {
        return "-".into();
    }
    let (y, mo, d, h, mi) = civil_from_unix(secs as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// Howard Hinnant's civil-from-days algorithm.
pub fn civil_from_unix(secs: i64) -> (i32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32)
}

/// Parses "100mb", "1.5g", "900k", "1234" into bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.replace(',', ".").parse().ok()?;
    let mul: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0f64.powi(3),
        "t" | "tb" | "tib" => 1024.0f64.powi(4),
        _ => return None,
    };
    Some((num * mul) as u64)
}

/// Parses "2024-01-31" / "2024-01" / "2024" into unix seconds (start of period).
pub fn parse_date(s: &str) -> Option<i64> {
    let p: Vec<&str> = s.trim().split(['-', '.', '/']).collect();
    let y: i32 = p.first()?.parse().ok()?;
    let mo: u32 = p.get(1).and_then(|v| v.parse().ok()).unwrap_or(1);
    let d: u32 = p.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400)
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
