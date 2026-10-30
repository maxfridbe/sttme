// Rust guideline compliant 2026-02-21
//! Top-center Wayland live waveform overlay and hold-to-talk release tracking.
//!
//! Maps a double-buffered `zwlr_layer_shell_v1` `Layer::Overlay` surface anchored at the
//! **top-center** of the active Wayland output (`KeyboardInteractivity::None` for global
//! compositor keybinds so active windows never lose focus). Tracks compositor key-repeat
//! keepalive heartbeats via [`HoldLease`] (or optional direct `wl_keyboard` release events).

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, memfd_create};
use tracing::{Level, event};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_keyboard::{self, KeyState, WlKeyboard};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::audio::WaveformMeter;
use crate::waveform::{HUD_HEIGHT, HUD_TOP_MARGIN, HUD_WIDTH, WaveformAnimator};

/// Linux evdev keycode for `F10` (`KEY_F10 = 68`).
///
/// Defined in `<linux/input-event-codes.h>`.
pub const EVDEV_KEY_F10: u32 = 68;

/// Linux evdev keycode for `F12` (`KEY_F12 = 88`).
///
/// Defined in `<linux/input-event-codes.h>`.
pub const EVDEV_KEY_F12: u32 = 88;

/// Linux evdev keycode for `Escape` (`KEY_ESC = 1`).
///
/// Pressing `Esc` while in keyboard-grab hold mode aborts recording without transcribing.
const EVDEV_KEY_ESC: u32 = 1;

/// Kernel `poll(2)` frame cadence (`20ms` = `50 FPS`) for smooth live waveform animation.
///
/// Drives steady `50 FPS` waveform rendering and checks [`HoldLease`] expiration every `20ms`.
const WAYLAND_FRAME_SLICE: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 20_000_000,
};

/// Grace period (`760ms`) before the first compositor key-repeat heartbeat arrives.
///
/// Covers standard Linux XKB `repeat_delay` (`600ms` default in Niri/Smithay) plus `160ms`
/// headroom so holding `F10` seamlessly transitions into repeat-heartbeat tracking.
const INITIAL_HOLD_LEASE: Duration = Duration::from_millis(760);

/// Lease window (`190ms`) once compositor key-repeat heartbeats (`cooldown-ms=70`) are active.
///
/// Gives `> 2.5x` tolerance over `70ms` repeat intervals while finishing listening within
/// `~110ms` average the instant the user lifts their finger off `F10`.
const ACTIVE_REPEAT_LEASE: Duration = Duration::from_millis(190);

/// Minimum elapsed time (`45ms`) after initial key-down to classify a new ping as key-repeat.
///
/// Distinguishes genuine XKB repeats from duplicate initial key events.
const REPEAT_CLASSIFY_THRESHOLD: Duration = Duration::from_millis(45);

/// Maximum wait (`600ms`) for `wl_keyboard.enter` when explicit keyboard-grab mode is used.
const FOCUS_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(600);

/// Safety ceiling (`120s`) on a single recording session.
///
/// Protects memory and disk if an object rests on `F10` or the session suspends mid-hold.
const MAX_HOLD_DURATION: Duration = Duration::from_secs(120);

/// Thread-safe keepalive lease refreshed by compositor key-repeat while `F10` is held down.
#[derive(Debug)]
pub struct HoldLease {
    state: Mutex<LeaseTimestamps>,
}

#[derive(Debug)]
struct LeaseTimestamps {
    started_at: Instant,
    last_ping_at: Instant,
    saw_repeat: bool,
}

impl Default for HoldLease {
    fn default() -> Self {
        Self::new()
    }
}

impl HoldLease {
    /// Starts a fresh hold lease anchored at `Instant::now()`.
    #[must_use]
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            state: Mutex::new(LeaseTimestamps {
                started_at: now,
                last_ping_at: now,
                saw_repeat: false,
            }),
        }
    }

    /// Refreshes the hold lease when a compositor key-repeat heartbeat arrives.
    pub fn ping(&self) {
        if let Ok(mut guard) = self.state.lock() {
            let now = Instant::now();
            if now.duration_since(guard.started_at) >= REPEAT_CLASSIFY_THRESHOLD {
                guard.saw_repeat = true;
            }
            guard.last_ping_at = now;
        }
    }

    /// Returns `true` once the user has released the held key and the lease window elapsed.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        let Ok(guard) = self.state.lock() else {
            return true;
        };
        let now = Instant::now();
        if guard.saw_repeat {
            now.duration_since(guard.last_ping_at) >= ACTIVE_REPEAT_LEASE
        } else {
            now.duration_since(guard.started_at) >= INITIAL_HOLD_LEASE
        }
    }
}

