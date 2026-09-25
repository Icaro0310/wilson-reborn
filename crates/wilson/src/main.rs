// SPDX-License-Identifier: GPL-3.0-or-later
//! Wilson Reborn — the Johnny Castaway screensaver, as a live window.
//!
//! Runs the [`wilson_engine`] runtime on the **original** game data and presents each
//! composited frame with `softbuffer` in a `winit` window. Any real key press or mouse
//! input quits, like a real screensaver (lone modifier keys are ignored — see
//! [`key_dismisses`]). Runs fullscreen by default (use `--windowed` for dev).
//!
//! It needs the original Johnny Castaway data (`RESOURCE.MAP` + `RESOURCE.001`):
//! - `wilson --data <dir>` — load the data from `<dir>`.
//! - `wilson` — auto-detects the data in the working directory or next to the executable.
//! - `wilson --windowed --mute --speed <pct> --scale fit|stretch|integer|extend
//!   --filter nearest|linear|xbr|xbrz --dedither --no-intro --intro-secs <s> --day <1-11>
//!   --story --story-secs <s> --transition none|dissolve` — options (`extend` fills widescreen;
//!   `--story` plays the arc in order; `--transition dissolve` = the original's tiled dissolve).
//! - Windows screensaver verbs: `/s` (show), `/c` (config — opens the settings file),
//!   `/p <hwnd>` (preview embedded in the configuration pane — Windows only).

// A screensaver is a GUI program: on Windows, link it for the "windows" subsystem so the
// `.scr` doesn't pop a black console window when launched from the Screen Saver settings.
// Kept in debug builds so `--debug` logging still has a console to print to.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(feature = "embed-data"))]
mod assets;
mod audio;
mod config;
#[cfg(feature = "embed-data")]
mod embedded;
mod font;
mod pace;
mod scale;
mod spatial;
mod state;
mod stats;
mod timectl;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wilson_engine::{clock, Director, Show};
use winit::event::{ElementState, Event, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, WindowBuilder};

