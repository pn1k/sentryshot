// SPDX-License-Identifier: GPL-2.0-or-later

use crate::blobs::Bbox;
use crate::config::BackgroundConfig;

/// Deterministic xorshift64, avoids a `rand` dependency and makes tests stable.
#[derive(Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Cheap counter-based finalizer (splitmix-style). Stateless per input, so
/// per-pixel draws do not form a serial dependency chain and the hot loop
/// stays pipelined.
#[inline]
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z ^= z >> 29;
    z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^= z >> 32;
    z
}

/// Maps a uniform `u32` to `[0, n)` without an integer division (Lemire's
/// fastrange).
#[inline]
fn fastrange(r: u32, n: u32) -> u32 {
    u32::try_from((u64::from(r) * u64::from(n)) >> 32).unwrap_or(0)
}

#[inline]
fn lo32(v: u64) -> u32 {
    u32::try_from(v & 0xFFFF_FFFF).unwrap_or(0)
}

#[inline]
fn hi32(v: u64) -> u32 {
    u32::try_from(v >> 32).unwrap_or(0)
}

// Foreground percent above which the reinit streak counts up is configurable
// (`reinit_fg`), as is the streak length (`reinit_streak`).

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

    // A random draw below this threshold triggers a model update, which
    // replaces an integer division per pixel with one comparison.
    update_threshold: u32,

    initialized: bool,
    // Sample-major (SoA) layout: plane `k` is `models[k * n..(k + 1) * n]`
    // and holds sample `k` of every pixel. Each plane is streamed linearly,
    // which lets the compiler vectorize the per-sample comparison.
    n: usize,
    models: Vec<u8>,
    // Per-pixel count of matching samples for the current frame.
    matches: Vec<u8>,
    rng: Rng,

    fg_streak: u32,

    // Global-change thresholds, from the config.
    reinit_fg: u64,
    reinit_streak: u32,
}

impl BackgroundModel {
    pub(crate) fn new(width: usize, height: usize, config: BackgroundConfig) -> Self {
        let samples = usize::from(config.samples.max(1));
        let update_rate = u32::from(config.update_rate.max(1));
        let n = width * height;
        Self {
            width,
            height,
            samples,
            threshold: config.threshold,
            min_match: config.min_match.clamp(1, config.samples.max(1)),
            update_threshold: u32::MAX / update_rate,
            initialized: false,
            n,
            models: vec![0; n * samples],
            matches: vec![0; n],
            rng: Rng::new(0x9E37_79B9_7F4A_7C15),
            fg_streak: 0,
            reinit_fg: u64::from(config.reinit_fg),
            reinit_streak: u32::from(config.reinit_streak),
        }
    }

    fn plane_mut(&mut self, k: usize) -> &mut [u8] {
        let n = self.n;
        &mut self.models[k * n..(k + 1) * n]
    }

