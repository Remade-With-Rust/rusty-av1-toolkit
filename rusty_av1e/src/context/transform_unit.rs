// Copyright (c) 2017-2022, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.

use super::*;
use crate::predict::PredictionMode;
use crate::predict::PredictionMode::*;
use crate::transform::TxType::*;
use std::mem::MaybeUninit;

pub const MAX_TX_SIZE: usize = 64;

pub const MAX_CODED_TX_SIZE: usize = 32;
pub const MAX_CODED_TX_SQUARE: usize = MAX_CODED_TX_SIZE * MAX_CODED_TX_SIZE;

pub const TX_SIZE_SQR_CONTEXTS: usize = 4; // Coded tx_size <= 32x32, so is the # of CDF contexts from tx sizes

pub const TX_SETS: usize = 6;
pub const TX_SETS_INTRA: usize = 3;
pub const TX_SETS_INTER: usize = 4;

pub const INTRA_MODES: usize = 13;
pub const UV_INTRA_MODES: usize = 14;

const MAX_VARTX_DEPTH: usize = 2;

pub const TXFM_PARTITION_CONTEXTS: usize =
  (TxSize::TX_SIZES - TxSize::TX_8X8 as usize) * 6 - 3;

// Number of transform types in each set type
pub static num_tx_set: [usize; TX_SETS] = [1, 2, 5, 7, 12, 16];
pub static av1_tx_used: [[usize; TX_TYPES]; TX_SETS] = [
  [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  [1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0],
  [1, 1, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0],
  [1, 1, 1, 1, 0, 0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0],
  [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0],
  [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
];

// Maps set types above to the indices used for intra
static tx_set_index_intra: [i8; TX_SETS] = [0, -1, 2, 1, -1, -1];
// Maps set types above to the indices used for inter
static tx_set_index_inter: [i8; TX_SETS] = [0, 3, -1, -1, 2, 1];

pub static av1_tx_ind: [[usize; TX_TYPES]; TX_SETS] = [
  [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  [1, 3, 4, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  [1, 5, 6, 4, 0, 0, 0, 0, 0, 0, 2, 3, 0, 0, 0, 0],
  [3, 4, 5, 8, 6, 7, 9, 10, 11, 0, 1, 2, 0, 0, 0, 0],
  [7, 8, 9, 12, 10, 11, 13, 14, 15, 0, 1, 2, 3, 4, 5, 6],
];

pub static max_txsize_rect_lookup: [TxSize; BlockSize::BLOCK_SIZES_ALL] = [
  TX_4X4,   // 4x4
  TX_4X8,   // 4x8
  TX_8X4,   // 8x4
  TX_8X8,   // 8x8
  TX_8X16,  // 8x16
  TX_16X8,  // 16x8
  TX_16X16, // 16x16
  TX_16X32, // 16x32
  TX_32X16, // 32x16
  TX_32X32, // 32x32
  TX_32X64, // 32x64
  TX_64X32, // 64x32
  TX_64X64, // 64x64
  TX_64X64, // 64x128
  TX_64X64, // 128x64
  TX_64X64, // 128x128
  TX_4X16,  // 4x16
  TX_16X4,  // 16x4
  TX_8X32,  // 8x32
  TX_32X8,  // 32x8
  TX_16X64, // 16x64
  TX_64X16, // 64x16
];

pub static sub_tx_size_map: [TxSize; TxSize::TX_SIZES_ALL] = [
  TX_4X4,   // TX_4X4
  TX_4X4,   // TX_8X8
  TX_8X8,   // TX_16X16
  TX_16X16, // TX_32X32
  TX_32X32, // TX_64X64
  TX_4X4,   // TX_4X8
  TX_4X4,   // TX_8X4
  TX_8X8,   // TX_8X16
  TX_8X8,   // TX_16X8
  TX_16X16, // TX_16X32
  TX_16X16, // TX_32X16
  TX_32X32, // TX_32X64
  TX_32X32, // TX_64X32
  TX_4X8,   // TX_4X16
  TX_8X4,   // TX_16X4
  TX_8X16,  // TX_8X32
  TX_16X8,  // TX_32X8
  TX_16X32, // TX_16X64
  TX_32X16, // TX_64X16
];

#[inline]
pub fn has_chroma(
  bo: TileBlockOffset, bsize: BlockSize, subsampling_x: usize,
  subsampling_y: usize, chroma_sampling: ChromaSampling,
) -> bool {
  if chroma_sampling == ChromaSampling::Cs400 {
    return false;
  };

  let bw = bsize.width_mi();
  let bh = bsize.height_mi();

  ((bo.0.x & 0x01) == 1 || (bw & 0x01) == 0 || subsampling_x == 0)
    && ((bo.0.y & 0x01) == 1 || (bh & 0x01) == 0 || subsampling_y == 0)
}

pub fn get_tx_set(
  tx_size: TxSize, is_inter: bool, use_reduced_set: bool,
) -> TxSet {
  let tx_size_sqr_up = tx_size.sqr_up();
  let tx_size_sqr = tx_size.sqr();

  if tx_size_sqr_up.block_size() > BlockSize::BLOCK_32X32 {
    return TxSet::TX_SET_DCTONLY;
  }

  if is_inter {
    if use_reduced_set || tx_size_sqr_up == TxSize::TX_32X32 {
      TxSet::TX_SET_INTER_3
    } else if tx_size_sqr == TxSize::TX_16X16 {
      TxSet::TX_SET_INTER_2
    } else {
      TxSet::TX_SET_INTER_1
    }
  } else if tx_size_sqr_up == TxSize::TX_32X32 {
    TxSet::TX_SET_DCTONLY
  } else if use_reduced_set || tx_size_sqr == TxSize::TX_16X16 {
    TxSet::TX_SET_INTRA_2
  } else {
    TxSet::TX_SET_INTRA_1
  }
}

pub fn get_tx_set_index(
  tx_size: TxSize, is_inter: bool, use_reduced_set: bool,
) -> i8 {
  let set_type = get_tx_set(tx_size, is_inter, use_reduced_set);

  if is_inter {
    tx_set_index_inter[set_type as usize]
  } else {
    tx_set_index_intra[set_type as usize]
  }
}

static intra_mode_to_tx_type_context: [TxType; INTRA_MODES] = [
  DCT_DCT,   // DC
  ADST_DCT,  // V
  DCT_ADST,  // H
  DCT_DCT,   // D45
  ADST_ADST, // D135
  ADST_DCT,  // D113
  DCT_ADST,  // D157
  DCT_ADST,  // D203
  ADST_DCT,  // D67
  ADST_ADST, // SMOOTH
  ADST_DCT,  // SMOOTH_V
  DCT_ADST,  // SMOOTH_H
  ADST_ADST, // PAETH
];

static uv2y: [PredictionMode; UV_INTRA_MODES] = [
  DC_PRED,       // UV_DC_PRED
  V_PRED,        // UV_V_PRED
  H_PRED,        // UV_H_PRED
  D45_PRED,      // UV_D45_PRED
  D135_PRED,     // UV_D135_PRED
  D113_PRED,     // UV_D113_PRED
  D157_PRED,     // UV_D157_PRED
  D203_PRED,     // UV_D203_PRED
  D67_PRED,      // UV_D67_PRED
  SMOOTH_PRED,   // UV_SMOOTH_PRED
  SMOOTH_V_PRED, // UV_SMOOTH_V_PRED
  SMOOTH_H_PRED, // UV_SMOOTH_H_PRED
  PAETH_PRED,    // UV_PAETH_PRED
  DC_PRED,       // CFL_PRED
];

pub fn uv_intra_mode_to_tx_type_context(pred: PredictionMode) -> TxType {
  intra_mode_to_tx_type_context[uv2y[pred as usize] as usize]
}

// Level Map
pub const TXB_SKIP_CONTEXTS: usize = 13;

pub const EOB_COEF_CONTEXTS: usize = 9;

const SIG_COEF_CONTEXTS_2D: usize = 26;
const SIG_COEF_CONTEXTS_1D: usize = 16;
pub const SIG_COEF_CONTEXTS_EOB: usize = 4;
pub const SIG_COEF_CONTEXTS: usize =
  SIG_COEF_CONTEXTS_2D + SIG_COEF_CONTEXTS_1D;

const COEFF_BASE_CONTEXTS: usize = SIG_COEF_CONTEXTS;
pub const DC_SIGN_CONTEXTS: usize = 3;

const BR_TMP_OFFSET: usize = 12;
const BR_REF_CAT: usize = 4;
pub const LEVEL_CONTEXTS: usize = 21;

pub const NUM_BASE_LEVELS: usize = 2;

pub const BR_CDF_SIZE: usize = 4;
pub const COEFF_BASE_RANGE: usize = 4 * (BR_CDF_SIZE - 1);

pub const COEFF_CONTEXT_BITS: usize = 6;
pub const COEFF_CONTEXT_MASK: usize = (1 << COEFF_CONTEXT_BITS) - 1;
const MAX_BASE_BR_RANGE: usize = COEFF_BASE_RANGE + NUM_BASE_LEVELS + 1;

const BASE_CONTEXT_POSITION_NUM: usize = 12;

// Pad 4 extra columns to remove horizontal availability check.
pub const TX_PAD_HOR_LOG2: usize = 2;
pub const TX_PAD_HOR: usize = 4;
// Pad 6 extra rows (2 on top and 4 on bottom) to remove vertical availability
// check.
pub const TX_PAD_TOP: usize = 2;
pub const TX_PAD_BOTTOM: usize = 4;
pub const TX_PAD_VER: usize = TX_PAD_TOP + TX_PAD_BOTTOM;
// Pad 16 extra bytes to avoid reading overflow in SIMD optimization.
const TX_PAD_END: usize = 16;
pub const TX_PAD_2D: usize = (MAX_CODED_TX_SIZE + TX_PAD_HOR)
  * (MAX_CODED_TX_SIZE + TX_PAD_VER)
  + TX_PAD_END;

const TX_CLASSES: usize = 3;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum TxClass {
  TX_CLASS_2D = 0,
  TX_CLASS_HORIZ = 1,
  TX_CLASS_VERT = 2,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum SegLvl {
  SEG_LVL_ALT_Q = 0,      /* Use alternate Quantizer .... */
  SEG_LVL_ALT_LF_Y_V = 1, /* Use alternate loop filter value on y plane vertical */
  SEG_LVL_ALT_LF_Y_H = 2, /* Use alternate loop filter value on y plane horizontal */
  SEG_LVL_ALT_LF_U = 3,   /* Use alternate loop filter value on u plane */
  SEG_LVL_ALT_LF_V = 4,   /* Use alternate loop filter value on v plane */
  SEG_LVL_REF_FRAME = 5,  /* Optional Segment reference frame */
  SEG_LVL_SKIP = 6,       /* Optional Segment (0,0) + skip mode */
  SEG_LVL_GLOBALMV = 7,
  SEG_LVL_MAX = 8,
}

pub const seg_feature_bits: [u32; SegLvl::SEG_LVL_MAX as usize] =
  [8, 6, 6, 6, 6, 3, 0, 0];

pub const seg_feature_is_signed: [bool; SegLvl::SEG_LVL_MAX as usize] =
  [true, true, true, true, true, false, false, false];

use crate::context::TxClass::*;

pub static tx_type_to_class: [TxClass; TX_TYPES] = [
  TX_CLASS_2D,    // DCT_DCT
  TX_CLASS_2D,    // ADST_DCT
  TX_CLASS_2D,    // DCT_ADST
  TX_CLASS_2D,    // ADST_ADST
  TX_CLASS_2D,    // FLIPADST_DCT
  TX_CLASS_2D,    // DCT_FLIPADST
  TX_CLASS_2D,    // FLIPADST_FLIPADST
  TX_CLASS_2D,    // ADST_FLIPADST
  TX_CLASS_2D,    // FLIPADST_ADST
  TX_CLASS_2D,    // IDTX
  TX_CLASS_VERT,  // V_DCT
  TX_CLASS_HORIZ, // H_DCT
  TX_CLASS_VERT,  // V_ADST
  TX_CLASS_HORIZ, // H_ADST
  TX_CLASS_VERT,  // V_FLIPADST
  TX_CLASS_HORIZ, // H_FLIPADST
];

pub static eob_to_pos_small: [u8; 33] = [
  0, 1, 2, // 0-2
  3, 3, // 3-4
  4, 4, 4, 4, // 5-8
  5, 5, 5, 5, 5, 5, 5, 5, // 9-16
  6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, // 17-32
];

pub static eob_to_pos_large: [u8; 17] = [
  6, // place holder
  7, // 33-64
  8, 8, // 65-128
  9, 9, 9, 9, // 129-256
  10, 10, 10, 10, 10, 10, 10, 10, // 257-512
  11, // 513-
];

pub static k_eob_group_start: [u16; 12] =
  [0, 1, 2, 3, 5, 9, 17, 33, 65, 129, 257, 513];
pub static k_eob_offset_bits: [u16; 12] = [0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

// The ctx offset table when TX is TX_CLASS_2D.
// TX col and row indices are clamped to 4

#[rustfmt::skip]
pub static av1_nz_map_ctx_offset: [[[i8; 5]; 5]; TxSize::TX_SIZES_ALL] = [
  // TX_4X4
  [
    [ 0,  1,  6,  6, 0],
    [ 1,  6,  6, 21, 0],
    [ 6,  6, 21, 21, 0],
    [ 6, 21, 21, 21, 0],
    [ 0,  0,  0,  0, 0]
  ],
  // TX_8X8
  [
    [ 0,  1,  6,  6, 21],
    [ 1,  6,  6, 21, 21],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_16X16
  [
    [ 0,  1,  6,  6, 21],
    [ 1,  6,  6, 21, 21],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_32X32
  [
    [ 0,  1,  6,  6, 21],
    [ 1,  6,  6, 21, 21],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_64X64
  [
    [ 0,  1,  6,  6, 21],
    [ 1,  6,  6, 21, 21],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_4X8
  [
    [ 0, 11, 11, 11, 0],
    [11, 11, 11, 11, 0],
    [ 6,  6, 21, 21, 0],
    [ 6, 21, 21, 21, 0],
    [21, 21, 21, 21, 0]
  ],
  // TX_8X4
  [
    [ 0, 16,  6,  6, 21],
    [16, 16,  6, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21],
    [ 0,  0,  0,  0, 0]
  ],
  // TX_8X16
  [
    [ 0, 11, 11, 11, 11],
    [11, 11, 11, 11, 11],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_16X8
  [
    [ 0, 16,  6,  6, 21],
    [16, 16,  6, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21]
  ],
  // TX_16X32
  [
    [ 0, 11, 11, 11, 11],
    [11, 11, 11, 11, 11],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_32X16
  [
    [ 0, 16,  6,  6, 21],
    [16, 16,  6, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21]
  ],
  // TX_32X64
  [
    [ 0, 11, 11, 11, 11],
    [11, 11, 11, 11, 11],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_64X32
  [
    [ 0, 16,  6,  6, 21],
    [16, 16,  6, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21]
  ],
  // TX_4X16
  [
    [ 0, 11, 11, 11, 0],
    [11, 11, 11, 11, 0],
    [ 6,  6, 21, 21, 0],
    [ 6, 21, 21, 21, 0],
    [21, 21, 21, 21, 0]
  ],
  // TX_16X4
  [
    [ 0, 16,  6,  6, 21],
    [16, 16,  6, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21],
    [ 0,  0,  0,  0, 0]
  ],
  // TX_8X32
  [
    [ 0, 11, 11, 11, 11],
    [11, 11, 11, 11, 11],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_32X8
  [
    [ 0, 16,  6,  6, 21],
    [16, 16,  6, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21]
  ],
  // TX_16X64
  [
    [ 0, 11, 11, 11, 11],
    [11, 11, 11, 11, 11],
    [ 6,  6, 21, 21, 21],
    [ 6, 21, 21, 21, 21],
    [21, 21, 21, 21, 21]
  ],
  // TX_64X16
  [
    [ 0, 16,  6,  6, 21],
    [16, 16,  6, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21],
    [16, 16, 21, 21, 21]
  ]
];

const NZ_MAP_CTX_0: usize = SIG_COEF_CONTEXTS_2D;
const NZ_MAP_CTX_5: usize = NZ_MAP_CTX_0 + 5;
const NZ_MAP_CTX_10: usize = NZ_MAP_CTX_0 + 10;

pub static nz_map_ctx_offset_1d: [usize; 32] = [
  NZ_MAP_CTX_0,
  NZ_MAP_CTX_5,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
  NZ_MAP_CTX_10,
];

const CONTEXT_MAG_POSITION_NUM: usize = 3;

static mag_ref_offset_with_txclass: [[[usize; 2]; CONTEXT_MAG_POSITION_NUM];
  3] = [
  [[0, 1], [1, 0], [1, 1]],
  [[0, 1], [1, 0], [0, 2]],
  [[0, 1], [1, 0], [2, 0]],
];

// End of Level Map

pub struct TXB_CTX {
  pub txb_skip_ctx: usize,
  pub dc_sign_ctx: usize,
}

impl ContextWriter<'_> {
  /// # Panics
  ///
  /// - If an invalid combination of `tx_type` and `tx_size` is passed
  pub fn write_tx_type<W: Writer>(
    &mut self, w: &mut W, tx_size: TxSize, tx_type: TxType,
    y_mode: PredictionMode, is_inter: bool, use_reduced_tx_set: bool,
  ) {
    let square_tx_size = tx_size.sqr();
    let tx_set = get_tx_set(tx_size, is_inter, use_reduced_tx_set);
    let num_tx_types = num_tx_set[tx_set as usize];

    if num_tx_types > 1 {
      let tx_set_index =
        get_tx_set_index(tx_size, is_inter, use_reduced_tx_set);
      assert!(tx_set_index > 0);
      assert!(av1_tx_used[tx_set as usize][tx_type as usize] != 0);

      if is_inter {
        let s = av1_tx_ind[tx_set as usize][tx_type as usize] as u32;
        if tx_set_index == 1 {
          let cdf = &self.fc.inter_tx_1_cdf[square_tx_size as usize];
          symbol_with_update!(self, w, s, cdf);
        } else if tx_set_index == 2 {
          let cdf = &self.fc.inter_tx_2_cdf[square_tx_size as usize];
          symbol_with_update!(self, w, s, cdf);
        } else {
          let cdf = &self.fc.inter_tx_3_cdf[square_tx_size as usize];
          symbol_with_update!(self, w, s, cdf);
        }
      } else {
        let intra_dir = y_mode;
        // TODO: Once use_filter_intra is enabled,
        // intra_dir =
        // fimode_to_intradir[mbmi->filter_intra_mode_info.filter_intra_mode];

        let s = av1_tx_ind[tx_set as usize][tx_type as usize] as u32;
        if tx_set_index == 1 {
          let cdf = &self.fc.intra_tx_1_cdf[square_tx_size as usize]
            [intra_dir as usize];
          symbol_with_update!(self, w, s, cdf);
        } else {
          let cdf = &self.fc.intra_tx_2_cdf[square_tx_size as usize]
            [intra_dir as usize];
          symbol_with_update!(self, w, s, cdf);
        }
      }
    }
  }

  fn get_tx_size_context(
    &self, bo: TileBlockOffset, bsize: BlockSize,
  ) -> usize {
    let max_tx_size = max_txsize_rect_lookup[bsize as usize];
    let max_tx_wide = max_tx_size.width() as u8;
    let max_tx_high = max_tx_size.height() as u8;
    let has_above = bo.0.y > 0;
    let has_left = bo.0.x > 0;
    let mut above = self.bc.above_tx_context[bo.0.x] >= max_tx_wide;
    let mut left = self.bc.left_tx_context[bo.y_in_sb()] >= max_tx_high;

    if has_above {
      let above_blk = self.bc.blocks.above_of(bo);
      if above_blk.is_inter() {
        above = (above_blk.n4_w << MI_SIZE_LOG2) >= max_tx_wide;
      };
    }
    if has_left {
      let left_blk = self.bc.blocks.left_of(bo);
      if left_blk.is_inter() {
        left = (left_blk.n4_h << MI_SIZE_LOG2) >= max_tx_high;
      };
    }
    if has_above && has_left {
      return above as usize + left as usize;
    };
    if has_above {
      return above as usize;
    };
    if has_left {
      return left as usize;
    };
    0
  }

  pub fn write_tx_size_intra<W: Writer>(
    &mut self, w: &mut W, bo: TileBlockOffset, bsize: BlockSize,
    tx_size: TxSize,
  ) {
    fn tx_size_to_depth(tx_size: TxSize, bsize: BlockSize) -> usize {
      let mut ctx_size = max_txsize_rect_lookup[bsize as usize];
      let mut depth: usize = 0;
      while tx_size != ctx_size {
        depth += 1;
        ctx_size = sub_tx_size_map[ctx_size as usize];
        debug_assert!(depth <= MAX_TX_DEPTH);
      }
      depth
    }
    fn bsize_to_max_depth(bsize: BlockSize) -> usize {
      let mut tx_size: TxSize = max_txsize_rect_lookup[bsize as usize];
      let mut depth = 0;
      while depth < MAX_TX_DEPTH && tx_size != TX_4X4 {
        depth += 1;
        tx_size = sub_tx_size_map[tx_size as usize];
        debug_assert!(depth <= MAX_TX_DEPTH);
      }
      depth
    }
    fn bsize_to_tx_size_cat(bsize: BlockSize) -> usize {
      let mut tx_size: TxSize = max_txsize_rect_lookup[bsize as usize];
      debug_assert!(tx_size != TX_4X4);
      let mut depth = 0;
      while tx_size != TX_4X4 {
        depth += 1;
        tx_size = sub_tx_size_map[tx_size as usize];
      }
      debug_assert!(depth <= MAX_TX_CATS);

      depth - 1
    }

    debug_assert!(!self.bc.blocks[bo].is_inter());
    debug_assert!(bsize > BlockSize::BLOCK_4X4);

    let tx_size_ctx = self.get_tx_size_context(bo, bsize);
    let depth = tx_size_to_depth(tx_size, bsize);

    let max_depths = bsize_to_max_depth(bsize);
    let tx_size_cat = bsize_to_tx_size_cat(bsize);

    debug_assert!(depth <= max_depths);
    debug_assert!(!tx_size.is_rect() || bsize.is_rect_tx_allowed());

    if tx_size_cat > 0 {
      let cdf = &self.fc.tx_size_cdf[tx_size_cat - 1][tx_size_ctx];
      symbol_with_update!(self, w, depth as u32, cdf);
    } else {
      let cdf = &self.fc.tx_size_8x8_cdf[tx_size_ctx];
      symbol_with_update!(self, w, depth as u32, cdf);
    }
  }

  // Based on https://aomediacodec.github.io/av1-spec/#cdf-selection-process
  // Used to decide the cdf (context) for txfm_split
  fn get_above_tx_width(
    &self, bo: TileBlockOffset, _bsize: BlockSize, _tx_size: TxSize,
    first_tx: bool,
  ) -> usize {
    let has_above = bo.0.y > 0;
    if first_tx {
      if !has_above {
        return 64;
      }
      let above_blk = self.bc.blocks.above_of(bo);
      if above_blk.skip && above_blk.is_inter() {
        return above_blk.bsize.width();
      }
    }
    self.bc.above_tx_context[bo.0.x] as usize
  }

  fn get_left_tx_height(
    &self, bo: TileBlockOffset, _bsize: BlockSize, _tx_size: TxSize,
    first_tx: bool,
  ) -> usize {
    let has_left = bo.0.x > 0;
    if first_tx {
      if !has_left {
        return 64;
      }
      let left_blk = self.bc.blocks.left_of(bo);
      if left_blk.skip && left_blk.is_inter() {
        return left_blk.bsize.height();
      }
    }
    self.bc.left_tx_context[bo.y_in_sb()] as usize
  }

  fn txfm_partition_context(
    &self, bo: TileBlockOffset, bsize: BlockSize, tx_size: TxSize, tbx: usize,
    tby: usize,
  ) -> usize {
    debug_assert!(tx_size > TX_4X4);
    debug_assert!(bsize > BlockSize::BLOCK_4X4);

    // TODO: from 2nd level partition, must know whether the tx block is the topmost(or leftmost) within a partition
    let above = (self.get_above_tx_width(bo, bsize, tx_size, tby == 0)
      < tx_size.width()) as usize;
    let left = (self.get_left_tx_height(bo, bsize, tx_size, tbx == 0)
      < tx_size.height()) as usize;

    let max_tx_size: TxSize = bsize.tx_size().sqr_up();
    let category: usize = (tx_size.sqr_up() != max_tx_size) as usize
      + (TxSize::TX_SIZES - 1 - max_tx_size as usize) * 2;

    debug_assert!(category < TXFM_PARTITION_CONTEXTS);

    category * 3 + above + left
  }

  pub fn write_tx_size_inter<W: Writer>(
    &mut self, w: &mut W, bo: TileBlockOffset, bsize: BlockSize,
    tx_size: TxSize, txfm_split: bool, tbx: usize, tby: usize, depth: usize,
  ) {
    if bo.0.x >= self.bc.blocks.cols() || bo.0.y >= self.bc.blocks.rows() {
      return;
    }
    debug_assert!(self.bc.blocks[bo].is_inter());
    debug_assert!(bsize > BlockSize::BLOCK_4X4);
    debug_assert!(!tx_size.is_rect() || bsize.is_rect_tx_allowed());

    if tx_size != TX_4X4 && depth < MAX_VARTX_DEPTH {
      let ctx = self.txfm_partition_context(bo, bsize, tx_size, tbx, tby);
      let cdf = &self.fc.txfm_partition_cdf[ctx];
      symbol_with_update!(self, w, txfm_split as u32, cdf);
    } else {
      debug_assert!(!txfm_split);
    }

    if !txfm_split {
      self.bc.update_tx_size_context(bo, tx_size.block_size(), tx_size, false);
    } else {
      // if txfm_split == true, split one level only
      let split_tx_size = sub_tx_size_map[tx_size as usize];
      let bw = bsize.width_mi() / split_tx_size.width_mi();
      let bh = bsize.height_mi() / split_tx_size.height_mi();

      for by in 0..bh {
        for bx in 0..bw {
          let tx_bo = TileBlockOffset(BlockOffset {
            x: bo.0.x + bx * split_tx_size.width_mi(),
            y: bo.0.y + by * split_tx_size.height_mi(),
          });
          self.write_tx_size_inter(
            w,
            tx_bo,
            bsize,
            split_tx_size,
            false,
            bx,
            by,
            depth + 1,
          );
        }
      }
    }
  }

  #[inline]
  pub const fn get_txsize_entropy_ctx(tx_size: TxSize) -> usize {
    (tx_size.sqr() as usize + tx_size.sqr_up() as usize + 1) >> 1
  }

  pub fn txb_init_levels<T: Coefficient>(
    &self, coeffs: &[T], height: usize, levels: &mut [u8],
    levels_stride: usize,
  ) {
    // Coefficients and levels are transposed from how they work in the spec
    for (coeffs_col, levels_col) in
      coeffs.chunks_exact(height).zip(levels.chunks_exact_mut(levels_stride))
    {
      for (coeff, level) in coeffs_col.iter().zip(levels_col) {
        *level = coeff.abs().min(T::cast_from(127)).as_();
      }
    }
  }

  // Since the coefficients and levels are transposed in relation to how they
  // work in the spec, use the log of block height in our calculations instead
  // of block width.
  #[inline]
  pub const fn get_txb_bhl(tx_size: TxSize) -> usize {
    av1_get_coded_tx_size(tx_size).height_log2()
  }

  /// Returns `(eob_pt, eob_extra)`
  ///
  /// # Panics
  ///
  /// - If `eob` is prior to the start of the group
  #[inline]
  pub fn get_eob_pos_token(eob: u16) -> (u32, u32) {
    let t = if eob < 33 {
      eob_to_pos_small[usize::from(eob)] as u32
    } else {
      let e = usize::from(cmp::min((eob - 1) >> 5, 16));
      eob_to_pos_large[e] as u32
    };
    assert!(eob as i32 >= k_eob_group_start[t as usize] as i32);
    let extra = eob as u32 - k_eob_group_start[t as usize] as u32;

    (t, extra)
  }

  pub fn get_nz_mag(levels: &[u8], bhl: usize, tx_class: TxClass) -> usize {
    // Levels are transposed from how they work in the spec

    // May version.
    // Note: AOMMIN(level, 3) is useless for decoder since level < 3.
    let mut mag = cmp::min(3, levels[1]); // { 1, 0 }
    mag += cmp::min(3, levels[(1 << bhl) + TX_PAD_HOR]); // { 0, 1 }

    if tx_class == TX_CLASS_2D {
      mag += cmp::min(3, levels[(1 << bhl) + TX_PAD_HOR + 1]); // { 1, 1 }
      mag += cmp::min(3, levels[2]); // { 2, 0 }
      mag += cmp::min(3, levels[(2 << bhl) + (2 << TX_PAD_HOR_LOG2)]); // { 0, 2 }
    } else if tx_class == TX_CLASS_VERT {
      mag += cmp::min(3, levels[2]); // { 2, 0 }
      mag += cmp::min(3, levels[3]); // { 3, 0 }
      mag += cmp::min(3, levels[4]); // { 4, 0 }
    } else {
      mag += cmp::min(3, levels[(2 << bhl) + (2 << TX_PAD_HOR_LOG2)]); // { 0, 2 }
      mag += cmp::min(3, levels[(3 << bhl) + (3 << TX_PAD_HOR_LOG2)]); // { 0, 3 }
      mag += cmp::min(3, levels[(4 << bhl) + (4 << TX_PAD_HOR_LOG2)]); // { 0, 4 }
    }

    mag as usize
  }

  // Scalar oracle for `nz_map_area_kernel` (brick B7a) — kept in-tree per the
  // optimize-codec discipline; exercised by nz_map_kernel_test and the
  // normal (--racecar off) path of `get_nz_map_contexts`.
  fn get_nz_map_ctx_from_stats(
    stats: usize,
    coeff_idx: usize, // raster order
    bhl: usize,
    tx_size: TxSize,
    tx_class: TxClass,
  ) -> usize {
    if (tx_class as u32 | coeff_idx as u32) == 0 {
      return 0;
    };

    // Coefficients are transposed from how they work in the spec
    let col: usize = coeff_idx >> bhl;
    let row: usize = coeff_idx - (col << bhl);

    let ctx = ((stats + 1) >> 1).min(4);

    ctx
      + match tx_class {
        TX_CLASS_2D => {
          // This is the algorithm to generate table av1_nz_map_ctx_offset[].
          // const int width = tx_size_wide[tx_size];
          // const int height = tx_size_high[tx_size];
          // if (width < height) {
          //   if (row < 2) return 11 + ctx;
          // } else if (width > height) {
          //   if (col < 2) return 16 + ctx;
          // }
          // if (row + col < 2) return ctx + 1;
          // if (row + col < 4) return 5 + ctx + 1;
          // return 21 + ctx;
          av1_nz_map_ctx_offset[tx_size as usize][cmp::min(row, 4)]
            [cmp::min(col, 4)] as usize
        }
        TX_CLASS_HORIZ => nz_map_ctx_offset_1d[col],
        TX_CLASS_VERT => nz_map_ctx_offset_1d[row],
      }
  }

  // Scalar oracle for `nz_map_area_kernel` (brick B7a) — kept in-tree per the
  // optimize-codec discipline; exercised by nz_map_kernel_test and the
  // normal (--racecar off) path of `get_nz_map_contexts`.
  fn get_nz_map_ctx(
    levels: &[u8], coeff_idx: usize, bhl: usize, area: usize, scan_idx: usize,
    is_eob: bool, tx_size: TxSize, tx_class: TxClass,
  ) -> usize {
    if is_eob {
      if scan_idx == 0 {
        return 0;
      }
      if scan_idx <= area / 8 {
        return 1;
      }
      if scan_idx <= area / 4 {
        return 2;
      }
      return 3;
    }

    // Levels are transposed from how they work in the spec
    let padded_idx = coeff_idx + ((coeff_idx >> bhl) << TX_PAD_HOR_LOG2);
    let stats = Self::get_nz_mag(&levels[padded_idx..], bhl, tx_class);

    Self::get_nz_map_ctx_from_stats(stats, coeff_idx, bhl, tx_size, tx_class)
  }

  /// Brick B7a (docs/entropy-bricks.md): full-area nz-map context kernel.
  ///
  /// The scalar path (`get_nz_map_ctx` per scan position) gathers a 5-point
  /// neighbour stencil per coded coefficient — measured at 870-890 ms
  /// (12-13% of encode). This kernel computes the context for EVERY raster
  /// position with column-contiguous u8 min/add loops over the padded
  /// `levels` buffer — the layout libaom designed for exactly this SIMD
  /// access pattern (see `TX_PAD_END`). The offset tables saturate (2D at
  /// row/col 4; 1D at index 2), so each column is a short scalar prologue
  /// plus a constant-offset tail.
  ///
  /// NOTE (B7a-SIMD asm inspection): LLVM does NOT auto-vectorize these loops
  /// (pure scalar codegen) — the restructure alone bought the −64%. This
  /// scalar version is the oracle and the non-AVX2 fallback; the deployed
  /// x86-64 path is `nz_map_area_kernel_avx2` below.
  ///
  /// Byte-identical to `get_nz_map_ctx` at every non-eob position — the
  /// scalar stays in-tree as the oracle (`nz_map_area_kernel_matches_scalar`)
  /// and as the sparse-block fallback.
  fn nz_map_area_kernel(
    levels: &[u8], height: usize, width: usize, tx_size: TxSize,
    tx_class: TxClass, out: &mut [u8; MAX_CODED_TX_SQUARE],
  ) {
    let stride = height + TX_PAD_HOR;

    #[inline(always)]
    fn m3(x: u8) -> u8 {
      x.min(3)
    }

    match tx_class {
      TX_CLASS_2D => {
        // stencil: {1,0} {2,0} in-column, {0,1} {1,1} col+1, {0,2} col+2
        #[inline(always)]
        fn ctx2d(col0: &[u8], col1: &[u8], col2: &[u8], r: usize) -> u8 {
          let mag = m3(col0[r + 1])
            + m3(col0[r + 2])
            + m3(col1[r])
            + m3(col1[r + 1])
            + m3(col2[r]);
          ((mag + 1) >> 1).min(4)
        }
        let offs = &av1_nz_map_ctx_offset[tx_size as usize];
        for c in 0..width {
          let lb = c * stride;
          let col0 = &levels[lb..lb + height + 2];
          let col1 = &levels[lb + stride..lb + stride + height + 1];
          let col2 = &levels[lb + 2 * stride..lb + 2 * stride + height];
          let out_col = &mut out[c * height..c * height + height];
          let mc = c.min(4);
          // per-row offsets saturate at row 4: scalar prologue, vector tail
          let pro = height.min(4);
          for r in 0..pro {
            out_col[r] = ctx2d(col0, col1, col2, r) + offs[r][mc] as u8;
          }
          let k = offs[4][mc] as u8;
          for r in pro..height {
            out_col[r] = ctx2d(col0, col1, col2, r) + k;
          }
        }
        // 2D DC: context is 0 regardless of stats
        out[0] = 0;
      }
      TX_CLASS_HORIZ => {
        // stencil: {1,0} in-column, then cols +1..+4 at the same row;
        // the 1D offset depends only on the column => constant per column
        for c in 0..width {
          let lb = c * stride;
          let col0 = &levels[lb..lb + height + 1];
          let col1 = &levels[lb + stride..lb + stride + height];
          let col2 = &levels[lb + 2 * stride..lb + 2 * stride + height];
          let col3 = &levels[lb + 3 * stride..lb + 3 * stride + height];
          let col4 = &levels[lb + 4 * stride..lb + 4 * stride + height];
          let k = nz_map_ctx_offset_1d[c] as u8;
          let out_col = &mut out[c * height..c * height + height];
          for r in 0..height {
            let mag = m3(col0[r + 1])
              + m3(col1[r])
              + m3(col2[r])
              + m3(col3[r])
              + m3(col4[r]);
            out_col[r] = ((mag + 1) >> 1).min(4) + k;
          }
        }
      }
      TX_CLASS_VERT => {
        // stencil: rows +1..+4 in-column plus {0,1} col+1;
        // the 1D offset depends on the row and saturates at index 2
        #[inline(always)]
        fn ctxv(col0: &[u8], col1: &[u8], r: usize) -> u8 {
          let mag = m3(col0[r + 1])
            + m3(col1[r])
            + m3(col0[r + 2])
            + m3(col0[r + 3])
            + m3(col0[r + 4]);
          ((mag + 1) >> 1).min(4)
        }
        for c in 0..width {
          let lb = c * stride;
          let col0 = &levels[lb..lb + height + 4];
          let col1 = &levels[lb + stride..lb + stride + height];
          let out_col = &mut out[c * height..c * height + height];
          let pro = height.min(2);
          for r in 0..pro {
            out_col[r] = ctxv(col0, col1, r) + nz_map_ctx_offset_1d[r] as u8;
          }
          let k = nz_map_ctx_offset_1d[2] as u8;
          for r in pro..height {
            out_col[r] = ctxv(col0, col1, r) + k;
          }
        }
      }
    }
  }

  /// Brick B7a-SIMD: AVX2 twin of `nz_map_area_kernel`.
  ///
  /// The coded height is ≤ 32, so ONE unaligned 32-lane ymm op covers an
  /// entire column: 5 `vpminub`(3)+`vpaddb` stencil loads, `vpavgb(mag,0)`
  /// (= exactly `(mag+1)>>1`), `vpminub`(4), `vpaddb` offset, one store.
  /// Lanes ≥ height compute garbage from pad/next-column bytes; they land in
  /// the NEXT column's `out` cells and are overwritten by that column's store
  /// (columns are written in ascending order), or fall beyond `area` inside
  /// the fixed 1024-byte array. Offset vectors (lane r = table[min(r,sat)])
  /// are precomputed per tx_size in a lazy static.
  ///
  /// # Safety
  ///
  /// Caller must ensure AVX2 is available AND that
  /// `(width+3)*(height+4) + 32 <= levels.len()` (the dispatch enforces this
  /// with a hard check). That expression is the supremum of all loads across
  /// the three class arms (HORIZ's last-column col+4 read dominates: 2D peaks
  /// at `(w+1)*stride+32`, VERT at `w*stride+32`). For the production buffer
  /// (`levels.len() = TX_PAD_2D − TX_PAD_TOP*stride = 1384 − 2*(h+4)`) it
  /// holds for every coded size — binding case 32×32: 35*36+32 = 1292 ≤ 1312,
  /// where the 16-byte `TX_PAD_END` provides the margin. All 32-byte stores
  /// stay inside `out`: `(width−1)*height + 32 ≤ 1024` for every coded size
  /// (equality only at 32×32, where the last store ends exactly at 1024).
  #[cfg(target_arch = "x86_64")]
  #[target_feature(enable = "avx2")]
  unsafe fn nz_map_area_kernel_avx2(
    levels: &[u8], height: usize, width: usize, tx_size: TxSize,
    tx_class: TxClass, out: &mut [u8; MAX_CODED_TX_SQUARE],
  ) {
    use std::arch::x86_64::*;

    let stride = height + TX_PAD_HOR;
    debug_assert!(height <= 32 && width <= 32);
    debug_assert!((width + 3) * stride + 32 <= levels.len());
    debug_assert!((width - 1) * height + 32 <= MAX_CODED_TX_SQUARE);

    // Per-tx-size offset vectors, lane r = av1_nz_map_ctx_offset[tx][min(r,4)][mc]
    // (2D) and lane r = nz_map_ctx_offset_1d[min(r,2)] (VERT), built once.
    struct OffTables {
      two_d: [[[u8; 32]; 5]; TxSize::TX_SIZES_ALL],
      vert: [u8; 32],
    }
    static OFF_TABLES: std::sync::OnceLock<OffTables> = std::sync::OnceLock::new();
    let tables = OFF_TABLES.get_or_init(|| {
      let mut t = OffTables {
        two_d: [[[0; 32]; 5]; TxSize::TX_SIZES_ALL],
        vert: [0; 32],
      };
      for ts in 0..TxSize::TX_SIZES_ALL {
        for mc in 0..5 {
          for r in 0..32 {
            t.two_d[ts][mc][r] = av1_nz_map_ctx_offset[ts][r.min(4)][mc] as u8;
          }
        }
      }
      for r in 0..32 {
        t.vert[r] = nz_map_ctx_offset_1d[r.min(2)] as u8;
      }
      t
    });

    let lp = levels.as_ptr();
    let op = out.as_mut_ptr();
    let zero = _mm256_setzero_si256();
    let three = _mm256_set1_epi8(3);
    let four = _mm256_set1_epi8(4);

    // min(3, 32 bytes at p)
    macro_rules! m3 {
      ($p:expr) => {
        _mm256_min_epu8(_mm256_loadu_si256($p as *const __m256i), three)
      };
    }

    match tx_class {
      TX_CLASS_2D => {
        // stencil: {1,0} {2,0} in-column, {0,1} {1,1} col+1, {0,2} col+2
        let offs = &tables.two_d[tx_size as usize];
        for c in 0..width {
          let p0 = lp.add(c * stride);
          let p1 = p0.add(stride);
          let p2 = p1.add(stride);
          let mag = _mm256_add_epi8(
            _mm256_add_epi8(
              _mm256_add_epi8(m3!(p0.add(1)), m3!(p0.add(2))),
              _mm256_add_epi8(m3!(p1), m3!(p1.add(1))),
            ),
            m3!(p2),
          );
          let base = _mm256_min_epu8(_mm256_avg_epu8(mag, zero), four);
          let off =
            _mm256_loadu_si256(offs[c.min(4)].as_ptr() as *const __m256i);
          _mm256_storeu_si256(
            op.add(c * height) as *mut __m256i,
            _mm256_add_epi8(base, off),
          );
        }
        // 2D DC: context is 0 regardless of stats (no later column rewrites
        // out[0] — stores only move forward)
        out[0] = 0;
      }
      TX_CLASS_HORIZ => {
        // stencil: {1,0} in-column + cols +1..+4 same row; offset is
        // column-constant
        for c in 0..width {
          let p0 = lp.add(c * stride);
          let mag = _mm256_add_epi8(
            _mm256_add_epi8(
              _mm256_add_epi8(m3!(p0.add(1)), m3!(p0.add(stride))),
              _mm256_add_epi8(m3!(p0.add(2 * stride)), m3!(p0.add(3 * stride))),
            ),
            m3!(p0.add(4 * stride)),
          );
          let base = _mm256_min_epu8(_mm256_avg_epu8(mag, zero), four);
          let off = _mm256_set1_epi8(nz_map_ctx_offset_1d[c] as i8);
          _mm256_storeu_si256(
            op.add(c * height) as *mut __m256i,
            _mm256_add_epi8(base, off),
          );
        }
      }
      TX_CLASS_VERT => {
        // stencil: rows +1..+4 in-column + {0,1} col+1; offset saturates at
        // row 2
        let off =
          _mm256_loadu_si256(tables.vert.as_ptr() as *const __m256i);
        for c in 0..width {
          let p0 = lp.add(c * stride);
          let mag = _mm256_add_epi8(
            _mm256_add_epi8(
              _mm256_add_epi8(m3!(p0.add(1)), m3!(p0.add(2))),
              _mm256_add_epi8(m3!(p0.add(3)), m3!(p0.add(4))),
            ),
            m3!(p0.add(stride)),
          );
          let base = _mm256_min_epu8(_mm256_avg_epu8(mag, zero), four);
          _mm256_storeu_si256(
            op.add(c * height) as *mut __m256i,
            _mm256_add_epi8(base, off),
          );
        }
      }
    }
  }

  /// B7a dispatch: cached AVX2 detection (honours `RAV1E_CPU_TARGET` via
  /// `CpuFeatureLevel::default()`, evaluated once).
  #[inline(always)]
  fn nz_map_area_kernel_dispatch(
    levels: &[u8], height: usize, width: usize, tx_size: TxSize,
    tx_class: TxClass, out: &mut [u8; MAX_CODED_TX_SQUARE],
  ) {
    #[cfg(target_arch = "x86_64")]
    {
      static HAS_AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
      // The hard length check makes the unsafe contract caller-proof in
      // release builds too (a short `levels` slice falls back to the scalar
      // kernel, which bounds-checks): one predictable branch per tx block.
      // Runtime AVX2 detection via `std` (not the `CpuFeatureLevel` enum), so this
      // fast path also compiles + runs in a `default-features = false` / no-asm
      // build, where the asm-only `CpuFeatureLevel` has no `AVX2` variant.
      if (width + 3) * (height + TX_PAD_HOR) + 32 <= levels.len()
        && *HAS_AVX2.get_or_init(|| std::is_x86_feature_detected!("avx2"))
      {
        // SAFETY: AVX2 presence and the in-bounds supremum are both checked
        // above; the full bounds derivation is on the kernel's # Safety doc.
        unsafe {
          Self::nz_map_area_kernel_avx2(
            levels, height, width, tx_size, tx_class, out,
          );
        }
        return;
      }
    }
    Self::nz_map_area_kernel(levels, height, width, tx_size, tx_class, out);
  }

  /// `coeff_contexts_no_scan` is not in the scan order.
  /// Value for `pos = scan[i]` is at `coeff[i]`, not at `coeff[pos]`.
  pub fn get_nz_map_contexts<'c>(
    &self, levels: &mut [u8], scan: &[u16], eob: u16, tx_size: TxSize,
    tx_class: TxClass, coeff_contexts_no_scan: &'c mut [MaybeUninit<i8>],
  ) -> &'c mut [i8] {
    let _prof = crate::prof::scope(crate::prof::Stage::NzMapCtx);
    let bhl = Self::get_txb_bhl(tx_size);
    let area = av1_get_coded_tx_size(tx_size).area();

    let scan = &scan[..usize::from(eob)];
    let coeffs = &mut coeff_contexts_no_scan[..usize::from(eob)];

    // Normal mode (--racecar off): the stock rav1e per-scan-position
    // stencil, verbatim from pre-B7a (434d9202~1). Byte-identical to the
    // racecar path below — this exists only for single-binary A/B.
    if !crate::racecar::on() {
      for (i, (coeff, pos)) in
        coeffs.iter_mut().zip(scan.iter().copied()).enumerate()
      {
        coeff.write(Self::get_nz_map_ctx(
          levels,
          pos as usize,
          bhl,
          area,
          i,
          i == usize::from(eob) - 1,
          tx_size,
          tx_class,
        ) as i8);
      }
      // SAFETY: every element has been initialized
      return unsafe { slice_assume_init_mut(coeffs) };
    }

    // Brick B7a: the area kernel's vectorized column loops beat the scalar
    // per-scan-position stencil at EVERY measured density (cutoff sweep
    // eob*K>=area, K=8/16/64/always: 449/338/311/320 ms) — vector throughput
    // covers the area-vs-eob work difference even for sparse blocks, so no
    // density cutoff. The scalar `get_nz_map_ctx` remains as the oracle.
    let height = 1usize << bhl;
    let width = area >> bhl;
    let mut ctx_area = [0u8; MAX_CODED_TX_SQUARE];
    Self::nz_map_area_kernel_dispatch(
      levels, height, width, tx_size, tx_class, &mut ctx_area,
    );
    let last = usize::from(eob) - 1;
    let (body, last_c) = coeffs.split_at_mut(last);
    for (coeff, &pos) in body.iter_mut().zip(&scan[..last]) {
      coeff.write(ctx_area[pos as usize] as i8);
    }
    // the eob position's context depends only on its scan index
    let eob_ctx: i8 = if last == 0 {
      0
    } else if last <= area / 8 {
      1
    } else if last <= area / 4 {
      2
    } else {
      3
    };
    last_c[0].write(eob_ctx);
    // SAFETY: every element has been initialized
    unsafe { slice_assume_init_mut(coeffs) }
  }

  pub fn get_br_ctx(
    levels: &[u8],
    coeff_idx: usize, // raster order
    bhl: usize,
    tx_class: TxClass,
  ) -> usize {
    // Coefficients and levels are transposed from how they work in the spec
    let col: usize = coeff_idx >> bhl;
    let row: usize = coeff_idx - (col << bhl);
    let stride: usize = (1 << bhl) + TX_PAD_HOR;
    let pos: usize = col * stride + row;
    let mut mag: usize = (levels[pos + 1] + levels[pos + stride]) as usize;

    match tx_class {
      TX_CLASS_2D => {
        mag += levels[pos + stride + 1] as usize;
        mag = cmp::min((mag + 1) >> 1, 6);
        if coeff_idx == 0 {
          return mag;
        }
        if (row < 2) && (col < 2) {
          return mag + 7;
        }
      }
      TX_CLASS_HORIZ => {
        mag += levels[pos + (stride << 1)] as usize;
        mag = cmp::min((mag + 1) >> 1, 6);
        if coeff_idx == 0 {
          return mag;
        }
        if col == 0 {
          return mag + 7;
        }
      }
      TX_CLASS_VERT => {
        mag += levels[pos + 2] as usize;
        mag = cmp::min((mag + 1) >> 1, 6);
        if coeff_idx == 0 {
          return mag;
        }
        if row == 0 {
          return mag + 7;
        }
      }
    }

    mag + 14
  }
}

#[cfg(test)]
mod nz_map_kernel_test {
  use super::*;
  use crate::transform::TxSize::*;
  use pretty_assertions::assert_eq;

  /// Brick B7a oracle: the full-area kernel must agree with the scalar
  /// `get_nz_map_ctx` at EVERY raster position (non-eob path), for every
  /// coded tx size and tx class, over pseudorandom levels.
  #[test]
  fn nz_map_area_kernel_matches_scalar() {
    const ALL: [TxSize; 19] = [
      TX_4X4, TX_8X8, TX_16X16, TX_32X32, TX_64X64, TX_4X8, TX_8X4,
      TX_8X16, TX_16X8, TX_16X32, TX_32X16, TX_32X64, TX_64X32, TX_4X16,
      TX_16X4, TX_8X32, TX_32X8, TX_16X64, TX_64X16,
    ];
    let classes = [TX_CLASS_2D, TX_CLASS_HORIZ, TX_CLASS_VERT];

    let mut rng: u32 = 0x1234_5678;
    let mut next = move || {
      rng ^= rng << 13;
      rng ^= rng >> 17;
      rng ^= rng << 5;
      rng
    };

    for &ts in &ALL {
      let coded = av1_get_coded_tx_size(ts);
      let height = coded.height();
      let width = coded.width();
      let area = coded.area();
      let bhl = ContextWriter::get_txb_bhl(ts);
      assert_eq!(1usize << bhl, height);

      for &tc in &classes {
        for _trial in 0..4 {
          // Build a padded levels buffer exactly like write_coeffs_lv_map:
          // zeroed, coded area filled column-wise (transposed layout).
          let mut levels_buf = [0u8; TX_PAD_2D];
          let levels =
            &mut levels_buf[TX_PAD_TOP * (height + TX_PAD_HOR)..];
          for c in 0..width {
            for r in 0..height {
              // mix of zeros (sparse) and levels up to the 127 clamp
              let v = match next() % 4 {
                0 | 1 => 0u8,
                2 => (next() % 4) as u8,
                _ => (next() % 128) as u8,
              };
              levels[c * (height + TX_PAD_HOR) + r] = v;
            }
          }

          let mut ctx_area = [0u8; MAX_CODED_TX_SQUARE];
          ContextWriter::nz_map_area_kernel(
            levels, height, width, ts, tc, &mut ctx_area,
          );

          // The production entry point (guard + cpu dispatch) must agree too.
          let mut ctx_dispatch = [0u8; MAX_CODED_TX_SQUARE];
          ContextWriter::nz_map_area_kernel_dispatch(
            levels, height, width, ts, tc, &mut ctx_dispatch,
          );

          // B7a-SIMD: the AVX2 twin must agree with the scalar kernel
          // bit-for-bit at every raster position (integer kernel gate).
          #[cfg(target_arch = "x86_64")]
          let avx2_area = if std::arch::is_x86_feature_detected!("avx2") {
            let mut a = [0u8; MAX_CODED_TX_SQUARE];
            // SAFETY: AVX2 detected; kernel bounds documented + asserted.
            unsafe {
              ContextWriter::nz_map_area_kernel_avx2(
                levels, height, width, ts, tc, &mut a,
              );
            }
            Some(a)
          } else {
            None
          };

          for pos in 0..area {
            let scalar = ContextWriter::get_nz_map_ctx(
              levels, pos, bhl, area, 1, false, ts, tc,
            );
            assert_eq!(
              ctx_area[pos] as usize,
              scalar,
              "mismatch at pos {pos} (r={}, c={}) ts={} tc={}",
              pos & (height - 1),
              pos >> bhl,
              ts as usize,
              tc as usize,
            );
            assert_eq!(
              ctx_dispatch[pos], ctx_area[pos],
              "dispatch/scalar mismatch at pos {pos} ts={} tc={}",
              ts as usize,
              tc as usize,
            );
            #[cfg(target_arch = "x86_64")]
            if let Some(a) = &avx2_area {
              assert_eq!(
                a[pos], ctx_area[pos],
                "AVX2/scalar mismatch at pos {pos} (r={}, c={}) ts={} tc={}",
                pos & (height - 1),
                pos >> bhl,
                ts as usize,
                tc as usize,
              );
            }
          }
        }
      }
    }
  }
}
