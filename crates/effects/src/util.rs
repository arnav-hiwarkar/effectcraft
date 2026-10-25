//! Shared helpers for effect implementations: single-channel float planes (separable box /
//! Gaussian blur, O(1)-per-pixel min/max filters, box means for guided filtering), channel
//! pickers, a small deterministic RNG and per-pixel helpers.

use effectcraft_color::{luminance, rgb_to_hsl};
use effectcraft_keyframe::Value;
use effectcraft_raster::{Image, Px};
use rayon::prelude::*;

/// A single-channel `f32` image (row-major).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn new(w: usize, h: usize) -> Plane {
        Plane { w, h, data: vec![0.0; w * h] }
    }
    /// Build from an image by mapping every (premultiplied) pixel.
    pub fn from_image(img: &Image, f: impl Fn(Px) -> f32 + Sync) -> Plane {
        Plane { w: img.width as usize, h: img.height as usize, data: img.data.par_iter().map(|&p| f(p)).collect() }
    }
    pub fn alpha(img: &Image) -> Plane {
        Plane::from_image(img, |p| p[3])
    }
    /// Straight-colour Rec. 709 luminance (0 where transparent).
    pub fn luma(img: &Image) -> Plane {
        Plane::from_image(img, |p| {
            let (c, _) = unpremul(p);
            luminance(c[0], c[1], c[2])
        })
    }
    #[inline]
    pub fn get(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.w + x]
    }
    #[inline]
    pub fn get_clamped(&self, x: i64, y: i64) -> f32 {
        let x = x.clamp(0, self.w as i64 - 1) as usize;
        let y = y.clamp(0, self.h as i64 - 1) as usize;
        self.data[y * self.w + x]
    }
    /// Bilinear sample at continuous pixel coordinates (centres at +0.5), edges clamped.
    pub fn sample(&self, x: f64, y: f64) -> f32 {
        let fx = x - 0.5;
        let fy = y - 0.5;
        let x0 = fx.floor();
        let y0 = fy.floor();
        let tx = (fx - x0) as f32;
        let ty = (fy - y0) as f32;
        let (x0, y0) = (x0 as i64, y0 as i64);
        let a = self.get_clamped(x0, y0);
        let b = self.get_clamped(x0 + 1, y0);
        let c = self.get_clamped(x0, y0 + 1);
        let d = self.get_clamped(x0 + 1, y0 + 1);
        let top = a + (b - a) * tx;
        let bot = c + (d - c) * tx;
        top + (bot - top) * ty
    }
    pub fn transpose(&self) -> Plane {
        let (w, h) = (self.w, self.h);
        let mut out = Plane::new(h, w);
        out.data.par_chunks_mut(h.max(1)).enumerate().for_each(|(x, col)| {
            for y in 0..h {
                col[y] = self.data[y * w + x];
            }
        });
        out
    }
    /// Apply `f(src_row, dst_row)` to every row in parallel.
    pub fn map_rows(&self, f: impl Fn(&[f32], &mut [f32]) + Sync) -> Plane {
        let mut out = Plane::new(self.w, self.h);
        if self.w == 0 {
            return out;
        }
        out.data.par_chunks_mut(self.w).zip(self.data.par_chunks(self.w)).for_each(|(o, s)| f(s, o));
        out
    }
    /// Element-wise combination with another plane of the same size.
    pub fn zip_map(&self, other: &Plane, f: impl Fn(f32, f32) -> f32 + Sync) -> Plane {
        Plane { w: self.w, h: self.h, data: self.data.par_iter().zip(other.data.par_iter()).map(|(&a, &b)| f(a, b)).collect() }
    }
    pub fn map(&self, f: impl Fn(f32) -> f32 + Sync) -> Plane {
        Plane { w: self.w, h: self.h, data: self.data.par_iter().map(|&a| f(a)).collect() }
    }
}

/// Box mean of radius `r` along each row (window 2r+1, edge pixels repeated).
fn box_row(src: &[f32], dst: &mut [f32], r: usize) {
    let n = src.len();
    if n == 0 {
        return;
    }
    if r == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let get = |i: isize| src[i.clamp(0, n as isize - 1) as usize];
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut acc = 0.0f64;
    for i in -(r as isize)..=(r as isize) {
        acc += get(i) as f64;
    }
    for x in 0..n {
        dst[x] = acc as f32 * norm;
        acc += get(x as isize + r as isize + 1) as f64 - get(x as isize - r as isize) as f64;
    }
}

/// Separable box mean with radii `rx`, `ry` (edges repeated).
pub fn box_plane(p: &Plane, rx: usize, ry: usize) -> Plane {
    let mut cur = if rx > 0 { p.map_rows(|s, d| box_row(s, d, rx)) } else { p.clone() };
    if ry > 0 {
        cur = cur.transpose().map_rows(|s, d| box_row(s, d, ry)).transpose();
    }
    cur
}

