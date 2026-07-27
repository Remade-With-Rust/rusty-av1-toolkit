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

/// How many frames either side of the target are blended in (see
/// `harvest::arnr_radius`). More neighbours average MORE noise away without
/// widening the per-pixel acceptance window the way strength does — a
/// different lever on the same goal.
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
  // The target block is always inside the frame, so this never needs the clamp.
  let (ts, ps) = (tgt.cfg.stride, pred.cfg.stride);
  let (t, p) = (tgt.data_origin(), pred.data_origin());
  let mut sad = 0u64;
  for y in 0..bh {
    let tr = &t[(by + y) * ts + bx..][..bw];
    let pr = &p[y * ps..][..bw];
    for x in 0..bw {
      sad +=
        (i32::cast_from(tr[x]) - i32::cast_from(pr[x])).unsigned_abs() as u64;
    }
  }
  sad
}

/// SAD of the block against `nb` displaced by an integer offset.
///
/// Two paths, byte-identical in result. The search evaluates this hundreds of
/// times per block, and the general path clamps EVERY pixel read — which both
/// costs a compare per sample and blocks vectorisation. Interior blocks (the
/// overwhelming majority) need no clamping at all, so they take a straight
/// row-slice loop that LLVM can vectorise; only blocks whose displaced window
/// actually crosses the frame edge pay for the clamp.
fn sad_int<T: Pixel>(
  tgt: &Plane<T>, nb: &Plane<T>, bx: usize, by: usize, bw: usize, bh: usize,
  dx: isize, dy: isize,
) -> u64 {
  let (x0, y0) = (bx as isize + dx, by as isize + dy);
  let (nw, nh) = (nb.cfg.width as isize, nb.cfg.height as isize);
  let mut sad = 0u64;
  if x0 >= 0 && y0 >= 0 && x0 + bw as isize <= nw && y0 + bh as isize <= nh {
    let (ts, ns) = (tgt.cfg.stride, nb.cfg.stride);
    let (t, n) = (tgt.data_origin(), nb.data_origin());
    let (x0, y0) = (x0 as usize, y0 as usize);
    for y in 0..bh {
      let tr = &t[(by + y) * ts + bx..][..bw];
      let nr = &n[(y0 + y) * ns + x0..][..bw];
      for x in 0..bw {
        sad +=
          (i32::cast_from(tr[x]) - i32::cast_from(nr[x])).unsigned_abs() as u64;
      }
    }
    return sad;
  }
  for y in 0..bh {
    for x in 0..bw {
      let a = px(tgt, (bx + x) as isize, (by + y) as isize);
      let b = px(nb, (bx + x) as isize + dx, (by + y) as isize + dy);
      sad += (a - b).unsigned_abs() as u64;
    }
  }
  sad
}

/// Coarse step search, used only when the configured range exceeds the fine
/// window. A full search over a wide window is quadratic and unaffordable; this
/// halves the step from `range/2` down to 2, which reaches a far offset in
/// ~8*log2(range) probes.
///
/// This exists because of `bus`: it pans fast enough that its true
/// frame-to-frame motion leaves a +-4 window, so the fine search saturated at
/// the window edge and the filter blended MISMATCHED content — the same
/// alignment failure the sub-pel work fixed, arriving via range instead of
/// precision. A filter that cannot reach the motion cannot align to it.
fn search_coarse<T: Pixel>(
  tgt: &Plane<T>, nb: &Plane<T>, bx: usize, by: usize, bw: usize, bh: usize,
  range: isize,
) -> (isize, isize) {
  let (mut cx, mut cy) = (0isize, 0isize);
  let mut best = sad_int(tgt, nb, bx, by, bw, bh, 0, 0);
  let mut step = range / 2;
  while step >= 2 {
    let (mut nx, mut ny) = (cx, cy);
    for (dy, dx) in [
      (-step, 0),
      (step, 0),
      (0, -step),
      (0, step),
      (-step, -step),
      (-step, step),
      (step, -step),
      (step, step),
    ] {
      let (ty, tx) = (cy + dy, cx + dx);
      if ty.abs() > range || tx.abs() > range {
        continue;
      }
      let sad = sad_int(tgt, nb, bx, by, bw, bh, tx, ty);
      if sad < best {
        best = sad;
        nx = tx;
        ny = ty;
      }
    }
    if nx == cx && ny == cy {
      step /= 2;
    } else {
      cx = nx;
      cy = ny;
    }
  }
  (cx, cy)
}

