// SPDX-License-Identifier: GPL-2.0-or-later

/// A bounding box in analyzed-frame pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bbox {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) area: u32,
}

/// Reusable buffers for binary morphology and connected-component labeling.
#[derive(Debug, Default)]
pub(crate) struct Blobs {
    scratch: Vec<u8>,
    labels: Vec<u32>,
    parent: Vec<u32>,
    area: Vec<u32>,
    min_x: Vec<u32>,
    min_y: Vec<u32>,
    max_x: Vec<u32>,
    max_y: Vec<u32>,
    boxes: Vec<Bbox>,
}

#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names,
    clippy::too_many_arguments
)]
impl Blobs {
    /// Applies optional open/close, labels 8-connected foreground, and returns
    /// boxes whose area is within `[min_area, max_area]`.
    pub(crate) fn detect(
        &mut self,
        mask: &mut [u8],
        w: usize,
        h: usize,
        open: bool,
        close: bool,
        min_area: u32,
        max_area: u32,
    ) -> &mut [Bbox] {
        debug_assert_eq!(mask.len(), w * h, "mask/size mismatch");
        self.scratch.resize(mask.len(), 0);
        if open {
            // open = erode then dilate.
            erode3(mask, &mut self.scratch, w, h);
            dilate3(&self.scratch, mask, w, h);
        }
        if close {
            // close = dilate then erode.
            dilate3(mask, &mut self.scratch, w, h);
            erode3(&self.scratch, mask, w, h);
        }

        self.label(mask, w, h, min_area, max_area);
        &mut self.boxes
    }

    fn label(&mut self, mask: &[u8], w: usize, h: usize, min_area: u32, max_area: u32) {
        let n = w * h;
        debug_assert_eq!(mask.len(), n, "mask/size mismatch");
        self.labels.clear();
        self.labels.resize(n, 0);
        // Union-find over provisional labels, index 0 is unused.
        self.parent.clear();
        self.parent.push(0);

        self.label_provisional(mask, w, h);

        let count = self.parent.len();
        self.area.clear();
        self.area.resize(count, 0);
        self.min_x.clear();
        self.min_x.resize(count, u32::MAX);
        self.min_y.clear();
        self.min_y.resize(count, u32::MAX);
        self.max_x.clear();
        self.max_x.resize(count, 0);
        self.max_y.clear();
        self.max_y.resize(count, 0);

        self.label_stats(w, h);

        self.boxes.clear();
        for r in 1..count {
            let ru = u32::try_from(r).unwrap_or(0);
            if find(self.parent.as_mut_slice(), ru) != ru {
                continue;
            }
            let area = self.area[r];
            if area < min_area || area > max_area {
                continue;
            }
            let x0 = self.min_x[r];
            let y0 = self.min_y[r];
            self.boxes.push(Bbox {
                x: x0,
                y: y0,
                w: self.max_x[r] - x0 + 1,
                h: self.max_y[r] - y0 + 1,
                area,
            });
        }
    }

    // First pass: provisional labels with union-find merges.
    fn label_provisional(&mut self, mask: &[u8], w: usize, h: usize) {
        let Self { labels, parent, .. } = self;
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let i = row + x;
                if mask[i] == 0 {
                    continue;
                }
                // 8-connectivity: only already-visited neighbors matter
                // (W, NW, N, NE).
                let mut best: u32 = 0;
                if y > 0 {
                    let up = i - w;
                    if x > 0 {
                        merge(parent, &mut best, labels[up - 1]);
                    }
                    merge(parent, &mut best, labels[up]);
                    if x + 1 < w {
                        merge(parent, &mut best, labels[up + 1]);
                    }
                }
                if x > 0 {
                    merge(parent, &mut best, labels[i - 1]);
                }
                if best == 0 {
                    best = u32::try_from(parent.len()).unwrap_or(1);
                    parent.push(best);
                }
                labels[i] = best;
            }
        }
    }

    // Second pass: resolve roots and accumulate bounding boxes.
    fn label_stats(&mut self, w: usize, h: usize) {
        let Self {
            labels,
            parent,
            area,
            min_x,
            min_y,
            max_x,
            max_y,
            ..
        } = self;
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let l = labels[row + x];
                if l == 0 {
                    continue;
                }
                let r = pidx(find(parent, l));
                let (xu, yu) = (u32::try_from(x).unwrap_or(0), u32::try_from(y).unwrap_or(0));
                area[r] += 1;
                if xu < min_x[r] {
                    min_x[r] = xu;
                }
                if yu < min_y[r] {
                    min_y[r] = yu;
                }
                if xu > max_x[r] {
                    max_x[r] = xu;
                }
                if yu > max_y[r] {
                    max_y[r] = yu;
                }
            }
        }
    }
}