/// Box radii for `n` passes approximating a Gaussian of `sigma`.
pub fn box_radii(sigma: f64, n: usize) -> Vec<usize> {
    if sigma <= 0.0 {
        return vec![0; n];
    }
    let w_ideal = (12.0 * sigma * sigma / n as f64 + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i64;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m_ideal = (12.0 * sigma * sigma - (n as i64 * wl * wl) as f64 - 4.0 * n as f64 * wl as f64 - 3.0 * n as f64) / (-4.0 * wl as f64 - 4.0);
    let m = m_ideal.round() as i64;
    (0..n as i64).map(|i| (((if i < m { wl } else { wu }) - 1) / 2).max(0) as usize).collect()
}

/// Approximate Gaussian blur of a plane (3 box passes per axis, edges repeated).
pub fn gauss_plane(p: &Plane, sigma_x: f64, sigma_y: f64) -> Plane {
    let mut cur = p.clone();
    if sigma_x > 0.05 {
        for r in box_radii(sigma_x, 3) {
            cur = cur.map_rows(|s, d| box_row(s, d, r));
        }
    }
    if sigma_y > 0.05 {
        let mut t = cur.transpose();
        for r in box_radii(sigma_y, 3) {
            t = t.map_rows(|s, d| box_row(s, d, r));
        }
        cur = t.transpose();
    }
    cur
}

/// Running min/max over a window of radius `r` (van Herk / Gil-Werman, O(1) per pixel; edges
/// repeated).
pub fn minmax_row(src: &[f32], dst: &mut [f32], r: usize, max: bool) {
    let n = src.len();
    if n == 0 {
        return;
    }
    if r == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let k = 2 * r + 1;
    let len = n + 2 * r;
    let get = |i: usize| src[(i as isize - r as isize).clamp(0, n as isize - 1) as usize];
    let op = |a: f32, b: f32| if max { a.max(b) } else { a.min(b) };
    let mut g = vec![0.0f32; len];
    let mut hh = vec![0.0f32; len];
    for i in 0..len {
        let v = get(i);
        g[i] = if i % k == 0 { v } else { op(g[i - 1], v) };
    }
    for i in (0..len).rev() {
        let v = get(i);
        hh[i] = if i == len - 1 || (i + 1) % k == 0 { v } else { op(hh[i + 1], v) };
    }
    for x in 0..n {
        dst[x] = op(hh[x], g[x + k - 1]);
    }
}

/// Grey-scale erosion (`max = false`) or dilation (`max = true`) with a (2rx+1)×(2ry+1) box.
pub fn morph_plane(p: &Plane, rx: usize, ry: usize, max: bool) -> Plane {
    let mut cur = if rx > 0 { p.map_rows(|s, d| minmax_row(s, d, rx, max)) } else { p.clone() };
    if ry > 0 {
        cur = cur.transpose().map_rows(|s, d| minmax_row(s, d, ry, max)).transpose();
    }
    cur
}

/// Morphology with a fractional radius (linear blend between the two nearest integer radii).
pub fn morph_frac(p: &Plane, radius: f64, max: bool) -> Plane {
    if radius <= 0.0 {
        return p.clone();
    }
    let r0 = radius.floor() as usize;
    let t = (radius - r0 as f64) as f32;
    let a = morph_plane(p, r0, r0, max);
    if t < 1e-4 {
        return a;
    }
    let b = morph_plane(p, r0 + 1, r0 + 1, max);
    a.zip_map(&b, |x, y| x + (y - x) * t)
}

/// He et al. guided filter: smooth `p` while following edges of guide `g` (radius `r`, regulariser `eps`).
pub fn guided_filter(g: &Plane, p: &Plane, r: usize, eps: f32) -> Plane {
    let mean_g = box_plane(g, r, r);
    let mean_p = box_plane(p, r, r);
    let gp = g.zip_map(p, |a, b| a * b);
    let gg = g.map(|a| a * a);
    let corr_gp = box_plane(&gp, r, r);
    let corr_gg = box_plane(&gg, r, r);
    let n = g.data.len();
    let mut a = Plane::new(g.w, g.h);
    let mut b = Plane::new(g.w, g.h);
    a.data.par_iter_mut().zip(b.data.par_iter_mut()).enumerate().for_each(|(i, (av, bv))| {
        let var = corr_gg.data[i] - mean_g.data[i] * mean_g.data[i];
        let cov = corr_gp.data[i] - mean_g.data[i] * mean_p.data[i];
        *av = cov / (var + eps);
        *bv = mean_p.data[i] - *av * mean_g.data[i];
    });
    debug_assert_eq!(n, a.data.len());
    let ma = box_plane(&a, r, r);
    let mb = box_plane(&b, r, r);
    Plane { w: g.w, h: g.h, data: (0..n).into_par_iter().map(|i| ma.data[i] * g.data[i] + mb.data[i]).collect() }
}

/// Split an image into its four premultiplied channel planes.
pub fn split(img: &Image) -> [Plane; 4] {
    [0, 1, 2, 3].map(|c| Plane::from_image(img, |p| p[c]))
}

/// Join four planes into an image.
pub fn join(ch: &[Plane; 4]) -> Image {
    let (w, h) = (ch[0].w, ch[0].h);
    let mut img = Image::new(w as u32, h as u32);
    img.data.par_iter_mut().enumerate().for_each(|(i, px)| {
        *px = [ch[0].data[i], ch[1].data[i], ch[2].data[i], ch[3].data[i]];
    });
    img
}

/// Premultiplied → (straight colour, alpha).
#[inline]
pub fn unpremul(p: Px) -> ([f32; 3], f32) {
    let a = p[3];
    if a > 1e-6 { ([p[0] / a, p[1] / a, p[2] / a], a) } else { ([0.0; 3], a.max(0.0)) }
}

#[inline]
pub fn premul(c: [f32; 3], a: f32) -> Px {
    [c[0] * a, c[1] * a, c[2] * a, a]
}

#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[inline]
pub fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)]
}

