// prom_av1e062: alt-ref temporal filter (ARNR).
//
// Priced at +5.02% BD on libaom (cpu2, bus+tempete) — the single largest
// mechanism measured in this campaign, and we had none of it: alt-refs were
// encoded straight from the unfiltered source.
//
// This is a PURE SOURCE-SIDE TRANSFORM. It denoises the source of a hidden
// alt-ref by motion-compensated blending with its temporal neighbours, then
// hands the result to the ordinary encode path. Nothing about the bitstream
// changes, so it is conformant by construction — there is no new syntax for a
// decoder to parse and no encoder/decoder symmetry to maintain. The gate is
// therefore purely BD-rate, never a decode check.
//
// Why filtering a reference pays: a hidden alt-ref is predicted FROM by many
// later frames. Source noise in it is unpredictable detail that costs bits to
// code and then propagates as a worse prediction for everything referencing it.
// Averaging along the motion trajectory removes noise while preserving real
// detail, because real detail is what the motion search aligns.
//
// SUB-PEL (prom_av1e062b): the first version blended at INTEGER pel, and its
// strength sweep on clean content was monotone toward "barely filter at all" —
// the signature of a filter destroying as much as it removes. Blending a
// neighbour that is misaligned by a fraction of a pixel mixes a shifted copy of
// the image into itself, which is a low-pass blur applied to exactly the real
// detail the filter is supposed to preserve. Real motion is not integer, so at
// integer pel most blocks are misaligned by construction. The fix is to align
// with the same 8-tap sub-pel MC the encoder itself predicts with: refine the
// integer match to 1/8 pel and blend against the INTERPOLATED block.

use crate::cpu_features::CpuFeatureLevel;
use crate::encoder::FrameInvariants;
use crate::frame::*;
use crate::mc::*;
use crate::util::{CastFromPrimitive, Pixel};
use std::sync::Arc;

/// How many frames either side of the target are blended in.
const RADIUS: i64 = 2;
/// Half-extent of the integer-pel block search, in pixels.
const SEARCH: isize = 4;
/// Filter block size, in luma pixels. A power of two and even, which is what
/// `put_8tap` requires of its output block.
const BLK: usize = 16;
/// Weight given to the target frame's own pixels. Neighbours contribute 0..16
/// depending on how well they match, so this holds the target at 2x the weight
/// of a perfectly-matching neighbour — the filter can smooth, but cannot walk
/// away from its own frame.
const CENTER_W: u32 = 32;

#[inline]
fn px<T: Pixel>(p: &Plane<T>, x: isize, y: isize) -> i32 {
  let w = p.cfg.width as isize;
  let h = p.cfg.height as isize;
  let xc = x.clamp(0, w - 1) as usize;
  let yc = y.clamp(0, h - 1) as usize;
  i32::cast_from(p.data_origin()[yc * p.cfg.stride + xc])
}

/// Motion-compensate a `bw`x`bh` block of `src` at (bx, by) displaced by the
/// 1/8-pel LUMA motion vector (mv_row, mv_col), into `dst`.
///
/// The MV is expressed in luma units for every plane and rescaled here exactly
/// as `PredictionMode::get_mv_params` does for inter prediction — that shared
/// convention is the point. The integer version scaled chroma by `dx >> xdec`,
/// which silently dropped the sub-pel part of the chroma displacement; here
/// chroma gets the same treatment the encoder's own predictor gives it.
fn mc_block<T: Pixel>(
  dst: &mut Plane<T>, src: &Plane<T>, bx: usize, by: usize, bw: usize,
  bh: usize, mv_row: i32, mv_col: i32, bit_depth: usize,
) {
  let &PlaneConfig { xdec, ydec, .. } = &src.cfg;
  let row_offset = mv_row >> (3 + ydec);
  let col_offset = mv_col >> (3 + xdec);
  let row_frac = (mv_row << (1 - ydec)) & 0xf;
  let col_frac = (mv_col << (1 - xdec)) & 0xf;
  let po = PlaneOffset {
    x: bx as isize + col_offset as isize - 3,
    y: by as isize + row_offset as isize - 3,
  };
  // `clamp()` edge-extends, so a block at the frame border - and the 3-pixel
  // filter margin around it - reads defined pixels instead of running off.
  let slice = src.slice(po).clamp().subslice(3, 3);
  put_8tap(
    &mut dst.as_region_mut(),
    slice,
    bw,
    bh,
    col_frac,
    row_frac,
    FilterMode::REGULAR,
    FilterMode::REGULAR,
    bit_depth,
    CpuFeatureLevel::default(),
  );
}

/// SAD between the target block and an already motion-compensated block.
fn sad_mc<T: Pixel>(
  tgt: &Plane<T>, bx: usize, by: usize, bw: usize, bh: usize, pred: &Plane<T>,
) -> u64 {
  let mut sad = 0u64;
  for y in 0..bh {
    for x in 0..bw {
      let a = px(tgt, (bx + x) as isize, (by + y) as isize);
      let b = i32::cast_from(pred.data_origin()[y * pred.cfg.stride + x]);
      sad += (a - b).unsigned_abs() as u64;
    }
  }
  sad
}

