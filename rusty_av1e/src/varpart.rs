//! prom_av1e031 — content-adaptive dispatch, Brick 1: the O(pixels) content
//! SIGNAL. A residual variance tree over a 64×64 superblock: per-8×8
//! (sum, sum-of-squares) aggregated 8→16→32→64, giving libvpx's
//! `256 · per-sample-variance` at every node (so its VAR_BASED_PARTITION
//! thresholds transfer). The signal is the variance of (source − reference)
//! = coding DIFFICULTY (a busy region the reference predicts well reads as
//! easy); `None` reference (key frames, no predictor) → source variance.
//!
//! This module is PURE and oracle-tested (tree == brute force at every
//! level). No encoder wiring lives here — that is Brick 2.
#![allow(dead_code)] // wired in Brick 2 (var_pick_partition)

use crate::tiling::PlaneRegion;
use crate::util::{CastFromPrimitive, Pixel};

/// One variance node: accumulated first/second residual moments over a
/// `2^log2_count`-pixel square region.
#[derive(Clone, Copy, Default)]
pub struct VarNode {
  sum: i64,
  sse: i64,
  log2_count: u32,
}

impl VarNode {
  /// libvpx `get_variance`: `256 · (SSE − sum²/N) / N`, integer, with
  /// `N = 2^log2_count`. Clamped at 0 (integer mean-square rounding can make
  /// the bracket slightly negative on flat regions).
  #[inline]
  pub fn variance(&self) -> i64 {
    let mean_sq = (self.sum * self.sum) >> self.log2_count;
    let num = (self.sse - mean_sq).max(0);
    (256 * num) >> self.log2_count
  }

  #[inline]
  fn merge(children: &[VarNode; 4]) -> VarNode {
    VarNode {
      sum: children.iter().map(|c| c.sum).sum(),
      sse: children.iter().map(|c| c.sse).sum(),
      log2_count: children[0].log2_count + 2,
    }
  }
}

/// Variance tree over a 64×64 SB. Each level is raster-ordered; a node at
/// level L covers a `(64 >> L)`-wide square. `v8` are the 8×8 leaves.
pub struct VarTree {
  pub v8: [VarNode; 64],
  pub v16: [VarNode; 16],
  pub v32: [VarNode; 4],
  pub v64: VarNode,
}

const SB: usize = 64;

/// Load a 64×64 region into a flat `i32` residual buffer, replicating the
/// last visible row/col past the `(w, h)` visible extent (libvpx edge-clamp:
/// every 8×8 leaf keeps a full 64-sample count). `refr = None` ⇒ source
/// values (residual against a zero predictor).
fn load_residual<T: Pixel>(
  src: &PlaneRegion<'_, T>, refr: Option<&PlaneRegion<'_, T>>, w: usize,
  h: usize, out: &mut [i32; SB * SB],
) {
  let w1 = w.max(1) - 1;
  let h1 = h.max(1) - 1;
  for y in 0..SB {
    let sy = y.min(h1);
    let src_row = &src[sy];
    let ref_row = refr.map(|r| &r[sy]);
    for x in 0..SB {
      let sx = x.min(w1);
      let s = i32::cast_from(src_row[sx]);
      let r = ref_row.map_or(0, |row| i32::cast_from(row[sx]));
      out[y * SB + x] = s - r;
    }
  }
}