    /// Compares `gray` with the model, writing `0` (background) or `255`
    /// (foreground) into `mask`, then updates the model.
    ///
    /// Returns the number of foreground pixels, so callers can skip the rest
    /// of the pipeline on a fully static frame.
    ///
    /// On the first frame (and after a reinit) the model is initialized from
    /// the current frame and `mask` is all background.
    pub(crate) fn update_and_detect(&mut self, gray: &[u8], mask: &mut [u8]) -> u64 {
        let n = gray.len();
        debug_assert_eq!(self.width * self.height, n, "model/frame size mismatch");
        debug_assert_eq!(mask.len(), n, "mask/frame size mismatch");
        if n == 0 {
            return 0;
        }

        if !self.initialized {
            self.init_from(gray, mask);
            return 0;
        }

        let samples = self.samples;
        let samples_u32 = u32::try_from(samples).unwrap_or(1);
        let threshold = self.threshold;
        let min_match = self.min_match;
        let update_threshold = self.update_threshold;

        // One random seed per frame; per-pixel randomness comes from hashing
        // the pixel index, which has no carried state.
        let seed = self.rng.next_u64();

        let width = self.width;
        let height = self.height;
        let plane_len = self.n;
        let Self {
            models, matches, ..
        } = self;

        // Phase A: count matching samples per pixel. Plane-major loops with
        // no early exit and branch-free compares, so the compiler vectorizes
        // them for any target without intrinsics.
        matches.fill(0);
        for k in 0..samples {
            let plane = &models[k * plane_len..(k + 1) * plane_len];
            for ((m, &v), &s) in matches.iter_mut().zip(gray).zip(plane) {
                // |v - s| < threshold, with abs_diff = max - min (no wrap).
                *m += u8::from(u8::abs_diff(v, s) < threshold);
            }
        }

        // Phase B: foreground mask and count. `matches <= samples <= 255`,
        // so the u8 counter cannot overflow.
        let mut fg_count: u64 = 0;
        for (out, &m) in mask.iter_mut().zip(matches.iter()) {
            let is_foreground = m < min_match;
            *out = if is_foreground { 255 } else { 0 };
            fg_count += u64::from(is_foreground);
        }

        // Phase C: sparse model update, scalar. Only background pixels take
        // part; foreground never updates so a stationary object is not
        // absorbed into the background.
        for (i, (&v, &out)) in gray.iter().zip(mask.iter()).enumerate() {
            if out != 0 {
                continue;
            }
            // Random sub-sampling update of a single sample.
            let h = mix(seed ^ u64::try_from(i).unwrap_or(0));
            if hi32(h) >= update_threshold {
                continue;
            }
            let idx = usize::try_from(fastrange(lo32(h), samples_u32)).unwrap_or(0);
            models[idx * plane_len + i] = v;

            // Spatial diffusion: also update a random sample of a random
            // neighbor so ghosts fade faster after an object leaves.
            let h2 = mix(h);
            let cell = fastrange(hi32(h2), 9);
            let dx = i64::from(cell % 3) - 1;
            let dy = i64::from(cell / 3) - 1;
            let nidx = usize::try_from(fastrange(lo32(h2), samples_u32)).unwrap_or(0);
            if let Some(nb) = neighbor_index(i, dx, dy, width, height) {
                models[nidx * plane_len + nb] = v;
            }
        }

        // Reinit safeguard: a near-total foreground that persists means the
        // scene changed globally; rebuild the model from the current frame.
        if fg_count * 100 > u64::try_from(n).unwrap_or(0) * self.reinit_fg {
            self.fg_streak = self.fg_streak.saturating_add(1);
            if self.fg_streak >= self.reinit_streak {
                self.init_from(gray, mask);
            }
        } else {
            self.fg_streak = 0;
        }

        fg_count
    }

    /// Forces the model to match the current frame inside the given
    /// rectangles, so objects that stood still stop producing foreground
    /// (a parked car, moved furniture) instead of triggering forever.
    pub(crate) fn absorb(&mut self, gray: &[u8], boxes: &[Bbox]) {
        let samples = self.samples;
        let w = self.width;
        let h = self.height;
        for b in boxes {
            let x0 = usize::try_from(b.x).unwrap_or(0).min(w);
            let y0 = usize::try_from(b.y).unwrap_or(0).min(h);
            let x1 = usize::try_from(b.x.saturating_add(b.w)).unwrap_or(0).min(w);
            let y1 = usize::try_from(b.y.saturating_add(b.h)).unwrap_or(0).min(h);
            for k in 0..samples {
                let plane = self.plane_mut(k);
                for y in y0..y1 {
                    let row = y * w;
                    for x in x0..x1 {
                        plane[row + x] = gray.get(row + x).copied().unwrap_or(0);
                    }
                }
            }
        }
    }

