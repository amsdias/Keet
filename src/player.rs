//! The playback loop. `main` sets everything up and hands it to a [`Player`];
//! the loop is then `start_track` once per track start (a skip, a jump, a
//! respawn after a rate change or a device recovery) and `tick` once per pass
//! of the steady-state loop. Each step answers with a [`Flow`] saying whether
//! playback goes on, starts the next track, or ends.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait};

use crate::audio;
use crate::cover;
use crate::crossfeed::{self, CrossfeedPreset};
use crate::decode::{await_consumer_drain, decode_playlist};
use crate::effects::{self, EffectsPreset};
use crate::eq::{self, EqPreset};
use crate::media_keys;
use crate::playlist::{next_cycle, shuffle_list};
use crate::resume::ResumeState;
use crate::state::{self, PlayerState, UiState, VizMode, VIZ_BUFFER_SIZE};
use crate::theme;
use crate::ui::{self, arm_auto_sort, format_time, poll_auto_sort, poll_input, poll_library_tree, print_status};
use crate::viz::{StatsMonitor, VizAnalyser};
use crate::{
    build_resume_state, locked_rate_note, open_output, output_failed, rate_label,
    repaint_if_needed, spawn_cover_worker, spawn_lyrics_worker, PRODUCER_THREAD,
};

/// What the loop does next.
pub enum Flow {
    /// Carry on with the current track.
    Continue,
    /// (Re)start playback at `ui.current` — `start_track` runs again.
    NextTrack,
    /// Stop: quit, the end of the playlist, or a thread that died.
    Quit,
}

/// Everything the playback loop works with.
///
/// Field order matters for drop order (first declared, first dropped): the
/// stream goes BEFORE `device_restore`, so a DAC's format is only ever put
/// back with no stream open (see audio::DeviceRestore).
pub struct Player {
    // --- output ---
    pub stream: Option<audio::Output>,
    pub device_restore: audio::DeviceRestore,
    pub device: cpal::Device,
    /// The ring's producer end. The producer thread owns it while it runs and
    /// hands it back when joined.
    pub prod: Option<rtrb::Producer<f32>>,
    pub viz_cons: rtrb::Consumer<f32>,
    pub stream_rate: u32,
    pub out_channels: u16,
    pub buffer_size: cpal::BufferSize,
    pub host: cpal::Host,

    // --- the session ---
    pub state: Arc<PlayerState>,
    pub ui: UiState,
    pub playlist: Vec<PathBuf>,
    pub eq_presets: Arc<Vec<EqPreset>>,
    pub fx_presets: Arc<Vec<EffectsPreset>>,
    pub cf_presets: Arc<Vec<CrossfeedPreset>>,
    pub hq_resampler: bool,
    pub crossfade_secs: u32,
    /// `--device` (or the resumed one): the output does not follow the
    /// system default then.
    pub device_arg: Option<String>,
    /// Resume-state writes go to a saver thread; None once closed for exit.
    pub save_tx: Option<Sender<ResumeState>>,
    pub media_controls: Option<souvlaki::MediaControls>,
    pub stats: StatsMonitor,
    /// Where to start the next track (seconds): a resumed session, or the
    /// point a device recovery interrupted.
    pub resume_position: i64,

    // --- frame state ---
    pub prev_frame_lines: usize,
    /// The Kitty analysis-spectrogram image was placed last frame (so it can
    /// be deleted, by id, when the user switches away from that mode).
    pub prev_viz_image_shown: bool,
    pub last_transition_count: usize,
    /// The output device's name as shown, and the poll that keeps it honest —
    /// the OS can move playback without telling us.
    pub shown_device_name: String,
    pub last_device_poll: Instant,
    /// Media-key now-playing throttle (see `render_frame`).
    pub last_mk_push: Instant,
    pub last_mk_paused: bool,

    // --- the track playing ---
    producer: Option<JoinHandle<rtrb::Producer<f32>>>,
    filename: String,
    track_ext: String,
    track_info: String,
    viz_analyser: VizAnalyser,
    viz_scratch: Vec<f32>,
    last_ui: Instant,
}

/// What `main` hands over to start playback.
pub struct PlayerSetup {
    pub stream: audio::Output,
    pub device_restore: audio::DeviceRestore,
    pub device: cpal::Device,
    pub prod: rtrb::Producer<f32>,
    pub viz_cons: rtrb::Consumer<f32>,
    pub stream_rate: u32,
    pub out_channels: u16,
    pub buffer_size: cpal::BufferSize,
    pub host: cpal::Host,
    pub state: Arc<PlayerState>,
    pub ui: UiState,
    pub playlist: Vec<PathBuf>,
    pub eq_presets: Arc<Vec<EqPreset>>,
    pub fx_presets: Arc<Vec<EffectsPreset>>,
    pub cf_presets: Arc<Vec<CrossfeedPreset>>,
    pub hq_resampler: bool,
    pub crossfade_secs: u32,
    pub device_arg: Option<String>,
    pub save_tx: Sender<ResumeState>,
    pub media_controls: Option<souvlaki::MediaControls>,
    pub resume_position: i64,
    pub shown_device_name: String,
}

