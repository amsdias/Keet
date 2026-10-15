# EQ Profiles

A collection of ready-to-use parametric EQ presets in the app's JSON format:

```json
{
  "name": "Rock",
  "bands": [
    { "type": "low_shelf", "freq": 90, "gain": 3.0, "q": 0.71 }
  ]
}
```

- **type** — `peak`, `low_shelf`, `high_shelf`, `low_cut`, `high_cut`
- **freq** — corner/centre frequency, 20–20000 Hz
- **gain** — dB, clamped to ±12 (ignored by `low_cut`/`high_cut`)
- **q** — 0.3 (broad) … 10 (surgical); shelves/broad bells use ~0.71
- Max **10 bands** per profile; extras are ignored.

### Layout note

The in-app EQ editor is a **10-slot parametric display**: it shows band *N* of
the JSON in slot *N* (left-to-right), and any slots a preset leaves empty are
filled with 0 dB placeholders. So slot position follows **array order, not
frequency**.

To make the editor read naturally low→high, every profile here is authored as a
**full 10-band set in ascending frequency**, with `0.0 dB` peaks as placeholders
in the slots a preset doesn't actively touch. A boosted band therefore sits where
you'd visually expect it (e.g. Treble Boost rises on the right).

## Installing

The app loads custom presets from your config directory, not from `assets/`:

- **Windows:** `%APPDATA%\keet\eq\`
- **macOS / Linux:** `~/.config/keet/eq/`

Copy any `.json` file there and it appears in the EQ preset list (sorted by `name`).

```powershell
# Windows
Copy-Item assets\eq\*.json "$env:APPDATA\keet\eq\"
```
```bash
# macOS / Linux
mkdir -p ~/.config/keet/eq && cp assets/eq/*.json ~/.config/keet/eq/
```

## The profiles

| File | Focus |
|------|-------|
| **Genre** | |
| `rock` | Punchy lows, de-mudded low-mids, guitar presence, bright top |
| `pop` | Bright, vocal-forward, boosted bass and air |
| `jazz` | Warm and natural, softened upper-mids |
| `classical` | Near-flat, hall air, tamed string harshness |
| `electronic` | Big sub, clean mids, crisp highs (V-shape) |
| `hiphop` | Deep 808/kick bass, present vocals |
| `rnb` | Warm lows, smooth mids, silky highs |
| `metal` | Tight low-cut, scooped low-mids, aggressive bite |
| `acoustic` | Natural body with string sparkle |
| `country` | Warm, clear vocals, bright strings |
| `blues` | Midrange-forward, warm and smooth |
| `reggae` | Deep bass, relaxed highs |
| **Use-case** | |
| `vocal` | Voice clarity: de-boom + presence lift |
| `podcast` | Speech intelligibility, stronger low-cut |
| `bass-boost` | Pure low-end lift |
| `treble-boost` | High-frequency lift |
| `loudness` | Bass + treble smile for low-volume listening |
| `warm` | Tilt toward lows, roll off highs |
| `bright` | Lift presence and air |
| `late-night` | Cut bass, keep dialogue clear |
| `small-speakers` | Fake warmth on laptop/phone speakers, de-tinny |
| `headphone` | Harman-style bass shelf + ear-gain presence |
