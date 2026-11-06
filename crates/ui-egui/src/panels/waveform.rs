//! Audio waveforms for the Timeline's Audio > Waveform property (revealed with `LL`).
//!
//! Each footage item's audio is summarised once into min/max peaks
//! (`effectcraft_render::audio::footage_peaks`, [`BINS_PER_SEC`] bins per second) on a
//! background thread; the timeline draws from that summary at any zoom.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use effectcraft_engine::project::{ItemId, ItemKind, Layer, LayerSource, Project, Property};
use effectcraft_engine::render::audio::db_to_gain;
use effectcraft_engine::time::Tick;
use egui::{Color32, Painter, Rect, Stroke, pos2};

use super::timeline::TMap;
use crate::EffectcraftApp;

/// Peak-summary resolution.
pub const BINS_PER_SEC: u32 = 200;

/// A footage item's peak summary: `[min L, max L, min R, max R]` per bin.
pub struct Summary {
    pub bins_per_sec: u32,
    pub peaks: Vec<[f32; 4]>,
    /// Min/max pyramid: `levels[j][i]` aggregates entries `i * 8..(i + 1) * 8` of the level
    /// below it (the base peaks below `j == 0`), so `levels[j][i]` spans `8^(j + 1)` bins.
    /// `range` only ever reads fully covered entries, so coarser levels are exact too.
    levels: Vec<Vec<[f32; 4]>>,
}

/// Folds a bin or pyramid entry into `[min L, max L, min R, max R]` from the zero baseline.
fn fold(agg: &mut [f32; 4], b: &[f32; 4]) {
    agg[0] = agg[0].min(b[0]);
    agg[1] = agg[1].max(b[1]);
    agg[2] = agg[2].min(b[2]);
    agg[3] = agg[3].max(b[3]);
}

impl Summary {
    /// Builds the summary and its min/max pyramid (about +14% memory on the peaks).
    pub fn new(bins_per_sec: u32, peaks: Vec<[f32; 4]>) -> Self {
        let mut levels: Vec<Vec<[f32; 4]>> = Vec::new();
        let mut len = peaks.len();
        while len > 1 {
            let mut next = Vec::with_capacity(len.div_ceil(8));
            let src: &[[f32; 4]] = match levels.last() {
                Some(l) => l.as_slice(),
                None => &peaks,
            };
            for kids in src.chunks(8) {
                let mut agg = [0.0f32; 4];
                for kid in kids {
                    fold(&mut agg, kid);
                }
                next.push(agg);
            }
            levels.push(next);
            len = levels.last().map_or(0, |l| l.len());
        }
        Self { bins_per_sec, peaks, levels }
    }

    /// Exact min/max fold of bins `[ba, bb)` from the zero baseline, as `draw`'s fold over
    /// `peaks[ba..bb]`, by decomposing bottom-up over the pyramid; `[0.0; 4]` when `ba >= bb`.
    /// At most 14 entry reads per level, so the cost barely grows with the span.
    pub fn range(&self, ba: usize, bb: usize) -> [f32; 4] {
        let mut agg = [0.0f32; 4];
        let n = self.peaks.len();
        let (mut l, mut r) = (ba.min(bb).min(n), bb.min(n));
        for src in std::iter::once(self.peaks.as_slice()).chain(self.levels.iter().map(Vec::as_slice)) {
            let (nl, nr) = ((l + 7) & !7, r & !7);
            if nl >= nr {
                // The range fits inside one 8-aligned block at this level: fold it and stop.
                if let Some(mid) = src.get(l..r) {
                    for a in mid {
                        fold(&mut agg, a);
                    }
                }
                return agg;
            }
            // Fold the ragged edges at this level and continue with the aligned middle.
            if let Some(edge) = src.get(l..nl) {
                for a in edge {
                    fold(&mut agg, a);
                }
            }
            if let Some(edge) = src.get(nr..r) {
                for a in edge {
                    fold(&mut agg, a);
                }
            }
            l = nl / 8;
            r = nr / 8;
        }
        agg
    }
}

/// Summaries by footage item (`None` while computing).
#[derive(Default)]
pub struct Cache {
    map: Arc<Mutex<HashMap<u64, Option<Arc<Summary>>>>>,
}

/// The footage item whose audio a layer plays, when it has audio.
pub fn audio_item(project: &Project, l: &Layer) -> Option<ItemId> {
    let LayerSource::Footage { item } = &l.source else { return None };
    match &project.item(*item)?.kind {
        ItemKind::Footage(f) if f.has_audio && !f.missing => Some(*item),
        _ => None,
    }
}

