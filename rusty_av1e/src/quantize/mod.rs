// Copyright (c) 2017-2022, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.

#![allow(non_upper_case_globals)]

mod tables;

cfg_if::cfg_if! {
  if #[cfg(nasm_x86_64)] {
    pub use crate::asm::x86::quantize::*;
  } else {
    pub use self::rust::*;
  }
}

pub use tables::*;

use crate::scan_order::av1_scan_orders;
use crate::transform::{TxSize, TxType};
use crate::util::*;
use std::mem;
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};

pub fn get_log_tx_scale(tx_size: TxSize) -> usize {
  /// By `tx_size as usize`: 1 for more than 256 pixels, plus 1 for more
  /// than 1024 (a lookup rather than the area from width and height).
  const LOG_TX_SCALE: [u8; TxSize::TX_SIZES_ALL] = {
    use TxSize::*;
    let sizes = [
      TX_4X4, TX_8X8, TX_16X16, TX_32X32, TX_64X64, TX_4X8, TX_8X4, TX_8X16,
      TX_16X8, TX_16X32, TX_32X16, TX_32X64, TX_64X32, TX_4X16, TX_16X4,
      TX_8X32, TX_32X8, TX_16X64, TX_64X16,
    ];
    let mut table = [0; TxSize::TX_SIZES_ALL];
    let mut i = 0;
    while i < sizes.len() {
      let num_pixels = sizes[i].area();
      table[sizes[i] as usize] = (num_pixels > 256) as u8 + (num_pixels > 1024) as u8;
      i += 1;
    }
    table
  };
  LOG_TX_SCALE[tx_size as usize] as usize
}

pub fn dc_q(qindex: u8, delta_q: i8, bit_depth: usize) -> NonZeroU16 {
  let dc_q: [&[NonZeroU16; 256]; 3] =
    [&dc_qlookup_Q3, &dc_qlookup_10_Q3, &dc_qlookup_12_Q3];
  let bd = ((bit_depth ^ 8) >> 1).min(2);
  dc_q[bd][((qindex as isize + delta_q as isize).max(0) as usize).min(255)]
}

pub fn ac_q(qindex: u8, delta_q: i8, bit_depth: usize) -> NonZeroU16 {
  let ac_q: [&[NonZeroU16; 256]; 3] =
    [&ac_qlookup_Q3, &ac_qlookup_10_Q3, &ac_qlookup_12_Q3];
  let bd = ((bit_depth ^ 8) >> 1).min(2);
  ac_q[bd][((qindex as isize + delta_q as isize).max(0) as usize).min(255)]
}

// TODO: Handle lossless properly.
fn select_qi(quantizer: i64, qlookup: &[NonZeroU16; QINDEX_RANGE]) -> u8 {
  if quantizer < qlookup[MINQ].get() as i64 {
    MINQ as u8
  } else if quantizer >= qlookup[MAXQ].get() as i64 {
    MAXQ as u8
  } else {
    match qlookup
      .binary_search(&NonZeroU16::new(quantizer as u16).expect("Not zero"))
    {
      Ok(qi) => qi as u8,
      Err(qi) => {
        debug_assert!(qi > MINQ);
        debug_assert!(qi <= MAXQ);
        // Pick the closest quantizer in the log domain.
        let qthresh =
          (qlookup[qi - 1].get() as i32) * (qlookup[qi].get() as i32);
        let q2_i32 = (quantizer as i32) * (quantizer as i32);
        if q2_i32 < qthresh {
          (qi - 1) as u8
        } else {
          qi as u8
        }
      }
    }
  }
}

pub fn select_dc_qi(quantizer: i64, bit_depth: usize) -> u8 {
  let qlookup = match bit_depth {
    8 => &dc_qlookup_Q3,
    10 => &dc_qlookup_10_Q3,
    12 => &dc_qlookup_12_Q3,
    _ => unimplemented!(),
  };
  select_qi(quantizer, qlookup)
}