/// Whether a keyboard event should dismiss the screensaver (like a real one: any real key
/// press quits). Ignores key *releases*, the **synthetic** key events the OS delivers when
/// a window gains/loses focus, and lone **modifier** keys (Alt/AltGr/Ctrl/Shift/Super/Meta/
/// locks/Fn). This matters on Windows: a borderless-fullscreen window receives a spurious
/// `AltGraph` press the instant it grabs focus, which would otherwise close it immediately.
fn key_dismisses(logical_key: &Key, state: ElementState, is_synthetic: bool) -> bool {
    if state != ElementState::Pressed || is_synthetic {
        return false;
    }
    !matches!(
        logical_key,
        Key::Named(
            NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Control
                | NamedKey::Shift
                | NamedKey::Super
                | NamedKey::Meta
                | NamedKey::Hyper
                | NamedKey::Symbol
                | NamedKey::Fn
                | NamedKey::FnLock
                | NamedKey::CapsLock
                | NamedKey::NumLock
                | NamedKey::ScrollLock
        )
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Help first, so `-h`/`--help` (Unix) and `/?`/`/help` (Windows) always work and
    // never touch the data/window.
    if wants_help(&args) {
        print_help();
        return;
    }

    // Load options (file → defaults), seed a default file on first run, then let CLI
    // flags override for this run only.
    let mut cfg = config::Config::load();
    if config::config_file().is_some_and(|p| !p.exists()) {
        cfg.save();
    }
    cfg.apply_args(&args);
    let wide_angle = args.iter().any(|arg| arg == "--wide-angle");
    // `--spatial` replaces the fisheye: the engine emits its un-flattened
    // layers and a raised 2.5D camera composes them (diorama view). The old
    // flag stays available for side-by-side comparison during development.
    let spatial = args.iter().any(|arg| arg == "--spatial");
    // Camera tuning flags (`--cam-*`): zoom/pan move the camera, never deform
    // the world — used for A/B captures and spatial tuning.
    let mut camera = spatial::SpatialCamera::default();
    let arg_f32 = |flag: &str| -> Option<f32> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
    };
    if let Some(v) = arg_f32("--cam-zoom") {
        camera.zoom = v.max(0.05);
    }
    if let Some(v) = arg_f32("--cam-pan-x") {
        camera.pan_x = v;
    }
    if let Some(v) = arg_f32("--cam-pan-y") {
        camera.pan_y = v;
    }
    if let Some(v) = arg_f32("--cam-horizon") {
        camera.horizon_v = v.clamp(0.02, 0.9);
    }
    if let Some(v) = arg_f32("--cam-seam") {
        camera.seam_v = v.clamp(0.02, 0.95);
    }
    if let Some(v) = arg_f32("--cam-tilt") {
        camera.tilt = v.clamp(1.0, 4.0);
    }
    if let Some(v) = arg_f32("--cam-far") {
        camera.far_compression = v.max(0.05);
    }
    if let Some(v) = arg_f32("--cam-near") {
        camera.near_expansion = v.max(0.05);
    }

    // Windows screensaver verbs.
    let action = screensaver_action(&args);
    match action {
        Action::Configure => {
            configure(&cfg);
            return;
        }
        Action::Preview(_) if !cfg!(windows) => {
            eprintln!("Wilson Reborn: the /p preview is only supported on Windows.");
            return;
        }
        _ => {}
    }
    let preview_parent: Option<isize> = match action {
        Action::Preview(hwnd) => Some(hwnd),
        _ => None,
    };
    let is_preview = preview_parent.is_some();

    // Original data + sounds: embedded (self-contained `embed-data` build) or loaded
    // from disk (`--data`/auto-detect).
    #[cfg(feature = "embed-data")]
    let (archive, palette, audio) = {
        let (archive, palette) = embedded::archive_and_palette();
        let audio =
            audio::Audio::from_sounds(embedded::sound_bytes(), audio_muted(cfg.mute, is_preview));
        (archive, palette, audio)
    };
    #[cfg(not(feature = "embed-data"))]
    let (archive, palette, audio) = {
        let data_arg = args
            .windows(2)
            .find(|w| w[0] == "--data")
            .map(|w| w[1].clone());
        // Accept a folder OR a .zip (e.g. the Internet Archive scrantic-run.zip), and
        // auto-detect either next to the executable / in the working directory.
        let data_dir = match assets::resolve_data_dir(data_arg.as_deref()) {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("{e}");
                return;
            }
        };
        let (archive, palette) = match assets::load(&data_dir) {
            Ok(loaded) => loaded,
            Err(e) => {
                eprintln!("Could not load data from {}: {e}", data_dir.display());
                return;
            }
        };
        // Sound effects: soundN.wav in the data dir, else extracted from SCRANTIC.EXE/.SCR
        // there. Degrades to silence without the `audio` feature, a device, or mute.
        let audio = audio::Audio::new(Some(data_dir.as_path()), audio_muted(cfg.mute, is_preview));
        (archive, palette, audio)
    };

    let real = clock::now();
    // Time controls (QoL, opt-in): `--story` plays the whole arc in order from a synthetic
    // clock; `--day N` starts at a chosen day. Both are one-off — in those modes we do NOT
    // persist the day, so the real saved arc isn't clobbered. Default: resume the persisted
    // day (or today), real wall-clock time, and apply the configured day/night cycle.
    let persist_day = !cfg.story && cfg.day == 0;
    let (director, clock) = if cfg.story {
        // Day 1, stored_yday 0: the story clock (day-index 0 at t=0) keeps the first run on
        // day 1, then advances the arc as its day-index ticks.
        (
            Director::new(1, 0),
            timectl::story_clock(real, 0, cfg.story_secs),
        )
    } else if cfg.day != 0 {
        (Director::new(cfg.day, real.yday), real)
    } else {
        let d = match state::DayState::load() {
            Some(s) => Director::new(s.current_day, s.stored_yday),
            None => Director::new(1, real.yday),
        };
        (d, real)
    };
    let director = director.with_daynight(cfg.daynight);
    let seed = args
        .iter()
        .position(|a| a == "--seed")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
        });
    let mut show = Show::new(&archive, &palette, 640, 480, director, clock, seed);
    if cfg.intro {
        // the original's startup intro screen (INTRO.SCR), held for the configured seconds
        show.enable_intro(
            &archive,
            wilson_engine::intro_ticks_from_secs(cfg.intro_secs),
        );
    }
    if cfg.transition == config::Transition::Dissolve {
        show.enable_dissolve(); // the original's dormant LFSR tiled dissolve, opt-in
    }
    if spatial {
        show.enable_layers(); // emit the un-flattened world/character planes
    }

    // Persist the story day whenever it advances, so the arc carries over to the next
    // run. `None` until the first frame establishes today's day.
    let mut last_saved: Option<(u8, i32)> = None;

    // Lifetime statistics: count this session and accumulate runtime.
    let mut stats = stats::Stats::load();
    stats.sessions += 1;
    stats.note_day(show.day_state().0);
    stats.save();
    let base_secs = stats.total_secs;
    let session_start = Instant::now();
    let mut last_flush = Instant::now();
    // Pace the engine off a per-frame deadline so spurious OS redraws (the startup burst,
    // resizes, scale changes) can't race past the timer and play the first frames — most
    // visibly the intro — too fast. `last_rgba` lets a non-due redraw repaint without
    // stepping the animation. See `pace::FramePacer`.
    let mut pacer = pace::FramePacer::new();
    let mut last_rgba: Option<Vec<u8>> = None;
    // The last layered frame's scene world, for repaint-without-step
    // (resize/redraw) under `--spatial`.
    let mut last_layers: Option<wilson_engine::SceneWorld> = None;

    // --debug diagnostics: measured FPS over a rolling 1-second window.
    let mut dbg_frames = 0u32;
    let mut dbg_fps = 0u32;
    let mut dbg_window = Instant::now();
    let mut dbg_first_frame = true; // log the first successful frame once
    if cfg.debug {
        eprintln!(
            "[wilson:debug] on — filter={} scale={} speed={}% dedither={} daynight={} windowed={} intro={}",
            cfg.filter.as_str(),
            cfg.scale.as_str(),
            cfg.speed,
            cfg.dedither,
            cfg.daynight.as_str(),
            cfg.windowed,
            cfg.intro,
        );
        eprintln!("[wilson:debug] audio: {}", audio.debug_summary());
    }

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut builder = WindowBuilder::new().with_title("Wilson Reborn — Johnny Castaway");
    builder = if let Some(hwnd) = preview_parent {
        apply_preview(builder, hwnd) // embedded preview pane (Windows /p)
    } else if cfg.windowed {
        builder.with_inner_size(winit::dpi::LogicalSize::new(640.0, 480.0))
    } else {
        builder.with_fullscreen(Some(Fullscreen::Borderless(None)))
    };
    let window = Rc::new(builder.build(&event_loop).expect("failed to create window"));
    if is_preview {
        install_rim_passthrough(&window);
    }
    if cfg.debug {
        let s = window.inner_size();
        let mode = if is_preview {
            "preview"
        } else if cfg.windowed {
            "windowed"
        } else {
            "fullscreen"
        };
        eprintln!(
            "[wilson:debug] window built ({mode}) {}x{}; entering event loop",
            s.width, s.height
        );
    }
    let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
    let mut surface =
        softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");

    // Last cursor position in child-window pixels; the Living Island drag needs it
    // because `MouseInput` events carry no coordinates.
    let mut cursor_pos = (0.0f64, 0.0f64);
    // Some(offset) while the user drags the planet by its inside (Living Island).
    let mut planet_drag = None;

    event_loop
        .run(move |event, elwt| match event {
            Event::WindowEvent { event, .. } => {
                // --debug: trace window events (skip the per-frame/noisy ones) so a quick
                // exit shows *which* event caused it (e.g. an input event closing it).
                if cfg.debug
                    && !matches!(
                        event,
                        WindowEvent::RedrawRequested
                            | WindowEvent::CursorMoved { .. }
                            | WindowEvent::AxisMotion { .. }
                            | WindowEvent::Moved(_)
                    )
                {
                    eprintln!("[wilson:debug] window event: {event:?}");
                }
                match event {
                    // The preview pane must keep running on hover/keypress; only a real
                    // close ends it. In normal/fullscreen mode, any input quits.
                    WindowEvent::CloseRequested => {
                        if cfg.debug {
                            eprintln!("[wilson:debug] exit: CloseRequested");
                        }
                        elwt.exit();
                    }
                    WindowEvent::CursorMoved { position, .. } if is_preview => {
                        cursor_pos = (position.x, position.y);
                        if let (Some(p), Some(g)) = (preview_parent, planet_drag) {
                            if left_button_down() {
                                planet_grab_apply(p, g);
                            } else {
                                // The button-up never reached us — a context menu
                                // or the modal resize loop stole our capture.
                                // Drop the stale drag instead of letting the
                                // planet chase the cursor with no button held.
                                planet_drag = None;
                            }
                        }
                    }
                    // Focus loss (menu popup, Alt+Tab, desktop switch) also
                    // interrupts the press/release pair — end the drag.
                    WindowEvent::Focused(false) if is_preview => {
                        if planet_drag.is_some() {
                            planet_drag = None;
                        }
                    }
                    // Living Island host integration: in the embedded preview the
                    // right-click must reach the WPF parent (it opens the chat menu);
                    // the screensaver itself ignores input here.
                    WindowEvent::MouseInput { button, state, .. }
                        if is_preview
                            && button == winit::event::MouseButton::Right
                            && state == ElementState::Pressed =>
                    {
                        notify_preview_parent(preview_parent, &window);
                    }
                    // A left press drags the planet: near the rim it resizes the
                    // host through the system loop, elsewhere we track the mouse
                    // and move it ourselves (the host's DefWindowProc ignores a
                    // posted HTCAPTION click).
                    WindowEvent::MouseInput { button, state, .. }
                        if is_preview
                            && button == winit::event::MouseButton::Left
                            && state == ElementState::Pressed =>
                    {
                        if let Some(p) = preview_parent {
                            planet_drag =
                                drag_preview_parent(p, &window, cursor_pos.0, cursor_pos.1);
                        }
                    }
                    WindowEvent::MouseInput { button, state, .. }
                        if is_preview
                            && button == winit::event::MouseButton::Left
                            && state == ElementState::Released =>
                    {
                        planet_drag = None;
                    }
                    WindowEvent::MouseInput { button, state, .. } if !is_preview => {
                        if cfg.debug {
                            eprintln!("[wilson:debug] exit: MouseInput {button:?} {state:?}");
                        }
                        elwt.exit();
                    }
                    WindowEvent::KeyboardInput {
                        event: key,
                        is_synthetic,
                        ..
                    } if !is_preview
                        && key_dismisses(&key.logical_key, key.state, is_synthetic) =>
                    {
                        if cfg.debug {
                            eprintln!("[wilson:debug] exit: KeyboardInput {:?}", key.logical_key);
                        }
                        elwt.exit();
                    }
                    WindowEvent::RedrawRequested => {
                        let size = window.inner_size();
                        if let (Some(w), Some(h)) =
                            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                        {
                            // Pace from when the frame became due (≈ when the timer fired),
                            // not from after we finish computing it: this absorbs the per-frame
                            // work (engine + xBR/scale/present) into the delay instead of adding
                            // it on top, matching jc_reborn's `lastTicks` pacing. Without this,
                            // heavier filters (xBR) make the animation run slower than the
                            // original.
                            let frame_start = Instant::now();
                            surface.resize(w, h).expect("resize surface");
                            if pacer.due(frame_start) {
                                // The current frame's hold has elapsed (or this is the first
                                // frame): step the engine. Refresh the clock so the day rolls
                                // over within a long session — story mode feeds a synthetic
                                // clock (the arc in order); otherwise the real wall clock.
                                show.set_clock(if cfg.story {
                                    timectl::story_clock(
                                        clock::now(),
                                        session_start.elapsed().as_secs(),
                                        cfg.story_secs,
                                    )
                                } else {
                                    clock::now()
                                });
                                let frame = show.next_frame(&archive);
                                for &id in &frame.sounds {
                                    let outcome = audio.play(id);
                                    if cfg.debug {
                                        eprintln!("[wilson:debug] sound cue {id}: {outcome:?}");
                                    }
                                }
                                let (day, yday) = show.day_state();
                                // Persist the day only in real-time mode — `--story`/`--day`
                                // are one-off overrides and must not clobber the saved arc.
                                if persist_day && last_saved != Some((day, yday)) {
                                    state::DayState {
                                        current_day: day,
                                        stored_yday: yday,
                                    }
                                    .save();
                                    last_saved = Some((day, yday));
                                }
                                // Update lifetime stats, flushing to disk occasionally.
                                stats.note_day(day);
                                if last_flush.elapsed() >= Duration::from_secs(30) {
                                    stats.total_secs =
                                        base_secs + session_start.elapsed().as_secs();
                                    stats.save();
                                    last_flush = Instant::now();
                                }
                                // Spatial mode composes the un-flattened layers through
                                // the camera; the flat RGBA is only needed otherwise.
                                let layered = spatial && frame.layers.is_some();
                                let rgba = if layered {
                                    Vec::new()
                                } else {
                                    let rgba = frame.surface.to_rgba(&palette);
                                    // Optional: smooth the dithered sea/sky before scaling.
                                    if cfg.dedither {
                                        wilson_engine::dedither(&rgba, 640, 480)
                                    } else {
                                        rgba
                                    }
                                };
                                {
                                    let mut buffer = surface.buffer_mut().expect("surface buffer");
                                    if layered {
                                        let layers = frame.layers.as_ref().unwrap();
                                        spatial::render(
                                            layers,
                                            &palette,
                                            &mut buffer,
                                            size.width as usize,
                                            size.height as usize,
                                            &camera,
                                            cfg.filter,
                                            cfg.dedither,
                                        );
                                        last_layers = Some(layers.clone());
                                    } else {
                                        scale::scale_rgba_to_argb_desktop(
                                            &rgba,
                                            640,
                                            480,
                                            &mut buffer,
                                            size.width as usize,
                                            size.height as usize,
                                            cfg.scale,
                                            cfg.filter,
                                            wide_angle,
                                        );
                                        last_layers = None;
                                    }
                                    if cfg.debug {
                                        // Measure FPS over a 1s window; emit a stdout status
                                        // line each second and draw the on-screen HUD.
                                        dbg_frames += 1;
                                        if dbg_window.elapsed() >= Duration::from_secs(1) {
                                            dbg_fps = dbg_frames;
                                            dbg_frames = 0;
                                            dbg_window = Instant::now();
                                            let d = show.debug_info();
                                            let scene = d
                                                .scene
                                                .map(|(n, t)| format!("{n}#{t}"))
                                                .unwrap_or_else(|| "-".into());
                                            let off = d
                                                .offset
                                                .map(|(x, y)| format!("{x},{y}"))
                                                .unwrap_or_else(|| "-".into());
                                            eprintln!(
                                                "[wilson:debug] fps={} delay={}t stage={} \
                                                 day={}/11 scene={} drift=({}) night={} \
                                                 tide={} raft={} holiday={:?}",
                                                dbg_fps,
                                                frame.delay_ticks,
                                                d.stage,
                                                d.day,
                                                scene,
                                                off,
                                                d.night as u8,
                                                d.low_tide as u8,
                                                d.raft,
                                                d.holiday,
                                            );
                                        }
                                        draw_debug_hud(
                                            &mut buffer,
                                            size.width as usize,
                                            size.height as usize,
                                            dbg_fps,
                                            frame.delay_ticks,
                                            &show.debug_info(),
                                            &cfg,
                                        );
                                    }
                                    buffer.present().expect("present");
                                }
                                if cfg.debug && dbg_first_frame {
                                    dbg_first_frame = false;
                                    eprintln!("[wilson:debug] first frame presented {w}x{h}");
                                }
                                let delay =
                                    Duration::from_millis(cfg.frame_delay_ms(frame.delay_ticks));
                                // Deadline from the frame's due time, so compute time is
                                // absorbed (period ≈ delay); if compute overran it is already
                                // in the past and the next frame runs immediately.
                                let due = pacer.schedule(frame_start, delay);
                                elwt.set_control_flow(ControlFlow::WaitUntil(due));
                                last_rgba = if layered { None } else { Some(rgba) };
                            } else {
                                // A redraw arrived before the current frame's hold elapsed
                                // (the OS startup burst, a resize, a scale change). Re-present
                                // the last frame at the current size so resizes still repaint —
                                // but do NOT step the animation, or the intro/first frames flash
                                // by. Re-arm the existing deadline without moving it.
                                if let Some(world) = &last_layers {
                                    let mut buffer = surface.buffer_mut().expect("surface buffer");
                                    spatial::render(
                                        world,
                                        &palette,
                                        &mut buffer,
                                        size.width as usize,
                                        size.height as usize,
                                        &camera,
                                        cfg.filter,
                                        cfg.dedither,
                                    );
                                    buffer.present().expect("present");
                                } else if let Some(rgba) = &last_rgba {
                                    let mut buffer = surface.buffer_mut().expect("surface buffer");
                                    scale::scale_rgba_to_argb_desktop(
                                        rgba,
                                        640,
                                        480,
                                        &mut buffer,
                                        size.width as usize,
                                        size.height as usize,
                                        cfg.scale,
                                        cfg.filter,
                                        wide_angle,
                                    );
                                    buffer.present().expect("present");
                                }
                                if let Some(due) = pacer.deadline() {
                                    elwt.set_control_flow(ControlFlow::WaitUntil(due));
                                }
                            }
                        } else if cfg.debug && dbg_first_frame {
                            dbg_first_frame = false;
                            eprintln!(
                                "[wilson:debug] redraw skipped: window size {}x{} (zero)",
                                size.width, size.height
                            );
                        }
                    }
                    _ => {}
                }
            }
            // Pace the animation: redraw only when the per-frame timer elapses (or on
            // the initial start), not on every loop iteration. Requesting a redraw on
            // every `AboutToWait` would preempt the `WaitUntil` deadline and run the
            // engine as fast as the CPU spins (the screensaver played far too fast).
            Event::NewEvents(cause @ (StartCause::Init | StartCause::ResumeTimeReached { .. })) => {
                if cfg.debug && matches!(cause, StartCause::Init) {
                    eprintln!("[wilson:debug] event loop started (NewEvents::Init)");
                }
                window.request_redraw();
            }
            Event::Resumed => {
                if cfg.debug {
                    eprintln!("[wilson:debug] resumed");
                }
            }
            Event::LoopExiting => {
                if cfg.debug {
                    eprintln!("[wilson:debug] loop exiting (process will end)");
                }
                stats.total_secs = base_secs + session_start.elapsed().as_secs();
                stats.save();
            }
            _ => {}
        })
        .expect("event loop error");
}