/// Reason a Wayland recording overlay session finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldOutcome {
    /// Push-to-talk key (`F10`) was released (or `Stop` was called); proceed with transcription.
    Released,
    /// User pressed `Esc` or externally cancelled; discard captured audio.
    Cancelled,
}

/// Configuration for an active Wayland recording overlay session.
#[derive(Debug)]
pub struct OverlayOptions {
    /// Compositor repeat-heartbeat lease for non-intrusive global hold-to-talk (`F10` in Niri).
    pub hold_lease: Option<Arc<HoldLease>>,
    /// Optional direct evdev keycode for `KeyboardInteractivity::Exclusive` client-bound holds.
    pub grab_key: Option<u32>,
    /// Whether to render the visible top-center `320x62` live waveform HUD (`true` by default).
    pub show_waveform: bool,
    /// Optional `$WAYLAND_DISPLAY` socket name from the caller's environment.
    pub wayland_display: Option<String>,
    /// Lock-free real-time microphone meter feeding the live waveform bars.
    pub meter: Arc<WaveformMeter>,
}

/// Parses a human-friendly key name (such as `"f10"` or `"f12"`) or numeric evdev code.
///
/// # Errors
///
/// Returns an error if `name` is neither a recognized key alias nor a valid `u32` evdev code.
pub fn parse_evdev_key(name: &str) -> Result<u32> {
    let normalized = name.trim().to_ascii_lowercase();
    let code = match normalized.as_str() {
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => EVDEV_KEY_F10,
        "f11" => 87,
        "f12" => EVDEV_KEY_F12,
        "capslock" | "caps_lock" => 58,
        "rctrl" | "rightctrl" => 97,
        "ralt" | "rightalt" => 100,
        "pause" => 119,
        "insert" => 110,
        "scrolllock" => 70,
        other => other
            .parse::<u32>()
            .with_context(|| format!("unknown push-to-talk key '{name}'"))?,
    };
    Ok(code)
}

/// Verifies that the active Wayland compositor advertises `zwlr_layer_shell_v1`.
///
/// # Errors
///
/// Returns an error if Wayland connection fails or `zwlr_layer_shell_v1` is not advertised.
pub fn probe_wayland_layer_shell(wayland_display: Option<&str>) -> Result<()> {
    let conn = connect_wayland(wayland_display)?;
    let (globals, event_queue) =
        registry_queue_init::<PttWaylandState>(&conn).context("Wayland registry init failed")?;
    let _shell: ZwlrLayerShellV1 = globals
        .bind(&event_queue.handle(), 1..=4, ())
        .context("compositor does not advertise zwlr_layer_shell_v1")?;
    Ok(())
}

/// Runs the top-center Wayland live waveform overlay and hold-to-talk release watch.
///
/// # Errors
///
/// Returns an error if the Wayland compositor connection or `zwlr_layer_shell_v1` setup fails.
pub fn run_recording_overlay(
    opts: &OverlayOptions,
    abort_flag: &AtomicBool,
) -> Result<HoldOutcome> {
    let conn = connect_wayland(opts.wayland_display.as_deref())?;
    let (globals, mut event_queue) =
        registry_queue_init::<PttWaylandState>(&conn).context("Wayland registry init failed")?;
    let qh = event_queue.handle();

    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .context("missing wl_compositor")?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=2, ()).context("missing wl_shm")?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).context("missing wl_seat")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("compositor missing zwlr_layer_shell_v1")?;

    let keyboard = opts.grab_key.map(|_| seat.get_keyboard(&qh, ()));
    let surface = compositor.create_surface(&qh, ());
    let empty_region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&empty_region));
    empty_region.destroy();

    let (width, height, top_margin) = if opts.show_waveform {
        (HUD_WIDTH, HUD_HEIGHT, HUD_TOP_MARGIN)
    } else {
        (1, 1, 0)
    };

    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "sttme-waveform".to_owned(),
        &qh,
        (),
    );
    layer_surface.set_size(width, height);
    layer_surface.set_anchor(Anchor::Top);
    layer_surface.set_margin(top_margin, 0, 0, 0);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(if opts.grab_key.is_some() {
        KeyboardInteractivity::Exclusive
    } else {
        KeyboardInteractivity::None
    });
    surface.commit();

    let mut state = PttWaylandState::new(opts.grab_key);
    event_queue
        .roundtrip(&mut state)
        .context("Wayland layer-surface configure roundtrip failed")?;

    let mut double_buf = DoubleShmBuffer::new(&shm, &qh, width, height)?;
    let outcome = animate_and_poll_loop(
        &conn,
        &mut event_queue,
        &mut state,
        &surface,
        &mut double_buf,
        opts,
        abort_flag,
    )?;

    layer_surface.destroy();
    surface.destroy();
    double_buf.destroy();
    if let Some(kb) = keyboard {
        kb.release();
    }
    drop(event_queue.roundtrip(&mut state));

    Ok(outcome)
}

