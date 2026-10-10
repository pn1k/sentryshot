// SPDX-License-Identifier: GPL-2.0-or-later

use crate::blobs::Bbox;

#[derive(Clone, Debug)]
struct Track {
    bbox: Bbox,
    hits: u8,
    age: u8,
    confirmed: bool,
    last_trigger_frame: Option<u64>,
}

/// Minimal `IoU` tracker that confirms tracks, drops stale ones and rate-limits
/// how often a single track can trigger.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    tracks: Vec<Track>,
    frame: u64,
}

impl Tracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Boxes of the currently confirmed tracks, for the live overlay.
    pub(crate) fn active(&self) -> Vec<Bbox> {
        self.tracks
            .iter()
            .filter(|t| t.confirmed)
            .map(|t| t.bbox)
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        detections: &[Bbox],
        min_hits: u8,
        max_age: u8,
        iou_threshold: f32,
        cooldown_frames: u64,
    ) -> Vec<Bbox> {
        self.frame += 1;
        let min_hits = min_hits.max(1);

        let mut matched_tracks = vec![false; self.tracks.len()];
        let mut triggers = Vec::new();

        for det in detections {
            let mut best: Option<(usize, f32)> = None;
            for (ti, track) in self.tracks.iter().enumerate() {
                if matched_tracks[ti] {
                    continue;
                }
                let iou = iou(&track.bbox, det);
                if iou >= iou_threshold && best.is_none_or(|(_, b)| iou > b) {
                    best = Some((ti, iou));
                }
            }

            if let Some((ti, _)) = best {
                matched_tracks[ti] = true;
                let track = &mut self.tracks[ti];
                track.bbox = *det;
                track.hits = track.hits.saturating_add(1);
                track.age = 0;
                if track.hits >= min_hits {
                    track.confirmed = true;
                }
                if track.confirmed && may_trigger(track, cooldown_frames, self.frame) {
                    track.last_trigger_frame = Some(self.frame);
                    triggers.push(*det);
                }
            } else {
                let confirmed = min_hits <= 1;
                self.tracks.push(Track {
                    bbox: *det,
                    hits: 1,
                    age: 0,
                    confirmed,
                    last_trigger_frame: confirmed.then_some(self.frame),
                });
                if confirmed {
                    triggers.push(*det);
                }
            }
        }

        for (ti, track) in self.tracks.iter_mut().enumerate() {
            if matched_tracks.get(ti).copied().unwrap_or(false) {
                continue;
            }
            track.age = track.age.saturating_add(1);
        }
        self.tracks.retain(|t| t.age <= max_age);

        triggers
    }
}

fn may_trigger(track: &Track, cooldown_frames: u64, frame: u64) -> bool {
    match track.last_trigger_frame {
        None => true,
        Some(last) => frame.saturating_sub(last) >= cooldown_frames,
    }
}

#[allow(clippy::cast_precision_loss, clippy::as_conversions)]
fn iou(a: &Bbox, b: &Bbox) -> f32 {
    let ax2 = a.x + a.w;
    let ay2 = a.y + a.h;
    let bx2 = b.x + b.w;
    let by2 = b.y + b.h;

    let ix = a.x.max(b.x);
    let iy = a.y.max(b.y);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);

    if ix2 <= ix || iy2 <= iy {
        return 0.0;
    }
    let inter = u64::from(ix2 - ix) * u64::from(iy2 - iy);
    let union = u64::from(a.w) * u64::from(a.h) + u64::from(b.w) * u64::from(b.h) - inter;
    if union == 0 {
        return 0.0;
    }
    inter as f32 / union as f32
}

#[allow(clippy::unwrap_used, clippy::assert_is_empty)]
#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(x: u32, y: u32) -> Bbox {
        Bbox {
            x,
            y,
            w: 10,
            h: 10,
            area: 100,
        }
    }

    #[test]
    fn test_confirms_after_min_hits() {
        let mut t = Tracker::new();
        // min_hits = 2, no cooldown.
        assert!(t.update(&[bbox(0, 0)], 2, 5, 0.3, 0).is_empty());
        assert_eq!(1, t.update(&[bbox(0, 0)], 2, 5, 0.3, 0).len());
    }

    #[test]
    fn test_immediate_when_min_hits_one() {
        let mut t = Tracker::new();
        assert_eq!(1, t.update(&[bbox(0, 0)], 1, 5, 0.3, 0).len());
    }

    #[test]
    fn test_cooldown_suppresses_repeat() {
        let mut t = Tracker::new();
        assert_eq!(1, t.update(&[bbox(0, 0)], 1, 5, 0.3, 3).len());
        // Within cooldown.
        assert!(t.update(&[bbox(0, 0)], 1, 5, 0.3, 3).is_empty());
        assert!(t.update(&[bbox(0, 0)], 1, 5, 0.3, 3).is_empty());
        // Cooldown elapsed (frame 4, last trigger frame 1, cooldown 3).
        assert_eq!(1, t.update(&[bbox(0, 0)], 1, 5, 0.3, 3).len());
    }

    #[test]
    fn test_drops_stale_track() {
        let mut t = Tracker::new();
        t.update(&[bbox(0, 0)], 1, 1, 0.3, 0);
        assert_eq!(1, t.tracks.len());
        // Two frames with no detection drop the track (age 2 > max_age 1).
        t.update(&[], 1, 1, 0.3, 0);
        t.update(&[], 1, 1, 0.3, 0);
        assert_eq!(0, t.tracks.len());
    }
}
