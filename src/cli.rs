//! Command-line arguments, parsed by hand (no clap dependency).

use std::path::PathBuf;

use crate::state::RgMode;
use crate::theme::ThemeKind;

/// What the command line asked for.
#[derive(Debug, PartialEq)]
pub struct Options {
    /// `--help`: print usage and exit.
    pub help: bool,
    /// `--list-devices` (with `--verbose`): print the output devices and exit.
    pub list_devices: bool,
    pub verbose: bool,
    /// No arguments at all: resume the saved session (or ask for a source).
    pub resume: bool,
    /// Files, folders and M3U playlists to play.
    pub sources: Vec<PathBuf>,
    pub shuffle: bool,
    pub repeat: bool,
    pub hq_resampler: bool,
    pub eq: Option<String>,
    pub fx: Option<String>,
    pub crossfade_secs: u32,
    pub rg_mode: RgMode,
    pub device: Option<String>,
    pub exclusive: bool,
    pub cover: bool,
    pub theme: Option<ThemeKind>,
    /// A `--theme` value that names no theme (warned about, then ignored).
    pub unknown_theme: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            help: false,
            list_devices: false,
            verbose: false,
            resume: false,
            sources: Vec::new(),
            shuffle: false,
            repeat: false,
            hq_resampler: false,
            eq: None,
            fx: None,
            crossfade_secs: 0,
            rg_mode: RgMode::Track,
            device: None,
            exclusive: false,
            cover: true,
            theme: None,
            unknown_theme: None,
        }
    }
}

/// Parse `args` (including the program name at index 0). Sequential: an
/// option's value is consumed with it and never read as a flag itself.
/// `--help` and `--list-devices` win wherever they appear, mistakes and all.
pub fn parse(args: &[String]) -> Result<Options, String> {
    let rest = args.get(1..).unwrap_or_default();
    let mut o = Options {
        help: rest.iter().any(|a| a == "--help" || a == "-h"),
        list_devices: rest.iter().any(|a| a == "--list-devices"),
        verbose: rest.iter().any(|a| a == "--verbose"),
        resume: rest.is_empty(),
        ..Options::default()
    };
    if o.help || o.list_devices || o.resume {
        return Ok(o);
    }

    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned();
        match arg.as_str() {
            "--shuffle" | "-s" => o.shuffle = true,
            "--repeat" | "-r" => o.repeat = true,
            "--quality" | "-q" => o.hq_resampler = true,
            "--exclusive" => o.exclusive = true,
            "--no-cover" => o.cover = false,
            "--verbose" => {}
            "--eq" | "-e" => o.eq = value(),
            "--fx" => o.fx = value(),
            "--crossfade" | "-x" => o.crossfade_secs = value().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--rg-mode" => {
                o.rg_mode = match value().map(|v| v.to_lowercase()).as_deref() {
                    Some("album") => RgMode::Album,
                    Some("off") => RgMode::Off,
                    _ => RgMode::Track,
                }
            }
            "--device" => o.device = value(),
            "--theme" => {
                if let Some(v) = value() {
                    match ThemeKind::from_str(&v) {
                        Some(t) => o.theme = Some(t),
                        None => o.unknown_theme = Some(v),
                    }
                }
            }
            a if a.starts_with("--") || (a.starts_with('-') && a.len() == 2) => {
                return Err(format!("Unknown option: {a}"));
            }
            a => o.sources.push(PathBuf::from(a)),
        }
    }
    if o.sources.is_empty() {
        return Err("No input files or folders specified".into());
    }
    Ok(o)
}