#[inline]
pub fn lerp4(a: Px, b: Px, t: f32) -> Px {
    [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t), lerp(a[3], b[3], t)]
}

#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if (e1 - e0).abs() < 1e-9 {
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
pub fn rgb3(c: [f32; 4]) -> [f32; 3] {
    [c[0], c[1], c[2]]
}

/// Channel sources used by Set Channels, Shift Channels, Set Matte, Displacement Map, …
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    Red,
    Green,
    Blue,
    Alpha,
    Luminance,
    Hue,
    Lightness,
    Saturation,
    Full,
    Half,
    Off,
}

/// Value of a channel source for a straight colour `c` with alpha `a`.
#[inline]
pub fn pick(src: Src, c: [f32; 3], a: f32) -> f32 {
    match src {
        Src::Red => c[0],
        Src::Green => c[1],
        Src::Blue => c[2],
        Src::Alpha => a,
        Src::Luminance => luminance(c[0], c[1], c[2]),
        Src::Hue => rgb_to_hsl(c[0], c[1], c[2]).0,
        Src::Lightness => rgb_to_hsl(c[0], c[1], c[2]).2,
        Src::Saturation => rgb_to_hsl(c[0], c[1], c[2]).1,
        Src::Full => 1.0,
        Src::Half => 0.5,
        Src::Off => 0.0,
    }
}

/// The common source list in AE order: Red, Green, Blue, Alpha, Luminance, Hue, Lightness,
/// Saturation, Full, Half, Off.
pub const SRC_ORDER: [Src; 11] =
    [Src::Red, Src::Green, Src::Blue, Src::Alpha, Src::Luminance, Src::Hue, Src::Lightness, Src::Saturation, Src::Full, Src::Half, Src::Off];
pub const SRC_NAMES: [&str; 11] = ["Red", "Green", "Blue", "Alpha", "Luminance", "Hue", "Lightness", "Saturation", "Full", "Half", "Off"];

pub fn src_at(i: u32) -> Src {
    SRC_ORDER[(i as usize).min(SRC_ORDER.len() - 1)]
}

/// Tiny deterministic RNG (xorshift*), for procedural generators.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03 | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    pub fn f(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform in [-1, 1).
    pub fn s(&mut self) -> f64 {
        self.f() * 2.0 - 1.0
    }
}

/// Integer hash → [0, 1).
#[inline]
pub fn hash1(a: u32, b: u32, seed: u32) -> f32 {
    effectcraft_raster::hash_noise(a, b, seed)
}

/// Map every pixel with its coordinates (row-parallel).
pub fn map_xy(img: &mut Image, f: impl Fn(usize, usize, Px) -> Px + Sync) {
    img.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            *px = f(x, y, *px);
        }
    });
}

/// Build a new image of size `w`×`h` from `f(x, y)` (row-parallel).
pub fn gen_image(w: u32, h: u32, f: impl Fn(usize, usize) -> Px + Sync) -> Image {
    let mut img = Image::new(w, h);
    map_xy(&mut img, |x, y, _| f(x, y));
    img
}

/// Replace the alpha of `img` with `new_a`, keeping straight colour. Pixels that were (nearly)
/// transparent take their colour from a small blur of the surroundings.
pub fn set_alpha(img: &mut Image, new_a: &Plane) {
    let needs_fill = img.data.par_iter().zip(new_a.data.par_iter()).any(|(p, &a)| p[3] <= 1e-4 && a > 1e-4);
    let fill = if needs_fill {
        let blurred = effectcraft_raster::gaussian_blur(img, 3.0, 3.0, true);
        Some(blurred)
    } else {
        None
    };
    img.data.par_iter_mut().zip(new_a.data.par_iter()).enumerate().for_each(|(i, (p, &na))| {
        let na = na.clamp(0.0, 1.0);
        let (c, a) = unpremul(*p);
        let c = if a > 1e-4 {
            c
        } else if let Some(f) = &fill {
            unpremul(f.data[i]).0
        } else {
            c
        };
        *p = premul(c, na);
    });
}

/// Normalised kernel-free resampling helper: sample an image with bilinear filtering, either
/// transparent or clamped outside.
#[inline]
pub fn sample(img: &Image, x: f64, y: f64, clamp: bool) -> Px {
    if clamp { img.sample_bilinear_clamped(x, y) } else { img.sample_bilinear(x, y) }
}

/// Inverse-mapped warp: each output pixel samples the source at `f(x, y)` (pixel centres +0.5).
pub fn remap(src: &Image, clamp: bool, f: impl Fn(f64, f64) -> Option<(f64, f64)> + Sync) -> Image {
    let mut out = Image::new(src.width, src.height);
    out.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            *px = match f(x as f64 + 0.5, y as f64 + 0.5) {
                Some((sx, sy)) => sample(src, sx, sy, clamp),
                None => [0.0; 4],
            };
        }
    });
    out
}

