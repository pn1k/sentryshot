// SPDX-License-Identifier: GPL-2.0-or-later

use crate::{
    background::BackgroundModel,
    blobs::Bbox,
    blobs::Blobs,
    config::{MaskConfig, ObjectDetectionCvConfig},
    source::{GrayError, build_div_lut, frame_to_gray},
    tracker::Tracker,
};
use common::{
    ArcMsgLogger, Detection, Label, LogLevel, RectangleNormalized, Region,
    recording::{normalize, vertex_inside_poly2},
    time::DurationH264,
};
use sentryshot_convert::Frame;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

// Upper bound on blobs per frame to keep the worst case (noise, rain) cheap.
const MAX_BLOBS: usize = 16;

// Log the average processing time once per this many milliseconds.
const TIMING_LOG_INTERVAL_MS: u64 = 5000;

/// Owns all per-monitor analysis state and buffers so nothing is allocated
/// per frame.
pub(crate) struct Detector {
    config: ObjectDetectionCvConfig,
    logger: ArcMsgLogger,
    label: Label,

    bg: Option<BackgroundModel>,
    blobs: Blobs,
    tracker: Tracker,

    lut: Vec<u8>,
    gray: Vec<u8>,
    mask: Vec<u8>,

    // Rasterized ignore-mask (255 = keep, 0 = ignored), empty when the mask
    // is disabled. Rebuilt on resolution changes.
    keep: Vec<u8>,

    aw: usize,
    ah: usize,
    first: bool,

    // Blob area bounds in analyzed pixels, cached per frame size.
    min_area: u32,
    max_area: u32,

    // Trigger cooldown in frames, cached from the config.
    cooldown: u64,

    // Stillness timeout in frames (`u64::MAX` disables absorption) and the
    // global-change foreground percent, cached from the config.
    static_frames: u64,
    reinit_fg: u64,

    // Detections of the currently confirmed tracks, for the live overlay.
    // Rebuilt only when the confirmed tracks change and shared through an
    // `Arc`, so publishing a frame is a refcount bump.
    current: Arc<Vec<Detection>>,
    last_active: Vec<Bbox>,

    // Timing statistics.
    timing_since: Instant,
    timing_frames: u32,
    timing_us: u128,
}

impl Detector {
    pub(crate) fn new(config: ObjectDetectionCvConfig, logger: ArcMsgLogger) -> Self {
        let label: Label = "object".to_owned().try_into().expect("valid");
        let cooldown = cooldown_frames(&config);
        let static_frames = static_frames(&config);
        let reinit_fg = u64::from(config.background.reinit_fg);
        let lut = if config.scale > 1 {
            build_div_lut(config.scale)
        } else {
            Vec::new()
        };
        Self {
            config,
            logger,
            label,
            bg: None,
            blobs: Blobs::default(),
            tracker: Tracker::new(),
            lut,
            gray: Vec::new(),
            mask: Vec::new(),
            keep: Vec::new(),
            aw: 0,
            ah: 0,
            first: true,
            min_area: 1,
            max_area: u32::MAX,
            cooldown,
            static_frames,
            reinit_fg,
            current: Arc::new(Vec::new()),
            last_active: Vec::new(),
            timing_since: Instant::now(),
            timing_frames: 0,
            timing_us: 0,
        }
    }

    /// Analyzed frame dimensions `(width, height)` of the last processed frame.
    pub(crate) fn dims(&self) -> (usize, usize) {
        (self.aw, self.ah)
    }

    /// Detections of the currently confirmed tracks, refreshed when the
    /// confirmed tracks change.
    pub(crate) fn current(&self) -> Arc<Vec<Detection>> {
        Arc::clone(&self.current)
    }

    /// Processes one frame and returns the boxes that should trigger an event.
    pub(crate) fn step(&mut self, frame: &Frame) -> Result<Vec<Bbox>, GrayError> {
        let started = Instant::now();
        let triggers = self.step_inner(frame)?;
        self.record_timing(started);
        Ok(triggers)
    }