/// Every key, by view: the one list `--help` prints and the `?` screen shows,
/// so the two cannot drift apart (the help said `E` cycled EQ presets long
/// after it had come to open the EQ editor).
pub const KEYS: &[(&str, &[(&str, &str)])] = &[
    ("PLAYER", &[
        ("Space", "Pause / resume"),
        ("Up / Down", "Next / previous track"),
        ("Right / Left", "Seek forward / backward 10 s"),
        ("+ / -", "Volume up / down (5% steps, 0–150%)"),
        ("v", "Next visualization"),
        ("b", "Visualization style (dots / bars)"),
        ("Shift+F", "Full-window visualization"),
        ("Shift+L", "More detail (VU history, legend)"),
        ("f", "Pre/post-fader metering"),
        ("e", "EQ editor"),
        ("x", "Next effects preset"),
        ("c", "Next crossfeed preset"),
        ("[ / ]", "Balance left / right (5% steps)"),
        ("l", "Playlist"),
        ("y", "Lyrics"),
        ("?", "This list"),
        ("z", "Shuffle on / off"),
        ("Shift+R", "Repeat (off → all → one)"),
        ("s", "Save the playlist as M3U"),
        ("r", "Rescan the folders for new files"),
        ("o", "Open a new source (type a path)"),
        ("p", "Pick a source (folder dialog)"),
        ("t", "Next theme"),
        ("i", "CPU / memory stats"),
        ("q", "Quit (or Esc twice)"),
    ]),
    ("PLAYLIST  (l)", &[
        ("Up / Down", "Move the cursor"),
        ("Home / End", "Top / bottom (also g / G)"),
        ("PgUp / PgDn", "Page up / down (also Ctrl+U/D)"),
        ("Enter", "Play the selected track"),
        ("a", "Queue it (plays next)"),
        ("d / Delete", "Remove the selected track"),
        ("/", "Search"),
        ("Shift+S", "Sort by artist, album, track"),
        ("Tab", "Artist / album tree"),
        ("Esc / l", "Close"),
    ]),
    ("LYRICS  (y)", &[
        ("w / s", "Scroll (stops following the song)"),
        ("a / d", "Sync −/+ 0.5 s (kept per track)"),
        ("0", "Reset the sync offset"),
        ("Esc / y", "Close"),
    ]),
    ("EQ EDITOR  (e)", &[
        ("Left / Right", "Select a band"),
        ("Up / Down", "Gain ±0.5 dB (Shift: ±0.1 dB)"),
        ("t / T", "Filter type"),
        (", / .", "Q broader / narrower"),
        ("< / >", "Frequency down / up"),
        ("[ / ]", "Previous / next preset"),
        ("0", "Reset the band"),
        ("a", "Preamp the headroom row suggests"),
        ("Esc / e", "Close"),
    ]),
];

