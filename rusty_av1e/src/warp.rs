// prom_av1e052 WARP M2 — local warped motion prediction, ported bit-exact from
// the dav1d reference (rusty_av1d_stock warpmv.rs affine solve + mc.rs
// warp_affine_8x8 + recon.rs warp_affine block loop). The decoder is the oracle;
// this is its forward twin. Gate: rav1e -r recon == dav1d output, byte-for-byte.

use crate::frame::*;
use crate::mc::MotionVector;
use crate::tiling::PlaneRegionMut;
use crate::util::{CastFromPrimitive, Pixel};
use crate::warp_tables::DAV1D_MC_WARP_FILTER;

/// A derived warp model. `valid` == dav1d's `type == Affine` (apply warp);
/// otherwise the block falls back to regular translation MC.
#[derive(Clone, Copy, PartialEq)]
pub struct WarpedMotionParams {
  pub matrix: [i32; 6],
  pub abcd: [i16; 4],
  pub valid: bool,
}

impl Default for WarpedMotionParams {
  fn default() -> Self {
    WarpedMotionParams {
      matrix: [0, 0, 1 << 16, 0, 0, 1 << 16],
      abcd: [0; 4],
      valid: false,
    }
  }
}

impl WarpedMotionParams {
  #[inline]
  pub fn alpha(&self) -> i16 {
    self.abcd[0]
  }
  #[inline]
  pub fn beta(&self) -> i16 {
    self.abcd[1]
  }
  #[inline]
  pub fn gamma(&self) -> i16 {
    self.abcd[2]
  }
  #[inline]
  pub fn delta(&self) -> i16 {
    self.abcd[3]
  }
}

// --- integer helpers (dav1d intops) -----------------------------------------
#[inline]
fn apply_sign(a: i32, b: i32) -> i32 {
  if b < 0 {
    -a
  } else {
    a
  }
}
#[inline]
fn apply_sign64(a: i32, b: i64) -> i32 {
  if b < 0 {
    -a
  } else {
    a
  }
}
#[inline]
fn iclip(v: i32, lo: i32, hi: i32) -> i32 {
  v.clamp(lo, hi)
}
#[inline]
fn ulog2(v: u32) -> i32 {
  31 - v.leading_zeros() as i32
}
#[inline]
fn u64log2(v: u64) -> i32 {
  63 - v.leading_zeros() as i32
}

#[rustfmt::skip]
static DIV_LUT: [u16; 257] = [
  16384, 16320, 16257, 16194, 16132, 16070, 16009, 15948, 15888, 15828, 15768, 15709, 15650,
  15592, 15534, 15477, 15420, 15364, 15308, 15252, 15197, 15142, 15087, 15033, 14980, 14926,
  14873, 14821, 14769, 14717, 14665, 14614, 14564, 14513, 14463, 14413, 14364, 14315, 14266,
  14218, 14170, 14122, 14075, 14028, 13981, 13935, 13888, 13843, 13797, 13752, 13707, 13662,
  13618, 13574, 13530, 13487, 13443, 13400, 13358, 13315, 13273, 13231, 13190, 13148, 13107,
  13066, 13026, 12985, 12945, 12906, 12866, 12827, 12788, 12749, 12710, 12672, 12633, 12596,
  12558, 12520, 12483, 12446, 12409, 12373, 12336, 12300, 12264, 12228, 12193, 12157, 12122,
  12087, 12053, 12018, 11984, 11950, 11916, 11882, 11848, 11815, 11782, 11749, 11716, 11683,
  11651, 11619, 11586, 11555, 11523, 11491, 11460, 11429, 11398, 11367, 11336, 11305, 11275,
  11245, 11215, 11185, 11155, 11125, 11096, 11067, 11038, 11009, 10980, 10951, 10923, 10894,
  10866, 10838, 10810, 10782, 10755, 10727, 10700, 10673, 10645, 10618, 10592, 10565, 10538,
  10512, 10486, 10460, 10434, 10408, 10382, 10356, 10331, 10305, 10280, 10255, 10230, 10205,
  10180, 10156, 10131, 10107, 10082, 10058, 10034, 10010, 9986, 9963, 9939, 9916, 9892, 9869,
  9846, 9823, 9800, 9777, 9754, 9732, 9709, 9687, 9664, 9642, 9620, 9598, 9576, 9554, 9533, 9511,
  9489, 9468, 9447, 9425, 9404, 9383, 9362, 9341, 9321, 9300, 9279, 9259, 9239, 9218, 9198, 9178,
  9158, 9138, 9118, 9098, 9079, 9059, 9039, 9020, 9001, 8981, 8962, 8943, 8924, 8905, 8886, 8867,
  8849, 8830, 8812, 8793, 8775, 8756, 8738, 8720, 8702, 8684, 8666, 8648, 8630, 8613, 8595, 8577,
  8560, 8542, 8525, 8508, 8490, 8473, 8456, 8439, 8422, 8405, 8389, 8372, 8355, 8339, 8322, 8306,
  8289, 8273, 8257, 8240, 8224, 8208, 8192,
];