impl Player {
    pub fn new(s: PlayerSetup) -> Self {
        let stream_rate = s.stream_rate;
        Self {
            stream: Some(s.stream),
            device_restore: s.device_restore,
            device: s.device,
            prod: Some(s.prod),
            viz_cons: s.viz_cons,
            stream_rate,
            out_channels: s.out_channels,
            buffer_size: s.buffer_size,
            host: s.host,
            state: s.state,
            ui: s.ui,
            playlist: s.playlist,
            eq_presets: s.eq_presets,
            fx_presets: s.fx_presets,
            cf_presets: s.cf_presets,
            hq_resampler: s.hq_resampler,
            crossfade_secs: s.crossfade_secs,
            device_arg: s.device_arg,
            save_tx: Some(s.save_tx),
            media_controls: s.media_controls,
            stats: StatsMonitor::new(),
            resume_position: s.resume_position,
            prev_frame_lines: usize::MAX,
            prev_viz_image_shown: false,
            last_transition_count: 0,
            shown_device_name: s.shown_device_name,
            last_device_poll: Instant::now(),
            last_mk_push: Instant::now() - Duration::from_secs(2),
            last_mk_paused: false,
            producer: None,
            filename: String::new(),
            track_ext: String::new(),
            track_info: String::new(),
            viz_analyser: VizAnalyser::new(stream_rate),
            viz_scratch: Vec::with_capacity(VIZ_BUFFER_SIZE),
            last_ui: Instant::now(),
        }
    }