/// Layer rectangle in buffer pixels: (x0, y0, width, height).
pub fn layer_rect(ctx: &crate::EffectCtx, b: &crate::Buf) -> (f64, f64, f64, f64) {
    layer_rect_at(ctx, b.offset, b.scale)
}

/// [`layer_rect`] in the pixels of a buffer with `offset` and `scale` (see [`crate::Buf`];
/// GPU buffers): the layer bounds start at effect-space (0, 0).
pub fn layer_rect_at(ctx: &crate::EffectCtx, offset: [f64; 2], scale: f64) -> (f64, f64, f64, f64) {
    (offset[0], offset[1], ctx.layer_size[0] * scale, ctx.layer_size[1] * scale)
}

/// Transparent pixels to add on every side ([`crate::Buf::pad`]) of a buffer `size` pixels big,
/// with `offset` and `scale`, so it covers the whole layer rectangle (at most 4096). A text or
/// shape layer's buffer only surrounds its content, but its layer bounds are comp-sized (see
/// the renderer's `effect_bounds`): an effect that fills the layer, such as CC Tiler, fills
/// those as in After Effects (#659).
pub fn layer_cover_pad(ctx: &crate::EffectCtx, size: [u32; 2], offset: [f64; 2], scale: f64) -> u32 {
    let (x0, y0, w, h) = layer_rect_at(ctx, offset, scale);
    let need = [-x0, -y0, x0 + w - f64::from(size[0]), y0 + h - f64::from(size[1])].into_iter().fold(0.0, f64::max);
    need.ceil().min(4096.0) as u32
}

/// Resample another layer (a layer parameter) into `b`'s pixel grid. With `stretch` the other
/// layer is scaled to this layer's size ("Stretch to Fit"); otherwise it is centred.
pub fn fit_layer(ctx: &crate::EffectCtx, b: &crate::Buf, other: &crate::LayerPixels, stretch: bool) -> Image {
    let ls = ctx.layer_size;
    let os = other.size;
    let (sx, sy) = if stretch && ls[0] > 0.0 && ls[1] > 0.0 { (os[0] / ls[0], os[1] / ls[1]) } else { (1.0, 1.0) };
    let (dx, dy) = if stretch { (0.0, 0.0) } else { ((os[0] - ls[0]) * 0.5, (os[1] - ls[1]) * 0.5) };
    let inv = 1.0 / b.scale.max(1e-9);
    gen_image(b.img.width, b.img.height, |x, y| {
        let px = (x as f64 + 0.5 - b.offset[0]) * inv;
        let py = (y as f64 + 0.5 - b.offset[1]) * inv;
        let qx = px * sx + dx;
        let qy = py * sy + dy;
        other.buf.img.sample_bilinear(qx * other.buf.scale + other.buf.offset[0], qy * other.buf.scale + other.buf.offset[1])
    })
}

/// Resample a buffer into another buffer's pixel grid without stretching its layer space.
/// Used to composite the pre-effect image after earlier effects expand or shift the bounds.
pub fn fit_buffer(source: &crate::Buf, width: u32, height: u32, offset: [f64; 2], scale: f64) -> Image {
    let inv = 1.0 / scale.max(1e-9);
    gen_image(width, height, |x, y| {
        let sx = (x as f64 + 0.5 - offset[0]) * inv * source.scale + source.offset[0];
        let sy = (y as f64 + 0.5 - offset[1]) * inv * source.scale + source.offset[1];
        source.img.sample_bilinear(sx, sy)
    })
}

/// The layer chosen in layer parameter `id` fitted to `b` (see [`fit_layer`]), or a copy of
/// `b`'s own pixels when none is chosen / available (After Effects' "self" behaviour).
pub fn layer_or_self(ctx: &crate::EffectCtx, b: &crate::Buf, id: &str, masks_and_effects: bool, stretch: bool) -> Image {
    match ctx.layer_param(id, masks_and_effects) {
        Some(o) => fit_layer(ctx, b, &o, stretch),
        None => b.img.clone(),
    }
}

/// A stable hash of an effect instance's evaluated parameters, seed, layer size and buffer
/// scale (cache key for simulations).
pub fn params_key(ctx: &crate::EffectCtx, b: &crate::Buf, salt: u64) -> u64 {
    params_key_except(ctx, b, salt, &[])
}

/// [`params_key`] without the parameters whose ids start with one of `skip` (ones that only
/// change how the simulation is viewed, so animating them doesn't restart it).
pub fn params_key_except(ctx: &crate::EffectCtx, b: &crate::Buf, salt: u64, skip: &[&str]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Sorted by parameter id, so the key does not depend on the map's iteration order.
    let mut entries: Vec<(&str, &Value)> =
        ctx.params.values.iter().filter(|(k, _)| !skip.iter().any(|s| k.starts_with(s))).map(|(k, v)| (k.as_str(), v)).collect();
    entries.sort_by_key(|e| e.0);
    for (k, v) in entries {
        k.hash(&mut h);
        hash_value(v, &mut h);
    }
    ctx.seed.hash(&mut h);
    ctx.layer_size[0].to_bits().hash(&mut h);
    ctx.layer_size[1].to_bits().hash(&mut h);
    b.scale.to_bits().hash(&mut h);
    salt.hash(&mut h);
    h.finish()
}