#[inline]
fn iclip_wmp(v: i32) -> i32 {
  let cv = iclip(v, i16::MIN as i32, i16::MAX as i32);
  apply_sign((cv.abs() + 32) >> 6, cv) * (1 << 6)
}

#[inline]
fn resolve_divisor_32(d: u32) -> (i32, i32) {
  let shift = ulog2(d);
  let e = d - (1 << shift);
  let f = if shift > 8 {
    (e + (1 << (shift - 9))) >> (shift - 8)
  } else {
    e << (8 - shift)
  };
  (shift + 14, DIV_LUT[f as usize] as i32)
}

fn resolve_divisor_64(d: u64) -> (i32, i32) {
  let shift = u64log2(d);
  let e = d - (1 << shift);
  let f = if shift > 8 {
    (e + (1 << (shift - 9))) >> (shift - 8)
  } else {
    e << (8 - shift)
  };
  (shift + 14, DIV_LUT[f as usize] as i32)
}

fn get_mult_shift_ndiag(px: i64, idet: i32, shift: i32) -> i32 {
  let v1 = px * idet as i64;
  let v2 = apply_sign64(((v1.abs() + (1 << shift >> 1)) >> shift) as i32, v1);
  iclip(v2, -0x1fff, 0x1fff)
}

fn get_mult_shift_diag(px: i64, idet: i32, shift: i32) -> i32 {
  let v1 = px * idet as i64;
  let v2 = apply_sign64(((v1.abs() + (1 << shift >> 1)) >> shift) as i32, v1);
  iclip(v2, 0xe001, 0x11fff)
}

/// dav1d rav1d_get_shear_params — fills abcd, returns `true` if the shear is
/// INVALID (block must not warp).
fn get_shear_params(wm: &mut WarpedMotionParams) -> bool {
  let mat = wm.matrix;
  if mat[2] <= 0 {
    return true;
  }
  let alpha = iclip_wmp(mat[2] - 0x10000) as i16;
  let beta = iclip_wmp(mat[3]) as i16;
  let (shift, y) = resolve_divisor_32((mat[2]).unsigned_abs());
  let y = apply_sign(y, mat[2]);
  let v1 = mat[4] as i64 * 0x10000 * y as i64;
  let rnd = (1 << shift) >> 1;
  let gamma =
    iclip_wmp(apply_sign64(((v1.abs() + rnd as i64) >> shift) as i32, v1)) as i16;
  let v2 = mat[3] as i64 * mat[4] as i64 * y as i64;
  let delta = iclip_wmp(
    mat[5] - apply_sign64(((v2.abs() + rnd as i64) >> shift) as i32, v2) - 0x10000,
  ) as i16;
  wm.abcd = [alpha, beta, gamma, delta];

  4 * (alpha as i32).abs() + 7 * (beta as i32).abs() >= 0x10000
    || 4 * (gamma as i32).abs() + 4 * (delta as i32).abs() >= 0x10000
}

