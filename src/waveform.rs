// Rust guideline compliant 2026-02-21
//! Anti-aliased top-center Wayland `ARGB8888` live voice waveform HUD renderer.
//!
//! Renders a translucent rounded LCARS capsule (`320x62`) with a pulsing recording beacon
//! and `34` real-time adaptive-gain scrolling voice waveform bars using signed-distance-field
//! (`SDF`) sub-pixel anti-aliasing and premultiplied alpha.

use std::f32::consts::PI;

/// Logical width in pixels of the top-center Wayland waveform overlay capsule.
///
/// Sized generously for `HiDPI` / 4K Wayland outputs while keeping a sleek floating pill profile.
pub const HUD_WIDTH: u32 = 320;

/// Logical height in pixels of the top-center Wayland waveform overlay capsule.
///
/// Gives `48px` of vertical bar excursion so spoken syllables leap unmistakably vs silence.
pub const HUD_HEIGHT: u32 = 62;

/// Top screen margin in logical pixels applied to the top-center `zwlr_layer_surface_v1`.
///
/// Leaves a `16px` breathing gap below the top edge of the active monitor.
pub const HUD_TOP_MARGIN: i32 = 16;

/// Number of vertical rounded capsule bars in the live voice waveform display.
///
/// At `50 FPS` (`20ms` per frame), `34` bars represent a `680ms` rolling voice history window.
pub const WAVEFORM_BARS: usize = 34;

/// Noise-floor offset (`0.0022`) subtracted before adaptive perceptual normalization.
///
/// Keeps room silence dead-calm at minimal `4px` dots so speech creates high visual contrast.
const NOISE_FLOOR_LEVEL: f32 = 0.0022;

/// Minimum adaptive reference ceiling (`0.032`) for soft conversational speech.
///
/// Prevents background fan hum from being amplified to full height while ensuring normal
/// laptop-microphone speech (`0.02..0.12` blended RMS+peak) reaches `70%..100%` excursion.
const MIN_ADAPTIVE_PEAK: f32 = 0.032;

/// Per-frame decay (`0.992`) of the adaptive peak reference (`~2.5s` half-life at `50 FPS`).
///
/// Smoothly re-sensitizes the display when transitioning from loud speech to quieter speech.
const ADAPTIVE_PEAK_DECAY: f32 = 0.992;

/// Fast attack smoothing weight (`0.86`) when voice energy rises.
///
/// Makes vowel and consonant onsets jump immediately on screen.
const LEVEL_ATTACK: f32 = 0.86;

/// Crisp release decay (`0.60`) per `20ms` frame when voice energy drops between syllables.
///
/// Carves deep valleys between spoken words so the scrolling waveform visibly dances.
const LEVEL_DECAY: f32 = 0.60;

/// Rolling adaptive-gain waveform bar state updated at `50 FPS` while listening.
#[derive(Debug, Clone)]
pub struct WaveformAnimator {
    bars: [f32; WAVEFORM_BARS],
    smoothed_level: f32,
    adaptive_peak: f32,
}

impl Default for WaveformAnimator {
    fn default() -> Self {
        Self::new()
    }
}

