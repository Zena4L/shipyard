//! Sizes in Docker's units (go-units' `HumanSize` and `BytesSize`), with Go's
//! `%g` formatting of the number.

/// A byte count as Docker's `HumanSizeWithPrecision` writes it: decimal
/// units (`kB`, `MB`…), `precision` significant digits (Go's `%g`).
pub(crate) fn human_size(bytes: u64, precision: usize) -> String {
    go_units(
        bytes,
        1000.0,
        &["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"],
        precision,
    )
}

/// A byte count as Docker's `BytesSize` writes it: binary units (`KiB`,
/// `GiB`), 4 significant digits (`11.73GiB`).
pub(crate) fn binary_size(bytes: u64) -> String {
    go_units(
        bytes,
        1024.0,
        &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"],
        4,
    )
}

/// `bytes` in the largest of `units` (each `base` times the last) under
/// which it's at least 1, to `precision` significant digits: go-units'
/// `CustomSize`.
fn go_units(bytes: u64, base: f64, units: &[&str], precision: usize) -> String {
    #[allow(
        clippy::cast_precision_loss,
        reason = "display only, to a few significant digits"
    )]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= base && unit + 1 < units.len() {
        size /= base;
        unit += 1;
    }
    format!(
        "{}{}",
        go_g(size, precision),
        units.get(unit).unwrap_or(&"")
    )
}

/// `value` as Go's `%.{precision}g` writes it: that many significant
/// digits, no trailing zeros, an exponent only when it's large or small.
fn go_g(value: f64, precision: usize) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let precision = precision.max(1);
    // Rounded to its significant digits first: that decides the exponent.
    let scientific = format!("{value:.*e}", precision - 1);
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let trimmed = |text: &str| -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text.to_owned()
        }
    };
    if exponent < -4 || exponent >= i32::try_from(precision).unwrap_or(i32::MAX) {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{}e{sign}{:02}", trimmed(mantissa), exponent.unsigned_abs());
    }
    let decimals =
        usize::try_from(i32::try_from(precision).unwrap_or(0) - 1 - exponent).unwrap_or(0);
    trimmed(&format!("{value:.decimals$}"))
}

#[cfg(test)]
mod tests;