/// Hash a parameter value into `h` field by field, without formatting it to text.
///
/// The variant is hashed first, so a scalar, an enum and a bool never hash alike; numbers go
/// in by their exact bits (so `-0.0` and `0.0` differ, as their `Debug` forms did). A text
/// document is the one exception: it is large and deeply nested, so it keeps the `Debug`
/// fallback.
fn hash_value(v: &Value, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    match v {
        Value::Scalar(x) => {
            0u8.hash(h);
            x.to_bits().hash(h);
        }
        Value::Vec2(v) => {
            1u8.hash(h);
            hash_f64s(v, h);
        }
        Value::Vec3(v) => {
            2u8.hash(h);
            hash_f64s(v, h);
        }
        Value::Color(v) => {
            3u8.hash(h);
            hash_f64s(v, h);
        }
        Value::Bool(b) => {
            4u8.hash(h);
            b.hash(h);
        }
        Value::Enum(i) => {
            5u8.hash(h);
            i.hash(h);
        }
        Value::Path(p) => {
            6u8.hash(h);
            p.vertices.len().hash(h);
            for v in &p.vertices {
                hash_f64s(v, h);
            }
            p.in_tangents.len().hash(h);
            for v in &p.in_tangents {
                hash_f64s(v, h);
            }
            p.out_tangents.len().hash(h);
            for v in &p.out_tangents {
                hash_f64s(v, h);
            }
            p.closed.hash(h);
            p.feather.len().hash(h);
            for f in &p.feather {
                f.segment.hash(h);
                f.t.to_bits().hash(h);
                f.radius.to_bits().hash(h);
                f.tension.to_bits().hash(h);
            }
        }
        Value::Text(t) => {
            7u8.hash(h);
            format!("{t:?}").hash(h);
        }
        Value::Layer(l) => {
            8u8.hash(h);
            match l {
                Some(id) => {
                    1u8.hash(h);
                    id.hash(h);
                }
                None => 0u8.hash(h),
            }
        }
        Value::Gradient(g) => {
            9u8.hash(h);
            g.colors.len().hash(h);
            for (pos, rgba) in &g.colors {
                pos.to_bits().hash(h);
                for c in rgba {
                    c.to_bits().hash(h);
                }
            }
            g.opacities.len().hash(h);
            for (pos, alpha) in &g.opacities {
                pos.to_bits().hash(h);
                alpha.to_bits().hash(h);
            }
        }
        Value::Str(s) => {
            10u8.hash(h);
            s.hash(h);
        }
    }
}

/// Hash a list of numbers by their exact bits.
fn hash_f64s(xs: &[f64], h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    xs.len().hash(h);
    for x in xs {
        x.to_bits().hash(h);
    }
}

/// Checkpointed, deterministic, time-seekable simulation cache.
///
/// A simulation is a state advanced in fixed steps from layer time 0. [`SimCache::run`] returns
/// the state after `target` steps, starting from the nearest cached checkpoint at or before it
/// (so scrubbing forward is incremental and seeking anywhere gives the same answer as
/// simulating from scratch). Entries are keyed by [`params_key`]; the least recently used key
/// is evicted beyond `cap` keys.
pub struct SimCache<S> {
    inner: std::sync::Mutex<Vec<SimEntry<S>>>,
    cap: usize,
}

struct SimEntry<S> {
    key: u64,
    used: u64,
    checkpoints: Vec<(u64, std::sync::Arc<S>)>,
}

/// Checkpoint every this many steps.
pub const SIM_CHECKPOINT: u64 = 15;

impl<S: Clone> SimCache<S> {
    pub const fn new(cap: usize) -> SimCache<S> {
        SimCache { inner: std::sync::Mutex::new(Vec::new()), cap }
    }