/// dav1d rav1d_find_affine_int — least-squares affine solve; returns `true` on
/// degenerate (block must not warp). Fills mat[2..6] then mat[0..2].
#[allow(clippy::too_many_arguments)]
fn find_affine_int(
  pts: &[[[i32; 2]; 2]; 8], np: usize, bw4: i32, bh4: i32, mv: MotionVector,
  wm: &mut WarpedMotionParams, bx4: i32, by4: i32,
) -> bool {
  let mut a = [[0i64; 2]; 2];
  let mut bx = [0i64; 2];
  let mut by = [0i64; 2];
  let rsuy = 2 * bh4 - 1;
  let rsux = 2 * bw4 - 1;
  let suy = rsuy * 8;
  let sux = rsux * 8;
  let duy = suy + mv.row as i32;
  let dux = sux + mv.col as i32;
  let isuy = by4 * 4 + rsuy;
  let isux = bx4 * 4 + rsux;

  for pts in &pts[..np] {
    let dx = pts[1][0] - dux;
    let dy = pts[1][1] - duy;
    let sx = pts[0][0] - sux;
    let sy = pts[0][1] - suy;
    if (sx - dx).abs() < 256 && (sy - dy).abs() < 256 {
      a[0][0] += ((sx * sx >> 2) + sx * 2 + 8) as i64;
      a[0][1] += ((sx * sy >> 2) + sx + sy + 4) as i64;
      a[1][1] += ((sy * sy >> 2) + sy * 2 + 8) as i64;
      bx[0] += ((sx * dx >> 2) + sx + dx + 8) as i64;
      bx[1] += ((sy * dx >> 2) + sy + dx + 4) as i64;
      by[0] += ((sx * dy >> 2) + sx + dy + 4) as i64;
      by[1] += ((sy * dy >> 2) + sy + dy + 8) as i64;
    }
  }

  let det = a[0][0] * a[1][1] - a[0][1] * a[0][1];
  if det == 0 {
    return true;
  }
  let (mut shift, idet) = resolve_divisor_64(det.unsigned_abs());
  let mut idet = apply_sign64(idet, det);
  shift -= 16;
  if shift < 0 {
    idet <<= -shift;
    shift = 0;
  }

  let mat = &mut wm.matrix;
  mat[2] =
    get_mult_shift_diag(a[1][1] * bx[0] - a[0][1] * bx[1], idet, shift);
  mat[3] =
    get_mult_shift_ndiag(a[0][0] * bx[1] - a[0][1] * bx[0], idet, shift);
  mat[4] =
    get_mult_shift_ndiag(a[1][1] * by[0] - a[0][1] * by[1], idet, shift);
  mat[5] =
    get_mult_shift_diag(a[0][0] * by[1] - a[0][1] * by[0], idet, shift);
  mat[0] = iclip(
    mv.col as i32 * 0x2000 - (isux * (mat[2] - 0x10000) + isuy * mat[3]),
    -0x800000,
    0x7fffff,
  );
  mat[1] = iclip(
    mv.row as i32 * 0x2000 - (isux * mat[4] + isuy * (mat[5] - 0x10000)),
    -0x800000,
    0x7fffff,
  );
  false
}

/// Solve the warp model for a block from its collected neighbour-MV `samples`
/// (each = [[in_x, in_y], [out_x, out_y]]). Mirrors dav1d derive_warpmv's tail:
/// filter by MV-diff threshold, then find_affine_int + get_shear_params.
/// Returns the warp params with `valid` == (both solves succeeded).
pub fn solve_warp(
  samples: &[[[i32; 2]; 2]], bw4: i32, bh4: i32, mv: MotionVector, bx4: i32,
  by4: i32,
) -> WarpedMotionParams {
  let mut pts = [[[0i32; 2]; 2]; 8];
  let np0 = samples.len().min(8);
  pts[..np0].copy_from_slice(&samples[..np0]);

  // select according to motion vector difference against a threshold
  let mut mvd = [0i32; 8];
  let mut ret = 0usize;
  let thresh = 4 * iclip(bw4.max(bh4), 4, 28);
  for i in 0..np0 {
    mvd[i] = (pts[i][1][0] - pts[i][0][0] - mv.col as i32).abs()
      + (pts[i][1][1] - pts[i][0][1] - mv.row as i32).abs();
    if mvd[i] > thresh {
      mvd[i] = -1;
    } else {
      ret += 1;
    }
  }
  if ret == 0 {
    ret = 1;
  } else if ret < np0 {
    let mut i = 0usize;
    let mut j = np0 - 1;
    for _ in 0..np0 - ret {
      while mvd[i] != -1 {
        i += 1;
      }
      while mvd[j] == -1 {
        j -= 1;
      }
      if i > j {
        break;
      }
      mvd[i] = mvd[j];
      pts[i] = pts[j];
      i += 1;
      j = j.wrapping_sub(1);
    }
  }

  let mut wm = WarpedMotionParams::default();
  let degenerate = find_affine_int(&pts, ret, bw4, bh4, mv, &mut wm, bx4, by4)
    || get_shear_params(&mut wm);
  wm.valid = !degenerate;
  wm
}

