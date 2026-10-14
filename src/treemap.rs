//! WizTree-style treemap: squarified layout of every file in the scan,
//! rendered as a cushion-shaded bitmap (van Wijk / WinDirStat cushions).
//!
//! Folders are never painted themselves: each nesting level only adds a
//! ridge to the cushion surface, so folder boundaries show up as shading
//! while every visible pixel belongs to a file coloured by its extension.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::RenderImage;

use crate::model::ScanResult;

/// Cushion parameters (WinDirStat defaults, which WizTree mirrors).
const HEIGHT: f32 = 0.38;
const SCALE_FACTOR: f32 = 0.91;
const AMBIENT: f32 = 0.13;
const BRIGHTNESS: f32 = 0.80;
const PALETTE_BRIGHTNESS: f32 = 0.6;
const LIGHT: (f32, f32, f32) = (-1.0, -1.0, 10.0);

/// Extensions ranked by total size get these colours, largest first.
/// Everything past the palette (and files with no extension) is grey.
const PALETTE: [u32; 24] = [
    0x2f6fdf, 0xdf2f2f, 0x2fbf2f, 0x2fbfbf, 0xbf2fbf, 0xdfdf2f, 0x6f6fff, 0xff8f2f,
    0x8fdf3f, 0x3fdf9f, 0xff5f9f, 0x9f5fdf, 0xdfaf6f, 0x5fafdf, 0xaf3f3f, 0x3f8f3f,
    0x3f3f9f, 0x9f9f3f, 0x9f3f8f, 0x3f9f9f, 0xcf7f7f, 0x7fcf7f, 0x7f7fcf, 0xcfcf7f,
];
const OTHER_COLOR: u32 = 0x5a5c60;
/// Files outside the highlighted type.
const DIM_COLOR: u32 = 0x202124;

/// Pull a palette entry most of the way toward a neutral grey so the
/// treemap reads as a tool, not a poster.
fn mute(c: u32) -> u32 {
    const GREY: f32 = 0x70 as f32;
    const KEEP: f32 = 0.35;
    let ch = |shift: u32| {
        let v = ((c >> shift) & 0xff) as f32;
        ((GREY + (v - GREY) * KEEP) * 0.9) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// Lowercased extension of a file name; borrows when already lowercase.
pub fn ext_key(name: &str) -> Option<Cow<'_, str>> {
    let ext = std::path::Path::new(name).extension()?.to_str()?;
    Some(if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(ext.to_ascii_lowercase())
    } else {
        Cow::Borrowed(ext)
    })
}

/// One row of the File Types view.
pub struct ExtStat {
    /// Lowercase extension, or empty for files without one.
    pub ext: String,
    pub size: u64,
    pub count: u64,
    pub color: u32,
}

/// Extension -> colour assignment, ranked by total bytes like WizTree's
/// File Types list. `ranked` holds every extension for the File Types view.
pub struct ExtColors {
    map: HashMap<String, u32>,
    pub ranked: Vec<ExtStat>,
}

impl ExtColors {
    pub fn build(scan: &ScanResult) -> Self {
        let mut totals: HashMap<String, (u64, u64)> = HashMap::new();
        for (id, n) in scan.nodes.iter().enumerate() {
            if n.is_dir || n.deleted {
                continue;
            }
            let key = ext_key(scan.name(id));
            let key = key.as_deref().unwrap_or("");
            // Only allocate the key the first time an extension is seen.
            match totals.get_mut(key) {
                Some(t) => {
                    t.0 += n.size;
                    t.1 += 1;
                }
                None => {
                    totals.insert(key.to_owned(), (n.size, 1));
                }
            }
        }
        let mut ranked: Vec<ExtStat> = totals
            .into_iter()
            .map(|(ext, (size, count))| ExtStat { ext, size, count, color: OTHER_COLOR })
            .collect();
        ranked.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.ext.cmp(&b.ext)));
        let mut map = HashMap::new();
        let mut palette = PALETTE.iter();
        for st in &mut ranked {
            // Extension-less files stay grey and don't consume a palette slot.
            if st.ext.is_empty() {
                continue;
            }
            let Some(&c) = palette.next() else { break };
            st.color = mute(c);
            map.insert(st.ext.clone(), st.color);
        }
        Self { map, ranked }
    }

    pub fn color_of(&self, name: &str) -> u32 {
        ext_key(name)
            .and_then(|e| self.map.get(e.as_ref()).copied())
            .unwrap_or(OTHER_COLOR)
    }
}

