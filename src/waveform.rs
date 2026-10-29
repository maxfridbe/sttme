// Rust guideline compliant 2026-02-21
//! Anti-aliased top-center Wayland `ARGB8888` live voice waveform HUD renderer.
//!
//! Renders a translucent rounded LCARS capsule (`272x50`) with a pulsing recording beacon
//! and `30` real-time scrolling voice waveform bars using signed-distance-field (`SDF`)
//! sub-pixel anti-aliasing and premultiplied alpha.

use std::f32::consts::PI;

/// Logical width in pixels of the top-center Wayland waveform overlay capsule.
///
/// Wide enough to display a `30`-bar scrolling voice trace without obscuring terminal content.
pub const HUD_WIDTH: u32 = 272;

/// Logical height in pixels of the top-center Wayland waveform overlay capsule.
///
/// Compact `50px` stadium height floats cleanly below the top screen edge on Niri.
pub const HUD_HEIGHT: u32 = 50;

/// Top screen margin in logical pixels applied to the top-center `zwlr_layer_surface_v1`.
///
/// Leaves a `14px` breathing gap below the top edge of the monitor.
pub const HUD_TOP_MARGIN: i32 = 14;

/// Number of vertical rounded capsule bars in the live voice waveform display.
///
/// At `50 FPS` (`20ms` per frame), `30` bars represent a `600ms` rolling voice history window.
pub const WAVEFORM_BARS: usize = 30;

/// Noise-floor offset (`0.0018`) subtracted before perceptual square-root compression.
///
/// Keeps room silence calm at the minimal idle bar height while preserving quiet speech.
const NOISE_FLOOR_RMS: f32 = 0.0018;

/// Gain multiplier (`11.0`) mapping normal laptop-mic speech RMS (`0.02..0.12`) to full scale.
///
/// Paired with `.sqrt()` compression so conversational speech fills `45%..95%` of the HUD.
const SPEECH_VISUAL_GAIN: f32 = 11.0;

/// Fast attack smoothing weight (`0.68`) when voice energy rises.
///
/// Makes vowel and consonant onsets feel instantaneous on screen.
const LEVEL_ATTACK: f32 = 0.68;

/// Smooth release decay (`0.78`) per `20ms` frame when voice energy drops.
///
/// Prevents jittery single-frame dropouts between syllables.
const LEVEL_DECAY: f32 = 0.78;

/// Rolling waveform bar state updated at `50 FPS` while listening.
#[derive(Debug, Clone)]
pub struct WaveformAnimator {
    bars: [f32; WAVEFORM_BARS],
    smoothed_level: f32,
}

impl Default for WaveformAnimator {
    fn default() -> Self {
        Self::new()
    }
}

