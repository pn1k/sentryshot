// SPDX-License-Identifier: GPL-2.0-or-later

use crate::blobs::Bbox;

// Minimum IoU between the current box and its anchor for the track to count
// as standing still.
const STILL_IOU: f32 = 0.8;

// Upper bound on remembered dead tracks used for cooldown inheritance.
const MAX_DEAD: usize = 16;

#[derive(Clone, Debug)]
struct Track {
    bbox: Bbox,
    // Reference box for stillness detection.
    anchor: Bbox,
    // Consecutive frames with the box close enough to the anchor.
    still_frames: u64,
    hits: u8,
    age: u8,
    confirmed: bool,
    last_trigger_frame: Option<u64>,
}

/// A recently dropped confirmed track, kept so a successor track at the same
/// position inherits its trigger cooldown instead of re-triggering right away.
#[derive(Clone, Debug)]
struct DeadTrack {
    bbox: Bbox,
    last_trigger_frame: u64,
}

/// Minimal `IoU` tracker: confirms tracks, drops stale ones, rate-limits how
/// often a single track can trigger, and reports tracks that stood still long
/// enough to be absorbed into the background model.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    tracks: Vec<Track>,
    dead: Vec<DeadTrack>,
    frame: u64,
    absorb: Vec<Bbox>,

    // Confirmed track boxes of the current frame, rebuilt in place.
    active: Vec<Bbox>,
}