/// Geometry of one treemap: depends on the scan, root and size, but not on
/// colours, so a highlight change re-rasterizes without re-laying out.
pub struct Layout {
    /// Bitmap size the layout was computed for; hit tests scale into it.
    pub width: u32,
    pub height: u32,
    root: usize,
    /// Every laid-out node (folders and files) in pre-order, in bitmap
    /// pixel coordinates. A child always follows its parent and sibling
    /// subtrees are disjoint, so the *last* rect containing a point is the
    /// deepest node under it.
    pub rects: Vec<(usize, RectF)>,
    /// Node id -> position in `rects` (`u32::MAX` = not laid out).
    index: Vec<u32>,
    /// `GRID`-px buckets of leaf rect positions, so a hit test only checks
    /// the few files under the cursor instead of every rect.
    grid: Vec<Vec<u32>>,
    grid_cols: usize,
    leaves: Vec<Leaf>,
}

pub struct Treemap {
    pub image: Arc<RenderImage>,
    pub layout: Arc<Layout>,
}

const GRID: f32 = 32.0;

impl Layout {
    pub fn new(scan: &ScanResult, root: usize, width: u32, height: u32) -> Self {
        let (rects, leaves) = layout(scan, root, width as f32, height as f32);
        let grid_cols = (width as f32 / GRID).ceil().max(1.0) as usize;
        let grid_rows = (height as f32 / GRID).ceil().max(1.0) as usize;
        let mut grid: Vec<Vec<u32>> = vec![Vec::new(); grid_cols * grid_rows];
        for leaf in &leaves {
            let r = leaf.rect;
            let gx0 = (r.x / GRID).max(0.0) as usize;
            let gy0 = (r.y / GRID).max(0.0) as usize;
            let gx1 = (((r.x + r.w) / GRID) as usize).min(grid_cols - 1);
            let gy1 = (((r.y + r.h) / GRID) as usize).min(grid_rows - 1);
            for gy in gy0..=gy1 {
                for gx in gx0..=gx1 {
                    grid[gy * grid_cols + gx].push(leaf.rect_ix);
                }
            }
        }
        let mut index = vec![u32::MAX; scan.nodes.len()];
        for (i, (id, _)) in rects.iter().enumerate() {
            index[*id] = i as u32;
        }
        Self { width, height, root, rects, index, grid, grid_cols, leaves }
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        if x < 0.0 || y < 0.0 || x >= self.width as f32 || y >= self.height as f32 {
            return None;
        }
        let cell = (y / GRID) as usize * self.grid_cols + (x / GRID) as usize;
        if let Some(hit) = self.grid.get(cell).and_then(|c| {
            c.iter()
                .map(|&i| &self.rects[i as usize])
                .find(|(_, r)| r.contains(x, y))
        }) {
            return Some(hit.0);
        }
        // No file under the point (culled or zero-size): deepest folder wins.
        self.rects
            .iter()
            .rev()
            .find(|(_, r)| r.contains(x, y))
            .map(|(id, _)| *id)
    }

    pub fn rect_of(&self, id: usize) -> Option<RectF> {
        self.index
            .get(id)
            .filter(|&&i| i != u32::MAX)
            .map(|&i| self.rects[i as usize].1)
    }
}