#[inline]
fn pidx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

// Records a labeled neighbor into the best-candidate slot, unioning labels
// when two different components meet.
fn merge(parent: &mut [u32], best: &mut u32, l: u32) {
    if l == 0 {
        return;
    }
    if *best == 0 {
        *best = l;
        return;
    }
    union(parent, *best, l);
    if l < *best {
        *best = l;
    }
}

fn find(parent: &mut [u32], x0: u32) -> u32 {
    let mut root = x0;
    while parent[pidx(root)] != root {
        root = parent[pidx(root)];
    }
    // Path compression.
    let mut x = x0;
    while parent[pidx(x)] != root {
        let next = parent[pidx(x)];
        parent[pidx(x)] = root;
        x = next;
    }
    root
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[pidx(hi)] = lo;
    }
}

fn neighbor(v: usize, d: isize, max: usize) -> usize {
    let v = isize::try_from(v).unwrap_or(0) + d;
    let v = v.clamp(0, isize::try_from(max.saturating_sub(1)).unwrap_or(0));
    usize::try_from(v).unwrap_or(0)
}

// Generic clamped-border pixel operations, used for frame borders and the
// scalar tails of the word-wise interior path.

fn erode_px(src: &[u8], w: usize, h: usize, x: usize, y: usize) -> u8 {
    for dy in -1..=1 {
        for dx in -1..=1 {
            let nx = neighbor(x, dx, w);
            let ny = neighbor(y, dy, h);
            if src[ny * w + nx] == 0 {
                return 0;
            }
        }
    }
    255
}

fn dilate_px(src: &[u8], w: usize, h: usize, x: usize, y: usize) -> u8 {
    for dy in -1..=1 {
        for dx in -1..=1 {
            let nx = neighbor(x, dx, w);
            let ny = neighbor(y, dy, h);
            if src[ny * w + nx] != 0 {
                return 255;
            }
        }
    }
    0
}

// Writes only the one-pixel frame border instead of scanning the whole image
// to find it.
fn borders(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    px: fn(&[u8], usize, usize, usize, usize) -> u8,
) {
    if w == 0 || h == 0 {
        return;
    }
    let last_x = w - 1;
    let last_y = h - 1;
    for x in 0..w {
        dst[x] = px(src, w, h, x, 0);
        dst[last_y * w + x] = px(src, w, h, x, last_y);
    }
    for y in 1..last_y {
        let row = y * w;
        dst[row] = px(src, w, h, 0, y);
        dst[row + last_x] = px(src, w, h, last_x, y);
    }
}

#[inline]
fn load8(s: &[u8]) -> u64 {
    u64::from_le_bytes(s.try_into().expect("8 bytes"))
}

#[inline]
fn store8(s: &mut [u8], v: u64) {
    s.copy_from_slice(&v.to_le_bytes());
}