pub fn select_ac_qi(quantizer: i64, bit_depth: usize) -> u8 {
  let qlookup = match bit_depth {
    8 => &ac_qlookup_Q3,
    10 => &ac_qlookup_10_Q3,
    12 => &ac_qlookup_12_Q3,
    _ => unimplemented!(),
  };
  select_qi(quantizer, qlookup)
}

#[derive(Debug, Clone, Copy)]
pub struct QuantizationContext {
  log_tx_scale: usize,
  dc_quant: NonZeroU16,
  dc_offset: u32,
  dc_mul_add: (u32, u32, u32),

  ac_quant: NonZeroU16,
  ac_offset_eob: u32,
  ac_offset0: u32,
  ac_offset1: u32,
  ac_mul_add: (u32, u32, u32),

  /// Per coding plane, the quantizers its last [`Self::update_plane`] derived.
  planes: [PlaneQuantizers; 3],
}

/// The quantizers and their division constants for one `(qindex, dc_delta_q,
/// ac_delta_q, bit depth)`, as cached by [`QuantizationContext::update_plane`].
#[derive(Debug, Clone, Copy)]
struct PlaneQuantizers {
  /// The inputs, packed by [`PlaneQuantizers::key`], or `u32::MAX` for none.
  key: u32,
  dc_quant: NonZeroU16,
  dc_mul_add: (u32, u32, u32),
  ac_quant: NonZeroU16,
  ac_mul_add: (u32, u32, u32),
  /// [`QuantizationContext::offsets`] for inter (0) and intra (1).
  offsets: [[u32; 4]; 2],
}

impl PlaneQuantizers {
  const NONE: Self = PlaneQuantizers {
    key: u32::MAX,
    dc_quant: NonZeroU16::MIN,
    dc_mul_add: (0, 0, 0),
    ac_quant: NonZeroU16::MIN,
    ac_mul_add: (0, 0, 0),
    offsets: [[0; 4]; 2],
  };

  /// Everything the quantizers depend on: `qindex`, the deltas, and the
  /// bit-depth table index that [`dc_q`] and [`ac_q`] select. Below 2^26, so
  /// never [`Self::NONE`]'s key.
  #[inline(always)]
  fn key(qindex: u8, dc_delta_q: i8, ac_delta_q: i8, bit_depth: usize) -> u32 {
    let bd = ((bit_depth ^ 8) >> 1).min(2);
    u32::from(qindex)
      | u32::from(dc_delta_q as u8) << 8
      | u32::from(ac_delta_q as u8) << 16
      | (bd as u32) << 24
  }

  #[inline(never)]
  fn derive(
    key: u32, qindex: u8, bit_depth: usize, dc_delta_q: i8, ac_delta_q: i8,
  ) -> Self {
    let dc_quant = dc_q(qindex, dc_delta_q, bit_depth);
    let ac_quant = ac_q(qindex, ac_delta_q, bit_depth);
    PlaneQuantizers {
      key,
      dc_quant,
      dc_mul_add: divu_gen(dc_quant.into()),
      ac_quant,
      ac_mul_add: divu_gen(ac_quant.into()),
      offsets: [false, true].map(|is_intra| {
        QuantizationContext::offsets(dc_quant, ac_quant, is_intra)
      }),
    }
  }
}

impl Default for QuantizationContext {
  fn default() -> Self {
    QuantizationContext {
      dc_quant: NonZeroU16::new(1).expect("Not zero"),
      ac_quant: NonZeroU16::new(1).expect("Not zero"),
      log_tx_scale: Default::default(),
      dc_offset: Default::default(),
      dc_mul_add: Default::default(),
      ac_offset_eob: Default::default(),
      ac_offset0: Default::default(),
      ac_offset1: Default::default(),
      ac_mul_add: Default::default(),
      planes: [PlaneQuantizers::NONE; 3],
    }
  }
}