/// What the screensaver was asked to do (from the Windows `/s` `/p` `/c` verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Show the screensaver (default).
    Show,
    /// Print/show the configuration (`/c`).
    Configure,
    /// Render the small preview embedded in the parent window handle (`/p <hwnd>`).
    Preview(isize),
}

/// Detect the screensaver action in `args`. Verbs may use `/` or `-`, be upper/lower
/// case, and carry an HWND either after a colon (`/p:1234`) or as the next argument
/// (`/p 1234`).
fn screensaver_action(args: &[String]) -> Action {
    let mut i = 1;
    while i < args.len() {
        let low = args[i].to_ascii_lowercase();
        if let Some(body) = low.strip_prefix('/').or_else(|| low.strip_prefix('-')) {
            let mut chars = body.chars();
            let verb = chars.next();
            let rest = chars.as_str(); // text after the verb letter
            let clean = rest.is_empty() || rest.starts_with(':');
            match verb {
                Some('c') if clean => return Action::Configure,
                Some('s') if clean => return Action::Show,
                Some('p') if clean => {
                    let hwnd = rest
                        .strip_prefix(':')
                        .and_then(parse_hwnd)
                        .or_else(|| args.get(i + 1).and_then(|n| parse_hwnd(n)))
                        .unwrap_or(0);
                    return Action::Preview(hwnd);
                }
                _ => {}
            }
        }
        i += 1;
    }
    Action::Show
}

