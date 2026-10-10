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
    labels: Vec<i32>,
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
    ) -> &[Bbox] {
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
        &self.boxes
    }

    fn label(&mut self, mask: &[u8], w: usize, h: usize, min_area: u32, max_area: u32) {
        let n = w * h;
        debug_assert_eq!(mask.len(), n, "mask/size mismatch");
        self.labels.clear();
        self.labels.resize(n, 0);
        // Union-find over provisional labels, index 0 is unused.
        self.parent.clear();
        self.parent.push(0);

        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if mask[i] == 0 {
                    continue;
                }
                // 8-connectivity: only already-visited neighbors matter.
                let mut best: u32 = 0;
                for (dx, dy) in left_neighbors() {
                    let Some(j) = offset(x, y, dx, dy, w, h) else {
                        continue;
                    };
                    let l = self.labels[j];
                    if l == 0 {
                        continue;
                    }
                    let l = u32::try_from(l).unwrap_or(0);
                    if best == 0 {
                        best = l;
                    } else {
                        self.union(best, l);
                        if l < best {
                            best = l;
                        }
                    }
                }
                if best == 0 {
                    best = u32::try_from(self.parent.len()).unwrap_or(1);
                    self.parent.push(best);
                }
                self.labels[i] = i32::try_from(best).unwrap_or(0);
            }
        }

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

        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let l = self.labels[i];
                if l == 0 {
                    continue;
                }
                let r = self.find(u32::try_from(l).unwrap_or(0));
                let xu = u32::try_from(x).unwrap_or(0);
                let yu = u32::try_from(y).unwrap_or(0);
                self.area[r as usize] += 1;
                if xu < self.min_x[r as usize] {
                    self.min_x[r as usize] = xu;
                }
                if yu < self.min_y[r as usize] {
                    self.min_y[r as usize] = yu;
                }
                if xu > self.max_x[r as usize] {
                    self.max_x[r as usize] = xu;
                }
                if yu > self.max_y[r as usize] {
                    self.max_y[r as usize] = yu;
                }
            }
        }

        self.boxes.clear();
        for r in 1..count {
            let r = u32::try_from(r).unwrap_or(0);
            if self.find(r) != r {
                continue;
            }
            let area = self.area[r as usize];
            if area < min_area || area > max_area {
                continue;
            }
            let x0 = self.min_x[r as usize];
            let y0 = self.min_y[r as usize];
            self.boxes.push(Bbox {
                x: x0,
                y: y0,
                w: self.max_x[r as usize] - x0 + 1,
                h: self.max_y[r as usize] - y0 + 1,
                area,
            });
        }
    }

    fn find(&mut self, mut x: u32) -> u32 {
        let mut root = x;
        while self.parent[root as usize] != root {
            root = self.parent[root as usize];
        }
        // Path compression.
        while self.parent[x as usize] != root {
            let next = self.parent[x as usize];
            self.parent[x as usize] = root;
            x = next;
        }
        root
    }

    fn union(&mut self, a: u32, b: u32) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi as usize] = lo;
        }
    }
}

// Neighbors that were already visited in a raster scan: left, up-left, up, up-right.
const fn left_neighbors() -> [(isize, isize); 4] {
    [(-1, 0), (-1, -1), (0, -1), (1, -1)]
}

fn offset(x: usize, y: usize, dx: isize, dy: isize, w: usize, h: usize) -> Option<usize> {
    let nx = isize::try_from(x).ok()? + dx;
    let ny = isize::try_from(y).ok()? + dy;
    if nx < 0 || ny < 0 {
        return None;
    }
    let nx = usize::try_from(nx).ok()?;
    let ny = usize::try_from(ny).ok()?;
    if nx >= w || ny >= h {
        return None;
    }
    Some(ny * w + nx)
}

fn neighbor(v: usize, d: isize, max: usize) -> usize {
    let v = isize::try_from(v).unwrap_or(0) + d;
    let v = v.clamp(0, isize::try_from(max.saturating_sub(1)).unwrap_or(0));
    usize::try_from(v).unwrap_or(0)
}

fn is_border(x: usize, y: usize, w: usize, h: usize) -> bool {
    x == 0 || y == 0 || x + 1 == w || y + 1 == h
}

// Generic clamped-border pixel operations, used for frame borders only.

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

fn borders(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    px: fn(&[u8], usize, usize, usize, usize) -> u8,
) {
    for y in 0..h {
        for x in 0..w {
            if !is_border(x, y, w, h) {
                continue;
            }
            dst[y * w + x] = px(src, w, h, x, y);
        }
    }
}

/// 3x3 erosion: a pixel survives only if the whole 3x3 neighborhood is set.
pub(crate) fn erode3(src: &[u8], dst: &mut [u8], w: usize, h: usize) {
    debug_assert_eq!(src.len(), w * h);
    debug_assert_eq!(dst.len(), w * h);

    // Interior fast path: direct indexing without clamping.
    if w >= 3 && h >= 3 {
        for y in 1..h - 1 {
            let row = y * w;
            let up = row - w;
            let down = row + w;
            for x in 1..w - 1 {
                let i = row + x;
                let all = src[up + x - 1] != 0
                    && src[up + x] != 0
                    && src[up + x + 1] != 0
                    && src[i - 1] != 0
                    && src[i] != 0
                    && src[i + 1] != 0
                    && src[down + x - 1] != 0
                    && src[down + x] != 0
                    && src[down + x + 1] != 0;
                dst[i] = if all { 255 } else { 0 };
            }
        }
    }
    borders(src, dst, w, h, erode_px);
}

/// 3x3 dilation: a pixel is set if any pixel of the 3x3 neighborhood is set.
pub(crate) fn dilate3(src: &[u8], dst: &mut [u8], w: usize, h: usize) {
    debug_assert_eq!(src.len(), w * h);
    debug_assert_eq!(dst.len(), w * h);

    // Interior fast path: direct indexing without clamping.
    if w >= 3 && h >= 3 {
        for y in 1..h - 1 {
            let row = y * w;
            let up = row - w;
            let down = row + w;
            for x in 1..w - 1 {
                let i = row + x;
                let any = src[up + x - 1] != 0
                    || src[up + x] != 0
                    || src[up + x + 1] != 0
                    || src[i - 1] != 0
                    || src[i] != 0
                    || src[i + 1] != 0
                    || src[down + x - 1] != 0
                    || src[down + x] != 0
                    || src[down + x + 1] != 0;
                dst[i] = if any { 255 } else { 0 };
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
}