fn divu_gen(d: NonZeroU32) -> (u32, u32, u32) {
  let nbits = (mem::size_of_val(&d) as u64) * 8;
  let m = nbits - d.leading_zeros() as u64 - 1;
  if d.is_power_of_two() {
    (0xFFFF_FFFF, 0xFFFF_FFFF, m as u32)
  } else {
    let d = NonZeroU64::from(d);
    let t = (1u64 << (m + nbits)) / d;

    let d = d.get();
    let r = (t * d + d) & ((1 << nbits) - 1);
    if r <= 1u64 << m {
      (t as u32 + 1, 0u32, m as u32)
    } else {
      (t as u32, t as u32, m as u32)
    }
  }
}

#[inline]
const fn divu_pair(x: u32, d: (u32, u32, u32)) -> u32 {
  let x = x as u64;
  let (a, b, shift) = d;
  let shift = shift as u64;
  let a = a as u64;
  let b = b as u64;

  // One shift: `shift` is floor(log2(d)) <= 31 (see `divu_gen`), so `32 + shift`
  // is always a valid u64 shift and equals shifting by 32 and then by `shift`.
  ((a * x + b) >> (32 + shift)) as u32
}

/// The highest scan index (`iscan`) among coefficients at least `deadzone` in
/// magnitude, or 0.
#[inline(always)]
fn max_scan_index_over<T: Coefficient>(
  iscan: &[u16], coeffs: &[T], deadzone: T,
) -> u16 {
  iscan
    .iter()
    .zip(coeffs)
    .map(|(&i, &c)| if c.abs() >= deadzone { i } else { 0 })
    .max()
    .unwrap_or(0)
}

/// [`max_scan_index_over`] compiled with AVX2 enabled, where LLVM vectorizes the
/// reduction (unsigned 16-bit max is not in the x86-64 baseline).
#[cfg(nasm_x86_64)]
#[target_feature(enable = "avx2")]
unsafe fn max_scan_index_over_avx2<T: Coefficient>(
  iscan: &[u16], coeffs: &[T], deadzone: T,
) -> u16 {
  max_scan_index_over(iscan, coeffs, deadzone)
}