    fn step_inner(&mut self, frame: &Frame) -> Result<Vec<Bbox>, GrayError> {
        let (aw, ah) = frame_to_gray(frame, self.config.scale, &self.lut, &mut self.gray)?;
        let aw = usize::from(aw);
        let ah = usize::from(ah);

        if self.bg.is_none() || self.aw != aw || self.ah != ah {
            self.aw = aw;
            self.ah = ah;
            self.mask.clear();
            self.mask.resize(aw * ah, 0);
            let total = aw * ah;
            self.min_area = area_px(self.config.blob.min_area, total);
            self.max_area = area_px(self.config.blob.max_area, total).max(self.min_area);
            self.bg = Some(BackgroundModel::new(aw, ah, self.config.background));
            self.keep = rasterize_keep(&self.config.mask, aw, ah);
            self.first = true;
            // Coordinates from the old resolution are meaningless: reset the
            // track state and the overlay.
            self.tracker = Tracker::new();
            self.last_active.clear();
            self.current = Arc::new(Vec::new());
        }

        let Some(bg) = self.bg.as_mut() else {
            return Ok(Vec::new());
        };
        let fg_count = bg.update_and_detect(&self.gray, &mut self.mask);

        // The first frame only initializes the model.
        if self.first {
            self.first = false;
            return Ok(Vec::new());
        }

        // A near-total foreground means a global change (light switch,
        // camera move): age the tracks but produce no triggers while the
        // model reinitializes.
        let total = u64::try_from(aw * ah).unwrap_or(0);
        let global_change = fg_count * 100 > total * self.reinit_fg;

        let triggers = if fg_count == 0 || global_change {
            // Nothing to analyze: only age the tracks so stale ones retire.
            self.tracker.update(
                &[],
                self.config.tracker.min_hits,
                self.config.tracker.max_age,
                self.config.tracker.iou,
                self.cooldown,
                self.static_frames,
            )
        } else {
            // Exclude masked pixels before morphology so blobs are cut at
            // the mask boundary instead of being filtered by blob center
            // afterwards.
            if !self.keep.is_empty() {
                for (m, k) in self.mask.iter_mut().zip(&self.keep) {
                    *m &= *k;
                }
            }
            let config = &self.config;
            let boxes = self.blobs.detect(
                &mut self.mask,
                aw,
                ah,
                config.blob.open,
                config.blob.close,
                self.min_area,
                self.max_area,
            );
            let boxes = filter_aspect(boxes, config);
            let boxes = cap_blobs(boxes);
            self.tracker.update(
                boxes,
                config.tracker.min_hits,
                config.tracker.max_age,
                config.tracker.iou,
                self.cooldown,
                self.static_frames,
            )
        };

        // Tracks that stood still long enough are absorbed into the
        // background model so they stop triggering (parked car).
        let absorb = self.tracker.take_absorb();
        if !absorb.is_empty() {
            if let Some(bg) = self.bg.as_mut() {
                bg.absorb(&self.gray, &absorb);
            }
        }

        self.refresh_current();
        Ok(triggers)
    }

    // Rebuilds the overlay detections only when the confirmed tracks changed.
    fn refresh_current(&mut self) {
        let active = self.tracker.active();
        if active != self.last_active.as_slice() {
            self.last_active.clear();
            self.last_active.extend_from_slice(active);
            self.current = Arc::new(to_detections(active, self.aw, self.ah, &self.label));
        }
    }

    fn record_timing(&mut self, started: Instant) {
        self.timing_us += started.elapsed().as_micros();
        self.timing_frames += 1;

        let elapsed_ms = u64::try_from(self.timing_since.elapsed().as_millis()).unwrap_or(0);
        if elapsed_ms < TIMING_LOG_INTERVAL_MS {
            return;
        }

        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let avg_ms = self.timing_us as f64 / f64::from(self.timing_frames.max(1)) / 1000.0;
        self.logger.log(
            LogLevel::Debug,
            &format!(
                "avg processing {avg_ms:.2} ms/frame over {} frames",
                self.timing_frames
            ),
        );
        self.timing_since = Instant::now();
        self.timing_frames = 0;
        self.timing_us = 0;
    }
}

// In-place slice counterpart of `Vec::retain`: keeps matching items at the
// front (order not preserved) and returns the retained prefix.
fn retain_slice<T>(v: &mut [T], mut keep: impl FnMut(&T) -> bool) -> &mut [T] {
    let mut n = 0;
    for i in 0..v.len() {
        if keep(&v[i]) {
            v.swap(n, i);
            n += 1;
        }
    }
    &mut v[..n]
}