/// Parse a window handle (decimal, or `0x`-prefixed hex), as Windows passes it.
fn parse_hwnd(s: &str) -> Option<isize> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => isize::from_str_radix(hex, 16).ok(),
        None => s.parse::<isize>().ok(),
    }
}

/// Whether audio should be silenced: when muted by config, **or** in the Windows preview pane
/// (the little monitor in the Screen Saver settings must never play sound).
fn audio_muted(cfg_mute: bool, is_preview: bool) -> bool {
    cfg_mute || is_preview
}

/// Handle the screensaver **Configure** verb (`/c`). Our settings live in a text file
/// (`config.txt`, with a commented option for each setting). On Windows a `.scr` has no
/// console, so open that file in the default editor for the user to edit; elsewhere (where a
/// console exists) print the current settings.
fn configure(cfg: &config::Config) {
    #[cfg(windows)]
    if let Some(path) = config::config_file() {
        // The file is already seeded in `main`; open it so the user can edit the options.
        // Prefer Notepad (always present, handles the path verbatim); fall back to the shell's
        // default handler.
        if std::process::Command::new("notepad")
            .arg(&path)
            .spawn()
            .is_err()
        {
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", "", &path.display().to_string()])
                .spawn();
        }
        return;
    }
    print_config_info(cfg);
}