/// The summary for `item`, starting its computation on first request.
pub fn summary(app: &EffectcraftApp, ctx: &egui::Context, item: ItemId) -> Option<Arc<Summary>> {
    let mut m = app.waveforms.map.lock().ok()?;
    if let Some(s) = m.get(&item.0) {
        return s.clone();
    }
    let ItemKind::Footage(f) = &app.session.project.item(item)?.kind else { return None };
    m.insert(item.0, None);
    let (f, footage, map, ctx) = (f.clone(), app.session.footage.clone(), app.waveforms.map.clone(), ctx.clone());
    // Settings ▸ Disk ▸ Database and Cache Folder: summaries are kept on disk between sessions.
    let cache = app.session.media_cache_folder();
    std::thread::Builder::new()
        .name("ec-waveform".into())
        .spawn(move || {
            use effectcraft_engine::media_cache;
            let file = cache.filter(|_| !f.missing).and_then(|d| media_cache::peaks_path(&d, &f.path, BINS_PER_SEC));
            let peaks = match file.as_deref().and_then(media_cache::load_peaks) {
                Some(p) => p,
                None => {
                    let p = effectcraft_engine::render::audio::footage_peaks(footage.as_ref(), item, &f, f.duration, BINS_PER_SEC);
                    if let Some(file) = &file
                        && let Err(e) = media_cache::store_peaks(file, &p)
                    {
                        log::warn!("media cache: {}: {e}", file.display());
                    }
                    p
                }
            };
            if let Ok(mut m) = map.lock() {
                m.insert(item.0, Some(Arc::new(Summary::new(BINS_PER_SEC, peaks))));
            }
            ctx.request_repaint();
        })
        .ok()?;
    None
}

/// Left/right gain of the layer's Audio Levels (dB) at comp time `t`; unity without the property.
fn level_gains(levels: Option<&Property>, t: Tick) -> [f32; 2] {
    match levels {
        Some(p) => {
            let [l, r] = p.value_at(t).as_vec2();
            [db_to_gain(l), db_to_gain(r)]
        }
        None => [1.0, 1.0],
    }
}