/// Integer-pel full search — stage 1. Full rather than stepped: the neighbours
/// of an alt-ref are temporally close so the window is small, and a full search
/// keeps the result deterministic (no early-exit ordering effects).
fn search_integer<T: Pixel>(
  tgt: &Plane<T>, nb: &Plane<T>, bx: usize, by: usize, bw: usize, bh: usize,
) -> (isize, isize) {
  let (mut best, mut bdx, mut bdy) = (u64::MAX, 0isize, 0isize);
  for dy in -SEARCH..=SEARCH {
    for dx in -SEARCH..=SEARCH {
      let mut sad = 0u64;
      for y in 0..bh {
        for x in 0..bw {
          let a = px(tgt, (bx + x) as isize, (by + y) as isize);
          let b = px(nb, (bx + x) as isize + dx, (by + y) as isize + dy);
          sad += (a - b).unsigned_abs() as u64;
        }
      }
      // Ties prefer the smaller offset, so a static block stays at (0,0).
      if sad < best
        || (sad == best && dx.abs() + dy.abs() < bdx.abs() + bdy.abs())
      {
        best = sad;
        bdx = dx;
        bdy = dy;
      }
    }
  }
  (bdx, bdy)
}

/// Refine an integer match to sub-pel by successive halving: half-pel (4/8),
/// then quarter-pel (2/8), then eighth-pel (1/8). Each stage tests the eight
/// neighbours of the current best at that step size and keeps it only if it
/// actually lowers SAD, so the result can never be worse than the integer match
/// it started from.
fn refine_subpel<T: Pixel>(
  tgt: &Plane<T>, nb: &Plane<T>, tmp: &mut Plane<T>, bx: usize, by: usize,
  bw: usize, bh: usize, mut mv_row: i32, mut mv_col: i32, bit_depth: usize,
) -> (i32, i32) {
  let (int_row, int_col) = (mv_row, mv_col);
  mc_block(tmp, nb, bx, by, bw, bh, mv_row, mv_col, bit_depth);
  let int_sad = sad_mc(tgt, bx, by, bw, bh, tmp);
  let mut best = int_sad;
  for step in [4i32, 2, 1] {
    loop {
      let (mut br, mut bc) = (mv_row, mv_col);
      let mut improved = false;
      for (dr, dc) in [
        (-step, 0),
        (step, 0),
        (0, -step),
        (0, step),
        (-step, -step),
        (-step, step),
        (step, -step),
        (step, step),
      ] {
        let (r, c) = (mv_row + dr, mv_col + dc);
        mc_block(tmp, nb, bx, by, bw, bh, r, c, bit_depth);
        let sad = sad_mc(tgt, bx, by, bw, bh, tmp);
        if sad < best {
          best = sad;
          br = r;
          bc = c;
          improved = true;
        }
      }
      mv_row = br;
      mv_col = bc;
      if !improved {
        break;
      }
    }
  }
  // Sub-pel alignment is not free: `put_8tap` at a fractional position is an
  // interpolation, and interpolation low-passes. Where the true motion IS
  // integer — a static or near-static block — the integer match is already
  // exact, and any sub-pel offset that "wins" is fitting noise while paying
  // that blur on real detail. So only take the refinement when it beats the
  // integer match by a real margin, never on a noise-level improvement.
  // MEASURED AND DEFAULTED OFF (margin 0.0): the hypothesis was that sub-pel
  // blur explained the near-static akiyo's +1.041%, but margins of 0.02/0.05/
  // 0.10 left akiyo at +0.971/+1.483/+1.332 and made every mean worse. Akiyo's
  // R-D points then showed differences of 0.02-0.09 dB and ~0.5% of a ~1-5
  // kbit/frame rate, non-monotone in QP — an ill-conditioned BD fit amplifying
  // encoder noise, not a regression to chase. The knob stays for content where
  // the trade is real.
  let margin = crate::harvest::arnr_subpel_margin();
  if (best as f64) > (int_sad as f64) * (1.0 - margin) {
    return (int_row, int_col);
  }
  (mv_row, mv_col)
}

/// Per-pixel blend weight for a neighbour, from how well it matches.
///
/// A large difference means the motion search did NOT find this pixel's true
/// correspondence (occlusion, a new object, real change) — blending there would
/// smear, so the weight falls to zero. A small difference means the two frames
/// agree, and their disagreement is noise worth averaging away. `strength`
/// widens the window of differences treated as noise.
#[inline]
fn weight(diff: i32, strength: u32) -> u32 {
  let m = ((diff * diff * 3) >> strength).min(16) as u32;
  16 - m
}