/// The `--help` text.
pub fn print_help() {
    println!("\x1B[1mKeet\x1B[0m — Terminal audio player with real-time visualization and parametric EQ");
    println!();
    println!("\x1B[1mUSAGE\x1B[0m");
    println!("  keet <file|folder|playlist>... [options]");
    println!("  keet                              Resume last session");
    println!();
    println!("\x1B[1mOPTIONS\x1B[0m");
    println!("  -s, --shuffle          Randomize playlist order (re-shuffles on each repeat)");
    println!("  -r, --repeat           Loop playlist (rescans sources for new files each cycle)");
    println!("  -q, --quality          HQ resampler (higher CPU, inaudible difference)");
    println!("  -e, --eq <name|path>   Start with EQ preset by name or JSON file path");
    println!("      --fx <name|path>   Start with effects preset by name or JSON file path");
    println!("  -x, --crossfade <secs> Crossfade duration between tracks (0 = disabled)");
    println!("      --rg-mode <mode>   ReplayGain: track (default), album, or off");
    println!("      --device <name>    Output device: its id (--list-devices), else its name");
    println!("                         (exact before part of it, any case)");
    println!("      --exclusive        Exclusive mode: bit-perfect, per-track sample rate, device lock");
    println!("                         (macOS: any device; Linux: a card's hw: device, see --list-devices)");
    println!("      --no-cover         Disable album cover display");
    println!("      --theme <name>     UI theme: classic (default), minimal, hifi");
    println!("      --list-devices     List available output devices and exit");
    println!("      --verbose          With --list-devices: every device, with its formats");
    println!("  -h, --help             Show this help");
    println!();
    println!("\x1B[1mFORMATS\x1B[0m  MP3, FLAC, WAV, OGG, AAC/M4A, ALAC, AIFF");
    println!();
    for (title, keys) in KEYS {
        println!("\x1B[1m{title}\x1B[0m");
        for (key, what) in *keys {
            println!("  {key:<14} {what}");
        }
        println!();
    }
    println!("\x1B[1mCUSTOM PRESETS\x1B[0m");
    println!("  EQ:      ~/.config/keet/eq/*.json");
    println!("  Effects: ~/.config/keet/effects/*.json");
    println!("  Crossfeed: ~/.config/keet/crossfeed/*.json");
    println!();
    println!("\x1B[1mCONFIG\x1B[0m");
    println!("  ~/.config/keet/config.json — persistent defaults, e.g. {{\"theme\": \"minimal\"}}");
    println!("  {{\"classic_use_truecolor\": true, \"classic_colors\": {{\"highlight\": \"#7DD3B8\"}}}}");
    println!("               Classic in your own colours (also \"warning\", \"error\"); default ANSI");
    println!("  NO_COLOR=1   No colour (symbols, bold and reverse video carry every state)");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(line: &str) -> Result<Options, String> {
        let args: Vec<String> = std::iter::once("keet").chain(line.split_whitespace()).map(String::from).collect();
        parse(&args)
    }

    #[test]
    fn no_arguments_resumes_the_saved_session() {
        let o = opts("").unwrap();
        assert!(o.resume && o.sources.is_empty());
        assert_eq!(o.rg_mode, RgMode::Track);
        assert!(o.cover && !o.exclusive && !o.shuffle);
    }

    #[test]
    fn sources_and_flags_in_any_order() {
        let o = opts("~/A -s --exclusive ~/B list.m3u --repeat -q --no-cover").unwrap();
        assert_eq!(o.sources, ["~/A", "~/B", "list.m3u"].map(PathBuf::from));
        assert!(o.shuffle && o.repeat && o.hq_resampler && o.exclusive && !o.cover && !o.resume);
    }

    #[test]
    fn value_options_take_the_next_argument() {
        let o = opts("--eq Rock --fx Hall -x 5 --rg-mode ALBUM --device FiiO --theme hifi song.flac").unwrap();
        assert_eq!(o.eq.as_deref(), Some("Rock"));
        assert_eq!(o.fx.as_deref(), Some("Hall"));
        assert_eq!(o.crossfade_secs, 5);
        assert_eq!(o.rg_mode, RgMode::Album);
        assert_eq!(o.device.as_deref(), Some("FiiO"));
        assert_eq!(o.theme, Some(ThemeKind::HiFi));
        assert_eq!(o.sources, [PathBuf::from("song.flac")]);
    }

    #[test]
    fn a_value_is_never_also_read_as_a_flag() {
        // Every flag used to be found by scanning the whole line, so this
        // named a device "--exclusive" AND turned exclusive mode on.
        let o = opts("--device --exclusive song.flac").unwrap();
        assert_eq!(o.device.as_deref(), Some("--exclusive"));
        assert!(!o.exclusive);
    }

    #[test]
    fn lenient_values_fall_back_to_defaults() {
        let o = opts("-x soon --rg-mode loud --theme neon a.mp3").unwrap();
        assert_eq!(o.crossfade_secs, 0);
        assert_eq!(o.rg_mode, RgMode::Track);
        assert_eq!(o.theme, None);
        assert_eq!(o.unknown_theme.as_deref(), Some("neon"));
        assert_eq!(opts("--rg-mode off a.mp3").unwrap().rg_mode, RgMode::Off);
    }

    #[test]
    fn mistakes_are_reported() {
        assert_eq!(opts("--shufle a.mp3").unwrap_err(), "Unknown option: --shufle");
        assert_eq!(opts("-z a.mp3").unwrap_err(), "Unknown option: -z");
        assert_eq!(opts("--exclusive").unwrap_err(), "No input files or folders specified");
        // A trailing option with its value missing is not a source.
        assert_eq!(opts("a.mp3 --eq").unwrap().eq, None);
    }

    #[test]
    fn help_and_device_listing_win_wherever_they_appear() {
        assert!(opts("a.mp3 -h").unwrap().help);
        assert!(opts("--bogus --help").unwrap().help, "help even beside a mistake");
        let o = opts("--list-devices --verbose").unwrap();
        assert!(o.list_devices && o.verbose);
    }
}