    fn init_from(&mut self, gray: &[u8], mask: &mut [u8]) {
        for k in 0..self.samples {
            self.plane_mut(k).copy_from_slice(gray);
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
    fn test_background_pixel_updates_model() {
        // updateRate 1 -> every background pixel updates its sample, so the
        // model follows the (still in-threshold) brightness change and only a
        // later jump beyond the threshold turns foreground.
        let mut m = BackgroundModel::new(
            1,
            1,
            BackgroundConfig {
                samples: 1,
                threshold: 50,
                min_match: 1,
                update_rate: 1,
                reinit_fg: 60,
                reinit_streak: 6,
            },
        );
        let mut mask = vec![0; 1];
        m.update_and_detect(&[10], &mut mask);
        assert_eq!(vec![0], mask);

        // |30 - 10| < 50: background, and the model must move to 30.
        m.update_and_detect(&[30], &mut mask);
        assert_eq!(vec![0], mask);

        // |100 - 30| >= 50: foreground, proving the model was updated.
        m.update_and_detect(&[100], &mut mask);
        assert_eq!(vec![255], mask);
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
        let streak = u32::from(BackgroundConfig::default().reinit_streak);
        for _ in 0..streak {
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
        let streak = u32::from(BackgroundConfig::default().reinit_streak);
        for _ in 0..(streak * 4) {
            m.update_and_detect(&[200, 10, 10, 10], &mut mask);
            assert_eq!(255, mask[0]);
        }
    }

    #[test]
    fn test_absorb_removes_standing_object() {
        let mut m = model(4, 4);
        let mut mask = vec![0; 16];
        let base = [10_u8; 16];
        m.update_and_detect(&base, &mut mask);

        // A 2x2 object appears at (1,1).
        let mut frame = base;
        for y in 1..3 {
            for x in 1..3 {
                frame[y * 4 + x] = 200;
            }
        }
        m.update_and_detect(&frame, &mut mask);
        assert_eq!(255, mask[5]);
        assert_eq!(255, mask[10]);

        // Absorbing its box forces the model to the current frame there.
        m.absorb(
            &frame,
            &[Bbox {
                x: 1,
                y: 1,
                w: 2,
                h: 2,
                area: 4,
            }],
        );
        m.update_and_detect(&frame, &mut mask);
        assert_eq!(vec![0; 16], mask);
    }

    #[test]
    fn test_absorb_clamps_to_frame() {
        let mut m = model(2, 2);
        // A box reaching outside the frame must not panic.
        m.absorb(
            &[1, 2, 3, 4],
            &[Bbox {
                x: 1,
                y: 1,
                w: 100,
                h: 100,
                area: 1,
            }],
        );
    }

    /// Reference implementation: the original array-of-structs model
    /// (`models[i * samples + k]`, indexed by pixel first) with the fused
    /// per-pixel loop, kept
    /// verbatim so the `SoA` rewrite can be checked bit for bit.
    struct AosReference {
        width: usize,
        height: usize,
        samples: usize,
        threshold: u8,
        min_match: u8,
        update_threshold: u32,
        initialized: bool,
        models: Vec<u8>,
        rng: Rng,
        fg_streak: u32,
        reinit_fg: u64,
        reinit_streak: u32,
    }

    impl AosReference {
        fn new(width: usize, height: usize, config: BackgroundConfig) -> Self {
            let samples = usize::from(config.samples.max(1));
            let update_rate = u32::from(config.update_rate.max(1));
            Self {
                width,
                height,
                samples,
                threshold: config.threshold,
                min_match: config.min_match.clamp(1, config.samples.max(1)),
                update_threshold: u32::MAX / update_rate,
                initialized: false,
                models: vec![0; width * height * samples],
                rng: Rng::new(0x9E37_79B9_7F4A_7C15),
                fg_streak: 0,
                reinit_fg: u64::from(config.reinit_fg),
                reinit_streak: u32::from(config.reinit_streak),
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

        fn absorb(&mut self, gray: &[u8], boxes: &[Bbox]) {
            let samples = self.samples;
            let w = self.width;
            let h = self.height;
            for b in boxes {
                let x0 = usize::try_from(b.x).unwrap_or(0).min(w);
                let y0 = usize::try_from(b.y).unwrap_or(0).min(h);
                let x1 = usize::try_from(b.x.saturating_add(b.w)).unwrap_or(0).min(w);
                let y1 = usize::try_from(b.y.saturating_add(b.h)).unwrap_or(0).min(h);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = y * w + x;
                        let v = gray.get(i).copied().unwrap_or(0);
                        self.models[i * samples..(i + 1) * samples].fill(v);
                    }
                }
            }
        }

        fn update_and_detect(&mut self, gray: &[u8], mask: &mut [u8]) -> u64 {
            let n = gray.len();
            if n == 0 {
                return 0;
            }
            if !self.initialized {
                self.init_from(gray, mask);
                return 0;
            }

            let samples = self.samples;
            let samples_u32 = u32::try_from(samples).unwrap_or(1);
            let threshold = self.threshold;
            let min_match = u32::from(self.min_match);
            let update_threshold = self.update_threshold;
            let seed = self.rng.next_u64();
            let width = self.width;
            let height = self.height;
            let models = &mut self.models;

            // Phase 1: classify every pixel against the model as it stood
            // at the start of the frame.
            let mut fg_count: u64 = 0;
            for (i, &v) in gray.iter().enumerate() {
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
                }
            }

            // Phase 2: model updates for background pixels only.
            for (i, &v) in gray.iter().enumerate() {
                if mask[i] != 0 {
                    continue;
                }
                let base = i * samples;
                let h = mix(seed ^ u64::try_from(i).unwrap_or(0));
                if hi32(h) >= update_threshold {
                    continue;
                }
                let idx = usize::try_from(fastrange(lo32(h), samples_u32)).unwrap_or(0);
                models[base + idx] = v;
                let h2 = mix(h);
                let cell = fastrange(hi32(h2), 9);
                let dx = i64::from(cell % 3) - 1;
                let dy = i64::from(cell / 3) - 1;
                let nidx = usize::try_from(fastrange(lo32(h2), samples_u32)).unwrap_or(0);
                if let Some(nb) = neighbor_index(i, dx, dy, width, height) {
                    models[nb * samples + nidx] = v;
                }
            }

            if fg_count * 100 > u64::try_from(n).unwrap_or(0) * self.reinit_fg {
                self.fg_streak = self.fg_streak.saturating_add(1);
                if self.fg_streak >= self.reinit_streak {
                    self.init_from(gray, mask);
                }
            } else {
                self.fg_streak = 0;
            }
            fg_count
        }

        // Model value of sample `k` for pixel `i`, in plane-independent terms.
        fn sample(&self, i: usize, k: usize) -> u8 {
            self.models[i * self.samples + k]
        }
    }

    /// Deterministic scene: noisy static background, a moving block, a
    /// block that stops and gets absorbed, and a global brightness jump
    /// that forces a reinit.
    fn scene_frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut seed: u64 = 0xDEAD_BEEF ^ u64::try_from(t).unwrap_or(0).wrapping_mul(0x1234_5679);
        let mut px = vec![0_u8; w * h];
        for v in &mut px {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *v = u8::try_from(100 + (seed >> 33) % 6).unwrap_or(100);
        }
        // Moving block for the first 120 frames, then parked at a fixed spot.
        let bx = if t < 120 { 4 + (t * 3) % (w - 20) } else { 40 };
        for y in 10..26 {
            for x in bx..bx + 14 {
                px[y * w + x] = 220;
            }
        }
        // Global lighting jump after frame 200 (triggers reinit).
        if (200..230).contains(&t) {
            for v in &mut px {
                *v = v.saturating_add(90);
            }
        }
        px
    }

    #[test]
    fn test_soa_matches_aos_reference_bit_exact() {
        let (w, h) = (48, 32);
        let config = BackgroundConfig::default();
        let mut new = BackgroundModel::new(w, h, config);
        let mut reference = AosReference::new(w, h, config);
        let mut mask_new = vec![0; w * h];
        let mut mask_ref = vec![0; w * h];

        for t in 0..300 {
            let frame = scene_frame(w, h, t);
            let fg_new = new.update_and_detect(&frame, &mut mask_new);
            let fg_ref = reference.update_and_detect(&frame, &mut mask_ref);
            assert_eq!(fg_ref, fg_new, "fg_count diverged at frame {t}");
            assert_eq!(mask_ref, mask_new, "mask diverged at frame {t}");

            // Model state must match too, not only the outputs.
            for i in 0..w * h {
                for k in 0..new.samples {
                    assert_eq!(
                        reference.sample(i, k),
                        new.models[k * new.n + i],
                        "model diverged at frame {t}, pixel {i}, sample {k}"
                    );
                }
            }

            // Park an object at frame 150 to exercise absorb() as well.
            if t == 150 {
                let b = Bbox {
                    x: 40,
                    y: 10,
                    w: 14,
                    h: 16,
                    area: 224,
                };
                new.absorb(&frame, &[b]);
                reference.absorb(&frame, &[b]);
            }
        }
    }
}
