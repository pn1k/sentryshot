// SPDX-License-Identifier: GPL-2.0-or-later

use crate::config::BackgroundConfig;

/// Deterministic xorshift64, avoids a `rand` dependency and makes tests stable.
#[derive(Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        u32::try_from(x >> 32).unwrap_or(0)
    }
}

// Reinitialize the model when nearly the whole frame stays foreground for this
// many consecutive frames (camera move, light switch).
const REINIT_STREAK: u32 = 12;

// Foreground fraction above which the reinit streak counts up.
const REINIT_FG_NUM: u64 = 85;
const REINIT_FG_DEN: u64 = 100;

/// Sample-based background model (ViBe-like).
///
/// Unlike a plain frame difference, each pixel keeps a set of samples so slow
/// noise and gradual lighting drift are absorbed, and only background pixels
/// update the model so a stationary object does not fade into the background.
#[derive(Debug)]
pub(crate) struct BackgroundModel {
    width: usize,
    height: usize,
    samples: usize,
    threshold: u8,
    min_match: u8,
    update_rate: u32,

    initialized: bool,
    models: Vec<u8>,
    rng: Rng,

    fg_streak: u32,
}

impl BackgroundModel {
    pub(crate) fn new(width: usize, height: usize, config: BackgroundConfig) -> Self {
        let samples = usize::from(config.samples.max(1));
        Self {
            width,
            height,
            samples,
            threshold: config.threshold,
            min_match: config.min_match.clamp(1, config.samples.max(1)),
            update_rate: u32::from(config.update_rate.max(1)),
            initialized: false,
            models: vec![0; width * height * samples],
            rng: Rng::new(0x9E37_79B9_7F4A_7C15),
            fg_streak: 0,
        }
    }

    /// Compares `gray` with the model, writing `0` (background) or `255`
    /// (foreground) into `mask`, then updates the model.
    ///
    /// On the first frame (and after a reinit) the model is initialized from
    /// the current frame and `mask` is all background.
    pub(crate) fn update_and_detect(&mut self, gray: &[u8], mask: &mut [u8]) {
        let n = gray.len();
        debug_assert_eq!(self.width * self.height, n, "model/frame size mismatch");
        debug_assert_eq!(mask.len(), n, "mask/frame size mismatch");
        if n == 0 {
            return;
        }

        let samples = self.samples;
        let threshold = self.threshold;
        let min_match = u32::from(self.min_match);

        if !self.initialized {
            self.init_from(gray, mask);
            return;
        }

        let update_rate = self.update_rate;
        let width = self.width;
        let height = self.height;
        let models = &mut self.models;
        let rng = &mut self.rng;

        let mut fg_count: u64 = 0;
        for i in 0..n {
            let v = gray[i];
            let base = i * samples;

            let mut matches: u32 = 0;
            for k in 0..samples {
                if u8::abs_diff(v, models[base + k]) < threshold {
                    matches += 1;
                    if matches >= min_match {
                        break;
                    }
                }
            }
            let is_foreground = matches < min_match;
            mask[i] = u8::from(is_foreground) * 255;
            if is_foreground {
                fg_count += 1;
                // Foreground pixels never update the model so a stationary
                // object is not absorbed into the background.
                continue;
            }

            // Random sub-sampling update of a single sample.
            let r = rng.next_u32();
            if r % update_rate == 0 {
                let idx = usize::try_from((r / update_rate) % u32::try_from(samples).unwrap_or(1))
                    .unwrap_or(0);
                models[base + idx] = v;

                // Spatial diffusion: also update a random sample of a random
                // neighbor so ghosts fade faster after an object leaves.
                let r2 = rng.next_u32();
                let dx = match r2 % 3 {
                    0 => -1,
                    1 => 0,
                    _ => 1,
                };
                let dy = match (r2 / 3) % 3 {
                    0 => -1,
                    1 => 0,
                    _ => 1,
                };
                let nidx =
                    usize::try_from((r2 / 9) % u32::try_from(samples).unwrap_or(1)).unwrap_or(0);
                if let Some(n) = neighbor_index(i, dx, dy, width, height) {
                    models[n * samples + nidx] = v;
                }
            }
        }

        // Reinit safeguard: a near-total foreground that persists means the
        // scene changed globally; rebuild the model from the current frame.
        if fg_count * REINIT_FG_DEN > u64::try_from(n).unwrap_or(0) * REINIT_FG_NUM {
            self.fg_streak = self.fg_streak.saturating_add(1);
            if self.fg_streak >= REINIT_STREAK {
                self.init_from(gray, mask);
            }
        } else {
            self.fg_streak = 0;
        }
    }