struct DoubleShmBuffer {
    file: File,
    pool: wl_shm_pool::WlShmPool,
    buffers: [wl_buffer::WlBuffer; 2],
    scratch: Vec<u8>,
    frame_bytes: usize,
    width: i32,
    height: i32,
    active_slot: usize,
}

impl DoubleShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<PttWaylandState>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let width_i32 = i32::try_from(width).context("HUD width overflow")?;
        let height_i32 = i32::try_from(height).context("HUD height overflow")?;
        let stride = width_i32 * 4;
        let frame_bytes = (width as usize) * (height as usize) * 4;
        let total_bytes = frame_bytes * 2;
        let frame_bytes_i32 = i32::try_from(frame_bytes).context("HUD frame size overflow")?;
        let total_bytes_i32 = i32::try_from(total_bytes).context("HUD pool size overflow")?;

        let memfd = memfd_create(c"sttme-hud-shm", MemfdFlags::CLOEXEC)
            .context("failed to allocate Wayland shm memfd")?;
        let file = File::from(memfd);
        file.set_len(total_bytes as u64)
            .context("failed to size Wayland shm memfd")?;

        let pool = shm.create_pool(file.as_fd(), total_bytes_i32, qh, ());
        let buf0 = pool.create_buffer(
            0,
            width_i32,
            height_i32,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        let buf1 = pool.create_buffer(
            frame_bytes_i32,
            width_i32,
            height_i32,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );

        Ok(Self {
            file,
            pool,
            buffers: [buf0, buf1],
            scratch: vec![0_u8; frame_bytes],
            frame_bytes,
            width: width_i32,
            height: height_i32,
            active_slot: 0,
        })
    }

    fn present_frame(&mut self, surface: &wl_surface::WlSurface) -> Result<()> {
        let slot = self.active_slot;
        let offset = (slot * self.frame_bytes) as u64;
        self.file
            .write_all_at(&self.scratch, offset)
            .context("failed to write Wayland HUD frame")?;
        surface.attach(Some(&self.buffers[slot]), 0, 0);
        surface.damage_buffer(0, 0, self.width, self.height);
        surface.commit();
        self.active_slot ^= 1;
        Ok(())
    }

    fn destroy(self) {
        for buf in self.buffers {
            buf.destroy();
        }
        self.pool.destroy();
    }
}

fn animate_and_poll_loop(
    conn: &Connection,
    event_queue: &mut EventQueue<PttWaylandState>,
    state: &mut PttWaylandState,
    surface: &wl_surface::WlSurface,
    double_buf: &mut DoubleShmBuffer,
    opts: &OverlayOptions,
    abort_flag: &AtomicBool,
) -> Result<HoldOutcome> {
    let started = Instant::now();
    let mut animator = WaveformAnimator::new();
    let mut last_render = Instant::now()
        .checked_sub(Duration::from_millis(25))
        .unwrap_or_else(Instant::now);
    let mut presented_transparent_once = false;

    loop {
        let now = Instant::now();
        let elapsed = now.duration_since(started);

        if opts.show_waveform && now.duration_since(last_render) >= Duration::from_millis(18) {
            let rms = opts.meter.take_rms();
            let secs = elapsed.as_secs_f32();
            animator.step(rms, secs);
            animator.render_argb8888(&mut double_buf.scratch, secs);
            double_buf.present_frame(surface)?;
            last_render = now;
        } else if !opts.show_waveform && !presented_transparent_once {
            double_buf.scratch.fill(0);
            double_buf.present_frame(surface)?;
            presented_transparent_once = true;
        }

        event_queue
            .dispatch_pending(state)
            .context("Wayland dispatch failed")?;
        conn.flush().context("Wayland socket flush failed")?;

        if abort_flag.load(Ordering::Relaxed) {
            return Ok(HoldOutcome::Released);
        }
        if let Some(lease) = &opts.hold_lease
            && lease.is_expired()
        {
            return Ok(HoldOutcome::Released);
        }
        if let Some(outcome) = state.outcome {
            return Ok(outcome);
        }
        if opts.grab_key.is_some() && !state.focused && elapsed >= FOCUS_ACQUIRE_TIMEOUT {
            event!(
                name: "ptt.focus.timeout",
                Level::WARN,
                "Wayland overlay did not receive keyboard focus within 600ms; finishing hold",
            );
            return Ok(HoldOutcome::Released);
        }
        if elapsed >= MAX_HOLD_DURATION {
            return Ok(HoldOutcome::Released);
        }

        if let Some(read_guard) = event_queue.prepare_read() {
            let fd = read_guard.connection_fd();
            let mut poll_fds = [PollFd::new(&fd, PollFlags::IN)];
            let ready = poll(&mut poll_fds, Some(&WAYLAND_FRAME_SLICE)).unwrap_or(0);
            if ready > 0 {
                drop(read_guard.read());
            }
        }
    }
}