    /// State after `target` steps. `init` builds step 0; `step(state, i)` advances from step
    /// `i` to `i + 1`.
    pub fn run(&self, key: u64, target: u64, init: impl FnOnce() -> S, mut step: impl FnMut(&mut S, u64)) -> std::sync::Arc<S> {
        use std::sync::Arc;
        let start = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let tick = g.iter().map(|e| e.used).max().unwrap_or(0) + 1;
            g.iter_mut().find(|e| e.key == key).and_then(|e| {
                e.used = tick;
                e.checkpoints.iter().filter(|(s, _)| *s <= target).max_by_key(|(s, _)| *s).map(|(s, st)| (*s, st.clone()))
            })
        };
        let had_start = start.is_some();
        let (mut i, mut st) = match start {
            Some((s, st)) if s == target => return st,
            Some((s, st)) => (s, (*st).clone()),
            None => (0, init()),
        };
        let mut fresh: Vec<(u64, Arc<S>)> = Vec::new();
        if !had_start {
            fresh.push((0, Arc::new(st.clone())));
        }
        while i < target {
            step(&mut st, i);
            i += 1;
            if i % SIM_CHECKPOINT == 0 && i != target {
                fresh.push((i, Arc::new(st.clone())));
            }
        }
        let out = Arc::new(st);
        fresh.push((target, out.clone()));
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tick = g.iter().map(|e| e.used).max().unwrap_or(0) + 1;
        let idx = match g.iter().position(|e| e.key == key) {
            Some(ix) => ix,
            None => {
                if g.len() >= self.cap.max(1) {
                    let old = g.iter().enumerate().min_by_key(|(_, e)| e.used).map(|(ix, _)| ix).unwrap_or(0);
                    g.remove(old);
                }
                g.push(SimEntry { key, used: tick, checkpoints: Vec::new() });
                g.len() - 1
            }
        };
        let e = &mut g[idx];
        e.used = tick;
        for (s, st) in fresh {
            if !e.checkpoints.iter().any(|(c, _)| *c == s) {
                e.checkpoints.push((s, st));
            }
        }
        // Keep memory bounded: drop unaligned (exact-target) checkpoints first, then thin.
        if e.checkpoints.len() > 256 {
            e.checkpoints.retain(|(s, _)| s % SIM_CHECKPOINT == 0);
            e.checkpoints.sort_by_key(|(s, _)| *s);
            let mut k = 0;
            e.checkpoints.retain(|_| {
                k += 1;
                k % 2 == 1
            });
        }
        out
    }
}

/// 1D squared-distance transform of sampled function `f` (Felzenszwalb & Huttenlocher 2012;
/// `f` is 0 at sites and +∞ elsewhere).
fn sq_dt_1d(f: &[f64], out: &mut [f64]) {
    let n = f.len();
    let sites: Vec<usize> = (0..n).filter(|&i| f[i].is_finite()).collect();
    if sites.is_empty() {
        out.iter_mut().for_each(|o| *o = f64::INFINITY);
        return;
    }
    let mut v = vec![0usize; sites.len()];
    let mut z = vec![0f64; sites.len() + 1];
    let mut k = 0usize;
    v[0] = sites[0];
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    let s_of = |q: usize, p: usize| ((f[q] + (q * q) as f64) - (f[p] + (p * p) as f64)) / (2.0 * q as f64 - 2.0 * p as f64);
    for &q in &sites[1..] {
        let mut s = s_of(q, v[k]);
        while s <= z[k] {
            k -= 1;
            s = s_of(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, o) in out.iter_mut().enumerate() {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let d = q as f64 - v[k] as f64;
        *o = d * d + f[v[k]];
    }
}

/// Euclidean distance from every pixel to the nearest `true` pixel of `mask` (`w`×`h`),
/// infinite when there is none.
pub fn distance_transform(mask: &[bool], w: usize, h: usize) -> Vec<f32> {
    let mut g: Vec<f64> = mask.iter().map(|&m| if m { 0.0 } else { f64::INFINITY }).collect();
    // Columns, then rows.
    let cols: Vec<Vec<f64>> = (0..w)
        .into_par_iter()
        .map(|x| {
            let f: Vec<f64> = (0..h).map(|y| g[y * w + x]).collect();
            let mut o = vec![0.0; h];
            sq_dt_1d(&f, &mut o);
            o
        })
        .collect();
    for (x, col) in cols.iter().enumerate() {
        for y in 0..h {
            g[y * w + x] = col[y];
        }
    }
    g.par_chunks_mut(w.max(1)).for_each(|row| {
        let f = row.to_vec();
        sq_dt_1d(&f, row);
    });
    g.iter().map(|&d| d.sqrt() as f32).collect()
}

/// Signed distance to the boundary of the `inside` region: negative inside, positive outside
/// (pixel centres; ±0.5 at the boundary).
pub fn signed_distance(inside: &[bool], w: usize, h: usize) -> Plane {
    let outside: Vec<bool> = inside.iter().map(|v| !v).collect();
    let to_in = distance_transform(inside, w, h);
    let to_out = distance_transform(&outside, w, h);
    Plane { w, h, data: to_in.iter().zip(&to_out).map(|(a, b)| if *a > 0.0 { a - 0.5 } else { -(b - 0.5) }).collect() }
}

/// Point-in-polygon (even-odd) for a closed polyline.
pub fn point_in_poly(pts: &[[f64; 2]], x: f64, y: f64) -> bool {
    let n = pts.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (pts[i], pts[j]);
        if (a[1] > y) != (b[1] > y) && x < (b[0] - a[0]) * (y - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Distance from a point to a polyline (closed adds the last→first segment).
pub fn dist_to_poly(pts: &[[f64; 2]], closed: bool, x: f64, y: f64) -> f64 {
    let n = pts.len();
    if n == 0 {
        return f64::INFINITY;
    }
    if n == 1 {
        return ((pts[0][0] - x).powi(2) + (pts[0][1] - y).powi(2)).sqrt();
    }
    let segs = if closed { n } else { n - 1 };
    let mut best = f64::INFINITY;
    for i in 0..segs {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let l2 = dx * dx + dy * dy;
        let t = if l2 > 0.0 { (((x - a[0]) * dx + (y - a[1]) * dy) / l2).clamp(0.0, 1.0) } else { 0.0 };
        let (px, py) = (a[0] + dx * t, a[1] + dy * t);
        best = best.min((px - x).powi(2) + (py - y).powi(2));
    }
    best.sqrt()
}

/// Total length of a polyline.
pub fn poly_length(pts: &[[f64; 2]], closed: bool) -> f64 {
    let n = pts.len();
    if n < 2 {
        return 0.0;
    }
    let segs = if closed { n } else { n - 1 };
    (0..segs)
        .map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % n]);
            ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
        })
        .sum()
}