/// Apply the Windows preview-window settings: make the window a borderless child of the
/// preview pane (`hwnd`). A no-op on other platforms (where `/p` isn't used).
fn apply_preview(builder: WindowBuilder, hwnd: isize) -> WindowBuilder {
    #[cfg(windows)]
    {
        use std::num::NonZeroIsize;
        use winit::raw_window_handle::{RawWindowHandle, Win32WindowHandle};
        // Fill the host preview pane: query its actual client size instead of guessing, so the
        // screensaver covers the whole little monitor (no black bands on the right/bottom). Fall
        // back to the classic ~152×112 if the query fails.
        let (w, h) = preview_pane_size(hwnd).unwrap_or((152, 112));
        let b = builder
            .with_decorations(false)
            .with_inner_size(winit::dpi::PhysicalSize::new(w, h));
        if let Some(nz) = NonZeroIsize::new(hwnd) {
            let handle = RawWindowHandle::Win32(Win32WindowHandle::new(nz));
            // SAFETY: `hwnd` is the preview window handle Windows passed on the command
            // line; it is valid for the lifetime of the preview.
            return unsafe { b.with_parent_window(Some(handle)) };
        }
        b
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        builder
    }
}

/// Tell the WPF host (Living Island desktop pet) that the user right-clicked the embedded
/// scene, by posting `WM_JOHNNY_CONTEXT` to the parent window with our child HWND in wParam.
/// No-op without a parent or off Windows.
#[cfg(windows)]
fn notify_preview_parent(parent: Option<isize>, window: &winit::window::Window) {
    const WM_JOHNNY_CONTEXT: u32 = 0x8041;
    #[link(name = "user32")]
    extern "system" {
        fn PostMessageW(hwnd: isize, msg: u32, wparam: isize, lparam: isize) -> i32;
    }
    let Some(parent) = parent else { return };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(w) = handle.as_raw() {
            // SAFETY: `parent` is the host HWND passed via /p; `PostMessageW` only queues
            // a message, no lifetime risk. `w.hwnd` is our own live window.
            unsafe { PostMessageW(parent, WM_JOHNNY_CONTEXT, w.hwnd.get(), 0) };
        }
    }
}

/// What a left press grabbed on the planet while embedded (Living Island).
/// Move drags the host by the cursor offset; Resize stretches the grabbed
/// rim edge/corner, always keeping the window square like the host's
/// `WM_SIZING` clamp intends.
#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
enum PlanetGrab {
    Move {
        dx: i32,
        dy: i32,
    },
    Resize {
        ht: isize,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    },
}

/// Living Island host integration: a left press in the embedded preview grabs
/// the planet. Rim presses never reach us — [`install_rim_passthrough`] makes
/// them fall through to the host, which answers `WM_NCHITTEST` with real
/// border codes so Windows runs the native sizing loop — so in practice this
/// only ever produces a `Move` grab; the `Resize` arm stays as a fallback if
/// the subclass was never installed. `None` without a parent or off Windows.
#[cfg(windows)]
fn drag_preview_parent(
    parent: isize,
    window: &winit::window::Window,
    x: f64,
    y: f64,
) -> Option<PlanetGrab> {
    const HTLEFT: isize = 10;
    const HTRIGHT: isize = 11;
    const HTTOP: isize = 12;
    const HTTOPLEFT: isize = 13;
    const HTTOPRIGHT: isize = 14;
    const HTBOTTOM: isize = 15;
    const HTBOTTOMLEFT: isize = 16;
    const HTBOTTOMRIGHT: isize = 17;
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(pt: *mut Point) -> i32;
        fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
    }
    let size = window.inner_size();
    let (w, h) = (f64::from(size.width), f64::from(size.height));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    // Rim band: grabbing near the planet's edge resizes instead of moving.
    let band = (w.min(h) * 0.09).clamp(10.0, 26.0);
    let (l, r, t, b) = (x < band, x >= w - band, y < band, y >= h - band);
    let ht = match (l, r, t, b) {
        (true, _, true, _) => HTTOPLEFT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, ..) => HTLEFT,
        (_, true, ..) => HTRIGHT,
        (.., true, _) => HTTOP,
        (.., true) => HTBOTTOM,
        _ => 0,
    };
    let mut pt = Point { x: 0, y: 0 };
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `parent` is the live host HWND and the pointers are valid
    // out-params. No explicit SetCapture here: winit already captures the mouse
    // on WM_LBUTTONDOWN and releases on WM_LBUTTONUP — a second capture desyncs
    // its internal capture_count when a popup or modal loop steals the mouse.
    unsafe {
        GetCursorPos(&mut pt);
        if GetWindowRect(parent, &mut rect) == 0 {
            return None;
        }
    }
    if ht != 0 {
        Some(PlanetGrab::Resize {
            ht,
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        })
    } else {
        Some(PlanetGrab::Move {
            dx: pt.x - rect.left,
            dy: pt.y - rect.top,
        })
    }
}

/// Apply one cursor step of the active planet grab to the host window
/// (Living Island): `Move` keeps the grab offset, `Resize` pulls the grabbed
/// edge/corner to the cursor and re-squares the window around the fixed side.
#[cfg(windows)]
fn planet_grab_apply(parent: isize, grab: PlanetGrab) {
    const HTLEFT: isize = 10;
    const HTRIGHT: isize = 11;
    const HTTOP: isize = 12;
    const HTTOPLEFT: isize = 13;
    const HTTOPRIGHT: isize = 14;
    const HTBOTTOM: isize = 15;
    const HTBOTTOMLEFT: isize = 16;
    const HTBOTTOMRIGHT: isize = 17;
    const MIN_SIDE: i32 = 120;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(pt: *mut Point) -> i32;
        fn SetWindowPos(
            hwnd: isize,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }
    let mut pt = Point { x: 0, y: 0 };
    // SAFETY: `parent` is the live host HWND; `pt` is a valid out-pointer.
    unsafe {
        GetCursorPos(&mut pt);
        match grab {
            PlanetGrab::Move { dx, dy } => SetWindowPos(
                parent,
                0,
                pt.x - dx,
                pt.y - dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            ),
            PlanetGrab::Resize {
                ht,
                left,
                top,
                right,
                bottom,
            } => {
                let (mut l, mut t, mut r, mut b) = (left, top, right, bottom);
                // Side = the dragged dimension; the square grows/shrinks from
                // the fixed opposite edge or corner (edge drags stay centered
                // on the untouched axis).
                let side = match ht {
                    HTLEFT => (right - pt.x).max(MIN_SIDE),
                    HTRIGHT => (pt.x - left).max(MIN_SIDE),
                    HTTOP => (bottom - pt.y).max(MIN_SIDE),
                    HTBOTTOM => (pt.y - top).max(MIN_SIDE),
                    HTTOPLEFT => (right - pt.x).max(bottom - pt.y).max(MIN_SIDE),
                    HTTOPRIGHT => (pt.x - left).max(bottom - pt.y).max(MIN_SIDE),
                    HTBOTTOMLEFT => (right - pt.x).max(pt.y - top).max(MIN_SIDE),
                    _ => (pt.x - left).max(pt.y - top).max(MIN_SIDE),
                };
                match ht {
                    HTLEFT => l = right - side,
                    HTRIGHT => r = left + side,
                    HTTOP => t = bottom - side,
                    HTBOTTOM => b = top + side,
                    HTTOPLEFT => {
                        l = right - side;
                        t = bottom - side;
                    }
                    HTTOPRIGHT => {
                        r = left + side;
                        t = bottom - side;
                    }
                    HTBOTTOMLEFT => {
                        l = right - side;
                        b = top + side;
                    }
                    HTBOTTOMRIGHT => {
                        r = left + side;
                        b = top + side;
                    }
                    _ => {}
                }
                // Edge drags keep the perpendicular axis centered on the
                // original window, mirroring the host's WM_SIZING clamp.
                match ht {
                    HTLEFT | HTRIGHT => {
                        let cy = (top + bottom) / 2;
                        t = cy - side / 2;
                        b = t + side;
                    }
                    HTTOP | HTBOTTOM => {
                        let cx = (left + right) / 2;
                        l = cx - side / 2;
                        r = l + side;
                    }
                    _ => {}
                }
                SetWindowPos(parent, 0, l, t, r - l, b - t, SWP_NOZORDER | SWP_NOACTIVATE)
            }
        };
    }
}