/// Draw `layer`'s waveform in `rect` (left channel above right), columns mapped through the
/// timeline's time map and the layer's time mapping, limited to its [in, out) span.
pub(crate) fn draw(p: &Painter, rect: Rect, tm: &TMap, layer: &Layer, s: &Summary, color: Color32) {
    let mid = [rect.min.y + rect.height() * 0.25, rect.min.y + rect.height() * 0.75];
    let half = rect.height() * 0.25 - 1.0;
    let x0 = tm.x(layer.in_point.seconds()).max(rect.min.x).floor() as i32;
    let x1 = tm.x(layer.out_point.seconds()).min(rect.max.x).ceil() as i32;
    let n = s.peaks.len() as i64;
    let levels = layer.props.sub("audio").and_then(|g| g.get("levels"));
    for c in 0..2 {
        p.line_segment([pos2(x0 as f32, mid[c]), pos2(x1 as f32, mid[c])], Stroke::new(1.0, color.gamma_multiply(0.5)));
    }
    for x in x0..x1 {
        let (ta, tb) = (tm.t(x as f32), tm.t(x as f32 + 1.0));
        let src = |t: f64| layer.layer_time(Tick::from_seconds_f64(t)).seconds() * s.bins_per_sec as f64;
        let (ba, bb) = (src(ta).floor() as i64, src(tb).ceil() as i64);
        let (ba, bb) = (ba.min(bb).clamp(0, n), ba.max(bb).max(ba + 1).clamp(0, n));
        if ba >= bb {
            continue;
        }
        let agg = s.range(ba as usize, bb as usize);
        let gain = level_gains(levels, Tick::from_seconds_f64((ta + tb) * 0.5));
        for c in 0..2 {
            let (lo, hi) = ((agg[c * 2] * gain[c]).clamp(-1.0, 1.0), (agg[c * 2 + 1] * gain[c]).clamp(-1.0, 1.0));
            let (ya, yb) = (mid[c] - hi * half, mid[c] - lo * half);
            p.line_segment([pos2(x as f32 + 0.5, ya), pos2(x as f32 + 0.5, yb.max(ya + 1.0))], Stroke::new(1.0, color));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::project::Value;

    #[test]
    fn waveform_follows_audio_levels() {
        let p = Property::new(1, "levels", "Audio Levels", Value::Vec2([-6.0206, -96.0]));
        let g = level_gains(Some(&p), Tick::ZERO);
        assert!((g[0] - 0.5).abs() < 1e-3, "{}", g[0]);
        assert_eq!(g[1], 0.0);
        assert_eq!(level_gains(None, Tick::ZERO), [1.0, 1.0]);
    }

    /// Deterministic pseudo-random value in about `[-0.7, 0.7]`.
    fn rnd(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / 2147483647.0) * 1.4 - 0.7
    }

    /// `n` deterministic pseudo-random bins `[min L, max L, min R, max R]`,
    /// with a 2.9-wide spike written into each index in `spikes`.
    fn random_peaks(seed: u64, n: usize, spikes: &[usize]) -> Vec<[f32; 4]> {
        let mut seed = seed;
        let mut peaks = Vec::with_capacity(n);
        for _ in 0..n {
            let (a, b, c, d) = (rnd(&mut seed), rnd(&mut seed), rnd(&mut seed), rnd(&mut seed));
            peaks.push([a.min(b), a.max(b), c.min(d), c.max(d)]);
        }
        for &i in spikes {
            peaks[i] = [0.0, 2.9, 0.0, 2.9];
        }
        peaks
    }

    /// The brute-force fold `draw` used to do per column.
    fn fold_brute(peaks: &[[f32; 4]], ba: usize, bb: usize) -> [f32; 4] {
        let mut agg = [0.0f32; 4];
        for b in &peaks[ba..bb] {
            agg[0] = agg[0].min(b[0]);
            agg[1] = agg[1].max(b[1]);
            agg[2] = agg[2].min(b[2]);
            agg[3] = agg[3].max(b[3]);
        }
        agg
    }

    #[test]
    fn pyramid_range_matches_brute_force() {
        let n = BINS_PER_SEC as usize * 600 + 3; // 10 minutes with an odd tail: 120,003 bins
        let s = Summary::new(BINS_PER_SEC, random_peaks(0x9E3779B97F4A7C15, n, &[3, 511, 4096, 65_000, n - 1]));
        let mut spans = Vec::new();
        for start in [0, 1, 7, 8, 63, 64, 511, 512, 1000, 4095, n / 2, n - 45] {
            for width in 1..=40 {
                spans.push((start, start + width));
            }
        }
        for (ba, bb) in [(0, 1000), (500, 11_500), (7, n), (0, n), (64, n - 64), (n - 1000, n), (n / 2, n / 2 + 10_000)] {
            spans.push((ba, bb));
        }
        for (ba, bb) in spans {
            assert_eq!(s.range(ba, bb), fold_brute(&s.peaks, ba, bb), "range {ba}..{bb}");
        }
    }

    #[test]
    fn pyramid_never_loses_a_spike() {
        let n = BINS_PER_SEC as usize * 600 + 3;
        for i in [0, 7, 8, 63, 64, 4095, 4096, n / 2, n - 2, n - 1] {
            let s = Summary::new(BINS_PER_SEC, random_peaks(0xC0FFEE00, n, &[i]));
            for width in [1, 7, 64, 120, 5000] {
                let (ba, bb) = (i.saturating_sub(width / 2), (i.saturating_sub(width / 2) + width).min(n));
                let agg = s.range(ba, bb);
                // Only the spiked bin reaches 2.9; a coarser fold must not lose it.
                assert_eq!(agg[1], 2.9, "spike {i}, width {width}");
                assert_eq!(agg[3], 2.9, "spike {i}, width {width}");
            }
        }
    }

    #[test]
    fn pyramid_handles_tiny_summaries() {
        for n in [0usize, 1, 7, 8, 9] {
            let s = Summary::new(BINS_PER_SEC, random_peaks(0xABCDEF, n, &[]));
            assert_eq!(s.range(0, n), fold_brute(&s.peaks, 0, n), "n {n}");
            assert_eq!(s.range(2, 2), [0.0; 4], "empty span at n {n}");
            assert_eq!(s.range(n + 1, n + 2), [0.0; 4], "out-of-bounds span at n {n}");
            if n > 1 {
                assert_eq!(s.range(n / 2 + 1, n / 2), [0.0; 4], "reversed span at n {n}");
            }
        }
    }

    #[test]
    #[ignore]
    fn pyramid_aggregate_timing() {
        let n = BINS_PER_SEC as usize * 3600; // 1 h: 720,000 bins
        let s = Summary::new(BINS_PER_SEC, random_peaks(0x5EED1234, n, &[17, 1000, 123_456, n - 1]));
        let cols = 1000;
        let width = n / cols; // 720 bins per column
        let (mut brute, mut pyr) = (Vec::new(), Vec::new());
        for _ in 0..50 {
            let start = std::time::Instant::now();
            for c in 0..cols {
                std::hint::black_box(fold_brute(&s.peaks, c * width, (c + 1) * width));
            }
            brute.push(start.elapsed());
            let start = std::time::Instant::now();
            for c in 0..cols {
                std::hint::black_box(s.range(c * width, (c + 1) * width));
            }
            pyr.push(start.elapsed());
        }
        brute.sort();
        pyr.sort();
        println!("1 h summary, {cols} columns of {width} bins; median ms per repaint of 50\nbrute fold: {:?}\npyramid range: {:?}", brute[25], pyr[25]);
    }
}