/// Point and unit tangent at arc length `s` along a polyline.
pub fn poly_point_at(pts: &[[f64; 2]], closed: bool, s: f64) -> ([f64; 2], [f64; 2]) {
    let n = pts.len();
    if n == 0 {
        return ([0.0; 2], [1.0, 0.0]);
    }
    if n == 1 {
        return (pts[0], [1.0, 0.0]);
    }
    let segs = if closed { n } else { n - 1 };
    let mut rem = s.max(0.0);
    for i in 0..segs {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        let l = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        if rem <= l || i == segs - 1 {
            let t = if l > 0.0 { (rem / l).min(1.0) } else { 0.0 };
            let d = if l > 0.0 { [(b[0] - a[0]) / l, (b[1] - a[1]) / l] } else { [1.0, 0.0] };
            return ([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t], d);
        }
        rem -= l;
    }
    (pts[n - 1], [1.0, 0.0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_cache_is_seek_consistent() {
        static C: SimCache<u64> = SimCache::new(2);
        let step = |s: &mut u64, i: u64| *s = s.wrapping_mul(6364136223846793005).wrapping_add(i);
        let direct = *C.run(1, 100, || 7, step);
        let fresh: SimCache<u64> = SimCache::new(2);
        let a = *fresh.run(1, 40, || 7, step);
        let b = *fresh.run(1, 100, || 7, step);
        let c = *fresh.run(1, 40, || 7, step);
        let d = *fresh.run(1, 77, || 7, step);
        let e = *C.run(1, 77, || 7, step);
        assert_eq!(b, direct);
        assert_eq!(a, c);
        assert_eq!(d, e);
    }

    #[test]
    fn poly_helpers() {
        let sq = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        assert!(point_in_poly(&sq, 5.0, 5.0));
        assert!(!point_in_poly(&sq, 15.0, 5.0));
        assert!((poly_length(&sq, true) - 40.0).abs() < 1e-9);
        assert!((dist_to_poly(&sq, true, 5.0, 5.0) - 5.0).abs() < 1e-9);
        let (pt, _) = poly_point_at(&sq, true, 15.0);
        assert!((pt[0] - 10.0).abs() < 1e-9 && (pt[1] - 5.0).abs() < 1e-9);
    }

    #[test]
    fn minmax_matches_brute_force() {
        let src: Vec<f32> = (0..37).map(|i| ((i * 7919) % 31) as f32).collect();
        for r in 0..5 {
            let mut d = vec![0.0; src.len()];
            minmax_row(&src, &mut d, r, false);
            for x in 0..src.len() {
                let lo = x.saturating_sub(r);
                let hi = (x + r).min(src.len() - 1);
                let m = src[lo..=hi].iter().cloned().fold(f32::INFINITY, f32::min);
                assert_eq!(d[x], m, "r={r} x={x}");
            }
        }
    }

    #[test]
    fn gauss_plane_preserves_constant() {
        let p = Plane { w: 20, h: 10, data: vec![0.25; 200] };
        let g = gauss_plane(&p, 3.0, 2.0);
        assert!(g.data.iter().all(|v| (v - 0.25).abs() < 1e-5));
    }

    /// The simulation cache key: stable across calls and map order, sensitive to every kind of
    /// value, blind to the parameters it is told to skip.
    #[test]
    fn params_key_stable_and_sensitive() {
        use effectcraft_keyframe::{FeatherPoint, Gradient, ShapePath, TextDoc};

        let text = |t: &str| {
            let d = TextDoc { text: t.to_string(), ..Default::default() };
            Value::Text(Box::new(d))
        };
        let path = |dx: f64| {
            let mut p = ShapePath::rect([10.0, 20.0], 30.0, 40.0);
            if let Some(v) = p.vertices.first_mut() {
                v[0] += dx;
            }
            p.feather.push(FeatherPoint { segment: 0, t: 0.5, radius: 2.0, tension: 1.0 });
            Value::Path(p)
        };
        let gradient = |shift: f64| {
            let mut g = Gradient::default();
            if let Some(stop) = g.colors.first_mut() {
                stop.0 += shift;
            }
            Value::Gradient(g)
        };

        let base: Vec<(&str, Value)> = vec![
            ("scalar", Value::Scalar(1.5)),
            ("vec2", Value::Vec2([1.0, 2.0])),
            ("vec3", Value::Vec3([1.0, 2.0, 3.0])),
            ("color", Value::Color([0.1, 0.2, 0.3, 1.0])),
            ("bool", Value::Bool(true)),
            ("enum", Value::Enum(3)),
            ("path", path(0.0)),
            ("text", text("hello")),
            ("layer", Value::Layer(Some(7))),
            ("gradient", gradient(0.0)),
            ("str", Value::Str("abc".to_string())),
        ];

        let key = |values: &[(&str, Value)], skip: &[&str]| -> u64 {
            let params = crate::Params { values: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() };
            let ctx =
                crate::EffectCtx { params: &params, time: 0.0, layer_size: [1920.0, 1080.0], seed: 7, adjustment: false, env: crate::EffectEnv::default() };
            let buf = crate::Buf { img: crate::Image::new(4, 4), offset: [0.0, 0.0], scale: 1.0 };
            params_key_except(&ctx, &buf, 3, skip)
        };

        let base_key = key(&base, &[]);
        // Stable across calls, and independent of the order the parameters went into the map.
        assert_eq!(base_key, key(&base, &[]));
        let mut reversed = base.clone();
        reversed.reverse();
        assert_eq!(base_key, key(&reversed, &[]));

        // Changing any one parameter changes the key.
        let changes: Vec<(&str, Value)> = vec![
            ("scalar", Value::Scalar(1.6)),
            ("vec2", Value::Vec2([1.0, 2.5])),
            ("vec3", Value::Vec3([1.0, 2.0, 3.5])),
            ("color", Value::Color([0.1, 0.2, 0.35, 1.0])),
            ("bool", Value::Bool(false)),
            ("enum", Value::Enum(4)),
            ("path", path(0.25)),
            ("text", text("hellp")),
            ("layer", Value::Layer(Some(8))),
            ("gradient", gradient(0.25)),
            ("str", Value::Str("abd".to_string())),
        ];
        for (id, changed) in &changes {
            let mut variant = base.clone();
            for (k, v) in variant.iter_mut() {
                if *k == *id {
                    *v = changed.clone();
                }
            }
            assert_ne!(key(&variant, &[]), base_key, "{id} change did not change the key");
        }

        // Different kinds never collide, even when they hold the same number.
        let one = |v: Value| key(&[("p", v)], &[]);
        assert_ne!(one(Value::Scalar(1.0)), one(Value::Enum(1)));
        assert_ne!(one(Value::Scalar(1.0)), one(Value::Bool(true)));
        assert_ne!(one(Value::Vec2([1.0, 0.0])), one(Value::Vec3([1.0, 0.0, 0.0])));
        assert_ne!(one(Value::Layer(None)), one(Value::Layer(Some(0))));

        // Skipped parameters are ignored, whatever they hold.
        let mut skipped = base.clone();
        skipped.push(("skipMe", Value::Scalar(99.0)));
        assert_eq!(key(&skipped, &["skip"]), base_key);
        let mut skipped2 = base.clone();
        skipped2.push(("skipMe", Value::Scalar(-99.0)));
        assert_eq!(key(&skipped2, &["skip"]), base_key);

        // -0.0 and 0.0 hash apart (as their Debug forms did): no panic, and no silent
        // collision — at worst a cache miss.
        assert_ne!(key(&[("p", Value::Scalar(-0.0))], &[]), key(&[("p", Value::Scalar(0.0))], &[]));
    }

    /// Timed measurement of [`params_key_except`]: 100,000 calls on a ~60-parameter set and on
    /// one with a 2,000-point path. Run with `--release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn params_key_bench() {
        use effectcraft_keyframe::ShapePath;

        let names: Vec<String> = (0..60).map(|i| format!("p{i}")).collect();
        let typical: Vec<(&str, Value)> = names.iter().map(|n| n.as_str()).zip((0..60).map(|i| Value::Scalar(f64::from(i) * 0.5))).collect();
        let pts: Vec<[f64; 2]> = (0..2000).map(|i| [f64::from(i % 97), f64::from(i % 53)]).collect();
        let mut with_path = typical.clone();
        with_path.push(("path", Value::Path(ShapePath::polygon(&pts, true))));

        let measure = |values: &[(&str, Value)], label: &str| {
            let params = crate::Params { values: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() };
            let ctx =
                crate::EffectCtx { params: &params, time: 0.0, layer_size: [1920.0, 1080.0], seed: 7, adjustment: false, env: crate::EffectEnv::default() };
            let buf = crate::Buf { img: crate::Image::new(4, 4), offset: [0.0, 0.0], scale: 1.0 };
            let start = std::time::Instant::now();
            let mut acc = 0u64;
            for _ in 0..100_000 {
                acc ^= params_key_except(&ctx, &buf, 3, &[]);
            }
            let ns = start.elapsed().as_nanos() as f64 / 100_000.0;
            println!("{label}: {ns:.0} ns/call (acc {acc})");
        };
        measure(&typical, "60 scalars");
        measure(&with_path, "60 scalars + 2000-point path");
    }

    #[test]
    fn guided_filter_keeps_constant() {
        let g = Plane { w: 16, h: 16, data: (0..256).map(|i| (i % 16) as f32 / 16.0).collect() };
        let p = Plane { w: 16, h: 16, data: vec![0.7; 256] };
        let o = guided_filter(&g, &p, 2, 1e-3);
        assert!(o.data.iter().all(|v| (v - 0.7).abs() < 1e-3));
    }
}