// --- prom_av1e053 T3b: PRE-PASS global shear estimate -------------------------

/// One weighted least-squares affine solve over the kept samples. Coordinates
/// are CENTERED on both sides so the translation term drops out of the normal
/// equations — we only ever want the linear part (the shear), and centering also
/// conditions the system (raw frame coordinates are large and nearly collinear).
/// Returns `[a, b, d, e]` = the Jacobian rows (du/dx, du/dy, dv/dx, dv/dy).
fn solve_affine_ls(
  samples: &[(f32, f32, f32, f32)], keep: &[bool], min_n: usize,
) -> Option<[f64; 4]> {
  let (mut n, mut mx, mut my, mut mu, mut mv) = (0f64, 0f64, 0f64, 0f64, 0f64);
  for (i, &(x, y, dx, dy)) in samples.iter().enumerate() {
    if !keep[i] {
      continue;
    }
    n += 1.0;
    mx += x as f64;
    my += y as f64;
    mu += (x + dx) as f64;
    mv += (y + dy) as f64;
  }
  if n < min_n as f64 {
    return None;
  }
  mx /= n;
  my /= n;
  mu /= n;
  mv /= n;
  let (mut sxx, mut sxy, mut syy) = (0f64, 0f64, 0f64);
  let (mut sxu, mut syu, mut sxv, mut syv) = (0f64, 0f64, 0f64, 0f64);
  for (i, &(x, y, dx, dy)) in samples.iter().enumerate() {
    if !keep[i] {
      continue;
    }
    let (px, py) = (x as f64 - mx, y as f64 - my);
    let (pu, pv) = ((x + dx) as f64 - mu, (y + dy) as f64 - mv);
    sxx += px * px;
    sxy += px * py;
    syy += py * py;
    sxu += px * pu;
    syu += py * pu;
    sxv += px * pv;
    syv += py * pv;
  }
  let det = sxx * syy - sxy * sxy;
  // Degenerate: all samples collinear (or a single row/column of blocks).
  if det.abs() <= 1e-6 * (sxx * syy).abs().max(1.0) {
    return None;
  }
  Some([
    (syy * sxu - sxy * syu) / det,
    (sxx * syu - sxy * sxu) / det,
    (syy * sxv - sxy * syv) / det,
    (sxx * syv - sxy * sxv) / det,
  ])
}