/// Build the residual variance tree for one 64×64 SB. `w`/`h` are the visible
/// dimensions within the SB (edge SBs clamp-fill the remainder).
pub fn build_var_tree<T: Pixel>(
  src: &PlaneRegion<'_, T>, refr: Option<&PlaneRegion<'_, T>>, w: usize,
  h: usize,
) -> VarTree {
  let mut buf = [0i32; SB * SB];
  load_residual(src, refr, w, h, &mut buf);

  // 8×8 leaves: raster over the 8×8 grid, full 64-sample count each.
  let mut v8 = [VarNode::default(); 64];
  for by in 0..8 {
    for bx in 0..8 {
      let mut sum = 0i64;
      let mut sse = 0i64;
      for iy in 0..8 {
        let row = (by * 8 + iy) * SB + bx * 8;
        for ix in 0..8 {
          let d = buf[row + ix] as i64;
          sum += d;
          sse += d * d;
        }
      }
      v8[by * 8 + bx] = VarNode { sum, sse, log2_count: 6 };
    }
  }

  // Aggregate 8→16→32→64. At each step a node merges the 2×2 children below
  // it (raster indices within the parent's grid).
  let merge_level = |src_nodes: &[VarNode], src_dim: usize| -> Vec<VarNode> {
    let dst_dim = src_dim / 2;
    let mut dst = vec![VarNode::default(); dst_dim * dst_dim];
    for py in 0..dst_dim {
      for px in 0..dst_dim {
        let c = |cy: usize, cx: usize| src_nodes[cy * src_dim + cx];
        let kids = [
          c(py * 2, px * 2),
          c(py * 2, px * 2 + 1),
          c(py * 2 + 1, px * 2),
          c(py * 2 + 1, px * 2 + 1),
        ];
        dst[py * dst_dim + px] = VarNode::merge(&kids);
      }
    }
    dst
  };

  let l16 = merge_level(&v8, 8);
  let l32 = merge_level(&l16, 4);
  let l64 = merge_level(&l32, 2);

  let mut v16 = [VarNode::default(); 16];
  v16.copy_from_slice(&l16);
  let mut v32 = [VarNode::default(); 4];
  v32.copy_from_slice(&l32);

  VarTree { v8, v16, v32, v64: l64[0] }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::frame::{AsRegion, Plane};
  use crate::tiling::Area;

  // Deterministic textured plane (busy) + a smoother reference, both larger
  // than one SB so region reads are in-bounds.
  fn planes<T: Pixel>() -> (Plane<T>, Plane<T>) {
    let mut src = Plane::new(128, 128, 0, 0, 8, 8);
    let mut refr = Plane::new(128, 128, 0, 0, 8, 8);
    for (i, row) in src.data.chunks_mut(src.cfg.stride).enumerate() {
      for (j, p) in row.iter_mut().enumerate() {
        *p = T::cast_from((((i * 7) ^ (j * 13)) & 0xff) as i32);
      }
    }
    for (i, row) in refr.data.chunks_mut(refr.cfg.stride).enumerate() {
      for (j, p) in row.iter_mut().enumerate() {
        *p = T::cast_from(((i + j) & 0xff) as i32);
      }
    }
    (src, refr)
  }

  // Brute-force `256 · variance` over a `dim × dim` region at pixel origin
  // (ox, oy), with the same edge-clamp as the tree.
  fn brute<T: Pixel>(
    src: &PlaneRegion<'_, T>, refr: Option<&PlaneRegion<'_, T>>, w: usize,
    h: usize, ox: usize, oy: usize, dim: usize,
  ) -> i64 {
    let w1 = w.max(1) - 1;
    let h1 = h.max(1) - 1;
    let (mut sum, mut sse) = (0i64, 0i64);
    for y in 0..dim {
      let sy = (oy + y).min(h1);
      for x in 0..dim {
        let sx = (ox + x).min(w1);
        let s = i32::cast_from(src[sy][sx]) as i64;
        let r = refr.map_or(0, |rr| i32::cast_from(rr[sy][sx]) as i64);
        let d = s - r;
        sum += d;
        sse += d * d;
      }
    }
    let n = (dim * dim) as u32;
    let log2 = n.trailing_zeros();
    let mean_sq = (sum * sum) >> log2;
    (256 * (sse - mean_sq).max(0)) >> log2
  }

  fn oracle_inner<T: Pixel>() {
    let (sp, rp) = planes::<T>();
    let area = Area::StartingAt { x: 0, y: 0 };
    let src = sp.region(area);
    let refr = rp.region(area);

    for &(w, h) in &[(64usize, 64usize), (64, 40), (30, 64), (17, 9)] {
      // Residual against a real reference, and source-only (None).
      for use_ref in [true, false] {
        let r = if use_ref { Some(&refr) } else { None };
        let t = build_var_tree(&src, r, w, h);

        assert_eq!(t.v64.variance(), brute(&src, r, w, h, 0, 0, 64));
        for i in 0..4 {
          let (ox, oy) = ((i % 2) * 32, (i / 2) * 32);
          assert_eq!(t.v32[i].variance(), brute(&src, r, w, h, ox, oy, 32));
        }
        for i in 0..16 {
          let (ox, oy) = ((i % 4) * 16, (i / 4) * 16);
          assert_eq!(t.v16[i].variance(), brute(&src, r, w, h, ox, oy, 16));
        }
        for i in 0..64 {
          let (ox, oy) = ((i % 8) * 8, (i / 8) * 8);
          assert_eq!(t.v8[i].variance(), brute(&src, r, w, h, ox, oy, 8));
        }
      }
    }
  }

  #[test]
  fn tree_matches_brute_force_u8() {
    oracle_inner::<u8>();
  }

  #[test]
  fn tree_matches_brute_force_u16() {
    oracle_inner::<u16>();
  }

  // Residual against an identical reference is exactly zero everywhere.
  #[test]
  fn zero_residual_when_ref_equals_src() {
    let (sp, _) = planes::<u8>();
    let area = Area::StartingAt { x: 0, y: 0 };
    let src = sp.region(area);
    let t = build_var_tree(&src, Some(&src), 64, 64);
    assert_eq!(t.v64.variance(), 0);
    assert!(t.v8.iter().all(|n| n.variance() == 0));
  }
}