    fn init_from(&mut self, gray: &[u8], mask: &mut [u8]) {
        let samples = self.samples;
        for (i, &v) in gray.iter().enumerate() {
            let base = i * samples;
            self.models[base..base + samples].fill(v);
        }
        self.initialized = true;
        self.fg_streak = 0;
        mask[..gray.len()].fill(0);
    }
}

// Index of the pixel offset by (dx, dy), clamped to the frame borders.
#[allow(clippy::many_single_char_names)]
fn neighbor_index(i: usize, dx: i64, dy: i64, w: usize, h: usize) -> Option<usize> {
    let w64 = i64::try_from(w).ok()?;
    let h64 = i64::try_from(h).ok()?;
    let x = i64::try_from(i % w).ok()? + dx;
    let y = i64::try_from(i / w).ok()? + dy;
    let x = x.clamp(0, w64 - 1);
    let y = y.clamp(0, h64 - 1);
    usize::try_from(y * w64 + x).ok()
}

#[allow(
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::unseparated_literal_suffix
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn model(w: usize, h: usize) -> BackgroundModel {
        BackgroundModel::new(w, h, BackgroundConfig::default())
    }

    #[test]
    fn test_first_frame_is_background() {
        let mut m = model(2, 2);
        let mut mask = vec![1; 4];
        m.update_and_detect(&[10, 20, 30, 40], &mut mask);
        assert_eq!(vec![0, 0, 0, 0], mask);
    }

    #[test]
    fn test_identical_frame_is_background() {
        let mut m = model(2, 2);
        let frame = [10, 20, 30, 40];
        let mut mask = vec![0; 4];
        m.update_and_detect(&frame, &mut mask);
        m.update_and_detect(&frame, &mut mask);
        assert_eq!(vec![0, 0, 0, 0], mask);
    }

    #[test]
    fn test_changed_pixels_are_foreground() {
        let mut m = model(2, 2);
        let frame = [10, 20, 30, 40];
        let mut mask = vec![0; 4];
        m.update_and_detect(&frame, &mut mask);
        // Large change on the first two pixels.
        m.update_and_detect(&[250, 250, 30, 40], &mut mask);
        assert_eq!(vec![255, 255, 0, 0], mask);
    }

    #[test]
    fn test_small_change_is_tolerated() {
        let mut m = model(2, 2);
        let mut mask = vec![0; 4];
        m.update_and_detect(&[100, 100, 100, 100], &mut mask);
        // Within threshold (default 20) -> still background.
        m.update_and_detect(&[108, 108, 108, 108], &mut mask);
        assert_eq!(vec![0, 0, 0, 0], mask);
    }

    #[test]
    fn test_standing_object_stays_foreground() {
        let mut m = model(2, 2);
        let base = [10, 20, 30, 40];
        let mut mask = vec![0; 4];
        m.update_and_detect(&base, &mut mask);

        // An object appears on pixel 0 and stays; it must not fade into the
        // background no matter how long it stands.
        let mut frame = base;
        frame[0] = 200;
        for _ in 0..300 {
            m.update_and_detect(&frame, &mut mask);
            assert_eq!(255, mask[0], "standing object was absorbed");
        }
    }

    #[test]
    fn test_global_change_reinits_model() {
        let mut m = model(2, 2);
        let mut mask = vec![0; 4];
        m.update_and_detect(&[10, 10, 10, 10], &mut mask);

        // Every pixel changes hugely; after the reinit streak the model is
        // rebuilt from the current frame and the scene is background again.
        for _ in 0..REINIT_STREAK {
            m.update_and_detect(&[200, 200, 200, 200], &mut mask);
        }
        m.update_and_detect(&[200, 200, 200, 200], &mut mask);
        assert_eq!(vec![0, 0, 0, 0], mask);
    }

    #[test]
    fn test_partial_change_does_not_reinit() {
        let mut m = model(2, 2);
        let mut mask = vec![0; 4];
        m.update_and_detect(&[10, 10, 10, 10], &mut mask);

        // One foreground pixel out of four (25%) is far below the reinit
        // threshold, so the streak never accumulates.
        for _ in 0..(REINIT_STREAK * 4) {
            m.update_and_detect(&[200, 10, 10, 10], &mut mask);
            assert_eq!(255, mask[0]);
        }
    }
}