/// Temporally filter `tgt` against `nbs`, returning a new frame.
pub fn filter_frame<T: Pixel>(
  tgt: &Frame<T>, nbs: &[Arc<Frame<T>>], strength: u32, bit_depth: usize,
) -> Frame<T> {
  let mut out = tgt.clone();
  let maxval = (1i32 << bit_depth) - 1;
  let subpel = crate::harvest::arnr_subpel();
  let (w, h) = (tgt.planes[0].cfg.width, tgt.planes[0].cfg.height);
  let nplanes = tgt.planes.len();

  // Scratch for the motion-compensated block, one per plane subsampling, plus
  // one prediction buffer per neighbour so the blend can read them together.
  let mut tmp: Plane<T> = Plane::new(BLK, BLK, 0, 0, 0, 0);
  let mut preds: Vec<Plane<T>> =
    (0..nbs.len()).map(|_| Plane::new(BLK, BLK, 0, 0, 0, 0)).collect();

  let mut by = 0;
  while by < h {
    let bh = BLK.min(h - by);
    let mut bx = 0;
    while bx < w {
      let bw = BLK.min(w - bx);

      // The motion is found ONCE on luma and reused by every plane — the planes
      // describe the same moving content, and a per-plane search would let them
      // disagree about where a block came from.
      let mvs: Vec<(i32, i32)> = nbs
        .iter()
        .map(|f| {
          let nb = &f.planes[0];
          let (dx, dy) = search_integer(&tgt.planes[0], nb, bx, by, bw, bh);
          let (mut r, mut c) = (dy as i32 * 8, dx as i32 * 8);
          if subpel {
            let (rr, cc) = refine_subpel(
              &tgt.planes[0],
              nb,
              &mut tmp,
              bx,
              by,
              bw,
              bh,
              r,
              c,
              bit_depth,
            );
            r = rr;
            c = cc;
          }
          (r, c)
        })
        .collect();

      for pli in 0..nplanes {
        let cfg = &tgt.planes[pli].cfg;
        let (pbx, pby) = (bx >> cfg.xdec, by >> cfg.ydec);
        if pbx >= cfg.width || pby >= cfg.height {
          continue;
        }
        let pbw = (((bw + (1 << cfg.xdec) - 1) >> cfg.xdec)
          .min(cfg.width - pbx))
        .max(1);
        let pbh = (((bh + (1 << cfg.ydec) - 1) >> cfg.ydec)
          .min(cfg.height - pby))
        .max(1);
        // put_8tap needs a power-of-two width and an even height, so always
        // compensate the full scratch block and use only the valid corner.
        let (fw, fh) = (BLK >> cfg.xdec, BLK >> cfg.ydec);
        for (n, f) in nbs.iter().enumerate() {
          mc_block(
            &mut preds[n],
            &f.planes[pli],
            pbx,
            pby,
            fw,
            fh,
            mvs[n].0,
            mvs[n].1,
            bit_depth,
          );
        }

        let tgt_p = &tgt.planes[pli];
        let stride = out.planes[pli].cfg.stride;
        for y in 0..pbh {
          for x in 0..pbw {
            let c = px(tgt_p, (pbx + x) as isize, (pby + y) as isize);
            let mut acc = c as u32 * CENTER_W;
            let mut cnt = CENTER_W;
            for p in preds.iter() {
              let v = i32::cast_from(p.data_origin()[y * p.cfg.stride + x]);
              let wt = weight(c - v, strength);
              acc += v as u32 * wt;
              cnt += wt;
            }
            let val = ((acc + cnt / 2) / cnt) as i32;
            out.planes[pli].data_origin_mut()
              [(pby + y) * stride + (pbx + x)] =
              T::cast_from(val.clamp(0, maxval));
          }
        }
      }
      bx += BLK;
    }
    by += BLK;
  }
  out
}

/// Filter this frame's source if it is a hidden alt-ref and ARNR is enabled.
///
/// Only HIDDEN frames are filtered. A hidden frame exists solely to be
/// referenced — it is never shown — so improving it as a prediction source is
/// pure gain, whereas filtering a displayed frame would trade its own fidelity
/// against its usefulness as a reference.
pub fn maybe_filter_altref<T: Pixel>(
  fi: &FrameInvariants<T>, frame: Arc<Frame<T>>,
  frame_q: &std::collections::BTreeMap<u64, Option<Arc<Frame<T>>>>,
) -> Arc<Frame<T>> {
  if !crate::harvest::arnr() || fi.show_frame || fi.intra_only {
    return frame;
  }
  // Only the TOP pyramid anchor. Every hidden frame is a reference, but they
  // are not reused equally: the level-0 anchor is predicted from across the
  // whole group, while a deeper hidden B-frame serves a couple of neighbours.
  // Filtering trades a frame's own fidelity for its quality AS A PREDICTOR, so
  // it only pays where the reuse is high enough to repay the trade — libaom
  // likewise filters the ARF, not every hidden frame.
  if fi.pyramid_level > crate::harvest::arnr_max_level() {
    return frame;
  }
  let cur = fi.input_frameno as i64;
  let mut nbs: Vec<Arc<Frame<T>>> = Vec::new();
  for k in 1..=RADIUS {
    for n in [cur - k, cur + k] {
      if n < 0 {
        continue;
      }
      if let Some(Some(f)) = frame_q.get(&(n as u64)) {
        nbs.push(f.clone());
      }
    }
  }
  if nbs.is_empty() {
    return frame;
  }
  let filtered = filter_frame(
    &frame,
    &nbs,
    crate::harvest::arnr_strength(),
    fi.sequence.bit_depth,
  );
  Arc::new(filtered)
}