/// 3x3 erosion: a pixel survives only if the whole 3x3 neighborhood is set.
///
/// Mask bytes are 0 or 255, so bitwise AND implements erosion. The interior
/// processes eight pixels per iteration through overlapping word loads.
#[allow(clippy::many_single_char_names)]
pub(crate) fn erode3(src: &[u8], dst: &mut [u8], w: usize, h: usize) {
    debug_assert_eq!(src.len(), w * h);
    debug_assert_eq!(dst.len(), w * h);

    // Interior fast path: direct indexing without clamping.
    if w >= 3 && h >= 3 {
        for y in 1..h - 1 {
            let row = y * w;
            if w >= 10 {
                let up = &src[row - w..row];
                let mid = &src[row..row + w];
                let down = &src[row + w..row + 2 * w];
                let out = &mut dst[row + 1..row + w - 1];
                let mut x = 0;
                while x + 8 <= out.len() {
                    let a = load8(&up[x..x + 8]) & load8(&mid[x..x + 8]) & load8(&down[x..x + 8]);
                    let b = load8(&up[x + 1..x + 9])
                        & load8(&mid[x + 1..x + 9])
                        & load8(&down[x + 1..x + 9]);
                    let c = load8(&up[x + 2..x + 10])
                        & load8(&mid[x + 2..x + 10])
                        & load8(&down[x + 2..x + 10]);
                    store8(&mut out[x..x + 8], a & b & c);
                    x += 8;
                }
                for (gx, o) in out.iter_mut().enumerate().skip(x) {
                    *o = erode_px(src, w, h, gx + 1, y);
                }
            } else {
                for x in 1..w - 1 {
                    dst[row + x] = erode_px(src, w, h, x, y);
                }
            }
        }
    }
    borders(src, dst, w, h, erode_px);
}

/// 3x3 dilation: a pixel is set if any pixel of the 3x3 neighborhood is set.
///
/// Mask bytes are 0 or 255, so bitwise OR implements dilation. The interior
/// processes eight pixels per iteration through overlapping word loads.
#[allow(clippy::many_single_char_names)]
pub(crate) fn dilate3(src: &[u8], dst: &mut [u8], w: usize, h: usize) {
    debug_assert_eq!(src.len(), w * h);
    debug_assert_eq!(dst.len(), w * h);

    // Interior fast path: direct indexing without clamping.
    if w >= 3 && h >= 3 {
        for y in 1..h - 1 {
            let row = y * w;
            if w >= 10 {
                let up = &src[row - w..row];
                let mid = &src[row..row + w];
                let down = &src[row + w..row + 2 * w];
                let out = &mut dst[row + 1..row + w - 1];
                let mut x = 0;
                while x + 8 <= out.len() {
                    let a = load8(&up[x..x + 8]) | load8(&mid[x..x + 8]) | load8(&down[x..x + 8]);
                    let b = load8(&up[x + 1..x + 9])
                        | load8(&mid[x + 1..x + 9])
                        | load8(&down[x + 1..x + 9]);
                    let c = load8(&up[x + 2..x + 10])
                        | load8(&mid[x + 2..x + 10])
                        | load8(&down[x + 2..x + 10]);
                    store8(&mut out[x..x + 8], a | b | c);
                    x += 8;
                }
                for (gx, o) in out.iter_mut().enumerate().skip(x) {
                    *o = dilate_px(src, w, h, gx + 1, y);
                }
            } else {
                for x in 1..w - 1 {
                    dst[row + x] = dilate_px(src, w, h, x, y);
                }
            }
        }
    }
    borders(src, dst, w, h, dilate_px);
}

