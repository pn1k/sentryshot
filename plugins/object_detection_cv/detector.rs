// SPDX-License-Identifier: GPL-2.0-or-later

use crate::{
    background::BackgroundModel,
    blobs::{Bbox, Blobs},
    config::ObjectDetectionCvConfig,
    source::{GrayError, frame_to_gray},
    tracker::Tracker,
};
use common::{
    ArcMsgLogger, Detection, Label, LogLevel, RectangleNormalized, Region,
    recording::{normalize, vertex_inside_poly2},
    time::DurationH264,
};
use sentryshot_convert::Frame;
use std::num::NonZeroU32;
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

    scratch: Vec<u8>,
    gray: Vec<u8>,
    mask: Vec<u8>,

    aw: usize,
    ah: usize,
    first: bool,

    // Detections of the currently confirmed tracks, for the live overlay.
    current: Vec<Detection>,

    // Timing statistics.
    timing_since: Instant,
    timing_frames: u32,
    timing_us: u128,
}

impl Detector {
    pub(crate) fn new(config: ObjectDetectionCvConfig, logger: ArcMsgLogger) -> Self {
        let label: Label = "object".to_owned().try_into().expect("valid");
        Self {
            config,
            logger,
            label,
            bg: None,
            blobs: Blobs::default(),
            tracker: Tracker::new(),
            scratch: Vec::new(),
            gray: Vec::new(),
            mask: Vec::new(),
            aw: 0,
            ah: 0,
            first: true,
            current: Vec::new(),
            timing_since: Instant::now(),
            timing_frames: 0,
            timing_us: 0,
        }
    }

    /// Analyzed frame dimensions `(width, height)` of the last processed frame.
    pub(crate) fn dims(&self) -> (usize, usize) {
        (self.aw, self.ah)
    }

    /// Detections of the currently confirmed tracks, refreshed every frame.
    pub(crate) fn current(&self) -> &[Detection] {
        &self.current
    }

    /// Processes one frame and returns the boxes that should trigger an event.
    pub(crate) fn step(&mut self, frame: &Frame) -> Result<Vec<Bbox>, GrayError> {
        let started = Instant::now();
        let triggers = self.step_inner(frame)?;
        self.record_timing(started);
        Ok(triggers)
    }

    fn step_inner(&mut self, frame: &Frame) -> Result<Vec<Bbox>, GrayError> {
        let (aw, ah) = frame_to_gray(frame, self.config.scale, &mut self.scratch, &mut self.gray)?;
        let aw = usize::from(aw);
        let ah = usize::from(ah);

        if self.bg.is_none() || self.aw != aw || self.ah != ah {
            self.aw = aw;
            self.ah = ah;
            self.mask.clear();
            self.mask.resize(aw * ah, 0);
            self.bg = Some(BackgroundModel::new(aw, ah, self.config.background));
            self.first = true;
        }

        let Some(bg) = self.bg.as_mut() else {
            return Ok(Vec::new());
        };
        bg.update_and_detect(&self.gray, &mut self.mask);

        // The first frame only initializes the model.
        if self.first {
            self.first = false;
            return Ok(Vec::new());
        }

        let total = aw * ah;
        let min_area = area_px(self.config.blob.min_area, total);
        let max_area = area_px(self.config.blob.max_area, total).max(min_area);
        let mut boxes: Vec<Bbox> = self
            .blobs
            .detect(
                &mut self.mask,
                aw,
                ah,
                self.config.blob.open,
                self.config.blob.close,
                min_area,
                max_area,
            )
            .to_vec();

        filter_masked(&mut boxes, aw, ah, &self.config);
        filter_aspect(&mut boxes, &self.config);

        // Keep only the largest blobs to bound the worst case.
        if boxes.len() > MAX_BLOBS {
            boxes.select_nth_unstable_by_key(MAX_BLOBS - 1, |b| std::cmp::Reverse(b.area));
            boxes.truncate(MAX_BLOBS);
        }

        let triggers = self.tracker.update(
            &boxes,
            self.config.tracker.min_hits,
            self.config.tracker.max_age,
            self.config.tracker.iou,
            cooldown_frames(&self.config),
        );
        self.current = to_detections(&self.tracker.active(), aw, ah, &self.label);
        Ok(triggers)
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

// Drop blobs whose center is inside the mask polygon.
fn filter_masked(boxes: &mut Vec<Bbox>, aw: usize, ah: usize, config: &ObjectDetectionCvConfig) {
    if !config.mask.enable {
        return;
    }
    let aw = u16::try_from(aw).unwrap_or(u16::MAX);
    let ah = u16::try_from(ah).unwrap_or(u16::MAX);
    boxes.retain(|b| {
        let cx = normalize(u16::try_from(b.x + b.w / 2).unwrap_or(0), aw);
        let cy = normalize(u16::try_from(b.y + b.h / 2).unwrap_or(0), ah);
        !vertex_inside_poly2(cx, cy, &config.mask.area)
    });
}

// Drop blobs outside the configured aspect ratio range.
#[allow(clippy::cast_precision_loss)]
fn filter_aspect(boxes: &mut Vec<Bbox>, config: &ObjectDetectionCvConfig) {
    let min = config.blob.min_aspect;
    let max = config.blob.max_aspect;
    if min <= 0.0 && max.is_infinite() {
        return;
    }
    boxes.retain(|b| {
        let aspect = f64::from(b.w) / f64::from(b.h);
        aspect >= f64::from(min) && aspect <= f64::from(max)
    });
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
}