/// Render the subtree under `root` into a `width` x `height` BGRA bitmap.
/// With `highlight` set, files of other extensions are dimmed. `reuse` is a
/// previous layout of the same scan; it is used when root and size match.
pub fn build(
    scan: &ScanResult,
    colors: &ExtColors,
    root: usize,
    highlight: Option<&str>,
    width: u32,
    height: u32,
    reuse: Option<Arc<Layout>>,
) -> Treemap {
    use rayon::prelude::*;
    let layout = reuse
        .filter(|l| l.root == root && l.width == width && l.height == height)
        .unwrap_or_else(|| Arc::new(Layout::new(scan, root, width, height)));
    let mut buf = vec![0u8; width as usize * height as usize * 4];
    // Rasterize in horizontal bands on all cores. Leaves are disjoint, so
    // each band only needs the leaves overlapping it, clipped to its rows.
    let band_count = (height as usize).div_ceil(BAND_ROWS).max(1);
    let mut bands: Vec<Vec<(u32, &Leaf)>> = vec![Vec::new(); band_count];
    // Colour lookup (extension parse + hash) dominates, so do it in parallel.
    let leaf_colors: Vec<u32> = layout
        .leaves
        .par_iter()
        .map(|leaf| {
            let name = scan.name(leaf.id);
            let dimmed = highlight.is_some_and(|h| ext_key(name).as_deref().unwrap_or("") != h);
            if dimmed { DIM_COLOR } else { colors.color_of(name) }
        })
        .collect();
    for (leaf, &color) in layout.leaves.iter().zip(&leaf_colors) {
        let r = leaf.rect;
        let y0 = r.y.round().max(0.0) as usize;
        let y1 = ((r.y + r.h).round().max(0.0) as usize).min(height as usize);
        if y0 >= y1 {
            continue;
        }
        for band in &mut bands[y0 / BAND_ROWS..=(y1 - 1) / BAND_ROWS] {
            band.push((color, leaf));
        }
    }
    let band_bytes = BAND_ROWS * width as usize * 4;
    buf.par_chunks_mut(band_bytes)
        .zip(bands.par_iter())
        .enumerate()
        .for_each(|(bi, (chunk, items))| {
            for px in chunk.chunks_exact_mut(4) {
                px[3] = 255;
            }
            let band_y0 = (bi * BAND_ROWS) as u32;
            let rows = (chunk.len() / (width as usize * 4)) as u32;
            for &(color, leaf) in items {
                draw_cushion(chunk, width, band_y0, band_y0 + rows, leaf.rect, &leaf.surface, color);
            }
        });
    let image = image::RgbaImage::from_raw(width, height, buf).expect("buffer size");
    let frame = image::Frame::new(image);
    Treemap {
        image: Arc::new(RenderImage::new(vec![frame])),
        layout,
    }
}

struct Leaf {
    id: usize,
    rect_ix: u32,
    rect: RectF,
    surface: [f32; 4],
}

fn layout(
    scan: &ScanResult,
    root: usize,
    width: f32,
    height: f32,
) -> (Vec<(usize, RectF)>, Vec<Leaf>) {
    let mut rects = Vec::new();
    let mut leaves = Vec::new();
    if scan.nodes.get(root).map(|n| n.size == 0 || n.deleted).unwrap_or(true) {
        return (rects, leaves);
    }
    let full = RectF { x: 0.0, y: 0.0, w: width, h: height };
    // (node, rect, surface, ridge height). The root gets no ridge of its own.
    let mut stack: Vec<(usize, RectF, [f32; 4], f32, bool)> =
        vec![(root, full, [0.0; 4], HEIGHT, true)];
    while let Some((id, rect, mut surface, h, is_root)) = stack.pop() {
        if rect.w * rect.h < MIN_AREA {
            continue;
        }
        if !is_root {
            add_ridge(rect, &mut surface, h);
        }
        rects.push((id, rect));
        let node = &scan.nodes[id];
        // Children that would get less than MIN_AREA px² are culled when
        // popped anyway, so drop them before sorting -- a folder of 10k
        // files in a 50 px tile otherwise sorts and lays out all 10k. The
        // full total is kept so the survivors keep their true proportions.
        let total = node.size;
        let min_size = (total as f64 * MIN_AREA as f64 / (rect.w as f64 * rect.h as f64)) as u64;
        let mut kids: Vec<(usize, u64)> = if node.is_dir {
            node.children()
                .filter(|&c| {
                    let n = &scan.nodes[c];
                    !n.deleted && n.size > 0 && n.size >= min_size
                })
                .map(|c| (c, scan.nodes[c].size))
                .collect()
        } else {
            Vec::new()
        };
        if kids.is_empty() {
            if !node.is_dir {
                leaves.push(Leaf { id, rect_ix: (rects.len() - 1) as u32, surface, rect });
            }
            continue;
        }
        kids.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        // Push in reverse so children pop (and land in `rects`) in order.
        let placed = squarify_total(&kids, rect, total);
        for (cid, crect) in placed.into_iter().rev() {
            stack.push((cid, crect, surface, h * SCALE_FACTOR, false));
        }
    }
    (rects, leaves)
}