// Keep only the largest blobs to bound the worst case.
fn cap_blobs(boxes: &mut [Bbox]) -> &mut [Bbox] {
    if boxes.len() > MAX_BLOBS {
        boxes.select_nth_unstable_by_key(MAX_BLOBS - 1, |b| std::cmp::Reverse(b.area));
        return &mut boxes[..MAX_BLOBS];
    }
    boxes
}

// Rasterizes the ignore-mask: 255 keeps the pixel, 0 excludes it from
// detection. Pixels inside the polygon are excluded; their blobs are cut at
// the mask boundary instead of being filtered by blob center.
fn rasterize_keep(mask: &MaskConfig, aw: usize, ah: usize) -> Vec<u8> {
    if !mask.enable || mask.area.is_empty() || aw == 0 || ah == 0 {
        return Vec::new();
    }
    let w2 = u16::try_from(aw * 2).unwrap_or(u16::MAX);
    let h2 = u16::try_from(ah * 2).unwrap_or(u16::MAX);
    let mut keep = Vec::with_capacity(aw * ah);
    for y in 0..ah {
        for x in 0..aw {
            // Pixel center, so the polygon boundary lands between pixels.
            let cx = normalize(u16::try_from(x * 2 + 1).unwrap_or(u16::MAX), w2);
            let cy = normalize(u16::try_from(y * 2 + 1).unwrap_or(u16::MAX), h2);
            keep.push(u8::from(!vertex_inside_poly2(cx, cy, &mask.area)));
        }
    }
    keep
}

// Drop blobs outside the configured aspect ratio range.
#[allow(clippy::cast_precision_loss)]
fn filter_aspect<'a>(boxes: &'a mut [Bbox], config: &ObjectDetectionCvConfig) -> &'a mut [Bbox] {
    let min = config.blob.min_aspect;
    let max = config.blob.max_aspect;
    if min <= 0.0 && max.is_infinite() {
        return boxes;
    }
    retain_slice(boxes, |b| {
        let aspect = f64::from(b.w) / f64::from(b.h);
        aspect >= f64::from(min) && aspect <= f64::from(max)
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::as_conversions
)]
fn area_px(percent: f32, total: usize) -> u32 {
    let v = f64::from(percent.max(0.0)) * (total as f64) / 100.0;
    let v = v.max(1.0);
    v as u32
}

fn cooldown_frames(config: &ObjectDetectionCvConfig) -> u64 {
    let cooldown = u64::from(config.tracker.cooldown);
    if cooldown == 0 {
        return 0;
    }
    let period = DurationH264::from(*config.feed_rate).as_nanos();
    if period <= 0 {
        // Feed rate 0 means no frame limit; the cooldown cannot be derived
        // from it, so it is disabled.
        return 0;
    }
    cooldown * 1_000_000_000 / u64::try_from(period).unwrap_or(1)
}

// Stillness timeout in frames; `u64::MAX` disables absorption.
fn static_frames(config: &ObjectDetectionCvConfig) -> u64 {
    let static_after = u64::from(config.tracker.static_after);
    if static_after == 0 {
        return u64::MAX;
    }
    let period = DurationH264::from(*config.feed_rate).as_nanos();
    if period <= 0 {
        // Feed rate 0 means no frame limit; the timeout cannot be derived
        // from it, so it is disabled.
        return u64::MAX;
    }
    static_after * 1_000_000_000 / u64::try_from(period).unwrap_or(1)
}

