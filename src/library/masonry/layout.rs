//! Pure layout math for the justified-row masonry grid.
//!
//! `pack_rows` greedily fills rows up to `canvas_w` then scales each row to
//! fit. Includes `O(log n)` row lookup helpers used by the snapshot + hit-test
//! paths.

const FALLBACK_W: f32 = 4.0;
const FALLBACK_H: f32 = 3.0;

pub(crate) const MIN_ROW_HEIGHT_NARROW: f32 = 120.0;
pub(crate) const MAX_ROW_HEIGHT_NARROW: f32 = 240.0;
pub(crate) const MIN_ROW_HEIGHT_WIDE: f32 = 180.0;
pub(crate) const MAX_ROW_HEIGHT_WIDE: f32 = 360.0;

pub(crate) const GAP: f32 = 0.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaidItem {
    pub asset_index: u32,
    pub x: f32,
    pub w: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowKind {
    Tiles,
    DateHeader { label: String },
}

impl Default for RowKind {
    fn default() -> Self {
        Self::Tiles
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LaidRow {
    pub y: f32,
    pub h: f32,
    pub items: Vec<LaidItem>,
    pub kind: RowKind,
}

impl LaidRow {
    pub fn tiles(y: f32, h: f32, items: Vec<LaidItem>) -> Self {
        Self { y, h, items, kind: RowKind::Tiles }
    }

    pub fn header(y: f32, h: f32, label: String) -> Self {
        Self { y, h, items: vec![], kind: RowKind::DateHeader { label } }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutConfig {
    pub min_row_height: f32,
    pub max_row_height: f32,
    pub gap: f32,
}

impl LayoutConfig {
    pub(crate) fn narrow() -> Self {
        Self {
            min_row_height: MIN_ROW_HEIGHT_NARROW,
            max_row_height: MAX_ROW_HEIGHT_NARROW,
            gap: GAP,
        }
    }

    pub(crate) fn wide() -> Self {
        Self {
            min_row_height: MIN_ROW_HEIGHT_WIDE,
            max_row_height: MAX_ROW_HEIGHT_WIDE,
            gap: GAP,
        }
    }
}

fn aspect(width: u32, height: u32) -> f32 {
    if width == 0 || height == 0 {
        FALLBACK_W / FALLBACK_H
    } else {
        (width as f32) / (height as f32)
    }
}

/// Greedy justified-row pack. `dims[i] = (w, h)` for asset i.
pub(crate) fn pack_rows(
    dims: &[(u32, u32)],
    canvas_w: f32,
    cfg: LayoutConfig,
) -> (Vec<LaidRow>, f32) {
    if dims.is_empty() || canvas_w <= 0.0 {
        return (Vec::new(), 0.0);
    }

    let mut rows: Vec<LaidRow> = Vec::new();
    let mut y_cursor = 0.0_f32;
    let mut i = 0_usize;

    while i < dims.len() {
        let (indices, next_i) = collect_row_indices(dims, i, canvas_w, cfg);
        i = next_i;

        let last_row = i >= dims.len();
        let row = build_row(dims, indices, canvas_w, y_cursor, last_row, cfg);
        y_cursor += row.h + cfg.gap;
        rows.push(row);
    }

    let total_height = (y_cursor - cfg.gap).max(0.0);
    (rows, total_height)
}

/// Greedily collect item indices that fit into a single row at max height.
fn collect_row_indices(
    dims: &[(u32, u32)],
    start: usize,
    canvas_w: f32,
    cfg: LayoutConfig,
) -> (Vec<usize>, usize) {
    let mut indices: Vec<usize> = Vec::new();
    let mut summed_w = 0.0_f32;
    let mut i = start;
    while i < dims.len() {
        let w_at_max = aspect(dims[i].0, dims[i].1) * cfg.max_row_height;
        let gap_before = if indices.is_empty() { 0.0 } else { cfg.gap };
        if !indices.is_empty() && summed_w + gap_before + w_at_max > canvas_w {
            break;
        }
        indices.push(i);
        summed_w += w_at_max + gap_before;
        i += 1;
    }
    (indices, i)
}

/// Scale, clamp, and place items into a single laid-out row.
fn build_row(
    dims: &[(u32, u32)],
    mut indices: Vec<usize>,
    canvas_w: f32,
    y: f32,
    last_row: bool,
    cfg: LayoutConfig,
) -> LaidRow {
    let mut row_h = scale_to_fit(&indices, dims, canvas_w, cfg);

    // Pop the trailing item if the row is too short -- it spills to the next row
    // (handled by the caller via the returned next index).
    if indices.len() > 1 && row_h < cfg.min_row_height {
        indices.pop();
        row_h = scale_to_fit(&indices, dims, canvas_w, cfg);
    }

    if last_row {
        row_h = row_h.clamp(cfg.min_row_height, cfg.max_row_height);
    }

    let mut placed = Vec::with_capacity(indices.len());
    let mut x_cursor = 0.0_f32;
    for &idx in &indices {
        let w = aspect(dims[idx].0, dims[idx].1) * row_h;
        placed.push(LaidItem {
            asset_index: idx as u32,
            x: x_cursor,
            w,
        });
        x_cursor += w + cfg.gap;
    }

    LaidRow::tiles(y, row_h, placed)
}

fn scale_to_fit(indices: &[usize], dims: &[(u32, u32)], canvas_w: f32, cfg: LayoutConfig) -> f32 {
    let total_gap = if indices.len() > 1 {
        cfg.gap * (indices.len() as f32 - 1.0)
    } else {
        0.0
    };
    let sum: f32 = indices
        .iter()
        .map(|&idx| aspect(dims[idx].0, dims[idx].1) * cfg.max_row_height)
        .sum();
    if sum <= 0.0 {
        return cfg.max_row_height;
    }
    let scale = ((canvas_w - total_gap) / sum).max(0.0);
    cfg.max_row_height * scale
}

const HEADER_H: f32 = 40.0;
pub(crate) const SQUARE_GAP: f32 = 4.0;

/// English ordinal suffix for a day-of-month (1 → "st", 2 → "nd", …, 11 → "th").
fn ordinal_suffix(day: u32) -> &'static str {
    match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

/// Format an ISO8601 `created_at` timestamp into an Immich-style day header,
/// e.g. "Monday, July 19th 2026". Falls back to the raw string (or "Unknown"
/// when empty) if the value can't be parsed as a date.
///
/// Only the calendar date is used — no timezone conversion — because grouping
/// is keyed on the stored `YYYY-MM-DD` prefix and the header must match the
/// group boundary exactly.
fn format_day_header(created_at: &str) -> String {
    use chrono::{Datelike, NaiveDate};
    // The date portion is the first 10 chars (YYYY-MM-DD) of the ISO string.
    let date_part = created_at.get(..10).unwrap_or(created_at);
    if let Ok(date) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
        let weekday = date.format("%A"); // e.g. "Monday"
        let month = date.format("%B"); // e.g. "July"
        let day = date.day();
        return format!(
            "{}, {} {}{} {}",
            weekday,
            month,
            day,
            ordinal_suffix(day),
            date.year()
        );
    }
    if created_at.trim().is_empty() {
        "Unknown".to_string()
    } else {
        created_at.to_string()
    }
}

/// Square-grid layout grouped by day.
///
/// `asset_dates` is a slice of `(model_index, created_at_iso8601)` pairs.
/// Groups consecutive runs of the same day (YYYY-MM-DD prefix), inserts a
/// date-header row before each group, then places tiles in a fixed-column grid.
pub(crate) fn pack_grid_squares_grouped(
    asset_dates: &[(u32, String)],
    total_width: f32,
    columns: usize,
    gap: f32,
) -> (Vec<LaidRow>, f32) {
    if asset_dates.is_empty() || total_width <= 0.0 || columns == 0 {
        return (vec![], 0.0);
    }
    let cols = columns as f32;
    let tile = ((total_width - gap * (cols - 1.0)) / cols).max(1.0);

    // Group consecutive entries by their date prefix (first 10 chars =
    // YYYY-MM-DD). The group key stays the raw day for exact boundary
    // matching; `label` is the pretty header ("Monday, July 19th 2026").
    let mut groups: Vec<(String, String, Vec<u32>)> = Vec::new();
    for (idx, created_at) in asset_dates {
        let day = created_at.get(..10).unwrap_or("").to_string();
        if groups.last().map(|(d, _, _)| d != &day).unwrap_or(true) {
            let label = format_day_header(created_at);
            groups.push((day, label, vec![]));
        }
        groups.last_mut().unwrap().2.push(*idx);
    }

    let mut rows = Vec::new();
    let mut y = 0.0_f32;

    for (_day, label, indices) in groups {
        rows.push(LaidRow::header(y, HEADER_H, label));
        y += HEADER_H + gap;

        let n = indices.len() as u32;
        let n_rows = n.div_ceil(columns as u32);
        for r in 0..n_rows {
            let start = (r * columns as u32) as usize;
            let end = ((start as u32 + columns as u32).min(n)) as usize;
            let items = indices[start..end]
                .iter()
                .enumerate()
                .map(|(c, &ai)| LaidItem {
                    asset_index: ai,
                    x: c as f32 * (tile + gap),
                    w: tile,
                })
                .collect();
            rows.push(LaidRow::tiles(y, tile, items));
            y += tile + gap;
        }
        y += gap;
    }

    (rows, y)
}

pub(crate) fn first_row_at_or_after(rows: &[LaidRow], y: f32) -> usize {
    let mut lo = 0;
    let mut hi = rows.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if rows[mid].y + rows[mid].h < y {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

pub(crate) fn row_at_y(rows: &[LaidRow], y: f32) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let mut lo = 0;
    let mut hi = rows.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let r = &rows[mid];
        if y < r.y {
            hi = mid;
        } else if y >= r.y + r.h {
            lo = mid + 1;
        } else {
            return Some(mid);
        }
    }
    None
}

pub(crate) fn item_at_x(row: &LaidRow, x: f32) -> Option<&LaidItem> {
    row.items.iter().find(|it| x >= it.x && x < it.x + it.w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LayoutConfig {
        LayoutConfig {
            min_row_height: 100.0,
            max_row_height: 200.0,
            gap: 0.0,
        }
    }

    #[test]
    fn empty_input_yields_empty_layout() {
        let (rows, h) = pack_rows(&[], 1000.0, cfg());
        assert!(rows.is_empty());
        assert_eq!(h, 0.0);
    }

    #[test]
    fn zero_canvas_width_yields_empty() {
        let (rows, h) = pack_rows(&[(100, 100)], 0.0, cfg());
        assert!(rows.is_empty());
        assert_eq!(h, 0.0);
    }

    #[test]
    fn fallback_aspect_when_dimensions_zero() {
        let (rows, _) = pack_rows(&[(0, 0), (0, 0), (0, 0)], 1200.0, cfg());
        assert_eq!(rows.len(), 1);
        assert!((rows[0].h - 200.0).abs() < 0.01);
    }

    #[test]
    fn full_row_fills_canvas_width_within_a_pixel() {
        let dims = &[(1600, 900), (1600, 900), (1600, 900), (1600, 900)];
        let (rows, _) = pack_rows(dims, 1200.0, cfg());
        let r1 = &rows[0];
        let last = r1.items.last().unwrap();
        let fill = last.x + last.w;
        assert!((fill - 1200.0).abs() < 1.0);
    }

    #[test]
    fn all_items_placed_across_rows() {
        let dims: Vec<(u32, u32)> = (0..8).map(|_| (3000, 1000)).collect();
        let (rows, _) = pack_rows(&dims, 1200.0, cfg());
        let total: usize = rows.iter().map(|r| r.items.len()).sum();
        assert_eq!(total, 8);
    }

    #[test]
    fn last_row_clamped_to_max_height() {
        let mut dims: Vec<(u32, u32)> = (0..6).map(|_| (4000, 3000)).collect();
        dims.push((1000, 1500));
        let (rows, _) = pack_rows(&dims, 1200.0, cfg());
        let last = rows.last().unwrap();
        assert!(last.h <= 200.0 + 0.01);
    }

    #[test]
    fn binary_search_finds_correct_row() {
        let rows = vec![
            LaidRow::tiles(0.0, 100.0, vec![]),
            LaidRow::tiles(100.0, 150.0, vec![]),
            LaidRow::tiles(250.0, 80.0, vec![]),
        ];
        assert_eq!(row_at_y(&rows, 0.0), Some(0));
        assert_eq!(row_at_y(&rows, 100.0), Some(1));
        assert_eq!(row_at_y(&rows, 329.9), Some(2));
        assert_eq!(row_at_y(&rows, 330.0), None);
    }

    #[test]
    fn item_hit_test_within_row() {
        let row = LaidRow::tiles(0.0, 100.0, vec![
            LaidItem { asset_index: 5, x: 0.0, w: 50.0 },
            LaidItem { asset_index: 6, x: 50.0, w: 80.0 },
            LaidItem { asset_index: 7, x: 130.0, w: 40.0 },
        ]);
        assert_eq!(item_at_x(&row, 0.0).map(|i| i.asset_index), Some(5));
        assert_eq!(item_at_x(&row, 50.0).map(|i| i.asset_index), Some(6));
        assert_eq!(item_at_x(&row, 130.0).map(|i| i.asset_index), Some(7));
        assert!(item_at_x(&row, 200.0).is_none());
    }

    #[test]
    fn gap_increases_total_layout_height() {
        let dims = &[(100, 100), (100, 100), (100, 100), (100, 100)];
        let (_, h0) = pack_rows(dims, 200.0, LayoutConfig { gap: 0.0, ..cfg() });
        let (_, h1) = pack_rows(dims, 200.0, LayoutConfig { gap: 10.0, ..cfg() });
        assert!(h1 > h0);
    }

    #[test]
    fn day_header_formats_immich_style() {
        assert_eq!(
            format_day_header("2026-07-19T15:42:00.000Z"),
            "Sunday, July 19th 2026"
        );
        assert_eq!(format_day_header("2026-07-01"), "Wednesday, July 1st 2026");
        assert_eq!(format_day_header("2026-07-02"), "Thursday, July 2nd 2026");
        assert_eq!(format_day_header("2026-07-03"), "Friday, July 3rd 2026");
        assert_eq!(format_day_header("2026-07-11"), "Saturday, July 11th 2026");
        assert_eq!(format_day_header("2026-07-21"), "Tuesday, July 21st 2026");
    }

    #[test]
    fn day_header_falls_back_on_unparseable() {
        assert_eq!(format_day_header("not-a-date"), "not-a-date");
        assert_eq!(format_day_header(""), "Unknown");
    }

    #[test]
    fn first_row_skip_lands_on_intersecting_row() {
        let rows = vec![
            LaidRow::tiles(0.0, 100.0, vec![]),
            LaidRow::tiles(100.0, 100.0, vec![]),
            LaidRow::tiles(200.0, 100.0, vec![]),
        ];
        assert_eq!(first_row_at_or_after(&rows, -50.0), 0);
        assert_eq!(first_row_at_or_after(&rows, 0.0), 0);
        assert_eq!(first_row_at_or_after(&rows, 150.0), 1);
        assert_eq!(first_row_at_or_after(&rows, 250.0), 2);
        assert_eq!(first_row_at_or_after(&rows, 500.0), 3);
    }
}