/// Tiles smaller than this (in px²) are not laid out or drawn.
const MIN_AREA: f32 = 0.25;

/// Bruls/Huizing/van Wijk squarified layout. `items` sorted descending.
#[cfg(test)]
pub fn squarify(items: &[(usize, u64)], rect: RectF) -> Vec<(usize, RectF)> {
    squarify_total(items, rect, items.iter().map(|i| i.1).sum())
}

/// Squarified layout of `items` as a share of `total`. When `total`
/// exceeds the items' sum (culled tiny children), the leftover area stays
/// empty instead of inflating the survivors.
pub fn squarify_total(items: &[(usize, u64)], rect: RectF, total: u64) -> Vec<(usize, RectF)> {
    let mut out = Vec::with_capacity(items.len());
    let item_sum: u64 = items.iter().map(|i| i.1).sum();
    let total = total.max(item_sum) as f64;
    // Only stretch the final row to the rect edge when nothing was culled.
    let complete = item_sum as f64 >= total;
    if total <= 0.0 {
        return out;
    }
    let mut r = rect;
    let mut remaining = total;
    let mut start = 0;
    while start < items.len() {
        let area_scale = (r.w as f64 * r.h as f64) / remaining;
        // Lay rows along the shorter side.
        let side = (r.w.min(r.h)) as f64;
        let mut end = start + 1;
        let mut row_sum = items[start].1 as f64;
        let mut best = worst_ratio(items[start].1 as f64, items[start].1 as f64, row_sum, side, area_scale);
        while end < items.len() {
            let v = items[end].1 as f64;
            let s = row_sum + v;
            let ratio = worst_ratio(items[start].1 as f64, v, s, side, area_scale);
            if ratio > best {
                break;
            }
            best = ratio;
            row_sum = s;
            end += 1;
        }
        let row = &items[start..end];
        let frac = (row_sum / remaining) as f32;
        if r.w >= r.h {
            // Column on the left.
            let col_w = if end == items.len() && complete { r.w } else { r.w * frac };
            let mut y = r.y;
            for (i, &(id, v)) in row.iter().enumerate() {
                let hh = if i + 1 == row.len() {
                    r.y + r.h - y
                } else {
                    (r.h as f64 * v as f64 / row_sum) as f32
                };
                out.push((id, RectF { x: r.x, y, w: col_w, h: hh }));
                y += hh;
            }
            r.x += col_w;
            r.w -= col_w;
        } else {
            // Row along the top.
            let row_h = if end == items.len() && complete { r.h } else { r.h * frac };
            let mut x = r.x;
            for (i, &(id, v)) in row.iter().enumerate() {
                let ww = if i + 1 == row.len() {
                    r.x + r.w - x
                } else {
                    (r.w as f64 * v as f64 / row_sum) as f32
                };
                out.push((id, RectF { x, y: r.y, w: ww, h: row_h }));
                x += ww;
            }
            r.y += row_h;
            r.h -= row_h;
        }
        remaining -= row_sum;
        start = end;
    }
    out
}

/// Worst aspect ratio of a row whose largest item is `max`, smallest `min`.
fn worst_ratio(max: f64, min: f64, sum: f64, side: f64, scale: f64) -> f64 {
    let s2 = (sum * scale) * (sum * scale);
    let w2 = side * side;
    if s2 <= 0.0 || w2 <= 0.0 {
        return f64::INFINITY;
    }
    let a = w2 * max * scale / s2;
    let b = s2 / (w2 * min * scale);
    a.max(b)
}