// Score is the blob solidity: the fraction of the bounding box filled by
// foreground pixels. A compact object scores higher than a scattered one.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::as_conversions
)]
pub(crate) fn to_detections(boxes: &[Bbox], aw: usize, ah: usize, label: &Label) -> Vec<Detection> {
    let aw = u16::try_from(aw).unwrap_or(u16::MAX);
    let ah = u16::try_from(ah).unwrap_or(u16::MAX);

    boxes
        .iter()
        .map(|b| {
            let x = normalize(u16::try_from(b.x).unwrap_or(0), aw);
            let y = normalize(u16::try_from(b.y).unwrap_or(0), ah);
            let width = normalize(u16::try_from(b.w).unwrap_or(1), aw);
            let height = normalize(u16::try_from(b.h).unwrap_or(1), ah);

            let box_area = f64::from(b.w) * f64::from(b.h);
            let fill = if box_area > 0.0 {
                f64::from(b.area) / box_area
            } else {
                0.0
            };

            Detection {
                label: label.clone(),
                score: (fill * 100.0) as f32,
                region: Region {
                    rectangle: Some(RectangleNormalized {
                        x,
                        y,
                        width: NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
                        height: NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
                    }),
                    polygon: None,
                },
            }
        })
        .collect()
}

#[allow(clippy::unwrap_used, clippy::assert_is_empty)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackgroundConfig, BlobConfig, MaskConfig, TrackerConfig};
    use common::DummyLogger;
    use common::recording::{DurationSec, FeedRateSec};
    use common::time::Duration;
    use sentryshot_convert::PixelFormat;
    use std::num::NonZeroU16;

    fn test_config() -> ObjectDetectionCvConfig {
        ObjectDetectionCvConfig {
            feed_rate: FeedRateSec::new(Duration::new(200_000_000)), // 5 fps.
            trigger_duration: DurationSec::new(Duration::from_secs(5)),
            use_sub_stream: true,
            scale: 1,
            mask: MaskConfig::default(),
            background: BackgroundConfig {
                samples: 8,
                threshold: 20,
                min_match: 2,
                update_rate: 16,
                reinit_fg: 60,
                reinit_streak: 6,
            },
            blob: BlobConfig {
                min_area: 0.1,
                max_area: 90.0,
                min_aspect: 0.1,
                max_aspect: 10.0,
                open: false,
                close: false,
            },
            tracker: TrackerConfig {
                min_hits: 1,
                max_age: 5,
                iou: 0.3,
                cooldown: 0,
                static_after: 0,
            },
        }
    }

    fn gray(w: u16, h: u16, pixels: &[u8]) -> Frame {
        Frame::from_raw(
            pixels,
            PixelFormat::GRAY8,
            NonZeroU16::new(w).unwrap(),
            NonZeroU16::new(h).unwrap(),
            1,
        )
        .unwrap()
    }

    #[test]
    fn test_detector_triggers_on_moving_block() {
        let mut det = Detector::new(test_config(), DummyLogger::new());
        let w: u16 = 16;
        let h: u16 = 16;
        let len = usize::from(w) * usize::from(h);
        let blank = vec![0_u8; len];

        // Frame 1 initializes the background model.
        assert!(det.step(&gray(w, h, &blank)).unwrap().is_empty());
        // Frame 2 is identical -> nothing.
        assert!(det.step(&gray(w, h, &blank)).unwrap().is_empty());

        // Frame 3 has a bright 4x4 block -> one trigger.
        let mut px = vec![0_u8; len];
        for y in 2..6_usize {
            for x in 2..6_usize {
                px[y * usize::from(w) + x] = 255;
            }
        }
        let triggers = det.step(&gray(w, h, &px)).unwrap();
        assert_eq!(1, triggers.len());
        assert_eq!((16, 16), det.dims());
    }

    #[test]
    fn test_detector_standing_block_keeps_triggering_only_on_cooldown() {
        // Cooldown 0 at min_hits 1: an object that stays triggers every frame
        // because the background model never absorbs it.
        let mut det = Detector::new(test_config(), DummyLogger::new());
        let w: u16 = 16;
        let h: u16 = 16;
        let len = usize::from(w) * usize::from(h);

        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();

        let mut px = vec![0_u8; len];
        for y in 2..6_usize {
            for x in 2..6_usize {
                px[y * usize::from(w) + x] = 255;
            }
        }
        for _ in 0..50 {
            let triggers = det.step(&gray(w, h, &px)).unwrap();
            assert_eq!(1, triggers.len(), "standing object disappeared");
        }
    }

    #[test]
    fn test_standing_object_absorbed_after_static_timeout() {
        let mut config = test_config();
        // staticAfter 1s at 5 fps = 5 frames.
        config.tracker.static_after = 1;
        let mut det = Detector::new(config, DummyLogger::new());
        let w: u16 = 16;
        let h: u16 = 16;
        let len = usize::from(w) * usize::from(h);
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();

        let mut px = vec![0_u8; len];
        for y in 2..6_usize {
            for x in 2..6_usize {
                px[y * usize::from(w) + x] = 255;
            }
        }

        // Triggers while the object stands, then gets absorbed and stays
        // silent afterwards.
        let mut saw = 0;
        for _ in 0..20 {
            saw += det.step(&gray(w, h, &px)).unwrap().len();
        }
        assert!(saw > 0, "standing object never triggered");

        for _ in 0..50 {
            assert!(
                det.step(&gray(w, h, &px)).unwrap().is_empty(),
                "standing object still triggers after the static timeout"
            );
        }
        // The overlay drops it too.
        assert!(det.current().is_empty());
    }

    #[test]
    fn test_global_brightness_change_does_not_trigger() {
        let mut det = Detector::new(test_config(), DummyLogger::new());
        let w: u16 = 16;
        let h: u16 = 16;
        let len = usize::from(w) * usize::from(h);

        det.step(&gray(w, h, &vec![100_u8; len])).unwrap();
        det.step(&gray(w, h, &vec![100_u8; len])).unwrap();

        // A jump that turns the whole frame foreground is a global change:
        // no triggers, and the model reinitializes.
        for _ in 0..10 {
            assert!(
                det.step(&gray(w, h, &vec![250_u8; len]))
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            det.step(&gray(w, h, &vec![250_u8; len]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_resolution_change_resets_state() {
        let mut det = Detector::new(test_config(), DummyLogger::new());
        let len = usize::from(16_u16) * usize::from(16_u16);
        det.step(&gray(16, 16, &vec![0_u8; len])).unwrap();

        let mut px = vec![0_u8; len];
        for y in 2..6_usize {
            for x in 2..6_usize {
                px[y * 16 + x] = 255;
            }
        }
        for _ in 0..4 {
            det.step(&gray(16, 16, &px)).unwrap();
        }
        assert!(!det.current().is_empty());

        // A different resolution must reset the tracks and the overlay
        // instead of carrying over stale coordinates.
        let big = vec![0_u8; 32 * 32];
        det.step(&gray(32, 32, &big)).unwrap();
        assert!(det.current().is_empty());
        let object = {
            let mut px = vec![0_u8; 32 * 32];
            for y in 4..12_usize {
                for x in 4..12_usize {
                    px[y * 32 + x] = 255;
                }
            }
            px
        };
        for _ in 0..4 {
            det.step(&gray(32, 32, &object)).unwrap();
        }
        assert!(!det.current().is_empty());
    }

    #[test]
    fn test_mask_cuts_blobs_at_boundary() {
        // Mask covering the left half of the frame.
        let mut config = test_config();
        config.mask = MaskConfig {
            enable: true,
            area: vec![
                common::PointNormalized { x: 0, y: 0 },
                common::PointNormalized { x: 500_000, y: 0 },
                common::PointNormalized {
                    x: 500_000,
                    y: 1_000_000,
                },
                common::PointNormalized { x: 0, y: 1_000_000 },
            ],
        };

        let w: u16 = 16;
        let h: u16 = 16;
        let len = usize::from(w) * usize::from(h);

        // Object fully inside the ignored half: never triggers.
        let mut det = Detector::new(config.clone(), DummyLogger::new());
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        let mut px = vec![0_u8; len];
        for y in 2..6_usize {
            for x in 2..6_usize {
                px[y * usize::from(w) + x] = 255;
            }
        }
        for _ in 0..5 {
            assert!(det.step(&gray(w, h, &px)).unwrap().is_empty());
        }

        // Object fully in the kept half: triggers.
        let mut det = Detector::new(config, DummyLogger::new());
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        let mut px = vec![0_u8; len];
        for y in 2..6_usize {
            for x in 10..14_usize {
                px[y * usize::from(w) + x] = 255;
            }
        }
        let triggers = det.step(&gray(w, h, &px)).unwrap();
        assert_eq!(1, triggers.len());
        // The blob is not cut.
        assert_eq!(4, triggers[0].w);
    }

    #[test]
    fn test_masked_blob_is_dropped() {
        let mut config = test_config();
        // Mask covers the whole frame.
        config.mask = MaskConfig {
            enable: true,
            area: vec![
                common::PointNormalized { x: 0, y: 0 },
                common::PointNormalized { x: 1_000_000, y: 0 },
                common::PointNormalized {
                    x: 1_000_000,
                    y: 1_000_000,
                },
                common::PointNormalized { x: 0, y: 1_000_000 },
            ],
        };

        let mut det = Detector::new(config, DummyLogger::new());
        let w: u16 = 16;
        let h: u16 = 16;
        let len = usize::from(w) * usize::from(h);
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();

        let mut px = vec![0_u8; len];
        for y in 2..6_usize {
            for x in 2..6_usize {
                px[y * usize::from(w) + x] = 255;
            }
        }
        // Everything is masked -> no triggers.
        assert!(det.step(&gray(w, h, &px)).unwrap().is_empty());
    }

    #[test]
    fn test_aspect_filter_drops_thin_stripe() {
        let mut config = test_config();
        config.blob.min_aspect = 0.5;

        let mut det = Detector::new(config, DummyLogger::new());
        let w: u16 = 16;
        let h: u16 = 16;
        let len = usize::from(w) * usize::from(h);
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();
        det.step(&gray(w, h, &vec![0_u8; len])).unwrap();

        // A 1x8 stripe has aspect 0.125 < 0.5 -> filtered.
        let mut px = vec![0_u8; len];
        for y in 2..10_usize {
            px[y * usize::from(w) + 4] = 255;
        }
        assert!(det.step(&gray(w, h, &px)).unwrap().is_empty());
    }

    #[test]
    fn test_area_px() {
        assert_eq!(1, area_px(0.0, 1000));
        assert_eq!(2, area_px(0.2, 1000));
        assert_eq!(500, area_px(50.0, 1000));
    }

    #[test]
    fn test_cooldown_frames() {
        let mut config = test_config();
        config.tracker.cooldown = 2;
        // 2 seconds at 5 fps = 10 frames.
        assert_eq!(10, cooldown_frames(&config));
    }

    #[test]
    fn test_cooldown_frames_zero_feed_rate() {
        let mut config = test_config();
        config.tracker.cooldown = 2;
        config.feed_rate = FeedRateSec::new(Duration::new(0));
        assert_eq!(0, cooldown_frames(&config));
    }

    // Manual performance check:
    // `cargo test --release -p object_detection_cv bench -- --ignored --nocapture`
    #[test]
    #[ignore = "manual performance check"]
    #[allow(
        clippy::cast_precision_loss,
        clippy::as_conversions,
        clippy::items_after_statements
    )]
    fn bench_step() {
        let w = 640;
        let h = 360;
        let mut det = Detector::new(test_config(), DummyLogger::new());

        // Mostly static scene: mild sensor noise plus a moving 40x40 object.
        let mut px = vec![96_u8; w * h];
        let mut noise = 0x1234_5678_u64;
        for v in &mut px {
            noise = noise
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *v = u8::try_from(96 + (noise >> 33) % 8).unwrap_or(96);
        }

        let gw = u16::try_from(w).unwrap();
        let gh = u16::try_from(h).unwrap();
        let mut x = 40_usize;
        let mut dir = true;
        let mut advance = |px: &mut [u8]| {
            for y in 100..140 {
                for dx in 0..40 {
                    px[y * w + x + dx] = 96;
                }
            }
            if dir {
                x += 8;
                if x >= 560 {
                    dir = false;
                }
            } else {
                x -= 8;
                if x <= 40 {
                    dir = true;
                }
            }
            for y in 100..140 {
                for dx in 0..40 {
                    px[y * w + x + dx] = 200;
                }
            }
        };

        // Warmup: initialize the model and fill the tracker.
        for _ in 0..30 {
            advance(&mut px);
            det.step(&gray(gw, gh, &px)).unwrap();
        }

        let frames = 300;
        let started = Instant::now();
        for _ in 0..frames {
            advance(&mut px);
            det.step(&gray(gw, gh, &px)).unwrap();
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        println!(
            "step: {frames} frames, {elapsed:.0} ms total, {:.2} ms/frame",
            elapsed / f64::from(frames)
        );
    }
}