#[inline]
const fn copysign(value: u32, signed: i32) -> i32 {
  if signed < 0 {
    -(value as i32)
  } else {
    value as i32
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::transform::TxSize::*;

  #[test]
  fn test_divu_pair() {
    for d in 1..1024 {
      for x in 0..1000 {
        let ab = divu_gen(NonZeroU32::new(d).unwrap());
        assert_eq!(x / d, divu_pair(x, ab));
      }
    }
  }
  #[test]
  fn gen_divu_table() {
    let b: Vec<(u32, u32, u32)> =
      dc_qlookup_Q3.iter().map(|&v| divu_gen(v.into())).collect();

    println!("{:?}", b);
  }
  #[test]
  fn test_tx_log_scale() {
    let tx_sizes = [
      (TX_4X4, 0),
      (TX_8X8, 0),
      (TX_16X16, 0),
      (TX_32X32, 1),
      (TX_64X64, 2),
      (TX_4X8, 0),
      (TX_8X4, 0),
      (TX_8X16, 0),
      (TX_16X8, 0),
      (TX_16X32, 1),
      (TX_32X16, 1),
      (TX_32X64, 2),
      (TX_64X32, 2),
      (TX_4X16, 0),
      (TX_16X4, 0),
      (TX_8X32, 0),
      (TX_32X8, 0),
      (TX_16X64, 1),
      (TX_64X16, 1),
    ];
    for &tx_size in tx_sizes.iter() {
      assert!(tx_size.1 == get_log_tx_scale(tx_size.0));
    }
  }
}

impl QuantizationContext {
  pub fn update(
    &mut self, qindex: u8, tx_size: TxSize, is_intra: bool, bit_depth: usize,
    dc_delta_q: i8, ac_delta_q: i8,
  ) {
    self.log_tx_scale = get_log_tx_scale(tx_size);

    self.dc_quant = dc_q(qindex, dc_delta_q, bit_depth);
    self.dc_mul_add = divu_gen(self.dc_quant.into());

    self.ac_quant = ac_q(qindex, ac_delta_q, bit_depth);
    self.ac_mul_add = divu_gen(self.ac_quant.into());

    self.set_offsets(is_intra);
  }

  /// [`Self::update`] for coding plane `plane` (0 luma, 1 and 2 chroma).
  ///
  /// The quantizers and their division constants (two table lookups and two
  /// 64-bit divisions) depend only on `qindex`, the deltas and the bit depth,
  /// which are per frame apart from `qindex`, while the planes are coded in
  /// turn with different deltas. So each plane keeps what it last derived and
  /// reuses it while its inputs are unchanged.
  #[inline]
  pub fn update_plane(
    &mut self, plane: usize, qindex: u8, tx_size: TxSize, is_intra: bool,
    bit_depth: usize, dc_delta_q: i8, ac_delta_q: i8,
  ) {
    let key = PlaneQuantizers::key(qindex, dc_delta_q, ac_delta_q, bit_depth);
    let q = &mut self.planes[plane];
    if q.key != key {
      *q = PlaneQuantizers::derive(
        key, qindex, bit_depth, dc_delta_q, ac_delta_q,
      );
    }
    let q = *q;
    self.log_tx_scale = get_log_tx_scale(tx_size);
    self.dc_quant = q.dc_quant;
    self.dc_mul_add = q.dc_mul_add;
    self.ac_quant = q.ac_quant;
    self.ac_mul_add = q.ac_mul_add;
    [self.dc_offset, self.ac_offset0, self.ac_offset1, self.ac_offset_eob] =
      q.offsets[is_intra as usize];
  }

  /// The rounding offsets, from the quantizers and `is_intra`.
  #[inline(always)]
  fn set_offsets(&mut self, is_intra: bool) {
    [self.dc_offset, self.ac_offset0, self.ac_offset1, self.ac_offset_eob] =
      Self::offsets(self.dc_quant, self.ac_quant, is_intra);
  }

  /// The rounding offsets `[dc, ac0, ac1, ac_eob]` for the quantizers and
  /// `is_intra`.
  #[inline(always)]
  fn offsets(
    dc_quant: NonZeroU16, ac_quant: NonZeroU16, is_intra: bool,
  ) -> [u32; 4] {
    // All of these biases were derived by measuring the cost of coding
    // a zero vs coding a one on any given coefficient position, or, in
    // the case of the EOB bias, the cost of coding the block with
    // the chosen EOB (rounding to one) vs rounding to zero and continuing
    // to choose a new EOB. This was done over several clips, with the
    // average of the bit costs taken over all blocks in the set, and a new
    // bias derived via the method outlined in Jean-Marc Valin's
    // Journal of Dubious Theoretical Results[1], aka:
    //
    // lambda = ln(2) / 6.0
    // threshold = 0.5 + (lambda * avg_rate_diff) / 2.0
    // bias = 1 - threshold
    //
    // lambda is a constant since our offsets are already adjusted for the
    // quantizer.
    //
    // Biases were then updated, and cost collection was re-run, until
    // the calculated biases started to converge after 2-4 iterations.
    //
    // In theory, the rounding biases for inter should be somewhat smaller
    // than the biases for intra, but this turns out to only be the case
    // for EOB optimization, or at least, is covered by EOB optimization.
    // The RD-optimal rounding biases for the actual coefficients seem
    // to be quite close (+/- 1/256), for both inter and intra,
    // post-deadzoning.
    //
    // [1] https://jmvalin.ca/notes/theoretical_results.pdf
    [
      dc_quant.get() as u32 * (if is_intra { 109 } else { 108 }) / 256,
      ac_quant.get() as u32 * (if is_intra { 98 } else { 97 }) / 256,
      ac_quant.get() as u32 * (if is_intra { 109 } else { 108 }) / 256,
      ac_quant.get() as u32 * (if is_intra { 88 } else { 44 }) / 256,
    ]
  }

  #[inline]
  pub fn quantize<T: Coefficient>(
    &self, coeffs: &[T], qcoeffs: &mut [T], tx_size: TxSize, tx_type: TxType,
    cpu: crate::cpu_features::CpuFeatureLevel,
  ) -> u16 {
    let _prof = crate::prof::scope(crate::prof::Stage::Quantize);
    let scan = av1_scan_orders[tx_size as usize][tx_type as usize].scan;
    let iscan = av1_scan_orders[tx_size as usize][tx_type as usize].iscan;

    qcoeffs[0] = {
      let coeff: i32 = i32::cast_from(coeffs[0]) << self.log_tx_scale;
      let abs_coeff = coeff.unsigned_abs();
      T::cast_from(copysign(
        divu_pair(abs_coeff + self.dc_offset, self.dc_mul_add),
        coeff,
      ))
    };

    // Find the last non-zero coefficient using our smaller biases and
    // zero everything else.
    // This threshold is such that `abs(coeff) < deadzone` implies:
    // (abs(coeff << log_tx_scale) + ac_offset_eob) / ac_quant == 0
    let deadzone = T::cast_from(
      (self.ac_quant.get() as usize - self.ac_offset_eob as usize)
        .align_power_of_two_and_shift(self.log_tx_scale),
    );
    let eob = {
      let _q1 = crate::prof::scope(crate::prof::Stage::QuantEobScan);
      #[cfg(nasm_x86_64)]
      let eob_minus_one =
        if cpu >= crate::cpu_features::CpuFeatureLevel::AVX2 {
          // SAFETY: AVX2 is available, checked just above.
          unsafe { max_scan_index_over_avx2(iscan, coeffs, deadzone) }
        } else {
          max_scan_index_over(iscan, coeffs, deadzone)
        };
      #[cfg(not(nasm_x86_64))]
      let eob_minus_one = {
        let _ = cpu;
        max_scan_index_over(iscan, coeffs, deadzone)
      };
      // We skip the DC coefficient since it has its own quantizer index.
      if eob_minus_one > 0 {
        eob_minus_one + 1
      } else {
        u16::from(qcoeffs[0] != T::cast_from(0))
      }
    };

    // Here we use different rounding biases depending on whether we've
    // had recent coefficients that are larger than one, or less than
    // one. The reason for this is that a block usually has a chunk of
    // large coefficients and a tail of zeroes and ones, and the tradeoffs
    // for coding these two are different. In the tail of zeroes and ones,
    // you'll likely end up spending most bits just saying where that
    // coefficient is in the block, whereas in the chunk of larger
    // coefficients, most bits will be spent on coding its magnitude.
    // To that end, we want to bias more toward rounding to zero for
    // that tail of zeroes and ones than we do for the larger coefficients.
    let _q2 = crate::prof::scope(crate::prof::Stage::QuantMainLoop);
    // The AC positions in scan order, as a plain slice: the same positions as
    // `scan.iter().take(eob).skip(1)` without the adapters' per-step bookkeeping.
    let ac_scan = scan.get(1..usize::from(eob).min(scan.len())).unwrap_or(&[]);
    // Every scan has a power-of-two length (the coded transform area) and holds
    // positions below it, so with both buffers sliced to that length, masking a
    // position with `n - 1` changes nothing but proves the index in bounds.
    let n = scan.len();
    debug_assert!(n.is_power_of_two());
    let mask = n - 1;
    let coeffs = &coeffs[..n];
    let qcoeffs = &mut qcoeffs[..n];
    // prom_av1e014 tried a two-pass split here (precompute both rounding
    // candidates, resolve the level_mode recurrence separately): REFUTED,
    // +39% slower — OoO already hides the 1-bit carried chain, and the
    // doubled rounding-compare muls + scratch traffic are pure cost.
    if crate::racecar::on() {
      // Brick Q2 (docs/entropy-bricks.md): the loop is inherently serial
      // (`level_mode` is loop-carried) so it can't vectorize, but the two
      // data-dependent branches (offset select + level_mode update) mispredict
      // on coefficient magnitude. Both are rewritten branchless — byte-identical
      // (see the case analysis in the comments) — to remove the mispredict tax.
      let mut level_mode: u32 = 1;
      let ac_quant = self.ac_quant.get() as u32;
      let ac_doffset = self.ac_offset1 - self.ac_offset0;
      for &pos in ac_scan {
        let coeff = i32::cast_from(coeffs[pos as usize & mask]) << self.log_tx_scale;
        let abs_coeff = coeff.unsigned_abs();

        let level0 = divu_pair(abs_coeff, self.ac_mul_add);
        // offset1 iff level0 > 1 - level_mode, else offset0 (ac_offset1 >= offset0).
        let offset =
          self.ac_offset0 + (level0 > 1 - level_mode) as u32 * ac_doffset;

        let abs_qcoeff: u32 =
          level0 + (abs_coeff + offset >= (level0 + 1) * ac_quant) as u32;
        // level_mode := 0 if abs_qcoeff==0, 1 if abs_qcoeff>1, else unchanged.
        level_mode =
          if abs_qcoeff == 1 { level_mode } else { (abs_qcoeff > 1) as u32 };

        qcoeffs[pos as usize & mask] = T::cast_from(copysign(abs_qcoeff, coeff));
      }
    } else {
      // Normal mode (--racecar off): the stock rav1e branchy loop, verbatim
      // from pre-Q2 (560e8e52~1). Byte-identical to the branchless path —
      // this exists only for single-binary A/B.
      let mut level_mode = 1;
      let ac_quant = self.ac_quant.get() as u32;
      for &pos in ac_scan {
        let coeff = i32::cast_from(coeffs[pos as usize & mask]) << self.log_tx_scale;
        let abs_coeff = coeff.unsigned_abs();

        let level0 = divu_pair(abs_coeff, self.ac_mul_add);
        let offset = if level0 > 1 - level_mode {
          self.ac_offset1
        } else {
          self.ac_offset0
        };

        let abs_qcoeff: u32 =
          level0 + (abs_coeff + offset >= (level0 + 1) * ac_quant) as u32;
        if level_mode != 0 && abs_qcoeff == 0 {
          level_mode = 0;
        } else if abs_qcoeff > 1 {
          level_mode = 1;
        }

        qcoeffs[pos as usize & mask] = T::cast_from(copysign(abs_qcoeff, coeff));
      }
    }

    // Rather than zeroing the tail in scan order, assume that qcoeffs is
    // pre-filled with zeros.

    // Check the eob is correct
    debug_assert_eq!(
      usize::from(eob),
      scan
        .iter()
        .rposition(|&i| qcoeffs[i as usize] != T::cast_from(0))
        .map(|n| n + 1)
        .unwrap_or(0)
    );

    eob
  }
}

pub mod rust {
  use super::*;
  use crate::cpu_features::CpuFeatureLevel;
  use std::mem::MaybeUninit;

  pub fn dequantize<T: Coefficient>(
    qindex: u8, coeffs: &[T], _eob: u16, rcoeffs: &mut [MaybeUninit<T>],
    tx_size: TxSize, bit_depth: usize, dc_delta_q: i8, ac_delta_q: i8,
    _cpu: CpuFeatureLevel,
  ) {
    let log_tx_scale = get_log_tx_scale(tx_size) as i32;
    let offset = (1 << log_tx_scale) - 1;

    let dc_quant = dc_q(qindex, dc_delta_q, bit_depth).get() as i32;
    let ac_quant = ac_q(qindex, ac_delta_q, bit_depth).get() as i32;

    for (i, (r, c)) in rcoeffs
      .iter_mut()
      .zip(coeffs.iter().map(|&c| i32::cast_from(c)))
      .enumerate()
    {
      let quant = if i == 0 { dc_quant } else { ac_quant };
      r.write(T::cast_from(
        (c * quant + ((c >> 31) & offset)) >> log_tx_scale,
      ));
    }
  }
}