fn add_ridge(r: RectF, s: &mut [f32; 4], h: f32) {
    let h4 = 4.0 * h;
    if r.w > 0.0 {
        let wf = h4 / r.w;
        s[2] += wf * (r.x + r.x + r.w);
        s[0] -= wf;
    }
    if r.h > 0.0 {
        let hf = h4 / r.h;
        s[3] += hf * (r.y + r.y + r.h);
        s[1] -= hf;
    }
}

const BAND_ROWS: usize = 16;

/// Paint one leaf into `buf`, which holds rows `band_y0..band_y1` only.
fn draw_cushion(
    buf: &mut [u8],
    width: u32,
    band_y0: u32,
    band_y1: u32,
    r: RectF,
    s: &[f32; 4],
    color: u32,
) {
    let x0 = r.x.round().max(0.0) as u32;
    let y0 = (r.y.round().max(0.0) as u32).max(band_y0);
    let x1 = ((r.x + r.w).round() as u32).min(width);
    let y1 = ((r.y + r.h).round() as u32).min(band_y1);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let len = (LIGHT.0 * LIGHT.0 + LIGHT.1 * LIGHT.1 + LIGHT.2 * LIGHT.2).sqrt();
    let (lx, ly, lz) = (LIGHT.0 / len, LIGHT.1 / len, LIGHT.2 / len);
    let gain = BRIGHTNESS / PALETTE_BRIGHTNESS;
    // out = c * gain * (AMBIENT + diffuse * max(cos, 0)), split into a
    // constant and a per-pixel term with the colour folded in.
    let rgb = [color & 0xff, (color >> 8) & 0xff, (color >> 16) & 0xff].map(|c| c as f32 * gain);
    let base = rgb.map(|c| c * AMBIENT);
    let diff = rgb.map(|c| c * (1.0 - AMBIENT));
    // Surface normal x is linear in the column: nx = ax * ix + bx.
    let ax = -2.0 * s[0];
    let bx = -(s[0] + s[2]) + ax * x0 as f32;
    for iy in y0..y1 {
        let ny = -(2.0 * s[1] * (iy as f32 + 0.5) + s[3]);
        let row_dot = ny * ly + lz;
        let row_len = ny * ny + 1.0;
        let start = (((iy - band_y0) * width + x0) * 4) as usize;
        let end = start + ((x1 - x0) * 4) as usize;
        for (i, px) in buf[start..end].chunks_exact_mut(4).enumerate() {
            let nx = ax * i as f32 + bx;
            let cosa = ((nx * lx + row_dot) / (nx * nx + row_len).sqrt()).clamp(0.0, 1.0);
            // BGRA, as gpui's RenderImage expects.
            px[0] = (base[0] + diff[0] * cosa).min(255.0) as u8;
            px[1] = (base[1] + diff[1] * cosa).min(255.0) as u8;
            px[2] = (base[2] + diff[2] * cosa).min(255.0) as u8;
            px[3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_covers_rect_exactly() {
        let items = vec![(1, 600u64), (2, 600), (3, 400), (4, 300), (5, 200), (6, 100), (7, 100)];
        let r = RectF { x: 0.0, y: 0.0, w: 600.0, h: 400.0 };
        let out = squarify(&items, r);
        assert_eq!(out.len(), items.len());
        let area: f32 = out.iter().map(|(_, r)| r.w * r.h).sum();
        assert!((area - 240_000.0).abs() < 1.0, "area {}", area);
        let total: u64 = items.iter().map(|i| i.1).sum();
        for ((_, v), (_, rr)) in items.iter().zip(out.iter()) {
            let expect = 240_000.0 * *v as f32 / total as f32;
            assert!((rr.w * rr.h - expect).abs() < 1.0);
            assert!(rr.x >= -0.01 && rr.y >= -0.01);
            assert!(rr.x + rr.w <= 600.01 && rr.y + rr.h <= 400.01);
        }
    }
}