impl WaveformAnimator {
    /// Creates an initialized waveform animator at idle baseline.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bars: [0.08; WAVEFORM_BARS],
            smoothed_level: 0.0,
        }
    }

    /// Advances the scrolling waveform by one frame given raw microphone `rms` and `elapsed_secs`.
    pub fn step(&mut self, raw_rms: f32, elapsed_secs: f32) {
        let compressed = ((raw_rms - NOISE_FLOOR_RMS).max(0.0) * SPEECH_VISUAL_GAIN)
            .sqrt()
            .clamp(0.0, 1.0);

        if compressed > self.smoothed_level {
            self.smoothed_level += (compressed - self.smoothed_level) * LEVEL_ATTACK;
        } else {
            self.smoothed_level *= LEVEL_DECAY;
        }

        self.bars.copy_within(1..WAVEFORM_BARS, 0);
        let shimmer = 0.06 + 0.03 * (elapsed_secs * 6.5).sin().abs();
        self.bars[WAVEFORM_BARS - 1] = (self.smoothed_level + shimmer).clamp(0.06, 1.0);
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
        // Gentle edge taper at both ends so tall peaks tuck smoothly inside the stadium curve.
        let norm_pos = (i as f32) / max_idx;
        let edge_taper = (PI * norm_pos).sin().powf(0.32).clamp(0.35, 1.0);
        let micro_wave = 0.85 * ((elapsed_secs * 7.0) + (i as f32) * 0.55).sin().abs();
        let half_h = 2.2 + (level * edge_taper * 15.8) + micro_wave * (0.3 + level * 0.7);
        out[i] = half_h.clamp(2.2, 18.0);
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

    // Base translucent dark obsidian fill (#10141EEB)
    let mut red = 16.0_f32;
    let mut green = 20.0_f32;
    let mut blue = 30.0_f32;
    let mut alpha = 236.0 * pill_alpha;

    // Anti-aliased 1.5px LCARS warm gold/amber border around the stadium perimeter
    let border_dist = (d_pill + 0.85).abs() - 0.75;
    let border_cov = smooth_coverage(border_dist) * pill_alpha;
    if border_cov > 0.0 {
        let br = 255.0;
        let bg = 158.0 + 35.0 * voice_boost;
        let bb = 58.0 + 40.0 * voice_boost;
        let strength = border_cov * (0.68 + 0.30 * voice_boost);
        red = lerp(red, br, strength);
        green = lerp(green, bg, strength);
        blue = lerp(blue, bb, strength);
    }

    // Left pulsing LCARS status beacon at (26.5, 25.0)
    let dot_dist = (x - 26.5).hypot(y - cy);
    let halo = ((1.0 - dot_dist / (11.5 + 3.0 * voice_boost)).max(0.0)).powi(2);
    if halo > 0.0 {
        red = lerp(red, 255.0, halo * (0.28 + 0.30 * pulse));
        green = lerp(green, 110.0, halo * (0.22 + 0.25 * pulse));
        blue = lerp(blue, 48.0, halo * 0.18);
    }
    let dot_radius = 4.8 + 0.8 * pulse + 1.1 * voice_boost;
    let dot_cov = smooth_coverage(dot_dist - dot_radius) * pill_alpha;
    if dot_cov > 0.0 {
        red = lerp(red, 255.0, dot_cov);
        green = lerp(green, 108.0 + 52.0 * pulse, dot_cov);
        blue = lerp(blue, 52.0, dot_cov);
    }

    // Vertical LCARS accent divider tick at x = 46.5
    let div_d = sdf_rounded_box(x - 46.5, y - cy, 1.2, 10.5, 1.2);
    let div_cov = smooth_coverage(div_d) * pill_alpha * 0.55;
    if div_cov > 0.0 {
        red = lerp(red, 255.0, div_cov);
        green = lerp(green, 166.0, div_cov);
        blue = lerp(blue, 74.0, div_cov);
    }

    // 30 live scrolling waveform bars across x = 58.0 .. 252.3 (step = 6.7px)
    if (54.0..=257.0).contains(&x) {
        let rel_x = x - 58.0;
        let idx = (rel_x / 6.7).round();
        if (0.0..(WAVEFORM_BARS as f32)).contains(&idx) {
            let bar_idx = idx as usize;
            let bar_cx = 58.0 + (bar_idx as f32) * 6.7;
            let half_h = bar_half_heights[bar_idx];
            let bar_d = sdf_rounded_box(x - bar_cx, y - cy, 1.85, half_h, 1.85);
            let bar_cov = smooth_coverage(bar_d) * pill_alpha;
            if bar_cov > 0.0 {
                let intensity = ((half_h - 2.2) / 15.8).clamp(0.0, 1.0);
                // Blend from LCARS sky-cyan (#58C4FF) at quiet levels to LCARS gold (#FFAE38) at peaks
                let wr = lerp(88.0, 255.0, intensity);
                let wg = lerp(196.0, 182.0, intensity);
                let wb = lerp(255.0, 58.0, intensity);
                red = lerp(red, wr, bar_cov);
                green = lerp(green, wg, bar_cov);
                blue = lerp(blue, wb, bar_cov);
                alpha = alpha.max(248.0 * pill_alpha * bar_cov);
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
    fn waveform_hud_renders_transparent_corners_and_opaque_bars() {
        let mut anim = WaveformAnimator::new();
        anim.step(0.08, 0.25);

        let mut pixels = vec![0_u8; (HUD_WIDTH * HUD_HEIGHT * 4) as usize];
        anim.render_argb8888(&mut pixels, 0.25);

        // Top-left corner (0, 0) outside rounded stadium must be completely transparent.
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);

        // Center pixel inside stadium must have high alpha and non-zero RGB.
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
