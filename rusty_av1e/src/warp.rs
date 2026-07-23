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