/// prom_av1e053 T3b: fit ONE affine model to a source-domain MV field. Returns
/// `(shear, gain)`:
///
/// * `shear` = max|alpha,beta,gamma,delta| in the same 1/65536 units as the
///   per-block latch, so the two dispatch signals are directly comparable. It
///   answers "is there a motion GRADIENT, and is it big enough to be worth
///   warping?"
/// * `gain` = how much of the field's motion variance the AFFINE terms explain
///   over a pure TRANSLATION model (the linear part's R², in 1/1000). It answers
///   the different and decisive question, "is the motion actually AFFINE?"
///
/// Both are needed, because shear alone does not predict warp's payoff: a camera
/// PAN across a scene with depth produces a genuine motion gradient (parallax +
/// object motion) that least-squares reports as shear — `bus` measured 1226
/// against a rotating clip's 2042, far too narrow a margin to gate on. That
/// gradient is not affine, so the fit explains little of the field and `gain`
/// separates it cleanly where magnitude cannot.
///
/// `samples` are `(x, y, dx, dy)` in PIXELS: a block's centre and its motion
/// displacement. The model therefore maps current-frame → reference-frame coords
/// exactly as `matrix` does, and pure translation fits to the identity → shear 0.
///
/// Robustness beats precision here: plain least squares is dragged by foreground
/// object motion, so the fit runs TWICE — the second pass drops samples whose
/// residual exceeds twice the median, which keeps a rotating BACKGROUND from
/// being averaged into a translation by a handful of moving objects. The matrix
/// is clamped exactly as `find_affine_int`'s `get_mult_shift_{diag,ndiag}` clamp
/// it, so a saturating global fit saturates identically to a per-block one.
pub fn fit_global_shear(
  samples: &[(f32, f32, f32, f32)],
) -> Option<(u64, u32)> {
  const MIN_SAMPLES: usize = 16;
  if samples.len() < MIN_SAMPLES {
    return None;
  }
  let mut keep = vec![true; samples.len()];
  let m1 = solve_affine_ls(samples, &keep, MIN_SAMPLES)?;

  // Residual trim (pass 2). Residuals are measured against pass 1's own model
  // in centered coordinates, so the intercept cancels the same way it did there.
  let (mut n, mut mx, mut my, mut mu, mut mv) = (0f64, 0f64, 0f64, 0f64, 0f64);
  for &(x, y, dx, dy) in samples {
    n += 1.0;
    mx += x as f64;
    my += y as f64;
    mu += (x + dx) as f64;
    mv += (y + dy) as f64;
  }
  mx /= n;
  my /= n;
  mu /= n;
  mv /= n;
  let resid: Vec<f64> = samples
    .iter()
    .map(|&(x, y, dx, dy)| {
      let (px, py) = (x as f64 - mx, y as f64 - my);
      let (pu, pv) = ((x + dx) as f64 - mu, (y + dy) as f64 - mv);
      (pu - (m1[0] * px + m1[1] * py)).abs()
        + (pv - (m1[2] * px + m1[3] * py)).abs()
    })
    .collect();
  let mut sorted = resid.clone();
  sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  let med = sorted[sorted.len() / 2];
  // Absolute floor: on a CLEAN fit the median residual is ~0, and a pure ratio
  // test would then trim almost every sample as an "outlier".
  let thr = (2.0 * med).max(1.0);
  for (i, r) in resid.iter().enumerate() {
    keep[i] = *r <= thr;
  }
  // Keep pass 1 if the trim left too little to solve (e.g. heavy uniform noise).
  let m = solve_affine_ls(samples, &keep, MIN_SAMPLES).unwrap_or(m1);

  let fp = |v: f64| -> i64 { (v * 65536.0).round() as i64 };
  let diag = |v: f64| iclip(fp(v).clamp(-(1 << 30), 1 << 30) as i32, 0xe001, 0x11fff);
  let ndiag =
    |v: f64| iclip(fp(v).clamp(-(1 << 30), 1 << 30) as i32, -0x1fff, 0x1fff);
  let mut wm = WarpedMotionParams {
    matrix: [0, 0, diag(m[0]), ndiag(m[1]), ndiag(m[2]), diag(m[3])],
    abcd: [0; 4],
    valid: false,
  };
  // Fills abcd. The bool ("too sheared for a BLOCK to warp") is irrelevant to a
  // frame-level magnitude estimate, so it is deliberately ignored.
  let _ = get_shear_params(&mut wm);
  let shear = (wm.alpha().unsigned_abs() as u64)
    .max(wm.beta().unsigned_abs() as u64)
    .max(wm.gamma().unsigned_abs() as u64)
    .max(wm.delta().unsigned_abs() as u64);

  // Model selection, over the WHOLE field — deliberately NOT the trimmed subset.
  // The trim exists to ESTIMATE the model robustly; evaluating on the survivors
  // would be circular (they are precisely the samples the model fits, which read
  // ~1.000 for every clip, affine or not). The question here is how much of ALL
  // the observed motion this one affine model accounts for. E_trans is the
  // field's variance once the global translation is removed; E_affine is what
  // the affine terms leave behind. `mx`/`my`/`mu`/`mv` are already the
  // all-sample means computed for the trim above.
  //
  // The translation baseline predicts the IDENTITY linear part (dst = src + t),
  // so its residual is each block's MV deviation from the mean MV — NOT the
  // centered destination, which is dominated by the ±176px position spread and
  // pins R² at ~1.000 for every clip regardless of content.
  let (mut e_trans, mut e_affine) = (0f64, 0f64);
  for &(x, y, dx, dy) in samples {
    let (px, py) = (x as f64 - mx, y as f64 - my);
    let (pu, pv) = ((x + dx) as f64 - mu, (y + dy) as f64 - mv);
    let (ddx, ddy) = (pu - px, pv - py);
    e_trans += ddx * ddx + ddy * ddy;
    let ru = pu - (m[0] * px + m[1] * py);
    let rv = pv - (m[2] * px + m[3] * py);
    e_affine += ru * ru + rv * rv;
  }
  // A field with no residual motion at all (every block moved identically) has
  // nothing for the affine terms to explain — and nothing to warp.
  let gain = if e_trans <= 1e-9 {
    0.0
  } else {
    (1.0 - e_affine / e_trans).clamp(0.0, 1.0)
  };
  Some((shear, (gain * 1000.0) as u32))
}

