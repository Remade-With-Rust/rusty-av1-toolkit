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

use crate::encoder::FrameInvariants;
use crate::frame::*;
use crate::util::{CastFromPrimitive, Pixel};
use std::sync::Arc;

/// How many frames either side of the target are blended in.
const RADIUS: i64 = 2;
/// Half-extent of the integer-pel block search, in pixels.
const SEARCH: isize = 4;
/// Filter block size, in luma pixels.
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

/// Best integer-pel offset of `nb` against `tgt` for the block at (bx, by),
/// by SAD. Full search over a small window: the neighbours of an alt-ref are
/// temporally close, so the true offset is small, and a full search keeps the
/// result deterministic (no seeded/early-exit ordering effects).
fn search_block<T: Pixel>(
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

fn filter_plane<T: Pixel>(
  out: &mut Plane<T>, tgt: &Plane<T>, nbs: &[&Plane<T>], mvs: &[(isize, isize)],
  bx: usize, by: usize, bw: usize, bh: usize, strength: u32, maxval: i32,
) {
  for y in 0..bh {
    for x in 0..bw {
      let (px_, py_) = ((bx + x) as isize, (by + y) as isize);
      let c = px(tgt, px_, py_);
      let mut acc = c as u32 * CENTER_W;
      let mut cnt = CENTER_W;
      for (nb, &(dx, dy)) in nbs.iter().zip(mvs.iter()) {
        let v = px(nb, px_ + dx, py_ + dy);
        let w = weight(c - v, strength);
        acc += v as u32 * w;
        cnt += w;
      }
      let val = ((acc + cnt / 2) / cnt) as i32;
      let stride = out.cfg.stride;
      out.data_origin_mut()[(by + y) * stride + (bx + x)] =
        T::cast_from(val.clamp(0, maxval));
    }
  }
}

/// Temporally filter `tgt` against `nbs`, returning a new frame.
pub fn filter_frame<T: Pixel>(
  tgt: &Frame<T>, nbs: &[Arc<Frame<T>>], strength: u32, bit_depth: usize,
) -> Frame<T> {
  let mut out = tgt.clone();
  let maxval = (1i32 << bit_depth) - 1;
  let (w, h) = (tgt.planes[0].cfg.width, tgt.planes[0].cfg.height);
  let luma_nbs: Vec<&Plane<T>> = nbs.iter().map(|f| &f.planes[0]).collect();

  let mut by = 0;
  while by < h {
    let mut bx = 0;
    let bh = BLK.min(h - by);
    while bx < w {
      let bw = BLK.min(w - bx);
      // The motion is found ONCE on luma and reused by chroma — the planes
      // describe the same moving content, and a chroma-only search would let
      // the planes disagree about where a block came from.
      let mvs: Vec<(isize, isize)> = luma_nbs
        .iter()
        .map(|nb| search_block(&tgt.planes[0], nb, bx, by, bw, bh))
        .collect();

      for pli in 0..tgt.planes.len() {
        let (xdec, ydec) =
          (tgt.planes[pli].cfg.xdec, tgt.planes[pli].cfg.ydec);
        let (pbx, pby) = (bx >> xdec, by >> ydec);
        let (pw, ph) =
          (tgt.planes[pli].cfg.width, tgt.planes[pli].cfg.height);
        if pbx >= pw || pby >= ph {
          continue;
        }
        let (pbw, pbh) =
          (((bw + (1 << xdec) - 1) >> xdec).min(pw - pbx),
           ((bh + (1 << ydec) - 1) >> ydec).min(ph - pby));
        let pmvs: Vec<(isize, isize)> =
          mvs.iter().map(|&(dx, dy)| (dx >> xdec, dy >> ydec)).collect();
        let pnbs: Vec<&Plane<T>> =
          nbs.iter().map(|f| &f.planes[pli]).collect();
        // Split the borrow: read the pristine target, write the copy.
        let tgt_p = &tgt.planes[pli];
        filter_plane(
          &mut out.planes[pli], tgt_p, &pnbs, &pmvs, pbx, pby, pbw, pbh,
          strength, maxval,
        );
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