impl WaveformAnimator {
    /// Creates an initialized waveform animator at calm idle baseline.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bars: [0.0; WAVEFORM_BARS],
            smoothed_level: 0.0,
            adaptive_peak: 0.045,
        }
    }

    /// Advances the scrolling waveform by one frame given microphone `level` and `elapsed_secs`.
    pub fn step(&mut self, raw_level: f32, _elapsed_secs: f32) {
        let clean = (raw_level - NOISE_FLOOR_LEVEL).max(0.0);
        self.adaptive_peak = (self.adaptive_peak * ADAPTIVE_PEAK_DECAY)
            .max(clean)
            .max(MIN_ADAPTIVE_PEAK);

        let normalized = (clean / self.adaptive_peak).powf(0.62).clamp(0.0, 1.0);

        if normalized > self.smoothed_level {
            self.smoothed_level += (normalized - self.smoothed_level) * LEVEL_ATTACK;
        } else {
            self.smoothed_level *= LEVEL_DECAY;
        }

        self.bars.copy_within(1..WAVEFORM_BARS, 0);
        self.bars[WAVEFORM_BARS - 1] = self.smoothed_level.clamp(0.0, 1.0);
    }

    /// Renders the current HUD frame into a `HUD_WIDTH * HUD_HEIGHT * 4` premultiplied `ARGB8888` slice.
    pub fn render_argb8888(&self, buf: &mut [u8], elapsed_secs: f32) {
        let width = HUD_WIDTH as usize;
        let height = HUD_HEIGHT as usize;
        let expected_bytes = width * height * 4;
        if buf.len() < expected_bytes {
            return;
        }

        let bar_half_heights = compute_bar_half_heights(&self.bars, elapsed_secs);
        let pulse = 0.5 + 0.5 * (elapsed_secs * 5.2).sin();
        let voice_boost = self.smoothed_level;

        for py in 0..height {
            let y = (py as f32) + 0.5;
            for px in 0..width {
                let x = (px as f32) + 0.5;
                let argb = shade_hud_pixel(x, y, &bar_half_heights, pulse, voice_boost);
                let idx = (py * width + px) * 4;
                buf[idx..idx + 4].copy_from_slice(&argb.to_ne_bytes());
            }
        }
    }
}

fn compute_bar_half_heights(
    bars: &[f32; WAVEFORM_BARS],
    elapsed_secs: f32,
) -> [f32; WAVEFORM_BARS] {
    let mut out = [2.0_f32; WAVEFORM_BARS];
    let max_idx = (WAVEFORM_BARS - 1) as f32;

    for (i, &level) in bars.iter().enumerate() {
        let norm_pos = (i as f32) / max_idx;
        let edge_taper = (PI * norm_pos).sin().powf(0.28).clamp(0.40, 1.0);
        let idle_ripple = 0.22 * ((elapsed_secs * 5.0) + (i as f32) * 0.45).sin().abs();
        let speech_swing = level * edge_taper * 22.5;
        out[i] = (2.0 + speech_swing + idle_ripple).clamp(2.0, 24.5);
    }
    out
}

fn shade_hud_pixel(
    x: f32,
    y: f32,
    bar_half_heights: &[f32; WAVEFORM_BARS],
    pulse: f32,
    voice_boost: f32,
) -> u32 {
    let cx = (HUD_WIDTH as f32) * 0.5;
    let cy = (HUD_HEIGHT as f32) * 0.5;
    let d_pill = sdf_rounded_box(x - cx, y - cy, cx - 1.5, cy - 1.5, cy - 1.5);

    if d_pill > 0.8 {
        return 0;
    }

    let pill_alpha = smooth_coverage(d_pill);

    // Deep translucent obsidian capsule (#10141EF0)
    let mut red = 16.0_f32;
    let mut green = 20.0_f32;
    let mut blue = 30.0_f32;
    let mut alpha = 240.0 * pill_alpha;

    // Anti-aliased 1.6px LCARS gold/amber perimeter border that brightens with voice level
    let border_dist = (d_pill + 0.9).abs() - 0.8;
    let border_cov = smooth_coverage(border_dist) * pill_alpha;
    if border_cov > 0.0 {
        let br = 255.0;
        let bg = 158.0 + 42.0 * voice_boost;
        let bb = 58.0 + 50.0 * voice_boost;
        let strength = border_cov * (0.72 + 0.28 * voice_boost);
        red = lerp(red, br, strength);
        green = lerp(green, bg, strength);
        blue = lerp(blue, bb, strength);
    }

    // Left pulsing LCARS status beacon at (30.0, 31.0)
    let dot_dist = (x - 30.0).hypot(y - cy);
    let halo = ((1.0 - dot_dist / (13.5 + 4.5 * voice_boost)).max(0.0)).powi(2);
    if halo > 0.0 {
        red = lerp(red, 255.0, halo * (0.30 + 0.35 * pulse));
        green = lerp(green, 112.0, halo * (0.24 + 0.26 * pulse));
        blue = lerp(blue, 48.0, halo * 0.18);
    }
    let dot_radius = 5.5 + 0.9 * pulse + 1.8 * voice_boost;
    let dot_cov = smooth_coverage(dot_dist - dot_radius) * pill_alpha;
    if dot_cov > 0.0 {
        red = lerp(red, 255.0, dot_cov);
        green = lerp(green, 104.0 + 58.0 * pulse, dot_cov);
        blue = lerp(blue, 50.0, dot_cov);
    }

    // Vertical LCARS accent divider tick at x = 52.0
    let div_d = sdf_rounded_box(x - 52.0, y - cy, 1.3, 13.0, 1.3);
    let div_cov = smooth_coverage(div_d) * pill_alpha * 0.6;
    if div_cov > 0.0 {
        red = lerp(red, 255.0, div_cov);
        green = lerp(green, 168.0, div_cov);
        blue = lerp(blue, 74.0, div_cov);
    }

    // 34 live scrolling waveform bars across x = 65.0 .. 296.0 (step = 7.0px)
    if (61.0..=301.0).contains(&x) {
        let rel_x = x - 65.0;
        let idx = (rel_x / 7.0).round();
        if (0.0..(WAVEFORM_BARS as f32)).contains(&idx) {
            let bar_idx = idx as usize;
            let bar_cx = 65.0 + (bar_idx as f32) * 7.0;
            let half_h = bar_half_heights[bar_idx];
            let bar_d = sdf_rounded_box(x - bar_cx, y - cy, 2.1, half_h, 2.1);
            let bar_cov = smooth_coverage(bar_d) * pill_alpha;
            if bar_cov > 0.0 {
                let intensity = ((half_h - 2.0) / 22.5).clamp(0.0, 1.0);
                // Blend from LCARS sky-cyan (#58C8FF) at quiet levels to vivid LCARS gold (#FFB43A)
                let wr = lerp(88.0, 255.0, intensity);
                let wg = lerp(200.0, 180.0, intensity);
                let wb = lerp(255.0, 58.0, intensity);
                red = lerp(red, wr, bar_cov);
                green = lerp(green, wg, bar_cov);
                blue = lerp(blue, wb, bar_cov);
                alpha = alpha.max(250.0 * pill_alpha * bar_cov);
            }
        }
    }

    pack_premultiplied_argb(red, green, blue, alpha)
}