/// Whether the left mouse button is physically held right now (Living Island).
/// The `Released` event can be lost when the modal resize loop or a context
/// menu steals our capture, so `CursorMoved` double-checks the real state.
#[cfg(windows)]
fn left_button_down() -> bool {
    const VK_LBUTTON: i32 = 0x01;
    #[link(name = "user32")]
    extern "system" {
        fn GetAsyncKeyState(key: i32) -> i16;
    }
    // SAFETY: a plain state query on a virtual key; no lifetime or memory risk.
    unsafe { (GetAsyncKeyState(VK_LBUTTON) as u16) & 0x8000 != 0 }
}

/// Previous window proc of the embedded child, chained to by [`rim_passthrough`].
#[cfg(windows)]
static PREV_CHILD_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Subclassed wndproc for the embedded preview child (Living Island). It
/// intercepts two messages so the planet's rim resizes the *host* window with
/// the real native sizing loop:
///
/// * `WM_NCHITTEST` inside the rim band answers the actual border code
///   (`HTLEFT`..`HTBOTTOMRIGHT`), so a rim press arrives as
///   `WM_NCLBUTTONDOWN` here instead of a client click.
/// * That `WM_NCLBUTTONDOWN` is forwarded to the host with `SendMessage`, and
///   the host's hook calls `DefWindowProc` — which runs the modal sizing loop
///   on the host's own thread, tracking the mouse and driving its `WM_SIZING`
///   square clamp. `HTTRANSPARENT` cannot be used because hit-testing only
///   falls through to windows on the *same* thread; the host lives in another
///   process.
///
/// Everything else defers to winit's proc.
#[cfg(windows)]
unsafe extern "system" fn rim_passthrough(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    const WM_NCHITTEST: u32 = 0x0084;
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const WM_NCLBUTTONDBLCLK: u32 = 0x00A3;
    const HTLEFT: isize = 10;
    const HTRIGHT: isize = 11;
    const HTTOP: isize = 12;
    const HTTOPLEFT: isize = 13;
    const HTTOPRIGHT: isize = 14;
    const HTBOTTOM: isize = 15;
    const HTBOTTOMLEFT: isize = 16;
    const HTBOTTOMRIGHT: isize = 17;
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;
        fn GetParent(hwnd: isize) -> isize;
        fn ScreenToClient(hwnd: isize, pt: *mut Point) -> i32;
        fn SendMessageW(hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
        fn CallWindowProcW(proc: isize, hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
    }
    if msg == WM_NCHITTEST {
        // lParam packs the screen-space cursor point into signed 16-bit halves.
        let mut pt = Point {
            x: lparam as i16 as i32,
            y: (lparam >> 16) as i16 as i32,
        };
        let mut rc = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: valid out-params; `hwnd` is our own live child window.
        if unsafe { GetClientRect(hwnd, &mut rc) } != 0
            && unsafe { ScreenToClient(hwnd, &mut pt) } != 0
        {
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
            let band = (f64::from(w.min(h)) * 0.09).clamp(10.0, 26.0) as i32;
            let (x, y) = (pt.x - rc.left, pt.y - rc.top);
            let (l, r) = (x < band, x >= w - band);
            let (t, b) = (y < band, y >= h - band);
            let ht = match (l, r, t, b) {
                (true, _, true, _) => HTTOPLEFT,
                (true, _, _, true) => HTBOTTOMLEFT,
                (_, true, true, _) => HTTOPRIGHT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, ..) => HTLEFT,
                (_, true, ..) => HTRIGHT,
                (.., true, _) => HTTOP,
                (.., true) => HTBOTTOM,
                _ => 0,
            };
            if ht != 0 {
                return ht;
            }
        }
    }
    if (msg == WM_NCLBUTTONDOWN || msg == WM_NCLBUTTONDBLCLK)
        && (HTLEFT..=HTBOTTOMRIGHT).contains(&(wparam as isize))
    {
        // A rim press: hand it to the host so its DefWindowProc runs the real
        // modal sizing loop (blocks until the drag ends).
        let parent = unsafe { GetParent(hwnd) };
        if parent != 0 {
            // SAFETY: plain cross-process SendMessage; lParam already carries
            // the screen-space point.
            return unsafe { SendMessageW(parent, msg, wparam, lparam) };
        }
    }
    let prev = PREV_CHILD_WNDPROC.load(std::sync::atomic::Ordering::Relaxed);
    // SAFETY: `prev` is the wndproc winit installed before our subclass;
    // chaining to it preserves all other message handling.
    unsafe { CallWindowProcW(prev, hwnd, msg, wparam, lparam) }
}