/// dav1d warp_affine block loop + warp_affine_8x8 kernel (fused). Predicts a
/// warped block into `dst` from `ref_plane`, reading pixels with explicit edge
/// clamp (== dav1d emu_edge). Coordinates: `bx_luma`/`by_luma` = block top-left
/// in LUMA pixels (b.x*4, b.y*4); `bw`/`bh` = block size in THIS plane's pixels.
#[allow(clippy::too_many_arguments)]
pub fn predict_warp<T: Pixel>(
  ref_plane: &Plane<T>, dst: &mut PlaneRegionMut<'_, T>, wm: &WarpedMotionParams,
  bx_luma: i32, by_luma: i32, bw: usize, bh: usize, ss_hor: i32, ss_ver: i32,
  bit_depth: usize,
) {
  let intermediate_bits: i32 = if bit_depth == 8 { 4 } else { 2 };
  let mat = &wm.matrix;
  let (alpha, beta, gamma, delta) =
    (wm.alpha() as i32, wm.beta() as i32, wm.gamma() as i32, wm.delta() as i32);
  let width = ref_plane.cfg.width as i32;
  let height = ref_plane.cfg.height as i32;
  let pmax = (1i32 << bit_depth) - 1;
  let rnd0 = (1 << (7 - intermediate_bits)) >> 1;
  let rnd1 = (1 << (7 + intermediate_bits)) >> 1;

  let px = |x: i32, y: i32| -> i32 {
    let cx = x.clamp(0, width - 1) as usize;
    let cy = y.clamp(0, height - 1) as usize;
    i32::cast_from(ref_plane.p(cx, cy))
  };

  let mut by = 0usize;
  while by < bh {
    let src_y = by_luma + ((by as i32 + 4) << ss_ver);
    let mat3_y = mat[3] as i64 * src_y as i64 + mat[0] as i64;
    let mat5_y = mat[5] as i64 * src_y as i64 + mat[1] as i64;
    let mut bx = 0usize;
    while bx < bw {
      let src_x = bx_luma + ((bx as i32 + 4) << ss_hor);
      let mvx = (mat[2] as i64 * src_x as i64 + mat3_y) >> ss_hor;
      let mvy = (mat[4] as i64 * src_x as i64 + mat5_y) >> ss_ver;
      let dx = (mvx >> 16) as i32 - 4;
      let mx = ((mvx as i32) & 0xffff) - alpha * 4 - beta * 7 & !0x3f;
      let dy = (mvy >> 16) as i32 - 4;
      let my = ((mvy as i32) & 0xffff) - gamma * 4 - delta * 4 & !0x3f;

      // 8x8 warp kernel (two-pass 8-tap), reading ref at (dx+..,dy+..) clamped.
      let mut mid = [[0i32; 8]; 15];
      for y in 0..15 {
        let mxr = mx + y as i32 * beta;
        for x in 0..8 {
          let tmx = mxr + x as i32 * alpha;
          let filter =
            &DAV1D_MC_WARP_FILTER[(64 + (tmx + 512 >> 10)) as usize];
          let sy = dy + y as i32 - 3;
          let mut sum = 0i32;
          for i in 0..8 {
            sum += filter[i] as i32 * px(dx + x as i32 - 3 + i as i32, sy);
          }
          mid[y][x] = (sum + rnd0) >> (7 - intermediate_bits);
        }
      }
      for y in 0..8 {
        let myr = my + y as i32 * delta;
        for x in 0..8 {
          let tmy = myr + x as i32 * gamma;
          let filter =
            &DAV1D_MC_WARP_FILTER[(64 + (tmy + 512 >> 10)) as usize];
          let mut sum = 0i32;
          for i in 0..8 {
            sum += filter[i] as i32 * mid[y + i][x];
          }
          let v = ((sum + rnd1) >> (7 + intermediate_bits)).clamp(0, pmax);
          if bx + x < bw && by + y < bh {
            dst[by + y][bx + x] = T::cast_from(v as u32);
          }
        }
      }
      bx += 8;
    }
    by += 8;
  }
}