fn sdf_rounded_box(px: f32, py: f32, half_w: f32, half_h: f32, radius: f32) -> f32 {
    let qx = px.abs() - (half_w - radius);
    let qy = py.abs() - (half_h - radius);
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    let inside = qx.max(qy).min(0.0);
    outside + inside - radius
}

fn smooth_coverage(signed_dist: f32) -> f32 {
    (0.5 - signed_dist).clamp(0.0, 1.0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn pack_premultiplied_argb(r: f32, g: f32, b: f32, a: f32) -> u32 {
    let alpha = a.round().clamp(0.0, 255.0);
    let scale = alpha / 255.0;
    let rp = (r * scale).round().clamp(0.0, alpha) as u32;
    let gp = (g * scale).round().clamp(0.0, alpha) as u32;
    let bp = (b * scale).round().clamp(0.0, alpha) as u32;
    let ap = alpha as u32;
    (ap << 24) | (rp << 16) | (gp << 8) | bp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_hud_renders_transparent_corners_and_reactive_speech_bars() {
        let mut anim = WaveformAnimator::new();
        anim.step(0.0, 0.10);
        let idle_level = anim.bars[WAVEFORM_BARS - 1];
        assert!(idle_level < 0.05);

        anim.step(0.05, 0.12);
        let speech_level = anim.bars[WAVEFORM_BARS - 1];
        assert!(
            speech_level > 0.80,
            "expected strong bar excursion, got {speech_level}"
        );

        let mut pixels = vec![0_u8; (HUD_WIDTH * HUD_HEIGHT * 4) as usize];
        anim.render_argb8888(&mut pixels, 0.12);

        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);

        let center_idx =
            (((HUD_HEIGHT as usize) / 2) * (HUD_WIDTH as usize) + ((HUD_WIDTH as usize) / 2)) * 4;
        let word = u32::from_ne_bytes([
            pixels[center_idx],
            pixels[center_idx + 1],
            pixels[center_idx + 2],
            pixels[center_idx + 3],
        ]);
        let alpha = (word >> 24) & 0xFF;
        assert!(
            alpha >= 220,
            "expected opaque HUD interior, got alpha={alpha}"
        );
    }
}