/// Install [`rim_passthrough`] on the embedded preview window (Living Island).
#[cfg(windows)]
fn install_rim_passthrough(window: &winit::window::Window) {
    const GWLP_WNDPROC: i32 = -4;
    #[link(name = "user32")]
    extern "system" {
        fn SetWindowLongPtrW(hwnd: isize, index: i32, new_proc: isize) -> isize;
    }
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(w) = handle.as_raw() else {
        return;
    };
    // SAFETY: called on the thread that owns the window; the returned value is
    // winit's wndproc, which [`rim_passthrough`] chains to.
    let prev = unsafe {
        SetWindowLongPtrW(
            w.hwnd.get(),
            GWLP_WNDPROC,
            rim_passthrough as *const () as usize as isize,
        )
    };
    if prev != 0 {
        PREV_CHILD_WNDPROC.store(prev, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(not(windows))]
fn left_button_down() -> bool {
    true
}

#[cfg(not(windows))]
fn install_rim_passthrough(_window: &winit::window::Window) {}

#[cfg(not(windows))]
fn drag_preview_parent(
    _parent: isize,
    _window: &winit::window::Window,
    _x: f64,
    _y: f64,
) -> Option<()> {
    None
}

#[cfg(not(windows))]
fn planet_grab_apply(_parent: isize, _grab: ()) {}

#[cfg(not(windows))]
fn notify_preview_parent(_parent: Option<isize>, _window: &winit::window::Window) {}

/// Query the client size (in px) of the Windows preview pane `hwnd`, so the embedded child can
/// fill it exactly. `None` if the handle is null or the query fails.
#[cfg(windows)]
fn preview_pane_size(hwnd: isize) -> Option<(u32, u32)> {
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;
    }
    if hwnd == 0 {
        return None;
    }
    let mut r = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `hwnd` is the preview window handle Windows passed; `r` is a valid out-param.
    let ok = unsafe { GetClientRect(hwnd, &mut r) };
    if ok == 0 {
        return None;
    }
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// Print the active configuration and where it lives (the textual `/c` dialog).
fn print_config_info(cfg: &config::Config) {
    let path = config::config_file()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown location)".to_string());
    println!("Wilson Reborn — configuration");
    println!("  file:     {path}");
    println!("  windowed: {}", cfg.windowed);
    println!("  mute:     {}", cfg.mute);
    println!("  speed:    {}%", cfg.speed);
    println!("  scale:    {}", cfg.scale.as_str());
    println!("  filter:   {}", cfg.filter.as_str());
    println!("  dedither: {}", cfg.dedither);
    println!("  daynight: {}", cfg.daynight.as_str());
    println!("  debug:    {}", cfg.debug);
    println!("  intro:    {} ({} s)", cfg.intro, cfg.intro_secs);
    println!(
        "  day:      {}",
        if cfg.day == 0 {
            "auto".to_string()
        } else {
            cfg.day.to_string()
        }
    );
    println!("  story:    {} ({} s/day)", cfg.story, cfg.story_secs);
    println!(
        "  transition: {}",
        match cfg.transition {
            config::Transition::None => "none (hard cut)",
            config::Transition::Dissolve => "dissolve",
        }
    );
    println!("  stats:    {}", stats::Stats::load().summary());
    println!(
        "Edit the file above, or pass --windowed/--mute/--dedither/--debug/--no-intro/--story/\
         --speed <pct>/--scale <mode>/--filter <nearest|linear|xbr|xbrz>/--daynight \
         <original|real24h>/--day <1-11>/--story-secs <s>/--transition <none|dissolve>."
    );
}

/// Whether `args` ask for help, accepting both Unix (`-h`, `--help`) and Windows
/// (`/?`, `/help`) conventions (case-insensitive).
fn wants_help(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| {
        matches!(
            a.to_ascii_lowercase().as_str(),
            "-h" | "-help" | "--help" | "help" | "/?" | "/h" | "/help"
        )
    })
}

/// Print detailed, platform-appropriate usage. Unix shows `-h/--help`; Windows also
/// lists the screensaver verbs (`/s /c /p /?`). The `--…` options work on every platform.
fn print_help() {
    let help_flags = if cfg!(windows) {
        "/?, /help"
    } else {
        "-h, --help"
    };
    let cfg_path = config::config_file()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown location)".to_string());

    println!(
        "Wilson Reborn — a modern, portable clone of the 1992 \"Johnny Castaway\" \
         screensaver (Sierra/Dynamix).\n"
    );
    println!("USAGE:");
    println!("  wilson [OPTIONS]                  run the screensaver (fullscreen)");
    println!("  wilson --data <DIR> [OPTIONS]     use the original game data in <DIR>\n");
    println!("DATA (required):");
    println!("  Needs the original files RESOURCE.MAP + RESOURCE.001 (and soundN.wav for");
    println!("  sound). Located via --data <DIR>, the WILSON_DATA_DIR env var, the current");
    println!("  directory, or next to the executable; file names match case-insensitively.\n");
    println!("OPTIONS:");
    println!("  {help_flags:<33}show this help and exit");
    println!("  --windowed                       run in a 640x480 window (default: fullscreen)");
    println!("  --mute                           disable sound effects (default: sound on)");
    println!("  --speed <PCT>                    playback speed 25-400; 100 = original (default)");
    println!("  --scale <MODE>                   fit | stretch | integer | extend (default: fit)");
    println!("                                     extend = fill widescreen, no bars/distortion");
    println!("  --wide-angle                     fisheye 'little planet' lens (scene in a disk)");
    println!("  --spatial                        2.5D diorama: raised camera + upright sprites");
    println!("                                     camera: --cam-zoom --cam-pan-x --cam-pan-y");
    println!("                                     --cam-horizon --cam-seam --cam-tilt");
    println!("                                     --cam-far --cam-near <FLOAT>");
    println!("  --filter <nearest|linear|xbr|xbrz> pixel sampling (default: linear):");
    println!("                                     nearest = crisp/retro,");
    println!("                                     linear  = smooth (bilinear, default),");
    println!(
        "                                     xbr     = smooth + sharp, dissolves dither (\"HD\"),"
    );
    println!("                                     xbrz    = smooth edges, keeps dither texture");
    println!("  --dedither                       smooth the dithered sea/sky (default: off)");
    println!("  --debug                          diagnostics: stdout status + on-screen HUD");
    println!("  --daynight <original|real24h>    day/night cycle (default: original 8h)");
    println!("  --no-intro                       skip the startup intro screen (default: shown)");
    println!(
        "  --intro-secs <S>                 how long to hold the intro screen (1-30; default 3)"
    );
    println!("  --day <1-11>                     start the 11-day story arc at this day");
    println!("  --story                          play the whole arc in order (day 1->11->1...)");
    println!("  --story-secs <S>                 story mode: real seconds per day (default 90)");
    println!("  --transition <none|dissolve>     scene transition (default: none = hard cut)");
    println!("  --data <DIR>                     game data folder (default: auto-detect)");
    if cfg!(windows) {
        println!("\nWINDOWS SCREENSAVER VERBS (as the OS invokes a .scr):");
        println!("  /s                               show the screensaver (same as no args)");
        println!("  /c                               configure (opens the settings file)");
        println!("  /p <HWND>                        preview inside the configuration pane");
        println!("  /?, /help                        show this help");
    }
    println!("\nCONFIG FILE (persists options; flags override it for one run):");
    println!("  {cfg_path}");
    println!("\nEXAMPLES:");
    if cfg!(windows) {
        println!("  wilson.scr /c                    show the configuration");
        println!("  wilson.exe --windowed --speed 200");
    } else {
        println!("  wilson --data ~/jc --filter xbr");
        println!("  wilson --windowed --speed 200 --scale integer");
    }
    println!("\nFree software under GPL-3.0-or-later. Plays only your own original game data.");
}