    /// Run until quit or the end of the playlist.
    pub fn run(&mut self) {
        'playlist: loop {
            match self.start_track() {
                Flow::Quit => break,
                Flow::NextTrack => continue,
                Flow::Continue => {}
            }
            loop {
                match self.tick() {
                    Flow::Continue => {}
                    Flow::NextTrack => continue 'playlist,
                    Flow::Quit => break 'playlist,
                }
            }
        }
    }

    /// Queue a resume-state save (written off the main thread).
    fn save(&self) {
        if let Some(tx) = &self.save_tx {
            let _ = tx.send(build_resume_state(
                &self.ui, &self.playlist, &self.state,
                &self.eq_presets, &self.fx_presets, &self.cf_presets, &self.device_arg,
            ));
        }
    }

    /// Stop queueing saves, so the saver thread can finish and be joined.
    pub fn close_saves(&mut self) {
        self.save_tx = None;
    }

    /// Give the output back: close the stream, then restore every device
    /// format exclusive mode changed and release hog mode — in that order.
    pub fn release_output(&mut self) {
        self.stream = None;
        self.device_restore.restore();
    }

    /// Join the producer thread, taking its ring end back. False if the
    /// thread died (nothing to recover: quit).
    fn join_producer(&mut self) -> bool {
        match self.producer.take().map(JoinHandle::join) {
            Some(Ok(p)) => {
                self.prod = Some(p);
                true
            }
            Some(Err(_)) => false,
            None => true,
        }
    }

    /// Repaint the whole screen when the layout changed (resize, theme, full
    /// window), or show the too-small screen.
    fn repaint_screen(&mut self) {
        let (w, h) = crossterm::terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((0, 0));
        if crate::ui::window_too_small(w, h) {
            if !self.ui.too_small {
                // Entering: wipe the frame and every image, and forget that
                // any image block is still in place.
                self.ui.too_small = true;
                if matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty) {
                    crate::term::out!("{}{}", cover::kitty_clear_escape(), cover::viz_image_clear_escape());
                }
                crate::term::out!("\x1B[0m\x1B[H\x1B[J");
                self.ui.cover_block_intact = false;
                self.ui.cover_dirty_frame = true;
                self.ui.spectro_block_intact = false;
                self.prev_viz_image_shown = false;
            }
            return;
        }
        if self.ui.too_small {
            // Leaving: a full repaint.
            self.ui.too_small = false;
            self.ui.terminal_resized = true;
        }
        if repaint_if_needed(&mut self.ui) {
            self.prev_frame_lines = usize::MAX;
            self.prev_viz_image_shown = false; // the repaint cleared any viz image
        }
    }

    /// The status block, drawn as one frame.
    fn draw_status(&mut self) {
        if self.ui.too_small {
            crate::ui::print_too_small(&self.state, &self.ui, &self.filename);
            return;
        }
        let current_eq = &self.eq_presets[self.state.eq_index()];
        let current_fx = &self.fx_presets[self.state.effects_index()].name;
        let current_cf = &self.cf_presets[self.state.crossfeed_index()].name;
        self.prev_frame_lines = print_status(
            &self.state, &mut self.ui, &self.filename, &self.track_info, &self.track_ext,
            current_eq, current_fx, current_cf, &mut self.stats, self.prev_frame_lines,
            &self.playlist, &self.viz_analyser,
        );
    }

    /// One frame for the wait loops (startup buffering, rate change): a full
    /// repaint if one is due, then the status block, as one synchronized
    /// update (DEC mode 2026: presented atomically, so terminals — notably
    /// Windows Terminal — don't show the erase-then-repaint as flicker).
    fn paint_wait_frame(&mut self) {
        crate::term::out!("\x1B[?2026h");
        self.repaint_screen();
        self.draw_status();
        crate::term::out!("\x1B[?2026l");
        crate::term::flush();
    }

    /// A theme switch changes the cover slot's cell size, and half-block and
    /// Sixel bake that in at decode time — so re-decode rather than stretch.
    /// Cheap: local sources are hit first and the worker is generation-counted
    /// like any other cover load.
    fn reload_cover_if_resized(&mut self) {
        if self.ui.cover_resize_pending {
            self.ui.cover_resize_pending = false;
            if let Some(path) = self.playlist.get(self.ui.current).cloned() {
                spawn_cover_worker(&mut self.ui, path, cover::CoverSize::for_theme(self.state.theme_kind()));
            }
        }
    }

    /// "03:42 • 16bit stereo • 44100Hz" for the track the producer published.
    fn build_track_info(&self) -> String {
        let state = &self.state;
        let src_rate = state.sample_rate.load(Ordering::Relaxed) as u32;
        let channels = state.channels.load(Ordering::Relaxed);
        let bits = state.bits_per_sample.load(Ordering::Relaxed);
        let ch_str = match channels {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{}ch", n),
        };
        let rate_str = rate_label(src_rate, self.stream_rate, state);
        format!("{} • {}bit {} • {}", format_time(state.total_secs()), bits, ch_str, rate_str)
    }

    /// Push the current track's title/artist/album to the OS now-playing
    /// widget. The cache supplies them once the scan has reached this track;
    /// until then the display name stands in for the title (the same fallback
    /// the UI itself uses).
    fn publish_now_playing(&mut self) {
        if let Some(ref mut mc) = self.media_controls {
            let (mk_artist, mk_album) = self.ui.metadata_cache.artist_album(self.ui.current);
            let mk_title = self.ui.metadata_cache.title(self.ui.current).unwrap_or_else(|| self.filename.clone());
            media_keys::update_metadata(mc, &mk_title, mk_artist.as_deref(), mk_album.as_deref(), self.state.total_secs());
            media_keys::update_playback(mc, self.state.is_paused(), 0.0);
        }
    }

    // ------------------------------------------------------------------------
    // Track start
    // ------------------------------------------------------------------------

    /// Start playback at `ui.current`: the repeat cycle at the end of the
    /// list, the device rate for the track (exclusive mode), a new producer,
    /// then the wait for its first audio.
    fn start_track(&mut self) -> Flow {
        if self.state.should_quit() {
            return Flow::Quit;
        }
        if self.ui.current >= self.playlist.len() && !self.repeat_cycle() {
            return Flow::Quit;
        }

        // Reset state for new producer
        let state = Arc::clone(&self.state);
        state.current_track.store(self.ui.current, Ordering::Relaxed);
        state.producer_done.store(false, Ordering::Relaxed);
        // A seek still pending here was aimed at audio a finished producer no
        // longer plays (pressed during a rate-change drain, or just before a
        // skip/jump). Left in place, the new producer would apply it to a
        // different track. A resume position is re-issued below, after this.
        state.take_seek();
        state.track_info_ready.store(false, Ordering::Relaxed);
        state.skip_next.store(false, Ordering::Relaxed);
        state.skip_prev.store(false, Ordering::Relaxed);
        state.buffer_level.store(0, Ordering::Relaxed);
        if let Ok(mut err) = state.decode_error.lock() { *err = None; }

        self.match_rate_to_track();

        let track_path = &self.playlist[self.ui.current];
        self.filename = self.ui.metadata_cache.display_name(self.ui.current, track_path);
        self.track_ext = track_path.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        self.spawn_producer();

        // Stage 1: wait for the producer to open the file and publish track info
        // (fast, usually < 50ms). Once this is set, sample rate / bits / duration
        // are available so we can build track_info and show the new status line
        // while the buffer fills underneath us.
        while !state.track_info_ready.load(Ordering::Relaxed)
              && !state.producer_done.load(Ordering::Relaxed)
              && !state.should_quit()
        {
            poll_input(&state, &mut self.ui, &mut self.playlist);
            thread::sleep(Duration::from_millis(10));
        }

        // If producer failed before track info, skip
        if state.producer_done.load(Ordering::Relaxed)
           && !state.track_info_ready.load(Ordering::Relaxed)
        {
            if !self.join_producer() {
                return Flow::Quit;
            }
            let err_msg = state.decode_error.lock().ok().and_then(|mut e| e.take());
            if let Some(msg) = err_msg {
                self.ui.set_status(format!("Skip: {}", msg));
            }
            self.ui.current += 1;
            // Force a full redraw so the next track's status line starts clean
            // instead of leaving orphan lines from the previous render.
            self.ui.terminal_resized = true;
            self.prev_frame_lines = usize::MAX;
            return Flow::NextTrack;
        }

        // Resume: seek to saved position (only on first track after resume)
        if self.resume_position > 0 {
            state.seek(self.resume_position);
            self.resume_position = 0;
        }

        self.track_info = self.build_track_info();

        // Load lyrics off the main thread so skip stays responsive.
        let dur = { let t = state.total_secs(); if t > 0.0 { Some(t as u32) } else { None } };
        self.ui.lyrics_scroll = 0;
        self.ui.lyrics_auto_scroll = true;
        let lyrics_path = self.playlist[self.ui.current].clone();
        spawn_lyrics_worker(&mut self.ui, lyrics_path.clone(), dur);
        spawn_cover_worker(&mut self.ui, lyrics_path, cover::CoverSize::for_theme(state.theme_kind()));

        // Visualization analyzer (created before the startup wait so print_status
        // can draw the waveform/lissajous/spectrogram viz modes during buffering).
        self.viz_analyser = VizAnalyser::new(self.stream_rate);
        self.viz_scratch.clear();

        // Stage 2: wait for the ring buffer to fill enough that the audio callback
        // won't underrun, while refreshing the status line so the user sees the new
        // track name immediately instead of staring at the old one.
        //
        // Wait for ~1 second of audio in the ring before entering the
        // steady-state loop, so the already-running callback can't underrun
        // right after the track starts. Using stream_rate (rather than a
        // fraction of the raw ring size) keeps the cushion consistent
        // across output rates.
        let startup_threshold = self.stream_rate as usize * 2;
        // Also stop on a rate-change request: that exit does not set
        // producer_done, so a track shorter than the 1 s threshold followed
        // by one at another rate never filled the ring and stalled here —
        // the loop that acts on the request is the one this wait gates.
        // And on a stream error: a dead (or unopened) output never drains
        // the ring, so the buffer level never rises and this waited
        // forever — the recovery block that acts on it is below.
        while state.buffer_level.load(Ordering::Relaxed) < startup_threshold
              && !state.producer_done.load(Ordering::Relaxed)
              && !state.rate_change_needed.load(Ordering::Relaxed)
              && !state.stream_error.load(Ordering::Relaxed)
              && !state.should_quit()
        {
            poll_input(&state, &mut self.ui, &mut self.playlist);
            // A theme switch here must re-decode the cover for the new slot
            // (Minimal drew the 20x10 Classic cover into its 18-col slot).
            self.reload_cover_if_resized();
            self.paint_wait_frame();
            thread::sleep(Duration::from_millis(20));
        }

        self.publish_now_playing();
        self.last_ui = Instant::now();
        Flow::Continue
    }

    /// The end of the playlist was reached. With repeat on, rebuild the list
    /// (folders are rescanned for new files) and start over; false = stop.
    /// The rebuild is disk I/O and runs on a worker; the UI keeps taking
    /// input and painting meanwhile, and quit still works.
    fn repeat_cycle(&mut self) -> bool {
        if self.ui.repeat_mode == state::RepeatMode::Off {
            return false;
        }
        let (sources, current, removed) =
            (self.ui.source_paths.clone(), self.playlist.clone(), self.ui.removed_paths.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(next_cycle(&sources, &current, &removed));
        });
        let mut next = loop {
            match rx.try_recv() {
                Ok(list) => break list,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break self.playlist.clone(),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    poll_input(&self.state, &mut self.ui, &mut self.playlist);
                    if self.state.should_quit() {
                        return false;
                    }
                    self.paint_wait_frame();
                    thread::sleep(Duration::from_millis(20));
                }
            }
        };

        // Everything gone (in-app removals + files deleted on disk): nothing
        // left to play. Without this guard the track fetch would index into
        // an empty playlist and panic.
        if next.is_empty() {
            return false;
        }
        if self.ui.shuffle {
            shuffle_list(&mut next);
        }
        let old_playlist = std::mem::replace(&mut self.playlist, next);
        self.state.total_tracks.store(self.playlist.len(), Ordering::Relaxed);

        // Reindex metadata cache
        ui::reindex_and_restart_scan(&mut self.ui, &self.playlist, &old_playlist);
        // Re-arm the artist→album auto-sort: the rebuild above is
        // path-ordered, so without this a folder played on repeat-all
        // reverts to filename order instead of staying tag-sorted.
        arm_auto_sort(&mut self.ui);

        self.ui.current = 0;
        true
    }

    /// Exclusive mode: match the device to the track about to start. The
    /// producer only checks the rate at a NATURAL track change, so a skip
    /// back, a playlist jump or a restart after recovery began the new
    /// producer at the previous track's rate — resampled, e.g. song B played
    /// at song C's rate after skipping back. The ring is empty here (every
    /// respawn path drains it first), so the stream can be rebuilt.
    fn match_rate_to_track(&mut self) {
        if !self.state.exclusive.load(Ordering::Relaxed) || self.stream.is_none() {
            return;
        }
        let Some(track_rate) = audio::probe_sample_rate(&self.playlist[self.ui.current]) else { return };
        let target = self.state.exclusive_target_rate(track_rate, self.stream_rate);
        if target != self.stream_rate {
            // Stream first, then the rate (see handle_rate_change): no stream
            // may be alive while the rate changes.
            self.stream = None;
            self.reopen_output(self.stream_rate, target);
        }
    }

    /// Open the output at `target` (see `open_output`); on failure say so and
    /// leave recovery to retry.
    fn reopen_output(&mut self, current_rate: u32, target: u32) {
        match open_output(&self.device, current_rate, target, self.out_channels, self.buffer_size, &self.state) {
            Ok((p, v, s, rate)) => {
                self.prod = Some(p);
                self.viz_cons = v;
                self.stream = Some(s);
                self.stream_rate = rate;
            }
            Err(e) => output_failed(&mut self.ui, &self.state, e.as_ref()),
        }
    }

    /// Spawn the producer thread (continuous — decodes multiple tracks).
    fn spawn_producer(&mut self) {
        let playlist_snapshot = self.playlist.clone();
        let start_idx = self.ui.current;
        let state_clone = Arc::clone(&self.state);
        let eq_presets_clone = Arc::clone(&self.eq_presets);
        let fx_presets_clone = Arc::clone(&self.fx_presets);
        let cf_presets_clone = Arc::clone(&self.cf_presets);
        let hq = self.hq_resampler;
        let sr = self.stream_rate;
        let xfade = self.crossfade_secs;
        let mut prod_for_thread = self.prod.take().expect("the ring's producer end is home between producers");

        let handle = thread::Builder::new().name(PRODUCER_THREAD.into()).spawn(move || {
            let mut eq_chain = eq::EqChain::new();
            if state_clone.is_eq_custom() {
                eq_chain.load_bands(&state_clone.eq_bands_array(), state_clone.eq_preamp_db(), sr as f32);
            } else {
                eq_chain.load_preset(&eq_presets_clone[state_clone.eq_index()], sr as f32);
            }
            let mut fx_chain = effects::EffectsChain::new(sr as f32);
            fx_chain.load_preset(&fx_presets_clone[state_clone.effects_index()], sr as f32);
            let mut cf_filter = crossfeed::CrossfeedFilter::new();
            cf_filter.load_preset(&cf_presets_clone[state_clone.crossfeed_index()], sr as f32);

            // A malformed file can panic inside symphonia. Uncaught, the thread
            // died without setting producer_done and main sat in silence
            // forever. Caught, the file is reported and playback moves on to
            // the next track (main's jump handler respawns the producer there).
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decode_playlist(
                    &playlist_snapshot, start_idx,
                    &mut prod_for_thread, &state_clone, sr, hq,
                    &mut eq_chain, &eq_presets_clone,
                    &mut fx_chain, &fx_presets_clone,
                    xfade,
                    &mut cf_filter, &cf_presets_clone,
                );
            }));
            if run.is_err() {
                let idx = state_clone.producer_decoding.load(Ordering::Relaxed);
                let name = playlist_snapshot
                    .get(idx)
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Ok(mut err) = state_clone.decode_error.lock() {
                    *err = Some(format!("{name}: decoder crashed, skipped"));
                }
                if idx + 1 < playlist_snapshot.len() {
                    state_clone.jump_to(idx + 1);
                } else {
                    state_clone.producer_done.store(true, Ordering::Relaxed);
                }
            }
            prod_for_thread // Return producer ownership
        }).expect("spawn producer thread");
        self.producer = Some(handle);
    }

    // ------------------------------------------------------------------------
    // Steady state
    // ------------------------------------------------------------------------

    /// One pass of the playback loop: input, background results, then the
    /// events that end or restart the track, then a frame.
    fn tick(&mut self) -> Flow {
        // Input. Also honor a quit that was raised while this loop wasn't
        // watching (the stage-1/2 buffering waits poll input but discard
        // the quit return) — otherwise Q during buffering keeps playing
        // until the ring drains, or hangs entirely when paused.
        if poll_input(&self.state, &mut self.ui, &mut self.playlist) || self.state.should_quit() {
            self.quit();
            return Flow::Quit;
        }

        // Fire the one-shot artist→album auto-sort once the metadata scan
        // has loaded tags (no-op until armed + scan finished + not shuffling).
        poll_auto_sort(&self.state, &mut self.ui, &mut self.playlist);
        // Apply a background rescan (R) once its disk scan has finished.
        ui::poll_rescan(&self.state, &mut self.ui, &mut self.playlist);
        // Keep the library tree fresh while it's showing (no-op otherwise).
        poll_library_tree(&mut self.ui, &self.playlist);
        self.reload_cover_if_resized();

        self.handle_transition();
        for handler in [Self::handle_jump, Self::handle_rate_change, Self::handle_recovery, Self::handle_producer_done] {
            match handler(self) {
                Flow::Continue => {}
                other => return other,
            }
        }

        // UI update. The analysis spectrogram is the one continuously-scrolling
        // mode; match the render cadence to its (sample-rate-adaptive) column
        // rate so it advances one column per frame, evenly. Other modes stay at
        // 20fps. The loop sleep below uses the SAME value, which keeps the cadence
        // even — an unequal sleep/interval is what made it judder before.
        let analysis_viz = self.state.viz_mode() == VizMode::SpectrogramAnalysis;
        let frame_ms: u64 = if analysis_viz {
            state::spectro_frame_ms(self.state.output_rate.load(Ordering::Relaxed))
        } else {
            50
        };
        if self.last_ui.elapsed() >= Duration::from_millis(frame_ms) {
            self.render_frame();
            self.last_ui = Instant::now();
        }

        media_keys::poll();
        thread::sleep(Duration::from_millis(frame_ms));
        Flow::Continue
    }

    /// The quit key: clear the frame, save the session, and — in exclusive
    /// mode — give the DAC back as it was found.
    fn quit(&mut self) {
        crate::term::out!("\x1B[?25h");
        if self.prev_frame_lines != usize::MAX && self.prev_frame_lines > 0 {
            // Back to the frame's anchor line, then erase down to wipe the
            // whole frame.
            crate::term::out!("\x1B[{}F", self.prev_frame_lines);
        }
        crate::term::out!("\x1B[J");
        crate::term::flush();
        self.save();
        // Stop the stream BEFORE giving the device back. Releasing hog mode
        // under running IO buzzed, and a format change under a live stream is
        // what StreamInvalidated reports; restore the format while hog mode is
        // still ours (no other app sees the in-between state), then release
        // it. Also taken when hog mode was refused but the rate/bit depth
        // were still changed.
        if self.device_restore.pending() {
            if let Some(s) = self.stream.take() {
                let _ = s.pause();
            }
            self.device_restore.restore();
        }
        // The producer exits once it sees the quit flag.
        if let Some(h) = self.producer.take() {
            let _ = h.join();
        }
    }

    /// Check for track transitions from the producer.
    fn handle_transition(&mut self) {
        let state = Arc::clone(&self.state);
        let current_count = state.track_transition_count.load(Ordering::Acquire);
        if current_count == self.last_transition_count {
            return;
        }
        let new_index = state.producer_track_index.load(Ordering::Relaxed);
        self.last_transition_count = current_count;

        // Surface mid-playlist decode failures. The producer skips a
        // bad file and signals the next track; without this the error
        // text it stored was never shown anywhere.
        let skip_err = state.decode_error.lock().ok().and_then(|mut e| e.take());
        if let Some(msg) = skip_err {
            self.ui.set_status(format!("Skip: {}", msg));
        }

        // Playlist was modified — producer's new_index is from the stale snapshot.
        // Schedule a jump to the right track; skip the rest of this transition so we
        // don't display/fetch-lyrics for the wrong file. The jump_to_track check on the
        // next loop iteration will respawn the producer with the fresh playlist.
        if self.ui.playlist_dirty {
            self.ui.playlist_dirty = false;
            let target = ui::track_after_edit(
                self.ui.current,
                self.ui.removed_current_next.take(),
                self.playlist.len(),
                self.ui.repeat_mode,
            );
            state.jump_to(target);
        } else if new_index < self.playlist.len() {
            let ui = &mut self.ui;
            ui.current = new_index;
            ui.enqueue_count = 0;
            state.current_track.store(ui.current, Ordering::Relaxed);

            if ui.view_mode == state::ViewMode::Playlist && ui.filtered_indices.is_empty() {
                ui.cursor = ui.current;
            }

            // Update display info for new track
            let new_path = self.playlist[ui.current].clone();
            self.filename = ui.metadata_cache.display_name(ui.current, &new_path);
            self.track_ext = new_path.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();

            ui.lyrics_scroll = 0;
            ui.lyrics_auto_scroll = true;
            let dur = { let t = state.total_secs(); if t > 0.0 { Some(t as u32) } else { None } };
            spawn_lyrics_worker(ui, new_path.clone(), dur);
            spawn_cover_worker(ui, new_path, cover::CoverSize::for_theme(state.theme_kind()));

            self.track_info = self.build_track_info();
            self.publish_now_playing();
            self.save();
        }
    }

    /// Skip-prev or jump: join the producer, drain the ring, restart there.
    fn handle_jump(&mut self) -> Flow {
        let state = Arc::clone(&self.state);
        if !(state.skip_prev.load(Ordering::Relaxed) || state.jump_to_track.load(Ordering::Relaxed) >= 0) {
            return Flow::Continue;
        }
        if !self.join_producer() {
            return Flow::Quit;
        }
        // Flush ring buffer, and wait for the callback to actually
        // consume the drain request before the respawned producer can
        // push — otherwise the drain may fire late and discard the new
        // track's first samples.
        let free = self.prod.as_ref().map_or(0, |p| p.slots());
        if state.ring_capacity.load(Ordering::Relaxed) - free > 0 {
            state.reset_consumer_counter.store(true, Ordering::Release);
            await_consumer_drain(&state);
        }
        // The respawn clears decode_error, so show it first — a producer that
        // caught a decoder crash jumps here to skip.
        if let Some(msg) = state.decode_error.lock().ok().and_then(|mut e| e.take()) {
            self.ui.set_status(format!("Skip: {msg}"));
        }
        if let Some(target) = state.take_jump() {
            self.ui.current = target;
        } else if state.take_skip_prev() {
            self.ui.current = self.ui.current.saturating_sub(1);
        }
        self.ui.enqueue_count = 0;
        Flow::NextTrack
    }

    /// Exclusive mode: the producer reached a track at another sample rate.
    fn handle_rate_change(&mut self) -> Flow {
        let state = Arc::clone(&self.state);
        if !state.rate_change_needed.swap(false, Ordering::Relaxed) {
            return Flow::Continue;
        }
        // Wait for the buffer to drain so the current track finishes before we
        // tear the stream down. A paused stream never drains, so wait out the
        // pause instead of bailing — bailing here would truncate the buffered
        // tail and click. The rate switch simply defers until playback resumes.
        //
        // The wait keeps the UI alive: it polls input (so pause/unpause
        // and quit work — a paused wait used to be unbreakable from the
        // keyboard, and raw mode swallows Ctrl+C) and keeps painting.
        // It also bails on a stream error: a dead callback never drains
        // the ring, so an unplug during the tail used to hang here
        // forever. Either bail falls through WITHOUT the rate switch —
        // quit is handled at the top of the next pass, and the stream
        // error by the recovery handler, which restarts the current track
        // where it was.
        while !state.should_quit()
            && !state.stream_error.load(Ordering::Relaxed)
            && (state.is_paused() || state.buffer_level.load(Ordering::Relaxed) > 0)
        {
            poll_input(&state, &mut self.ui, &mut self.playlist);
            self.paint_wait_frame();
            thread::sleep(Duration::from_millis(20));
        }
        if state.should_quit() || state.stream_error.load(Ordering::Relaxed) {
            return Flow::Continue;
        }
        // Kept: if the new stream cannot be opened, the next producer still
        // needs a ring to fill while recovery retries.
        if !self.join_producer() {
            return Flow::Quit;
        }

        // Drop the old stream BEFORE changing the device rate: a stream alive
        // during the change gets StreamInvalidated from cpal's rate listener,
        // which ran a full recovery ("output moved", restart at the last
        // whole second) on every switch.
        self.stream = None;
        // The same rule the producer used to predict this switch
        // (state::RateCaps::resolve): the file's rate, else a whole-number
        // ratio of it, else the next rate up — never just capped to the
        // device maximum (352.8k -> 192k).
        let new_rate = state.next_track_rate.load(Ordering::Relaxed);
        let target_rate = state.exclusive_target_rate(new_rate, self.stream_rate);
        self.reopen_output(self.stream_rate, target_rate);

        // Continue the playlist from the track that needs the new rate.
        let new_idx = state.producer_track_index.load(Ordering::Relaxed);
        if new_idx < self.playlist.len() {
            self.ui.current = new_idx;
        }
        Flow::NextTrack
    }

    /// Stream error recovery (device disconnected, AirPods removed, etc.). A
    /// failed attempt to open the output schedules the next one a second out
    /// (output_failed) rather than hammering the device.
    fn handle_recovery(&mut self) -> Flow {
        let state = Arc::clone(&self.state);
        let retry_due = self.ui.recovery_retry_at.is_none_or(|t| Instant::now() >= t);
        if !(retry_due && state.stream_error.swap(false, Ordering::Relaxed)) {
            return Flow::Continue;
        }
        self.ui.recovery_retry_at = None;
        // Normal mode follows the current default output device. Exclusive
        // mode stays on ITS device while that device exists (chasing the
        // default is what split the stream from the hogged device); only if
        // it is gone does it move to the new default, pinned again.
        let replacement = if state.exclusive.load(Ordering::Relaxed) {
            audio::pin_device(&self.host, &self.device).or_else(|| {
                self.host.default_output_device()
                    .and_then(|d| audio::prepare_exclusive_device(&self.host, &d).ok())
            })
        } else {
            self.host.default_output_device()
        };
        let Some(new_device) = replacement else {
            // No default device right now — Windows can report none for a
            // while after a USB DAC is yanked. The swap above already consumed
            // the flag; put it back so we retry on the next frame instead of
            // abandoning recovery forever. The loop keeps polling input, so
            // quit stays responsive.
            state.stream_error.store(true, Ordering::Relaxed);
            self.ui.set_status("audio device lost — waiting for an output device".to_string());
            return Flow::Continue;
        };

        // Recovery tears down the producer and restarts the track from 0:00.
        // Capture where we were so it can seek back: changing output device
        // should not lose your place in the song. Read before the teardown —
        // the producer zeroes samples_played on restart.
        let resume_at = state.time_secs().floor().max(0.0) as i64;
        // Signal the producer to exit — it may be stuck in the buffer-full
        // sleep loop since the audio callback stopped draining the ring.
        state.jump_to(self.ui.current);
        // Kept for the next producer should this attempt fail.
        if !self.join_producer() {
            return Flow::Quit;
        }
        // Consume the unstick signal: it was only set to break the old
        // producer out of its buffer-full sleep. Leaving it set would make the
        // producer respawned next peek jump_to_track >= 0 and exit
        // immediately, wasting a spawn/join cycle before playback resumes.
        state.take_jump();
        self.stream = None;

        let moved = self.device.id().ok() != new_device.id().ok();
        self.device = new_device;

        // Re-label the header: recovery has moved playback to a different
        // endpoint, and it still names the one that was unplugged.
        if let Ok(desc) = self.device.description() {
            let new_name = desc.name().to_string();
            self.ui.device_name = new_name.clone();
            if moved {
                self.ui.set_status(format!("output moved to {new_name}"));
            }
        }
        // The new device's own channel count: carrying the old one over made
        // a shared-mode WASAPI stream on a device with another layout fail to
        // open.
        self.out_channels = self.device
            .default_output_config()
            .map(|c| c.channels())
            .unwrap_or(2)
            .max(1);
        let device_rate = self.device.default_output_config()
            .map(|c| c.sample_rate())
            .unwrap_or(48000);

        // Re-acquire exclusive (hog) mode on the new device if it was active
        // before the disconnect. The old hog refers to the (likely gone)
        // previous device — release is best-effort and harmless if the device
        // no longer exists.
        let target_rate = if state.exclusive.load(Ordering::Relaxed) {
            if let Some(old_id) = self.device_restore.hog.take() {
                audio::release_exclusive_mode(old_id);
            }
            // Its format is about to change: remember it first, so quitting
            // puts THIS device back too.
            self.device_restore.capture(&self.device);
            if let Ok(Some(id)) = audio::set_exclusive_mode(&self.device) {
                self.device_restore.hog = Some(id);
            }
            // A new device means new reachable rates.
            if let Ok(mut caps) = state.exclusive_caps.lock() {
                *caps = audio::probe_rate_caps(&self.device);
            }
            if let Some(note) = locked_rate_note(&state, &self.device) {
                self.ui.set_status_for(note, Duration::from_secs(10));
            }
            // Straight to the current track's rate: opening at the device's
            // default first meant a second reopen at the track start a moment
            // later.
            self.playlist
                .get(self.ui.current)
                .and_then(|p| audio::probe_sample_rate(p))
                .map_or(device_rate, |r| state.exclusive_target_rate(r, device_rate))
        } else {
            device_rate
        };

        self.reopen_output(device_rate, target_rate);
        // Resume the current track where it left off. After a failed attempt
        // nothing plays, so the clock stays here and the next attempt resumes
        // at the same point.
        self.resume_position = resume_at;
        Flow::NextTrack
    }

    /// Producer done (playlist exhausted or error) and the ring played out.
    fn handle_producer_done(&mut self) -> Flow {
        if !(self.state.producer_done.load(Ordering::Relaxed)
            && self.state.buffer_level.load(Ordering::Relaxed) == 0)
        {
            return Flow::Continue;
        }
        thread::sleep(Duration::from_millis(200));
        if !self.join_producer() {
            return Flow::Quit;
        }
        self.save();
        self.ui.current = self.playlist.len(); // Will trigger repeat-cycle or exit
        Flow::NextTrack
    }

    /// One UI frame: feed the analyser, collect background results, draw.
    fn render_frame(&mut self) {
        let state = Arc::clone(&self.state);
        if state.viz_mode() != VizMode::None {
            let viz_available = self.viz_cons.slots();
            if viz_available > 0 {
                if let Ok(chunk) = self.viz_cons.read_chunk(viz_available) {
                    let (first, second) = chunk.as_slices();
                    self.viz_scratch.clear();
                    self.viz_scratch.extend_from_slice(first);
                    self.viz_scratch.extend_from_slice(second);
                    chunk.commit_all();
                    self.viz_analyser.process(&self.viz_scratch, 2, &state);
                }
            }
        } else {
            let viz_available = self.viz_cons.slots();
            if viz_available > 0 {
                if let Ok(chunk) = self.viz_cons.read_chunk(viz_available) {
                    chunk.commit_all();
                }
            }
        }

        // Minimal keeps cpu/mem in its SIGNAL panel permanently, so the
        // sampler has to run there regardless of the `I` toggle.
        if state.show_stats() || state.theme_kind() == theme::ThemeKind::Minimal {
            self.stats.update();
        }

        self.follow_default_device();

        // Check if background lyrics fetch has completed
        if let Some(ref rx) = self.ui.lyrics_receiver {
            if let Ok(lyrics) = rx.try_recv() {
                if let Some((parsed, source)) = lyrics {
                    self.ui.lyrics = Some(parsed);
                    self.ui.lyrics_source = Some(source);
                }
                self.ui.lyrics_receiver = None;
            }
        }

        // Check if background cover fetch has completed
        if let Some(ref rx) = self.ui.cover_receiver {
            if let Ok(cover) = rx.try_recv() {
                self.ui.cover = cover;
                self.ui.cover_receiver = None;
                // Minimal draws the cover per-frame with emit-on-change,
                // so a newly arrived image needs exactly one repaint.
                // The renderers draw the cover per frame, emit-on-change:
                // a newly arrived image needs exactly one repaint.
                self.ui.cover_dirty_frame = true;
            }
        }

        // Begin synchronized update (DEC mode 2026) before any frame output —
        // including the full repaint below, which also runs on every viz
        // mode/style key — so the erase-then-repaint is presented atomically.
        // Closed after the status block. Ignored by terminals that don't
        // support it.
        crate::term::out!("\x1B[?2026h");
        self.repaint_screen();

        // Refresh filename from the metadata cache once the background
        // scan has caught up (replaces the raw filename fallback shown
        // right after a skip).
        if self.ui.current < self.playlist.len() {
            let fresh = self.ui.metadata_cache.display_name(self.ui.current, &self.playlist[self.ui.current]);
            if fresh != self.filename {
                self.filename = fresh;
            }
        }

        // Delete the Kitty analysis-spectrogram image (by id) when it's no
        // longer being drawn — leaving the mode OR switching away from the
        // Player view (playlist/lyrics). The image is a graphics overlay, so
        // unlike the text viz it isn't painted over by the new view.
        let viz_image_shown = self.ui.view_mode == state::ViewMode::Player
            && state.viz_mode() == VizMode::SpectrogramAnalysis
            && matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty);
        if self.prev_viz_image_shown && !viz_image_shown {
            crate::term::out!("{}", cover::viz_image_clear_escape());
        }
        self.prev_viz_image_shown = viz_image_shown;

        self.draw_status();
        crate::term::out!("\x1B[?2026l");
        crate::term::flush();

        // OS now-playing refresh: pushing one every UI frame (~20 Hz) is
        // needless objc/D-Bus traffic. Pause-state changes go out immediately;
        // the position otherwise syncs at ~1 Hz.
        if let Some(ref mut mc) = self.media_controls {
            let paused_now = state.is_paused();
            if paused_now != self.last_mk_paused
                || self.last_mk_push.elapsed() >= Duration::from_secs(1)
            {
                media_keys::update_playback(mc, paused_now, state.time_secs());
                self.last_mk_paused = paused_now;
                self.last_mk_push = Instant::now();
            }
        }
    }

    /// Detect an endpoint change the stream never reported.
    ///
    /// Unplugging a device does not always raise a stream error: WASAPI
    /// shared mode reroutes the stream to the new default endpoint
    /// transparently, so playback carries on and the error callback never
    /// fires — leaving the header naming a device that is no longer producing
    /// sound. Only meaningful when we follow the default (an explicit
    /// --device stays put), and polled at ~1 Hz because enumerating endpoints
    /// is a COM call.
    fn follow_default_device(&mut self) {
        // The OS rerouted our default-device stream (cpal DeviceChanged, e.g.
        // AirPods connecting). The stream is STILL PLAYING on the new output
        // — rebuilding it (the old behaviour) restarted the track at the last
        // whole second. Only our device handle and the header's label follow;
        // the poll below runs now rather than after its 1 s throttle.
        // Exclusive mode never gets here: its reroutes are classified as a
        // rebuild (see classify_stream_error).
        let rerouted = self.state.device_rerouted.swap(false, Ordering::Relaxed);
        if rerouted {
            if let Some(d) = self.host.default_output_device() {
                self.device = d;
            }
        }
        // Exclusive mode is pinned to its device, so the system default moving
        // (which Keet's own hog mode causes) is not our output moving —
        // relabelling to it announced "output now on MacBook speakers" while
        // playback stayed on the DAC.
        if self.device_arg.is_none()
            && !self.state.exclusive.load(Ordering::Relaxed)
            && (rerouted || self.last_device_poll.elapsed() >= Duration::from_secs(1))
        {
            self.last_device_poll = Instant::now();
            let current = self.host
                .default_output_device()
                .and_then(|d| d.description().ok().map(|desc| desc.name().to_string()));
            if let Some(name) = current {
                if name != self.shown_device_name {
                    self.shown_device_name = name.clone();
                    self.ui.device_name = name.clone();
                    self.ui.set_status(format!("output now on {name}"));
                }
            }
        }
    }
}