fn connect_wayland(wayland_display: Option<&str>) -> Result<Connection> {
    if let Some(display_name) = wayland_display
        && let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
    {
        let sock_path = PathBuf::from(runtime_dir).join(display_name);
        if sock_path.exists() {
            let stream =
                std::os::unix::net::UnixStream::connect(&sock_path).with_context(|| {
                    format!(
                        "failed to connect to Wayland socket {}",
                        sock_path.display()
                    )
                })?;
            return Connection::from_socket(stream)
                .context("failed to initialize Wayland connection");
        }
    }
    Connection::connect_to_env()
        .map_err(|err| anyhow::anyhow!("Wayland connection unavailable: {err}"))
}

/// Checks whether a Wayland `wl_keyboard.enter` raw `keys` byte array contains `target_evdev`.
#[must_use]
pub fn keys_array_contains_evdev(raw_keys: &[u8], target_evdev: u32) -> bool {
    raw_keys
        .as_chunks::<4>()
        .0
        .iter()
        .any(|&chunk| u32::from_ne_bytes(chunk) == target_evdev)
}

struct PttWaylandState {
    target_key: Option<u32>,
    configured: bool,
    focused: bool,
    outcome: Option<HoldOutcome>,
}

impl PttWaylandState {
    const fn new(target_key: Option<u32>) -> Self {
        Self {
            target_key,
            configured: false,
            focused: false,
            outcome: None,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for PttWaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for PttWaylandState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                proxy.ack_configure(serial);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.outcome = Some(HoldOutcome::Released);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for PttWaylandState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(target_key) = state.target_key else {
            return;
        };
        match event {
            wl_keyboard::Event::Enter { keys, .. } => {
                state.focused = true;
                if !keys_array_contains_evdev(&keys, target_key) {
                    state.outcome = Some(HoldOutcome::Released);
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                if key == EVDEV_KEY_ESC && matches!(key_state, KeyState::Pressed) {
                    state.outcome = Some(HoldOutcome::Cancelled);
                } else if key == target_key && matches!(key_state, KeyState::Released) {
                    state.outcome = Some(HoldOutcome::Released);
                }
            }
            wl_keyboard::Event::Leave { .. } if state.focused && state.outcome.is_none() => {
                state.outcome = Some(HoldOutcome::Released);
            }
            _ => {}
        }
    }
}

delegate_noop!(PttWaylandState: ignore wl_compositor::WlCompositor);
delegate_noop!(PttWaylandState: ignore wl_surface::WlSurface);
delegate_noop!(PttWaylandState: ignore wl_region::WlRegion);
delegate_noop!(PttWaylandState: ignore wl_shm::WlShm);
delegate_noop!(PttWaylandState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(PttWaylandState: ignore wl_buffer::WlBuffer);
delegate_noop!(PttWaylandState: ignore wl_seat::WlSeat);
delegate_noop!(PttWaylandState: ignore ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_evdev_key_resolves_f10_f12_and_numeric_codes() {
        assert_eq!(parse_evdev_key("F10").expect("F10"), 68);
        assert_eq!(parse_evdev_key("F12").expect("F12"), 88);
        assert_eq!(parse_evdev_key("68").expect("68"), 68);
        let _ = parse_evdev_key("not-a-key").unwrap_err();
    }

    #[test]
    fn keys_array_detects_depressed_f10_evdev_code() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&42_u32.to_ne_bytes());
        raw.extend_from_slice(&EVDEV_KEY_F10.to_ne_bytes());
        assert!(keys_array_contains_evdev(&raw, EVDEV_KEY_F10));
        assert!(!keys_array_contains_evdev(&raw, 59));
    }

    #[test]
    fn hold_lease_stays_alive_while_pinged() {
        let lease = HoldLease::new();
        assert!(!lease.is_expired());
        lease.ping();
        assert!(!lease.is_expired());
    }
}