/// Integer-pel search — stage 1. A full search over the fine window (optionally
/// recentred by a coarse pass): full rather than stepped so the result is
/// deterministic, with no early-exit ordering effects.
fn search_integer<T: Pixel>(
  tgt: &Plane<T>, nb: &Plane<T>, bx: usize, by: usize, bw: usize, bh: usize,
) -> (isize, isize, u64) {
  let range = crate::harvest::arnr_range() as isize;
  let (cx, cy) = if range > SEARCH {
    search_coarse(tgt, nb, bx, by, bw, bh, range)
  } else {
    (0, 0)
  };
  let (mut best, mut bdx, mut bdy) = (u64::MAX, cx, cy);
  for dy in (cy - SEARCH)..=(cy + SEARCH) {
    for dx in (cx - SEARCH)..=(cx + SEARCH) {
      let sad = sad_int(tgt, nb, bx, by, bw, bh, dx, dy);
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
  (bdx, bdy, best)
}

/// Refine an integer match to sub-pel by successive halving: half-pel (4/8),
/// then quarter-pel (2/8), then eighth-pel (1/8). Each stage tests the eight
/// neighbours of the current best at that step size and keeps it only if it
/// actually lowers SAD, so the result can never be worse than the integer match
/// it started from.
fn refine_subpel<T: Pixel>(
  tgt: &Plane<T>, nb: &Plane<T>, tmp: &mut Plane<T>, bx: usize, by: usize,
  bw: usize, bh: usize, mut mv_row: i32, mut mv_col: i32, bit_depth: usize,
) -> (i32, i32, u64) {
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
    return (int_row, int_col, int_sad);
  }
  (mv_row, mv_col, best)
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

  let (mut stat_sum, mut stat_hi, mut stat_n) = (0u64, 0u64, 0u64);
  let mut by = 0;
  while by < h {
    let bh = BLK.min(h - by);
    let mut bx = 0;
    while bx < w {
      let bw = BLK.min(w - bx);

      // The motion is found ONCE on luma and reused by every plane — the planes
      // describe the same moving content, and a per-plane search would let them
      // disagree about where a block came from.
      let mvs: Vec<(i32, i32, u32)> = nbs
        .iter()
        .map(|f| {
          let nb = &f.planes[0];
          let (dx, dy, mut sad) =
            search_integer(&tgt.planes[0], nb, bx, by, bw, bh);
          let (mut r, mut c) = (dy as i32 * 8, dx as i32 * 8);
          if subpel {
            let (rr, cc, ss) = refine_subpel(
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
            sad = ss;
          }
          // PER-BLOCK, PER-NEIGHBOUR STRENGTH DISPATCH (prom_av1e062d).
          //
          // Strength decides how much a MISMATCHED neighbour still gets
          // blended, so the quantity that should set it is how well this
          // neighbour actually aligned — which the search just measured, for
          // free. Where the residual is low the two frames genuinely agree and
          // their remaining difference is noise worth averaging harder; where
          // it is high the match is the best of a bad set (occlusion, complex
          // or fast local motion) and blending harder would smear real detail.
          //
          // This is why the clip-level sign-flip existed at all: strength 6
          // won on the well-aligning clips and lost on bus and foreman. The
          // split is not really between CLIPS, it is between BLOCKS that
          // aligned and blocks that did not — and clips differ only in their
          // mixture. Dispatching per block routes both cases correctly inside
          // a single clip.
          let mean_resid = (sad / (bw * bh) as u64) as u32;
          stat_sum += mean_resid as u64;
          stat_hi += u64::from(mean_resid > 8);
          stat_n += 1;
          let s = if mean_resid <= crate::harvest::arnr_resid_t() {
            crate::harvest::arnr_strength_hi()
          } else {
            strength
          };
          (r, c, s)
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
            for (p, mv) in preds.iter().zip(mvs.iter()) {
              let v = i32::cast_from(p.data_origin()[y * p.cfg.stride + x]);
              let wt = weight(c - v, mv.2);
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
  if std::env::var("RAV1E_ARNR_STATS").is_ok() {
    // Harvest the candidate FRAME-level dispatch signal before building a
    // dispatcher on it: does the post-MC residual actually separate the clips
    // where flat strength 6 wins from the two where it loses?
    let n = stat_n.max(1);
    eprintln!(
      "ARNR_STAT frame_mean_resid={:.2} frac_blocks_resid_over_8={:.3} blocks={}",
      stat_sum as f64 / n as f64,
      stat_hi as f64 / n as f64,
      stat_n
    );
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
  for k in 1..=(crate::harvest::arnr_radius() as i64) {
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