#[allow(
    clippy::unwrap_used,
    clippy::unseparated_literal_suffix,
    clippy::assert_is_empty
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn to_text(mask: &[u8], w: usize, h: usize) -> String {
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push(if mask[y * w + x] != 0 { 'X' } else { '.' });
            }
        }
        s
    }

    #[test]
    fn test_open_removes_speckle() {
        // 3x3 with a lone center pixel.
        let mut mask = vec![0u8; 9];
        mask[4] = 255;
        let mut buf = vec![0u8; 9];
        erode3(&mask, &mut buf, 3, 3);
        assert_eq!(".........", to_text(&buf, 3, 3));
        dilate3(&buf, &mut mask, 3, 3);
        assert_eq!(".........", to_text(&mask, 3, 3));
    }

    #[test]
    fn test_open_keeps_solid_block() {
        let mask = vec![255u8; 9];
        let mut buf = vec![0u8; 9];
        erode3(&mask, &mut buf, 3, 3);
        // Center survives, corners are eroded but we only check it stayed solid.
        assert_eq!(255, buf[4]);
    }

    #[test]
    fn test_connected_components_two_boxes() {
        // 6x3: two 2x2 blocks.
        let w = 6;
        let h = 3;
        let mut mask = vec![0u8; w * h];
        for (bx, by) in [(0usize, 0usize), (4, 1)] {
            for dy in 0..2 {
                for dx in 0..2 {
                    mask[(by + dy) * w + bx + dx] = 255;
                }
            }
        }
        let mut b = Blobs::default();
        let boxes = b.detect(&mut mask, w, h, false, false, 1, u32::MAX);
        assert_eq!(2, boxes.len());
        assert_eq!(
            Bbox {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
                area: 4
            },
            boxes[0]
        );
        assert_eq!(
            Bbox {
                x: 4,
                y: 1,
                w: 2,
                h: 2,
                area: 4
            },
            boxes[1]
        );
    }

    #[test]
    fn test_area_filter() {
        let w = 6;
        let h = 3;
        let mut mask = vec![0u8; w * h];
        for dy in 0..2 {
            for dx in 0..2 {
                mask[dy * w + dx] = 255;
            }
        }
        let mut b = Blobs::default();
        // min_area above the 4-pixel blob -> filtered out.
        let boxes = b.detect(&mut mask, w, h, false, false, 5, u32::MAX);
        assert!(boxes.is_empty());
    }

    // Independent reference implementations to verify the fast interior path
    // matches the clamped semantics everywhere.
    fn reference(src: &[u8], w: usize, h: usize, erode: bool) -> Vec<u8> {
        let at = |x: i64, y: i64| -> u8 {
            let x = x.clamp(0, i64::try_from(w).unwrap() - 1);
            let y = y.clamp(0, i64::try_from(h).unwrap() - 1);
            src[usize::try_from(y * i64::try_from(w).unwrap() + x).unwrap()]
        };
        let mut out = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let mut acc = if erode { 255 } else { 0 };
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let v = at(
                            i64::try_from(x).unwrap() + dx,
                            i64::try_from(y).unwrap() + dy,
                        );
                        if erode {
                            acc &= v;
                        } else {
                            acc |= v;
                        }
                    }
                }
                out[y * w + x] = acc;
            }
        }
        out
    }

    #[test]
    fn test_morphology_matches_reference() {
        // 7x5 pseudo-random mask.
        let w = 7;
        let h = 5;
        let mut mask = vec![0u8; w * h];
        let mut seed = 0x1234_5678_u64;
        for v in &mut mask {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *v = if (seed >> 33) % 2 == 0 { 255 } else { 0 };
        }

        let mut out = vec![0u8; w * h];
        erode3(&mask, &mut out, w, h);
        assert_eq!(reference(&mask, w, h, true), out);
        dilate3(&mask, &mut out, w, h);
        assert_eq!(reference(&mask, w, h, false), out);
    }

    #[test]
    fn test_morphology_matches_reference_word_path() {
        // Sizes that exercise the 8-bytes-per-iteration interior path plus
        // its scalar tail and the frame border.
        for (w, h) in [(34_usize, 9_usize), (33, 5), (10, 4), (25, 3)] {
            let mut mask = vec![0u8; w * h];
            let mut seed = 0xDEAD_BEEF_CAFE_1234_u64;
            for v in &mut mask {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *v = if (seed >> 33) % 3 == 0 { 255 } else { 0 };
            }

            let mut out = vec![0u8; w * h];
            erode3(&mask, &mut out, w, h);
            assert_eq!(reference(&mask, w, h, true), out, "erode {w}x{h}");
            dilate3(&mask, &mut out, w, h);
            assert_eq!(reference(&mask, w, h, false), out, "dilate {w}x{h}");
        }
    }
}