#[cfg(test)]
mod prepass_tests {
  use super::*;

  /// Build an MV field over a 352x288 (CIF) grid from a known affine model:
  /// the reference position of a block centred at (x, y) is `M*(x,y) + t`.
  fn field(
    a: f64, b: f64, d: f64, e: f64, tx: f64, ty: f64,
  ) -> Vec<(f32, f32, f32, f32)> {
    let mut v = Vec::new();
    let (cx, cy) = (176.0, 144.0);
    let mut y = 8.0;
    while y < 288.0 {
      let mut x = 8.0;
      while x < 352.0 {
        // Rotate/scale about the frame centre so the fit sees the linear part
        // regardless of where the origin sits.
        let (px, py) = (x - cx, y - cy);
        let ux = a * px + b * py + cx + tx;
        let uy = d * px + e * py + cy + ty;
        v.push((x as f32, y as f32, (ux - x) as f32, (uy - y) as f32));
        x += 16.0;
      }
      y += 16.0;
    }
    v
  }

  #[test]
  fn pure_translation_has_zero_shear() {
    // The whole point of the gate: a pan — however fast — must read exactly 0.
    for (tx, ty) in [(0.0, 0.0), (3.0, 0.0), (-7.0, 11.0), (24.0, -19.0)] {
      let (s, _) = fit_global_shear(&field(1.0, 0.0, 0.0, 1.0, tx, ty)).unwrap();
      assert_eq!(s, 0, "translation ({tx},{ty}) must have zero shear");
    }
  }

  #[test]
  fn rotation_recovers_expected_shear() {
    // 3 degrees: beta = -65536*sin(3 deg) = -3430 -> rounded to a multiple of 64.
    let th: f64 = 3.0_f64.to_radians();
    let (c, s) = (th.cos(), th.sin());
    let (got, gain) = fit_global_shear(&field(c, -s, s, c, 0.0, 0.0)).unwrap();
    let want = (65536.0 * s) as u64; // ~3430
    // A pure rotation IS affine, so the model explains essentially all of it.
    assert!(gain > 950, "clean rotation should be near-fully explained, got {gain}");
    assert!(
      got.abs_diff(want) <= 128,
      "rotation shear {got} not within a rounding step of {want}"
    );
  }

  #[test]
  fn zoom_recovers_expected_shear() {
    // 5% zoom: alpha = 65536*0.05 = 3277.
    let (got, gain) = fit_global_shear(&field(1.05, 0.0, 0.0, 1.05, 0.0, 0.0)).unwrap();
    assert!(gain > 950, "clean zoom should be near-fully explained, got {gain}");
    assert!(
      got.abs_diff(3277) <= 128,
      "zoom shear {got} not within a rounding step of 3277"
    );
  }

  #[test]
  fn foreground_outliers_do_not_hide_a_rotating_background() {
    // The robustness claim: a quarter of the field carries unrelated object
    // motion, and the trimmed refit must still report the background rotation.
    let th: f64 = 3.0_f64.to_radians();
    let (c, s) = (th.cos(), th.sin());
    let mut f = field(c, -s, s, c, 0.0, 0.0);
    let mut lcg: u32 = 12345;
    let n = f.len();
    for i in 0..n / 4 {
      lcg = lcg.wrapping_mul(1103515245).wrapping_add(12345);
      let jx = ((lcg >> 16) % 32) as f32 - 16.0;
      lcg = lcg.wrapping_mul(1103515245).wrapping_add(12345);
      let jy = ((lcg >> 16) % 32) as f32 - 16.0;
      f[i * 4].2 = jx;
      f[i * 4].3 = jy;
    }
    let (got, _) = fit_global_shear(&f).unwrap();
    let want = (65536.0 * s) as u64;
    assert!(
      got.abs_diff(want) <= 384,
      "outlier-contaminated rotation shear {got} strayed from {want}"
    );
  }

  #[test]
  fn degenerate_inputs_return_none() {
    assert!(fit_global_shear(&[]).is_none());
    assert!(fit_global_shear(&[(0.0, 0.0, 1.0, 1.0); 4]).is_none());
    // All samples on one horizontal line: the system has no y information.
    let collinear: Vec<_> =
      (0..40).map(|i| (i as f32 * 8.0, 16.0, 1.0, 0.0)).collect();
    assert!(fit_global_shear(&collinear).is_none());
  }
}