/// Draw the `--debug` HUD (top-left) into the window buffer: a dark panel with the live
/// runtime state, so a single screenshot captures everything needed to diagnose a report.
fn draw_debug_hud(
    buf: &mut [u32],
    dw: usize,
    dh: usize,
    fps: u32,
    delay_ticks: u16,
    info: &wilson_engine::DebugInfo,
    cfg: &config::Config,
) {
    let scene = info
        .scene
        .map(|(n, t)| format!("{n}#{t}"))
        .unwrap_or_else(|| "-".into());
    let off = info
        .offset
        .map(|(x, y)| format!("{x},{y}"))
        .unwrap_or_else(|| "-".into());
    let lines = [
        "WILSON DEBUG".to_string(),
        format!("FPS {fps}  FRAME {delay_ticks}T"),
        format!("DAY {}/11  STAGE {}", info.day, info.stage.to_uppercase()),
        format!("SCENE {scene}"),
        format!("DRIFT {off}  ISLAND {}", info.on_island as u8),
        format!(
            "NIGHT {} TIDE {} RAFT {}",
            info.night as u8, info.low_tide as u8, info.raft
        ),
        format!(
            "FILTER {}  SCALE {}",
            cfg.filter.as_str().to_uppercase(),
            cfg.scale.as_str().to_uppercase()
        ),
    ];
    let scale = 2usize;
    let pad = 4usize;
    let lh = font::line_height(scale) + 2;
    let w = lines
        .iter()
        .map(|l| font::text_width(l, scale))
        .max()
        .unwrap_or(0)
        + pad * 2;
    let h = lines.len() * lh + pad * 2;
    font::fill_rect(buf, dw, dh, 0, 0, w, h, 0x0000_0000); // black panel
    for (i, line) in lines.iter().enumerate() {
        font::draw_text(buf, dw, dh, pad, pad + i * lh, line, scale, 0x0000_FF00);
        // green
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("wilson")
            .chain(list.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn modifier_and_synthetic_keys_do_not_dismiss() {
        // The bug: a fullscreen window on Windows gets a spurious AltGraph press on focus,
        // closing the screensaver instantly. Modifier keys (and synthetic focus events)
        // must NOT dismiss.
        for m in [
            NamedKey::AltGraph,
            NamedKey::Alt,
            NamedKey::Control,
            NamedKey::Shift,
            NamedKey::Super,
            NamedKey::CapsLock,
            NamedKey::NumLock,
        ] {
            assert!(
                !key_dismisses(&Key::Named(m), ElementState::Pressed, false),
                "modifier {m:?} must not dismiss"
            );
        }
        // Synthetic events (delivered for keys held when focus changes) never dismiss.
        assert!(!key_dismisses(
            &Key::Named(NamedKey::Escape),
            ElementState::Pressed,
            true
        ));
        // Key releases never dismiss.
        assert!(!key_dismisses(
            &Key::Named(NamedKey::Space),
            ElementState::Released,
            false
        ));
    }

    #[test]
    fn real_key_presses_dismiss() {
        for k in [
            NamedKey::Escape,
            NamedKey::Space,
            NamedKey::Enter,
            NamedKey::ArrowLeft,
        ] {
            assert!(
                key_dismisses(&Key::Named(k), ElementState::Pressed, false),
                "real key {k:?} should dismiss"
            );
        }
        // A character key dismisses too.
        assert!(key_dismisses(
            &Key::Character("a".into()),
            ElementState::Pressed,
            false
        ));
    }

    #[test]
    fn actions_are_detected() {
        assert_eq!(screensaver_action(&args(&["/c"])), Action::Configure);
        assert_eq!(screensaver_action(&args(&["-s"])), Action::Show);
        assert_eq!(screensaver_action(&args(&["/S:0"])), Action::Show);
        assert_eq!(screensaver_action(&args(&[])), Action::Show);
    }

    #[test]
    fn help_is_detected_per_platform() {
        // Unix and Windows conventions, case-insensitive.
        for f in [
            "-h", "--help", "-help", "help", "/?", "/h", "/help", "--HELP", "/Help",
        ] {
            assert!(wants_help(&args(&[f])), "should detect help flag {f:?}");
        }
        // Real options must not be mistaken for help.
        assert!(!wants_help(&args(&[])));
        assert!(!wants_help(&args(&["--windowed", "--filter", "xbr"])));
        assert!(!wants_help(&args(&["/c"])));
        assert!(!wants_help(&args(&["--data", "/some/dir"])));
    }

    #[test]
    fn preview_action_captures_hwnd() {
        // HWND as the next argument, or after a colon, or absent (0).
        assert_eq!(
            screensaver_action(&args(&["/p", "1234"])),
            Action::Preview(1234)
        );
        assert_eq!(
            screensaver_action(&args(&["/P:5678"])),
            Action::Preview(5678)
        );
        assert_eq!(screensaver_action(&args(&["-p"])), Action::Preview(0));
    }

    #[test]
    fn preview_pane_is_always_silent() {
        // Regression: the little preview monitor in the Screen Saver settings ran the engine
        // with sound. It must be muted regardless of the config's mute setting.
        assert!(
            audio_muted(false, true),
            "preview must be silent even when not muted"
        );
        assert!(
            audio_muted(true, true),
            "preview stays silent when muted too"
        );
        assert!(
            audio_muted(true, false),
            "config mute still mutes a normal run"
        );
        assert!(!audio_muted(false, false), "a normal run plays sound");
    }

    #[test]
    fn non_verbs_are_ignored() {
        assert_eq!(
            screensaver_action(&args(&["--data", "/some/dir"])),
            Action::Show
        );
        assert_eq!(
            screensaver_action(&args(&["--windowed", "--scale", "fit"])),
            Action::Show
        );
    }

    #[test]
    fn parse_hwnd_decimal_and_hex() {
        assert_eq!(parse_hwnd("1234"), Some(1234));
        assert_eq!(parse_hwnd("0x10"), Some(16));
        assert_eq!(parse_hwnd(" 42 "), Some(42));
        assert_eq!(parse_hwnd("nope"), None);
    }
}
