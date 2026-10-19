//! Display formatting helpers (presentation conventions, not domain logic).
//! Pure text — shared by the GPUI and TUI frontends.

/// Compact token count: `>=1e8` → "3.5亿", `>=1e6` → "9.1M", `>=1e3` → "641K",
/// otherwise the raw number.
pub fn format_tokens_compact(v: u64) -> String {
    if v >= 100_000_000 {
        format!("{:.1}亿", v as f64 / 1e8)
    } else if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1e6)
    } else if v >= 1_000 {
        format!("{:.0}K", v as f64 / 1e3)
    } else {
        v.to_string()
    }
}

/// Cost in USD micros → "$12.34".
pub fn format_cost_usd(micros: u64) -> String {
    format!("${:.2}", micros as f64 / 1e6)
}

/// Full integer with thousands separators: `123456789` → "123,456,789".
/// Used where the compact M/K/亿 abbreviation must NOT be applied (e.g. the
/// floating usage ball shows the raw total on purpose).
pub fn format_int_grouped(v: u64) -> String {
    let s = v.to_string();
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n + n / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (n - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Chinese-unit compact count with **万 as the floor unit** — the floating
/// ball's docked **square** form (see `ui::floating`).
///
/// The unit is the largest of 亿 / 千万 / 百万 / 万 the count reaches, and the
/// mantissa is bumped up a unit only when rounding would lift it *to* 10
/// ("9,999,999" → "1千万", not "10百万"). A mantissa that is legitimately in
/// `[10, 100)` stays where it is, so "999,999" reads "100万" and "12,345" reads
/// "1.2万" — the form a Chinese reader expects, not a forced 百万 / 千万.
///
/// Below 万 the count keeps its 万 unit and goes fractional ("0.5万"): the unit
/// is never dropped, so the number stays comparable with every larger one.
///
/// One decimal, with a redundant ".0" (and other trailing zeros) dropped —
/// `10_000_000` → "1千万", `1_000_000` → "1百万", `12_345_678` → "1.2千万".
/// The sub-万 case carries two, so 500 tokens does not round up into "0.1万".
pub fn format_tokens_wan(v: u64) -> String {
    const UNITS: [(f64, &str); 4] = [(1e8, "亿"), (1e7, "千万"), (1e6, "百万"), (1e4, "万")];
    let tokens = v as f64;
    // Index of the largest unit the count reaches. 万 is the last entry and
    // therefore the floor: everything below it is reported in 万.
    let mut unit = UNITS
        .iter()
        .position(|(scale, _)| tokens >= *scale)
        .unwrap_or(UNITS.len() - 1);
    // A sub-10 mantissa that *rounds* to 10 belongs one unit up ("9.99百万" is
    // "1千万"). A mantissa already ≥ 10 is the normal `万` / `亿` reading and
    // must be left alone — bumping it would turn "12.3万" into "0.12百万".
    while unit > 0 {
        let mantissa = tokens / UNITS[unit].0;
        if mantissa < 10.0 && round1(mantissa) >= 10.0 {
            unit -= 1;
        } else {
            break;
        }
    }
    let (scale, suffix) = UNITS[unit];
    let mantissa = tokens / scale;
    let decimals = if mantissa < 1.0 { 2 } else { 1 };
    format!("{}{}", trimmed(mantissa, decimals), suffix)
}

/// `v` to `decimals` places, with redundant trailing zeros removed
/// (`1.00` → `1`, `0.50` → `0.5`).
fn trimmed(v: f64, decimals: usize) -> String {
    let text = format!("{:.1$}", v, decimals);
    match text.split_once('.') {
        Some((whole, frac)) => {
            let frac = frac.trim_end_matches('0');
            if frac.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{frac}")
            }
        }
        None => text,
    }
}

/// `v` rounded to one decimal place — used only to test a mantissa against the
/// 10 boundary, i.e. against the precision the string is actually printed at.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Compact token count for chart `f64` values. Token counts are integral, so
/// the truncating cast is lossless.
pub fn format_tokens_compact_f64(v: f64) -> String {
    format_tokens_compact(v as u64)
}

/// Cost in dollars (already converted from micros) → "$12.34".
pub fn format_cost_f64(v: f64) -> String {
    format!("${:.2}", v)
}

/// Percentage (0–100) → "85.3%".
pub fn format_percent_f64(v: f64) -> String {
    format!("{v:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_formats_by_magnitude() {
        assert_eq!(format_tokens_compact(0), "0");
        assert_eq!(format_tokens_compact(999), "999");
        assert_eq!(format_tokens_compact(641_000), "641K");
        assert_eq!(format_tokens_compact(9_100_000), "9.1M");
        assert_eq!(format_tokens_compact(350_000_000), "3.5亿");
    }

    #[test]
    fn cost_formats_usd() {
        assert_eq!(format_cost_usd(0), "$0.00");
        assert_eq!(format_cost_usd(317_490_000), "$317.49");
    }

    /// The docked square's number: 万 is the floor unit, the mantissa stays
    /// readable, and the unit only steps up when rounding crosses a unit
    /// boundary — never because the mantissa happens to be ≥ 10.
    #[test]
    fn wan_units_keep_the_mantissa_readable() {
        // Sub-万: still 万, fractional, never a smaller unit.
        assert_eq!(format_tokens_wan(0), "0万");
        assert_eq!(format_tokens_wan(500), "0.05万");
        assert_eq!(format_tokens_wan(5_000), "0.5万");
        assert_eq!(format_tokens_wan(9_999), "1万");
        // 万 range, including the legitimate ≥ 10 mantissa.
        assert_eq!(format_tokens_wan(10_000), "1万");
        assert_eq!(format_tokens_wan(12_345), "1.2万");
        assert_eq!(format_tokens_wan(123_456), "12.3万");
        assert_eq!(format_tokens_wan(999_999), "100万");
        // 百万 / 千万 / 亿, with the ".0" dropped.
        assert_eq!(format_tokens_wan(1_000_000), "1百万");
        assert_eq!(format_tokens_wan(1_234_567), "1.2百万");
        assert_eq!(format_tokens_wan(10_000_000), "1千万");
        assert_eq!(format_tokens_wan(12_345_678), "1.2千万");
        assert_eq!(format_tokens_wan(100_000_000), "1亿");
        assert_eq!(format_tokens_wan(999_999_999), "10亿");
        // Rounding *to* 10 is what steps up a unit...
        assert_eq!(format_tokens_wan(9_999_999), "1千万");
        assert_eq!(format_tokens_wan(99_999_999), "1亿");
    }

    /// Whatever the magnitude, the result is a short string that still names a
    /// unit — the docked square has no room for 20 characters.
    #[test]
    fn wan_output_is_always_short_and_unit_suffixed() {
        // Up to a trillion tokens — far past anything a scan can aggregate.
        for v in [0, 1, 999, 10_000, 1_000_000, 100_000_000, 1_000_000_000_000] {
            let text = format_tokens_wan(v);
            assert!(
                text.ends_with('万') || text.ends_with('亿'),
                "{text} lost its unit"
            );
            assert!(text.chars().count() <= 10, "{text} is too long");
        }
    }

    #[test]
    fn f64_variants_match_u64() {
        assert_eq!(format_tokens_compact_f64(9_100_000.0), "9.1M");
        assert_eq!(format_tokens_compact_f64(350_000_000.0), "3.5亿");
        assert_eq!(format_cost_f64(317.49), "$317.49");
    }
}