impl Tracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Boxes of the currently confirmed tracks, for the live overlay.
    pub(crate) fn active(&self) -> &[Bbox] {
        &self.active
    }

    /// Boxes of tracks that stood still long enough and should be absorbed
    /// into the background model. Drains the list.
    pub(crate) fn take_absorb(&mut self) -> Vec<Bbox> {
        std::mem::take(&mut self.absorb)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        detections: &[Bbox],
        min_hits: u8,
        max_age: u8,
        iou_threshold: f32,
        cooldown_frames: u64,
        // Frames after which a standing track is reported for absorption;
        // `u64::MAX` disables the stillness detection.
        static_frames: u64,
    ) -> Vec<Bbox> {
        self.frame += 1;
        let min_hits = min_hits.max(1);

        // Forget dead tracks whose cooldown window has closed. A cooldown of
        // zero drops everything: there is nothing to inherit then.
        self.dead
            .retain(|d| self.frame.saturating_sub(d.last_trigger_frame) < cooldown_frames);

        // Associate greedily by descending IoU over all candidate pairs, so
        // the assignment does not depend on the detection order.
        let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
        for (di, det) in detections.iter().enumerate() {
            for (ti, track) in self.tracks.iter().enumerate() {
                let score = iou(&track.bbox, det);
                if score >= iou_threshold {
                    pairs.push((score, di, ti));
                }
            }
        }
        pairs.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut det_matched = vec![false; detections.len()];
        let mut matched_tracks = vec![false; self.tracks.len()];
        let mut triggers = Vec::new();

        for (_, di, ti) in pairs {
            if det_matched[di] || matched_tracks[ti] {
                continue;
            }
            det_matched[di] = true;
            matched_tracks[ti] = true;
            let det = detections[di];

            let track = &mut self.tracks[ti];
            track.bbox = det;
            track.hits = track.hits.saturating_add(1);
            track.age = 0;
            if iou(&det, &track.anchor) >= STILL_IOU {
                track.still_frames += 1;
            } else {
                track.anchor = det;
                track.still_frames = 0;
            }
            if track.hits >= min_hits {
                track.confirmed = true;
            }
            if track.confirmed && may_trigger(track, cooldown_frames, self.frame) {
                track.last_trigger_frame = Some(self.frame);
                triggers.push(det);
            }
        }

        // Unmatched detections start new tracks, inheriting the cooldown of a
        // recently dropped track at the same position.
        for (di, det) in detections.iter().enumerate() {
            if det_matched[di] {
                continue;
            }
            let confirmed = min_hits <= 1;
            let mut last_trigger = None;
            if let Some(pos) = self
                .dead
                .iter()
                .position(|d| iou(&d.bbox, det) >= iou_threshold)
            {
                last_trigger = Some(self.dead.remove(pos).last_trigger_frame);
            }

            let mut track = Track {
                bbox: *det,
                anchor: *det,
                still_frames: 0,
                hits: 1,
                age: 0,
                confirmed,
                last_trigger_frame: None,
            };
            if confirmed {
                if let Some(last) = last_trigger {
                    // Inherited cooldown: only trigger if enough time has
                    // passed since the predecessor's last trigger.
                    track.last_trigger_frame = Some(last);
                    if may_trigger(&track, cooldown_frames, self.frame) {
                        track.last_trigger_frame = Some(self.frame);
                        triggers.push(*det);
                    }
                } else {
                    track.last_trigger_frame = Some(self.frame);
                    triggers.push(*det);
                }
            }
            self.tracks.push(track);
            // Keep the matched flags in sync with `tracks`: the new track
            // already consumed this detection, so it must not be associated
            // with another one in the same frame, and it must not age.
            matched_tracks.push(true);
        }
        // Age unmatched tracks, drop stale ones and report tracks that stood
        // still long enough for background absorption.
        self.retire(&matched_tracks, max_age, static_frames);

        self.active.clear();
        self.active
            .extend(self.tracks.iter().filter(|t| t.confirmed).map(|t| t.bbox));

        triggers
    }

    // Ages unmatched tracks, drops stale ones and reports tracks that stood
    // still long enough for background absorption. Unconfirmed tracks must
    // hit in every frame: a single miss drops them, which makes `min_hits`
    // mean consecutive frames.
    fn retire(&mut self, matched: &[bool], max_age: u8, static_frames: u64) {
        let mut absorbed: Vec<Bbox> = Vec::new();
        let mut write = 0;
        for read in 0..self.tracks.len() {
            let mut track = self.tracks[read].clone();
            if !matched.get(read).copied().unwrap_or(false) {
                track.age = track.age.saturating_add(1);
            }

            if track.confirmed && static_frames != u64::MAX && track.still_frames >= static_frames {
                absorbed.push(track.bbox);
                continue;
            }

            let keep = if track.confirmed {
                track.age <= max_age
            } else {
                track.age == 0
            };
            if !keep {
                if let Some(last) = track.last_trigger_frame {
                    self.dead.push(DeadTrack {
                        bbox: track.bbox,
                        last_trigger_frame: last,
                    });
                }
                continue;
            }
            self.tracks[write] = track;
            write += 1;
        }
        self.tracks.truncate(write);
        if self.dead.len() > MAX_DEAD {
            let drop = self.dead.len() - MAX_DEAD;
            self.dead.drain(0..drop);
        }

        self.absorb = absorbed;
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
        assert!(t.update(&[bbox(0, 0)], 2, 5, 0.3, 0, u64::MAX).is_empty());
        assert_eq!(1, t.update(&[bbox(0, 0)], 2, 5, 0.3, 0, u64::MAX).len());
    }

    #[test]
    fn test_immediate_when_min_hits_one() {
        let mut t = Tracker::new();
        assert_eq!(1, t.update(&[bbox(0, 0)], 1, 5, 0.3, 0, u64::MAX).len());
    }

    #[test]
    fn test_cooldown_suppresses_repeat() {
        let mut t = Tracker::new();
        assert_eq!(1, t.update(&[bbox(0, 0)], 1, 5, 0.3, 3, u64::MAX).len());
        // Within cooldown.
        assert!(t.update(&[bbox(0, 0)], 1, 5, 0.3, 3, u64::MAX).is_empty());
        assert!(t.update(&[bbox(0, 0)], 1, 5, 0.3, 3, u64::MAX).is_empty());
        // Cooldown elapsed (frame 4, last trigger frame 1, cooldown 3).
        assert_eq!(1, t.update(&[bbox(0, 0)], 1, 5, 0.3, 3, u64::MAX).len());
    }

    #[test]
    fn test_drops_stale_track() {
        let mut t = Tracker::new();
        t.update(&[bbox(0, 0)], 1, 1, 0.3, 0, u64::MAX);
        assert_eq!(1, t.tracks.len());
        // Two frames with no detection drop the track (age 2 > max_age 1).
        t.update(&[], 1, 1, 0.3, 0, u64::MAX);
        t.update(&[], 1, 1, 0.3, 0, u64::MAX);
        assert_eq!(0, t.tracks.len());
    }

    #[test]
    fn test_two_detections_on_empty_tracker() {
        // Two detections in one frame must not panic (the matched flags have
        // to grow together with the tracks) and must both become tracks.
        let mut t = Tracker::new();
        let triggers = t.update(&[bbox(0, 0), bbox(100, 100)], 2, 5, 0.3, 0, u64::MAX);
        assert!(triggers.is_empty());
        assert_eq!(2, t.tracks.len());
        assert!(t.active().is_empty());

        // Second frame confirms both.
        let triggers = t.update(&[bbox(0, 0), bbox(100, 100)], 2, 5, 0.3, 0, u64::MAX);
        assert_eq!(2, triggers.len());
        assert_eq!(2, t.active().len());
    }

    #[test]
    fn test_new_track_does_not_age_on_creation() {
        // max_age = 0: a fresh track must still survive to be confirmed by
        // the next hit instead of dying at birth.
        let mut t = Tracker::new();
        assert!(t.update(&[bbox(0, 0)], 2, 0, 0.3, 0, u64::MAX).is_empty());
        assert_eq!(1, t.update(&[bbox(0, 0)], 2, 0, 0.3, 0, u64::MAX).len());
    }

    #[test]
    fn test_min_hits_requires_consecutive_frames() {
        // hit, miss, hit: the miss kills the unconfirmed track, so the third
        // frame starts over instead of confirming the old one.
        let mut t = Tracker::new();
        assert!(t.update(&[bbox(0, 0)], 2, 5, 0.3, 0, u64::MAX).is_empty());
        t.update(&[], 2, 5, 0.3, 0, u64::MAX);
        assert_eq!(0, t.tracks.len());
        assert!(t.update(&[bbox(0, 0)], 2, 5, 0.3, 0, u64::MAX).is_empty());
        assert_eq!(1, t.update(&[bbox(0, 0)], 2, 5, 0.3, 0, u64::MAX).len());
    }

    #[test]
    fn test_dead_track_cooldown_inherited() {
        let mut t = Tracker::new();
        // Trigger at frame 1, cooldown 10.
        assert_eq!(1, t.update(&[bbox(0, 0)], 1, 0, 0.3, 10, u64::MAX).len());
        // Track dies on the miss but is remembered.
        t.update(&[], 1, 0, 0.3, 10, u64::MAX);
        // The successor at the same place inherits the cooldown (frame 3,
        // last trigger frame 1) and must not trigger yet.
        assert!(t.update(&[bbox(0, 0)], 1, 0, 0.3, 10, u64::MAX).is_empty());
    }

    #[test]
    fn test_standing_track_reported_for_absorption() {
        let mut t = Tracker::new();
        // Static after 3 frames of no significant movement.
        for _ in 0..3 {
            t.update(&[bbox(0, 0)], 1, 5, 0.3, 0, 3);
            assert!(t.take_absorb().is_empty());
        }
        t.update(&[bbox(0, 0)], 1, 5, 0.3, 0, 3);
        let absorb = t.take_absorb();
        assert_eq!(1, absorb.len());
        assert!(t.active().is_empty());
    }

    #[test]
    fn test_moving_track_not_reported_for_absorption() {
        let mut t = Tracker::new();
        // Moving 5px per frame keeps the track matched (IoU ~0.33) but below
        // the stillness threshold (0.8), resetting the counter.
        for i in 0..10_u32 {
            t.update(&[bbox(i * 5, 0)], 1, 5, 0.3, 0, 3);
        }
        assert!(t.take_absorb().is_empty());
        assert_eq!(1, t.active().len());
    }

    #[test]
    fn test_absorption_disabled() {
        let mut t = Tracker::new();
        for _ in 0..20 {
            t.update(&[bbox(0, 0)], 1, 5, 0.3, 0, u64::MAX);
        }
        assert!(t.take_absorb().is_empty());
        assert_eq!(1, t.active().len());
    }
}
