//! The decisions behind WASAPI exclusive mode (`wasapi_out.rs`), kept free of
//! COM so they compile — and their tests run — on every platform. CI tests on
//! Linux, where `wasapi_out` itself does not even build.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

// AUDCLNT_E_* HRESULTs (values from windows-rs 0.62, Win32::Media::Audio).
pub const E_DEVICE_IN_USE: u32 = 0x8889_000A;
pub const E_BUFFER_SIZE_NOT_ALIGNED: u32 = 0x8889_0019;

/// Sample layouts tried, most precise first: (bits stored, bits used).
/// 24-in-32 is the common USB DAC layout; packed 24 and 16 are fallbacks.
pub const LAYOUTS: [(u16, u16); 4] = [(32, 32), (32, 24), (24, 24), (16, 16)];

/// Rates probed for exclusive-mode support.
pub const RATES: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// Another program holds the device exclusively: worth a plain message, not a
/// fallback that cannot help.
pub fn is_busy(code: Option<u32>) -> bool {
    code == Some(E_DEVICE_IN_USE)
}

/// Initialize refused the period; the documented recovery is to re-create the
/// client with the aligned buffer's period (Intel HDA).
pub fn needs_realign(code: Option<u32>) -> bool {
    code == Some(E_BUFFER_SIZE_NOT_ALIGNED)
}

/// An error line: the step, the HRESULT in hex, then Windows' text. AUDCLNT
/// codes often have no message text at all, so the code must be kept — a bare
/// "exclusive mode: open: " says nothing.
pub fn error_message(what: &str, code: Option<u32>, text: &str) -> String {
    match code {
        Some(h) => format!("{what}: {h:#010x} {text}"),
        None => format!("{what}: {text}"),
    }
    .trim_end()
    .to_string()
}

/// The most precise layout the device accepts.
pub fn pick_layout(mut accepts: impl FnMut((u16, u16)) -> bool) -> Option<(u16, u16)> {
    LAYOUTS.iter().copied().find(|&l| accepts(l))
}

/// One layout's answer to the exclusive format check.
pub enum Check {
    Ok,
    /// Accepted once the channel mask or WAVEFORMATEX form was adjusted.
    OkWithQuirks,
    /// Refused, with the HRESULT (or error text) of the plain check.
    Refused(String),
}

/// Header of the `--list-devices --verbose` exclusive-mode table.
pub fn report_header(channels: u16) -> String {
    format!("exclusive ({channels} ch): rate  check[32/32 32/24 24/24 16/16]  open")
}

/// One rate's row: every layout's check, then what a real open did.
pub fn report_row(rate: u32, checks: &[Check], opened: Result<(u16, u16), &str>) -> String {
    let checks: Vec<String> = checks
        .iter()
        .map(|c| match c {
            Check::Ok => "ok".to_string(),
            Check::OkWithQuirks => "ok*".to_string(),
            Check::Refused(code) => code.clone(),
        })
        .collect();
    let opened = match opened {
        Ok((store, valid)) => format!("ok ({store}/{valid})"),
        Err(e) => e.trim_start_matches("exclusive mode: ").to_string(),
    };
    format!("  {rate}: {}  {opened}", checks.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_their_hresult_even_without_text() {
        assert_eq!(error_message("exclusive mode: open", Some(0x8889_000F), ""), "exclusive mode: open: 0x8889000f");
        assert_eq!(
            error_message("exclusive mode: open", Some(0x8889_000A), "Device in use"),
            "exclusive mode: open: 0x8889000a Device in use"
        );
        assert_eq!(error_message("exclusive mode: device", None, "not found"), "exclusive mode: device: not found");
    }

    #[test]
    fn error_codes_are_classified() {
        assert!(is_busy(Some(0x8889_000A)));
        assert!(!is_busy(Some(0x8889_0008)) && !is_busy(None));
        assert!(needs_realign(Some(0x8889_0019)));
        assert!(!needs_realign(Some(0x8889_000A)));
    }

    #[test]
    fn the_most_precise_accepted_layout_wins() {
        assert_eq!(pick_layout(|_| true), Some((32, 32)));
        // The Scarlett: refuses 32/32, accepts packed 24 and 16.
        assert_eq!(pick_layout(|l| l == (24, 24) || l == (16, 16)), Some((24, 24)));
        // The FiiO KA17: refuses packed 24 only.
        assert_eq!(pick_layout(|l| l != (24, 24)), Some((32, 32)));
        assert_eq!(pick_layout(|_| false), None);
    }

    #[test]
    fn report_rows_read_like_the_diagnostic_that_found_the_driver_lock() {
        assert_eq!(report_header(2), "exclusive (2 ch): rate  check[32/32 32/24 24/24 16/16]  open");
        let refused = || Check::Refused("0x88890008".into());
        assert_eq!(
            report_row(44_100, &[refused(), refused(), refused(), refused()], Err("exclusive mode: open: 0x8889000f")),
            "  44100: 0x88890008 0x88890008 0x88890008 0x88890008  open: 0x8889000f"
        );
        assert_eq!(
            report_row(96_000, &[refused(), refused(), Check::Ok, Check::OkWithQuirks], Ok((24, 24))),
            "  96000: 0x88890008 0x88890008 ok ok*  ok (24/24)"
        );
    }
}
