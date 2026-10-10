// SPDX-License-Identifier: GPL-2.0-or-later

use common::{
    PolygonNormalized,
    monitor::MonitorConfig,
    recording::{DurationSec, FeedRateSec},
};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ParseConfigError {
    #[error("parse: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("invalid value: {0}")]
    Invalid(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ObjectDetectionCvConfig {
    pub(crate) feed_rate: FeedRateSec,
    pub(crate) trigger_duration: DurationSec,
    pub(crate) use_sub_stream: bool,

    /// Analysis downscale factor applied to the luma plane (1 = full res).
    pub(crate) scale: u8,

    pub(crate) mask: MaskConfig,
    pub(crate) background: BackgroundConfig,
    pub(crate) blob: BlobConfig,
    pub(crate) tracker: TrackerConfig,
}

impl ObjectDetectionCvConfig {
    // Returns `None` if unset or disabled.
    pub(crate) fn parse(raw: serde_json::Value) -> Result<Option<Self>, ParseConfigError> {
        #[derive(Deserialize)]
        struct Temp {
            #[serde(rename = "objectDetectionCv")]
            object_detection_cv: serde_json::Value,
        }
        let Ok(temp) = serde_json::from_value::<Temp>(raw) else {
            return Ok(None);
        };
        if temp.object_detection_cv == serde_json::Value::Object(serde_json::Map::new()) {
            return Ok(None);
        }

        let config: RawConfigV1 = serde_json::from_value(temp.object_detection_cv)?;
        if !config.enable {
            return Ok(None);
        }
        config.validate()?;

        Ok(Some(Self {
            feed_rate: config.feed_rate,
            trigger_duration: config.duration,
            use_sub_stream: config.use_sub_stream,
            scale: config.scale.clamp(1, 8),
            mask: config.mask,
            background: config.background,
            blob: config.blob,
            tracker: config.tracker,
        }))
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct RawConfigV1 {
    enable: bool,

    #[serde(rename = "feedRate")]
    feed_rate: FeedRateSec,

    // Trigger duration.
    duration: DurationSec,

    #[serde(rename = "useSubStream")]
    use_sub_stream: bool,

    #[serde(default = "default_scale")]
    scale: u8,

    #[serde(default)]
    mask: MaskConfig,

    #[serde(default)]
    background: BackgroundConfig,

    #[serde(default)]
    blob: BlobConfig,

    #[serde(default)]
    tracker: TrackerConfig,
}

impl RawConfigV1 {
    // The `!` form also rejects NaN: every comparison with NaN is false.
    fn validate(&self) -> Result<(), ParseConfigError> {
        let bg = &self.background;
        if bg.threshold < 1 {
            return Err(ParseConfigError::Invalid(
                "background.threshold must be at least 1",
            ));
        }
        if bg.update_rate < 1 {
            return Err(ParseConfigError::Invalid(
                "background.updateRate must be at least 1",
            ));
        }
        if !(1..=100).contains(&bg.reinit_fg) {
            return Err(ParseConfigError::Invalid(
                "background.reinitFg must be within 1..=100",
            ));
        }
        if bg.reinit_streak < 1 {
            return Err(ParseConfigError::Invalid(
                "background.reinitStreak must be at least 1",
            ));
        }

        let blob = &self.blob;
        let bad_range = |a: f32, b: f32| a.is_nan() || b.is_nan() || a > b;
        if bad_range(blob.min_area, blob.max_area) {
            return Err(ParseConfigError::Invalid(
                "blob.minArea must not exceed blob.maxArea",
            ));
        }
        if bad_range(blob.min_aspect, blob.max_aspect) {
            return Err(ParseConfigError::Invalid(
                "blob.minAspect must not exceed blob.maxAspect",
            ));
        }

        let tracker = &self.tracker;
        if tracker.iou.is_nan() || !(tracker.iou > 0.0 && tracker.iou <= 1.0) {
            return Err(ParseConfigError::Invalid(
                "tracker.iou must be within (0, 1]",
            ));
        }
        Ok(())
    }
}

const fn default_scale() -> u8 {
    1
}

/// Areas to ignore. Blob centers inside the polygon do not trigger.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct MaskConfig {
    pub(crate) enable: bool,
    pub(crate) area: PolygonNormalized,
}

/// ViBe-style background model parameters.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct BackgroundConfig {
    /// Number of samples per pixel.
    pub(crate) samples: u8,

    /// Color distance threshold for a sample to count as a match.
    pub(crate) threshold: u8,

    /// Minimum number of matching samples for a pixel to be background.
    pub(crate) min_match: u8,

    /// 1 in `update_rate` chance to update a pixel model each frame.
    pub(crate) update_rate: u16,

    /// Foreground percent above which the frame counts as a global change:
    /// no triggers are produced and the reinit streak grows.
    pub(crate) reinit_fg: u8,

    /// Consecutive global-change frames before the model reinitializes.
    pub(crate) reinit_streak: u8,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            samples: 8,
            threshold: 20,
            min_match: 2,
            update_rate: 16,
            reinit_fg: 60,
            reinit_streak: 6,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct BlobConfig {
    /// Minimum blob area, percent of the analyzed frame.
    pub(crate) min_area: f32,

    /// Maximum blob area, percent of the analyzed frame.
    pub(crate) max_area: f32,

    /// Minimum blob aspect ratio (width / height).
    pub(crate) min_aspect: f32,

    /// Maximum blob aspect ratio (width / height).
    pub(crate) max_aspect: f32,

    /// Apply a 3x3 opening (erode then dilate) to remove speckle.
    pub(crate) open: bool,

    /// Apply a 3x3 closing (dilate then erode) to fill holes.
    pub(crate) close: bool,
}

impl Default for BlobConfig {
    fn default() -> Self {
        Self {
            min_area: 0.2,
            max_area: 50.0,
            min_aspect: 0.1,
            max_aspect: 10.0,
            open: true,
            close: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct TrackerConfig {
    /// Number of consecutive hits before a track is confirmed.
    pub(crate) min_hits: u8,

    /// Number of missed frames before a track is dropped.
    pub(crate) max_age: u8,

    /// Minimum `IoU` to associate a detection with a track.
    pub(crate) iou: f32,

    /// Seconds before the same track may trigger again. Derived from the feed
    /// rate; a feed rate of `0` (no limit) disables it.
    pub(crate) cooldown: u16,

    /// Seconds without significant movement after which a confirmed track is
    /// absorbed into the background (parked car, moved furniture). `0`
    /// disables absorption.
    pub(crate) static_after: u16,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            min_hits: 2,
            max_age: 5,
            iou: 0.3,
            cooldown: 5,
            static_after: 60,
        }
    }
}

pub(crate) fn set_enable(config: &MonitorConfig, value: bool) -> Option<MonitorConfig> {
    let mut raw = config.raw().clone();
    let Value::Object(root) = &mut raw else {
        return None;
    };
    let Value::Object(field) = root.get_mut("objectDetectionCv")? else {
        return None;
    };
    let Value::Bool(enable) = field.get_mut("enable")? else {
        return None;
    };
    *enable = value;

    serde_json::from_value(raw).expect("config should still be valid after toggling")
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use common::time::Duration;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn test_parse_ok() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true,
                "feedRate": 5,
                "duration": 15,
                "useSubStream": true,
                "scale": 2,
                "mask": { "enable": true, "area": [[10, 11], [12, 13]] },
                "background": { "samples": 4, "threshold": 25, "minMatch": 2, "updateRate": 8 },
                "blob": {
                    "minArea": 1.0, "maxArea": 40.0,
                    "minAspect": 0.2, "maxAspect": 8.0,
                    "open": false, "close": true
                },
                "tracker": { "minHits": 3, "maxAge": 4, "iou": 0.5, "cooldown": 7 }
            }
        });

        let got = ObjectDetectionCvConfig::parse(raw).unwrap().unwrap();
        let want = ObjectDetectionCvConfig {
            feed_rate: FeedRateSec::new(Duration::from_millis(200)),
            trigger_duration: DurationSec::new(Duration::from_secs(15)),
            use_sub_stream: true,
            scale: 2,
            mask: MaskConfig {
                enable: true,
                area: vec![
                    common::PointNormalized { x: 10, y: 11 },
                    common::PointNormalized { x: 12, y: 13 },
                ],
            },
            background: BackgroundConfig {
                samples: 4,
                threshold: 25,
                min_match: 2,
                update_rate: 8,
                reinit_fg: 60,
                reinit_streak: 6,
            },
            blob: BlobConfig {
                min_area: 1.0,
                max_area: 40.0,
                min_aspect: 0.2,
                max_aspect: 8.0,
                open: false,
                close: true,
            },
            tracker: TrackerConfig {
                min_hits: 3,
                max_age: 4,
                iou: 0.5,
                cooldown: 7,
                static_after: 60,
            },
        };
        assert_eq!(want, got);
    }

    #[test]
    fn test_parse_defaults() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true,
                "feedRate": 5,
                "duration": 15,
                "useSubStream": true
            }
        });

        let got = ObjectDetectionCvConfig::parse(raw).unwrap().unwrap();
        assert_eq!(1, got.scale);
        assert_eq!(MaskConfig::default(), got.mask);
        assert_eq!(BackgroundConfig::default(), got.background);
        assert_eq!(BlobConfig::default(), got.blob);
        assert_eq!(TrackerConfig::default(), got.tracker);
    }

    #[test]
    fn test_parse_empty() {
        let raw = serde_json::Value::String(String::new());
        assert!(ObjectDetectionCvConfig::parse(raw).unwrap().is_none());
    }

    #[test]
    fn test_parse_empty2() {
        let raw = json!({"objectDetectionCv": {}});
        assert!(ObjectDetectionCvConfig::parse(raw).unwrap().is_none());
    }

    #[test]
    fn test_parse_disabled() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": false,
                "feedRate": 5,
                "duration": 15,
                "useSubStream": true
            }
        });
        assert!(ObjectDetectionCvConfig::parse(raw).unwrap().is_none());
    }

    #[test]
    fn test_parse_reinit_defaults() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true,
                "feedRate": 5,
                "duration": 15,
                "useSubStream": true
            }
        });
        let got = ObjectDetectionCvConfig::parse(raw).unwrap().unwrap();
        assert_eq!(60, got.background.reinit_fg);
        assert_eq!(6, got.background.reinit_streak);
        assert_eq!(60, got.tracker.static_after);
    }

    #[test]
    fn test_parse_invalid_threshold() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true, "feedRate": 5, "duration": 15, "useSubStream": true,
                "background": { "threshold": 0 }
            }
        });
        assert!(matches!(
            ObjectDetectionCvConfig::parse(raw),
            Err(ParseConfigError::Invalid(_))
        ));
    }

    #[test]
    fn test_parse_invalid_update_rate() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true, "feedRate": 5, "duration": 15, "useSubStream": true,
                "background": { "updateRate": 0 }
            }
        });
        assert!(matches!(
            ObjectDetectionCvConfig::parse(raw),
            Err(ParseConfigError::Invalid(_))
        ));
    }

    #[test]
    fn test_parse_invalid_iou() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true, "feedRate": 5, "duration": 15, "useSubStream": true,
                "tracker": { "iou": 0.0 }
            }
        });
        assert!(matches!(
            ObjectDetectionCvConfig::parse(raw),
            Err(ParseConfigError::Invalid(_))
        ));

        let raw = json!({
            "objectDetectionCv": {
                "enable": true, "feedRate": 5, "duration": 15, "useSubStream": true,
                "tracker": { "iou": 1.5 }
            }
        });
        assert!(matches!(
            ObjectDetectionCvConfig::parse(raw),
            Err(ParseConfigError::Invalid(_))
        ));
    }

    #[test]
    fn test_parse_invalid_aspect_range() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true, "feedRate": 5, "duration": 15, "useSubStream": true,
                "blob": { "minAspect": 5.0, "maxAspect": 2.0 }
            }
        });
        assert!(matches!(
            ObjectDetectionCvConfig::parse(raw),
            Err(ParseConfigError::Invalid(_))
        ));
    }

    #[test]
    fn test_parse_invalid_area_range() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true, "feedRate": 5, "duration": 15, "useSubStream": true,
                "blob": { "minArea": 60.0, "maxArea": 40.0 }
            }
        });
        assert!(matches!(
            ObjectDetectionCvConfig::parse(raw),
            Err(ParseConfigError::Invalid(_))
        ));
    }

    #[test]
    fn test_parse_invalid_reinit_fg() {
        let raw = json!({
            "objectDetectionCv": {
                "enable": true, "feedRate": 5, "duration": 15, "useSubStream": true,
                "background": { "reinitFg": 0 }
            }
        });
        assert!(matches!(
            ObjectDetectionCvConfig::parse(raw),
            Err(ParseConfigError::Invalid(_))
        ));
    }

    #[test]
    fn test_set_enable() {
        let y = json!({
            "id": "123",
            "name": "test",
            "enable": false,
            "alwaysRecord": false,
            "videoLength": 0,
            "source": "rtsp",
            "sourcertsp": {
                "protocol": "tcp",
                "mainStream": "rtsp://x"
            },
            "objectDetectionCv": {
                "enable": true,
                "feedRate": 5,
                "duration": 15,
                "useSubStream": true
            }
        });
        let config: MonitorConfig = serde_json::from_value(y).unwrap();
        assert!(
            ObjectDetectionCvConfig::parse(config.raw().clone())
                .unwrap()
                .is_some()
        );

        let config = set_enable(&config, false).unwrap();
        assert!(
            ObjectDetectionCvConfig::parse(config.raw().clone())
                .unwrap()
                .is_none()
        );

        let config = set_enable(&config, true).unwrap();
        assert!(
            ObjectDetectionCvConfig::parse(config.raw().clone())
                .unwrap()
                .is_some()
        );
    }
}
