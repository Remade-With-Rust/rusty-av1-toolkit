// Copyright (c) 2018-2023, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.

use std::collections::VecDeque;
use std::io::Write;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::{fmt, io, mem};

use arg_enum_proc_macro::ArgEnum;
use arrayvec::*;
use bitstream_io::{BigEndian, BitWrite, BitWriter};
use rayon::iter::*;

use crate::activity::*;
use crate::api::*;
use crate::cdef::*;
use crate::context::*;
use crate::deblock::*;
use crate::ec::*;
use crate::frame::*;
use crate::header::*;
use crate::lrf::*;
use crate::mc::{FilterMode, MotionVector};
use crate::me::*;
use crate::partition::PartitionType::*;
use crate::partition::RefType::*;
use crate::partition::*;
use crate::predict::{
  luma_ac, AngleDelta, IntraEdgeFilterParameters, IntraParam, PredictionMode,
};
use crate::quantize::*;
use crate::rate::{
  QuantizerParameters, FRAME_SUBTYPE_I, FRAME_SUBTYPE_P, QSCALE,
};
use crate::rdo::*;
use crate::segmentation::*;
use crate::serialize::{Deserialize, Serialize};
use crate::stats::EncoderStats;
use crate::tiling::*;
use crate::transform::*;
use crate::util::*;
use crate::wasm_bindgen::*;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CDEFSearchMethod {
  PickFromQ,
  FastSearch,
  FullSearch,
}

#[inline(always)]
fn poly2(q: f32, a: f32, b: f32, c: f32, max: i32) -> i32 {
  clamp((q * q).mul_add(a, q.mul_add(b, c)).round() as i32, 0, max)
}

pub static TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];

const MAX_NUM_TEMPORAL_LAYERS: usize = 8;
const MAX_NUM_SPATIAL_LAYERS: usize = 4;
const MAX_NUM_OPERATING_POINTS: usize =
  MAX_NUM_TEMPORAL_LAYERS * MAX_NUM_SPATIAL_LAYERS;

/// Size of blocks for the importance computation, in pixels.
pub const IMPORTANCE_BLOCK_SIZE: usize =
  1 << (IMPORTANCE_BLOCK_TO_BLOCK_SHIFT + BLOCK_TO_PLANE_SHIFT);

#[derive(Debug, Clone)]
pub struct ReferenceFrame<T: Pixel> {
  pub order_hint: u32,
  pub width: u32,
  pub height: u32,
  pub render_width: u32,
  pub render_height: u32,
  pub frame: Arc<Frame<T>>,
  pub input_hres: Arc<Plane<T>>,
  pub input_qres: Arc<Plane<T>>,
  pub cdfs: CDFContext,
  pub frame_me_stats: RefMEStats,
  pub output_frameno: u64,
  pub segmentation: SegmentationState,
}

#[derive(Debug, Clone, Default)]
pub struct ReferenceFramesSet<T: Pixel> {
  pub frames: [Option<Arc<ReferenceFrame<T>>>; REF_FRAMES],
  pub deblock: [DeblockState; REF_FRAMES],
}

impl<T: Pixel> ReferenceFramesSet<T> {
  pub fn new() -> Self {
    Self { frames: Default::default(), deblock: Default::default() }
  }
}

#[wasm_bindgen]
#[derive(
  ArgEnum, Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default,
)]
#[repr(C)]
pub enum Tune {
  Psnr,
  #[default]
  Psychovisual,
}

const FRAME_ID_LENGTH: u32 = 15;
const DELTA_FRAME_ID_LENGTH: u32 = 14;

#[derive(Copy, Clone, Debug)]
pub struct Sequence {
  /// OBU Sequence header of AV1
  pub profile: u8,
  pub num_bits_width: u32,
  pub num_bits_height: u32,
  pub bit_depth: usize,
  pub chroma_sampling: ChromaSampling,
  pub chroma_sample_position: ChromaSamplePosition,
  pub pixel_range: PixelRange,
  pub color_description: Option<ColorDescription>,
  pub mastering_display: Option<MasteringDisplay>,
  pub content_light: Option<ContentLight>,
  pub max_frame_width: u32,
  pub max_frame_height: u32,
  pub frame_id_numbers_present_flag: bool,
  pub frame_id_length: u32,
  pub delta_frame_id_length: u32,
  pub use_128x128_superblock: bool,
  pub order_hint_bits_minus_1: u32,
  /// 0 - force off
  /// 1 - force on
  /// 2 - adaptive
  pub force_screen_content_tools: u32,
  /// 0 - Not to force. MV can be in 1/4 or 1/8
  /// 1 - force to integer
  /// 2 - adaptive
  pub force_integer_mv: u32,
  /// Video is a single frame still picture
  pub still_picture: bool,
  /// Use reduced header for still picture
  pub reduced_still_picture_hdr: bool,
  /// enables/disables `filter_intra`
  pub enable_filter_intra: bool,
  /// enables/disables corner/edge filtering and upsampling
  pub enable_intra_edge_filter: bool,
  /// enables/disables `interintra_compound`
  pub enable_interintra_compound: bool,
  /// enables/disables masked compound
  pub enable_masked_compound: bool,
  /// 0 - disable dual interpolation filter
  /// 1 - enable vert/horiz filter selection
  pub enable_dual_filter: bool,
  /// 0 - disable order hint, and related tools
  /// `jnt_comp`, `ref_frame_mvs`, `frame_sign_bias`
  /// if 0, `enable_jnt_comp` and
  /// `enable_ref_frame_mvs` must be set zs 0.
  pub enable_order_hint: bool,
  /// 0 - disable joint compound modes
  /// 1 - enable it
  pub enable_jnt_comp: bool,
  /// 0 - disable ref frame mvs
  /// 1 - enable it
  pub enable_ref_frame_mvs: bool,
  /// 0 - disable warped motion for sequence
  /// 1 - enable it for the sequence
  pub enable_warped_motion: bool,
  /// 0 - Disable superres for the sequence, and disable
  ///     transmitting per-frame superres enabled flag.
  /// 1 - Enable superres for the sequence, and also
  ///     enable per-frame flag to denote if superres is
  ///     enabled for that frame.
  pub enable_superres: bool,
  /// To turn on/off CDEF
  pub enable_cdef: bool,
  /// To turn on/off loop restoration
  pub enable_restoration: bool,
  /// To turn on/off larger-than-superblock loop restoration units
  pub enable_large_lru: bool,
  /// allow encoder to delay loop filter RDO/coding until after frame reconstruciton is complete
  pub enable_delayed_loopfilter_rdo: bool,
  pub operating_points_cnt_minus_1: usize,
  pub operating_point_idc: [u16; MAX_NUM_OPERATING_POINTS],
  pub display_model_info_present_flag: bool,
  pub decoder_model_info_present_flag: bool,
  pub level_idx: [u8; MAX_NUM_OPERATING_POINTS],
  /// `seq_tier` in the spec. One bit: 0 or 1.
  pub tier: [usize; MAX_NUM_OPERATING_POINTS],
  pub film_grain_params_present: bool,
  pub timing_info_present: bool,
  pub tiling: TilingInfo,
  pub time_base: Rational,
}

impl Sequence {
  /// # Panics
  ///
  /// Panics if the resulting tile sizes would be too large.
  pub fn new(config: &EncoderConfig) -> Sequence {
    let width_bits = 32 - (config.width as u32).leading_zeros();
    let height_bits = 32 - (config.height as u32).leading_zeros();
    assert!(width_bits <= 16);
    assert!(height_bits <= 16);

    let profile = if config.bit_depth == 12
      || config.chroma_sampling == ChromaSampling::Cs422
    {
      2
    } else {
      u8::from(config.chroma_sampling == ChromaSampling::Cs444)
    };

    let operating_point_idc: [u16; MAX_NUM_OPERATING_POINTS] =
      [0; MAX_NUM_OPERATING_POINTS];
    let level_idx: [u8; MAX_NUM_OPERATING_POINTS] =
      if let Some(level_idx) = config.level_idx {
        [level_idx; MAX_NUM_OPERATING_POINTS]
      } else {
        [31; MAX_NUM_OPERATING_POINTS]
      };
    let tier: [usize; MAX_NUM_OPERATING_POINTS] =
      [0; MAX_NUM_OPERATING_POINTS];

    // Restoration filters are not useful for very small frame sizes,
    // so disable them in that case.
    let enable_restoration_filters = config.width >= 32 && config.height >= 32;
    let use_128x128_superblock = false;

    let frame_rate = config.frame_rate();
    let sb_size_log2 = Self::sb_size_log2(use_128x128_superblock);

    let mut tiling = TilingInfo::from_target_tiles(
      sb_size_log2,
      config.width,
      config.height,
      frame_rate,
      TilingInfo::tile_log2(1, config.tile_cols).unwrap(),
      TilingInfo::tile_log2(1, config.tile_rows).unwrap(),
      config.chroma_sampling == ChromaSampling::Cs422,
    );

    if config.tiles > 0 {
      let mut tile_rows_log2 = 0;
      let mut tile_cols_log2 = 0;
      while (tile_rows_log2 < tiling.max_tile_rows_log2)
        || (tile_cols_log2 < tiling.max_tile_cols_log2)
      {
        tiling = TilingInfo::from_target_tiles(
          sb_size_log2,
          config.width,
          config.height,
          frame_rate,
          tile_cols_log2,
          tile_rows_log2,
          config.chroma_sampling == ChromaSampling::Cs422,
        );

        if tiling.rows * tiling.cols >= config.tiles {
          break;
        };

        if ((tiling.tile_height_sb >= tiling.tile_width_sb)
          && (tiling.tile_rows_log2 < tiling.max_tile_rows_log2))
          || (tile_cols_log2 >= tiling.max_tile_cols_log2)
        {
          tile_rows_log2 += 1;
        } else {
          tile_cols_log2 += 1;
        }
      }
    }

    Sequence {
      tiling,
      profile,
      num_bits_width: width_bits,
      num_bits_height: height_bits,
      bit_depth: config.bit_depth,
      chroma_sampling: config.chroma_sampling,
      chroma_sample_position: config.chroma_sample_position,
      pixel_range: config.pixel_range,
      color_description: config.color_description,
      mastering_display: config.mastering_display,
      content_light: config.content_light,
      max_frame_width: config.width as u32,
      max_frame_height: config.height as u32,
      frame_id_numbers_present_flag: false,
      frame_id_length: FRAME_ID_LENGTH,
      delta_frame_id_length: DELTA_FRAME_ID_LENGTH,
      use_128x128_superblock,
      order_hint_bits_minus_1: 5,
      force_screen_content_tools: if config.still_picture { 2 } else { 0 },
      force_integer_mv: 2,
      still_picture: config.still_picture,
      reduced_still_picture_hdr: config.still_picture,
      enable_filter_intra: false,
      enable_intra_edge_filter: true,
      enable_interintra_compound: false,
      enable_masked_compound: false,
      enable_dual_filter: false,
      enable_order_hint: !config.still_picture,
      enable_jnt_comp: false,
      enable_ref_frame_mvs: false,
      enable_warped_motion: false,
      enable_superres: false,
      enable_cdef: config.speed_settings.cdef && enable_restoration_filters,
      enable_restoration: config.speed_settings.lrf
        && enable_restoration_filters,
      enable_large_lru: true,
      enable_delayed_loopfilter_rdo: true,
      operating_points_cnt_minus_1: 0,
      operating_point_idc,
      display_model_info_present_flag: false,
      decoder_model_info_present_flag: false,
      level_idx,
      tier,
      film_grain_params_present: config
        .film_grain_params
        .as_ref()
        .map(|entries| !entries.is_empty())
        .unwrap_or(false),
      timing_info_present: config.enable_timing_info,
      time_base: config.time_base,
    }
  }

  pub const fn get_relative_dist(&self, a: u32, b: u32) -> i32 {
    let diff = a as i32 - b as i32;
    let m = 1 << self.order_hint_bits_minus_1;
    (diff & (m - 1)) - (diff & m)
  }

  pub fn get_skip_mode_allowed<T: Pixel>(
    &self, fi: &FrameInvariants<T>, inter_cfg: &InterConfig,
    reference_select: bool,
  ) -> bool {
    if fi.intra_only || !reference_select || !self.enable_order_hint {
      return false;
    }

    let mut forward_idx: isize = -1;
    let mut backward_idx: isize = -1;
    let mut forward_hint = 0;
    let mut backward_hint = 0;

    for i in inter_cfg.allowed_ref_frames().iter().map(|rf| rf.to_index()) {
      if let Some(ref rec) = fi.rec_buffer.frames[fi.ref_frames[i] as usize] {
        let ref_hint = rec.order_hint;

        if self.get_relative_dist(ref_hint, fi.order_hint) < 0 {
          if forward_idx < 0
            || self.get_relative_dist(ref_hint, forward_hint) > 0
          {
            forward_idx = i as isize;
            forward_hint = ref_hint;
          }
        } else if self.get_relative_dist(ref_hint, fi.order_hint) > 0
          && (backward_idx < 0
            || self.get_relative_dist(ref_hint, backward_hint) > 0)
        {
          backward_idx = i as isize;
          backward_hint = ref_hint;
        }
      }
    }

    if forward_idx < 0 {
      false
    } else if backward_idx >= 0 {
      // set skip_mode_frame
      true
    } else {
      let mut second_forward_idx: isize = -1;
      let mut second_forward_hint = 0;

      for i in inter_cfg.allowed_ref_frames().iter().map(|rf| rf.to_index()) {
        if let Some(ref rec) = fi.rec_buffer.frames[fi.ref_frames[i] as usize]
        {
          let ref_hint = rec.order_hint;

          if self.get_relative_dist(ref_hint, forward_hint) < 0
            && (second_forward_idx < 0
              || self.get_relative_dist(ref_hint, second_forward_hint) > 0)
          {
            second_forward_idx = i as isize;
            second_forward_hint = ref_hint;
          }
        }
      }

      // TODO: Set skip_mode_frame, when second_forward_idx is not less than 0.
      second_forward_idx >= 0
    }
  }

  #[inline(always)]
  const fn sb_size_log2(use_128x128_superblock: bool) -> usize {
    6 + (use_128x128_superblock as usize)
  }
}

#[derive(Debug, Clone)]
pub struct FrameState<T: Pixel> {
  pub sb_size_log2: usize,
  pub input: Arc<Frame<T>>,
  pub input_hres: Arc<Plane<T>>, // half-resolution version of input luma
  pub input_qres: Arc<Plane<T>>, // quarter-resolution version of input luma
  pub rec: Arc<Frame<T>>,
  pub cdfs: CDFContext,
  pub context_update_tile_id: usize, // tile id used for the CDFontext
  pub max_tile_size_bytes: u32,
  pub deblock: DeblockState,
  pub segmentation: SegmentationState,
  pub restoration: RestorationState,
  // Because we only reference these within a tile context,
  // these are stored per-tile for easier access.
  pub frame_me_stats: RefMEStats,
  pub enc_stats: EncoderStats,
}

impl<T: Pixel> FrameState<T> {
  pub fn new(fi: &FrameInvariants<T>) -> Self {
    // TODO(negge): Use fi.cfg.chroma_sampling when we store VideoDetails in FrameInvariants
    FrameState::new_with_frame(
      fi,
      Arc::new(Frame::new(fi.width, fi.height, fi.sequence.chroma_sampling)),
    )
  }

  /// Similar to [`FrameState::new_with_frame`], but takes an `me_stats`
  /// and `rec` to enable reusing the same underlying allocations to create
  /// a `FrameState`
  ///
  /// This function primarily exists for [`estimate_inter_costs`], and so
  /// it does not create hres or qres versions of `frame` as downscaling is
  /// somewhat expensive and are not needed for [`estimate_inter_costs`].
  pub fn new_with_frame_and_me_stats_and_rec(
    fi: &FrameInvariants<T>, frame: Arc<Frame<T>>, me_stats: RefMEStats,
    rec: Arc<Frame<T>>,
  ) -> Self {
    let rs = RestorationState::new(fi, &frame);

    let hres = Plane::new(0, 0, 0, 0, 0, 0);
    let qres = Plane::new(0, 0, 0, 0, 0, 0);

    Self {
      sb_size_log2: fi.sb_size_log2(),
      input: frame,
      input_hres: Arc::new(hres),
      input_qres: Arc::new(qres),
      rec,
      cdfs: CDFContext::new(0),
      context_update_tile_id: 0,
      max_tile_size_bytes: 0,
      deblock: Default::default(),
      segmentation: Default::default(),
      restoration: rs,
      frame_me_stats: me_stats,
      enc_stats: Default::default(),
    }
  }

  pub fn new_with_frame(
    fi: &FrameInvariants<T>, frame: Arc<Frame<T>>,
  ) -> Self {
    let rs = RestorationState::new(fi, &frame);
    let luma_width = frame.planes[0].cfg.width;
    let luma_height = frame.planes[0].cfg.height;

    let hres = frame.planes[0].downsampled(fi.width, fi.height);
    let qres = hres.downsampled(fi.width, fi.height);

    Self {
      sb_size_log2: fi.sb_size_log2(),
      input: frame,
      input_hres: Arc::new(hres),
      input_qres: Arc::new(qres),
      rec: Arc::new(Frame::new(
        luma_width,
        luma_height,
        fi.sequence.chroma_sampling,
      )),
      cdfs: CDFContext::new(0),
      context_update_tile_id: 0,
      max_tile_size_bytes: 0,
      deblock: Default::default(),
      segmentation: Default::default(),
      restoration: rs,
      frame_me_stats: FrameMEStats::new_arc_array(fi.w_in_b, fi.h_in_b),
      enc_stats: Default::default(),
    }
  }

  pub fn apply_tile_state_mut<F, R>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut TileStateMut<'_, T>) -> R,
  {
    let PlaneConfig { width, height, .. } = self.rec.planes[0].cfg;
    let sbo_0 = PlaneSuperBlockOffset(SuperBlockOffset { x: 0, y: 0 });
    let frame_me_stats = self.frame_me_stats.clone();
    let frame_me_stats = &mut *frame_me_stats.write().expect("poisoned lock");
    let ts = &mut TileStateMut::new(
      self,
      sbo_0,
      self.sb_size_log2,
      width,
      height,
      frame_me_stats,
    );

    f(ts)
  }
}

#[derive(Copy, Clone, Debug)]
pub struct DeblockState {
  pub levels: [u8; MAX_PLANES + 1], // Y vertical edges, Y horizontal, U, V
  pub sharpness: u8,
  pub deltas_enabled: bool,
  pub delta_updates_enabled: bool,
  pub ref_deltas: [i8; REF_FRAMES],
  pub mode_deltas: [i8; 2],
  pub block_deltas_enabled: bool,
  pub block_delta_shift: u8,
  pub block_delta_multi: bool,
}

impl Default for DeblockState {
  fn default() -> Self {
    DeblockState {
      levels: [8, 8, 4, 4],
      sharpness: 0,
      deltas_enabled: false, // requires delta_q_enabled
      delta_updates_enabled: false,
      ref_deltas: [1, 0, 0, 0, 0, -1, -1, -1],
      mode_deltas: [0, 0],
      block_deltas_enabled: false,
      block_delta_shift: 0,
      block_delta_multi: false,
    }
  }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct SegmentationState {
  pub enabled: bool,
  pub update_data: bool,
  pub update_map: bool,
  pub preskip: bool,
  pub last_active_segid: u8,
  pub features: [[bool; SegLvl::SEG_LVL_MAX as usize]; 8],
  pub data: [[i16; SegLvl::SEG_LVL_MAX as usize]; 8],
  pub threshold: [DistortionScale; 7],
  pub min_segment: u8,
  pub max_segment: u8,
}

impl SegmentationState {
  #[profiling::function]
  pub fn update_threshold(&mut self, base_q_idx: u8, bd: usize) {
    let base_ac_q = ac_q(base_q_idx, 0, bd).get() as u64;
    let real_ac_q = ArrayVec::<_, MAX_SEGMENTS>::from_iter(
      self.data[..=self.max_segment as usize].iter().map(|data| {
        ac_q(base_q_idx, data[SegLvl::SEG_LVL_ALT_Q as usize] as i8, bd).get()
          as u64
      }),
    );
    self.threshold.fill(DistortionScale(0));
    for ((q1, q2), threshold) in
      real_ac_q.iter().skip(1).zip(&real_ac_q).zip(&mut self.threshold)
    {
      *threshold = DistortionScale::new(base_ac_q.pow(2), q1 * q2);
    }
  }

  #[cfg(feature = "dump_lookahead_data")]
  pub fn dump_threshold(
    &self, data_location: std::path::PathBuf, input_frameno: u64,
  ) {
    use byteorder::{NativeEndian, WriteBytesExt};
    let file_name = format!("{:010}-thresholds", input_frameno);
    let max_segment = self.max_segment;
    // dynamic allocation: debugging only
    let mut buf = vec![];
    buf.write_u64::<NativeEndian>(max_segment as u64).unwrap();
    for &v in &self.threshold[..max_segment as usize] {
      buf.write_u32::<NativeEndian>(v.0).unwrap();
    }
    ::std::fs::write(data_location.join(file_name).with_extension("bin"), buf)
      .unwrap();
  }
}

// Frame Invariants are invariant inside a frame
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct FrameInvariants<T: Pixel> {
  pub sequence: Arc<Sequence>,
  pub config: Arc<EncoderConfig>,
  pub width: usize,
  pub height: usize,
  pub render_width: u32,
  pub render_height: u32,
  pub frame_size_override_flag: bool,
  pub render_and_frame_size_different: bool,
  pub sb_width: usize,
  pub sb_height: usize,
  pub w_in_b: usize,
  pub h_in_b: usize,
  pub input_frameno: u64,
  pub order_hint: u32,
  pub show_frame: bool,
  pub showable_frame: bool,
  pub error_resilient: bool,
  pub intra_only: bool,
  pub allow_high_precision_mv: bool,
  pub frame_type: FrameType,
  pub frame_to_show_map_idx: u32,
  pub use_reduced_tx_set: bool,
  pub reference_mode: ReferenceMode,
  pub use_prev_frame_mvs: bool,
  pub partition_range: PartitionRange,
  pub globalmv_transformation_type: [GlobalMVMode; INTER_REFS_PER_FRAME],
  pub num_tg: usize,
  pub large_scale_tile: bool,
  pub disable_cdf_update: bool,
  pub allow_screen_content_tools: u32,
  pub force_integer_mv: u32,
  pub primary_ref_frame: u32,
  pub refresh_frame_flags: u32, // a bitmask that specifies which
  // reference frame slots will be updated with the current frame
  // after it is decoded.
  pub allow_intrabc: bool,
  pub use_ref_frame_mvs: bool,
  pub is_filter_switchable: bool,
  pub is_motion_mode_switchable: bool,
  pub disable_frame_end_update_cdf: bool,
  pub allow_warped_motion: bool,
  pub cdef_search_method: CDEFSearchMethod,
  pub cdef_damping: u8,
  pub cdef_bits: u8,
  pub cdef_y_strengths: [u8; 8],
  pub cdef_uv_strengths: [u8; 8],
  pub delta_q_present: bool,
  pub ref_frames: [u8; INTER_REFS_PER_FRAME],
  pub ref_frame_sign_bias: [bool; INTER_REFS_PER_FRAME],
  pub rec_buffer: ReferenceFramesSet<T>,
  pub base_q_idx: u8,
  pub dc_delta_q: [i8; 3],
  pub ac_delta_q: [i8; 3],
  pub lambda: f64,
  pub me_lambda: f64,
  pub dist_scale: [DistortionScale; 3],
  pub me_range_scale: u8,
  pub use_tx_domain_distortion: bool,
  pub use_tx_domain_rate: bool,
  pub idx_in_group_output: u64,
  pub pyramid_level: u64,
  pub enable_early_exit: bool,
  pub tx_mode_select: bool,
  pub enable_inter_txfm_split: bool,
  pub default_filter: FilterMode,
  pub enable_segmentation: bool,
  pub t35_metadata: Box<[T35]>,
  /// Target CPU feature level.
  pub cpu_feature_level: crate::cpu_features::CpuFeatureLevel,

  // These will be set if this is a coded (non-SEF) frame.
  // We do not need them for SEFs.
  pub coded_frame_data: Option<CodedFrameData<T>>,
}

/// These frame invariants are only used on coded frames, i.e. non-SEFs.
/// They are stored separately to avoid useless allocations
/// when we do not need them.
///
/// Currently this consists only of lookahaed data.
/// This may change in the future.
#[derive(Debug, Clone)]
pub struct CodedFrameData<T: Pixel> {
  /// The lookahead version of `rec_buffer`, used for storing and propagating
  /// the original reference frames (rather than reconstructed ones). The
  /// lookahead uses both `rec_buffer` and `lookahead_rec_buffer`, where
  /// `rec_buffer` contains the current frame's reference frames and
  /// `lookahead_rec_buffer` contains the next frame's reference frames.
  pub lookahead_rec_buffer: ReferenceFramesSet<T>,
  /// Frame width in importance blocks.
  pub w_in_imp_b: usize,
  /// Frame height in importance blocks.
  pub h_in_imp_b: usize,
  /// Intra prediction cost estimations for each importance block.
  pub lookahead_intra_costs: Box<[u32]>,
  /// Future importance values for each importance block. That is, a value
  /// indicating how much future frames depend on the block (for example, via
  /// inter-prediction).
  pub block_importances: Box<[f32]>,
  /// Pre-computed `distortion_scale`.
  pub distortion_scales: Box<[DistortionScale]>,
  /// Pre-computed `activity_scale`.
  pub activity_scales: Box<[DistortionScale]>,
  pub activity_mask: ActivityMask,
  /// Combined metric of activity and distortion
  pub spatiotemporal_scores: Box<[DistortionScale]>,
}

impl<T: Pixel> CodedFrameData<T> {
  pub fn new(fi: &FrameInvariants<T>) -> CodedFrameData<T> {
    // Width and height are padded to 8×8 block size.
    let w_in_imp_b = fi.w_in_b / 2;
    let h_in_imp_b = fi.h_in_b / 2;

    CodedFrameData {
      lookahead_rec_buffer: ReferenceFramesSet::new(),
      w_in_imp_b,
      h_in_imp_b,
      // This is never used before it is assigned
      lookahead_intra_costs: Box::new([]),
      // dynamic allocation: once per frame
      block_importances: vec![0.; w_in_imp_b * h_in_imp_b].into_boxed_slice(),
      distortion_scales: vec![
        DistortionScale::default();
        w_in_imp_b * h_in_imp_b
      ]
      .into_boxed_slice(),
      activity_scales: vec![
        DistortionScale::default();
        w_in_imp_b * h_in_imp_b
      ]
      .into_boxed_slice(),
      activity_mask: Default::default(),
      spatiotemporal_scores: Default::default(),
    }
  }

  // Assumes that we have already computed activity scales and distortion scales
  // Returns -0.5 log2(mean(scale))
  #[profiling::function]
  pub fn compute_spatiotemporal_scores(&mut self) -> i64 {
    let mut scores = self
      .distortion_scales
      .iter()
      .zip(self.activity_scales.iter())
      .map(|(&d, &a)| d * a)
      .collect::<Box<_>>();

    let inv_mean = DistortionScale::inv_mean(&scores);

    for score in scores.iter_mut() {
      *score *= inv_mean;
    }

    for scale in self.distortion_scales.iter_mut() {
      *scale *= inv_mean;
    }

    self.spatiotemporal_scores = scores;

    inv_mean.blog64() >> 1
  }

  // Assumes that we have already computed distortion_scales
  // Returns -0.5 log2(mean(scale))
  #[profiling::function]
  pub fn compute_temporal_scores(&mut self) -> i64 {
    let inv_mean = DistortionScale::inv_mean(&self.distortion_scales);
    for scale in self.distortion_scales.iter_mut() {
      *scale *= inv_mean;
    }
    self.spatiotemporal_scores.clone_from(&self.distortion_scales);
    inv_mean.blog64() >> 1
  }

  #[cfg(feature = "dump_lookahead_data")]
  pub fn dump_scales(
    &self, data_location: std::path::PathBuf, scales: Scales,
    input_frameno: u64,
  ) {
    use byteorder::{NativeEndian, WriteBytesExt};
    let file_name = format!(
      "{:010}-{}",
      input_frameno,
      match scales {
        Scales::ActivityScales => "activity_scales",
        Scales::DistortionScales => "distortion_scales",
        Scales::SpatiotemporalScales => "spatiotemporal_scales",
      }
    );
    // dynamic allocation: debugging only
    let mut buf = vec![];
    buf.write_u64::<NativeEndian>(self.w_in_imp_b as u64).unwrap();
    buf.write_u64::<NativeEndian>(self.h_in_imp_b as u64).unwrap();
    for &v in match scales {
      Scales::ActivityScales => &self.activity_scales[..],
      Scales::DistortionScales => &self.distortion_scales[..],
      Scales::SpatiotemporalScales => &self.spatiotemporal_scores[..],
    } {
      buf.write_u32::<NativeEndian>(v.0).unwrap();
    }
    ::std::fs::write(data_location.join(file_name).with_extension("bin"), buf)
      .unwrap();
  }
}

#[cfg(feature = "dump_lookahead_data")]
pub enum Scales {
  ActivityScales,
  DistortionScales,
  SpatiotemporalScales,
}

pub(crate) const fn pos_to_lvl(pos: u64, pyramid_depth: u64) -> u64 {
  // Derive level within pyramid for a frame with a given coding order position
  // For example, with a pyramid of depth 2, the 2 least significant bits of the
  // position determine the level:
  // 00 -> 0
  // 01 -> 2
  // 10 -> 1
  // 11 -> 2
  pyramid_depth - (pos | (1 << pyramid_depth)).trailing_zeros() as u64
}

impl<T: Pixel> FrameInvariants<T> {
  #[allow(clippy::erasing_op, clippy::identity_op)]
  /// # Panics
  ///
  /// - If the size of `T` does not match the sequence's bit depth
  pub fn new(config: Arc<EncoderConfig>, sequence: Arc<Sequence>) -> Self {
    assert!(
      sequence.bit_depth <= mem::size_of::<T>() * 8,
      "bit depth cannot fit into u8"
    );

    let (width, height) = (config.width, config.height);
    let frame_size_override_flag = width as u32 != sequence.max_frame_width
      || height as u32 != sequence.max_frame_height;

    let (render_width, render_height) = config.render_size();
    let render_and_frame_size_different =
      render_width != width || render_height != height;

    let use_reduced_tx_set = config.speed_settings.transform.reduced_tx_set;
    // prom_av1e034: 2-tier funnel lever. Estimate coefficient rate from
    // tx-domain distortion in the RDO trials (TxDistEstRate) instead of
    // running the full coefficient coder (TxDistRealRate) per candidate —
    // the winner is re-coded exactly at final encode, so only the RANKING
    // uses estimated rate. Directly removes the ~19.5% in-trial entropy.
    // estimate_rate needs tx-domain distortion, so txrate forces both on
    // (they are a pair; the pixel-distortion path has no tx_dist to estimate).
    let use_tx_domain_rate = config.speed_settings.transform.tx_domain_rate
      || crate::harvest::txrate();
    let use_tx_domain_distortion = use_tx_domain_rate
      || (config.tune == Tune::Psnr
        && config.speed_settings.transform.tx_domain_distortion);

    let w_in_b = 2 * config.width.align_power_of_two_and_shift(3); // MiCols, ((width+7)/8)<<3 >> MI_SIZE_LOG2
    let h_in_b = 2 * config.height.align_power_of_two_and_shift(3); // MiRows, ((height+7)/8)<<3 >> MI_SIZE_LOG2

    Self {
      width,
      height,
      render_width: render_width as u32,
      render_height: render_height as u32,
      frame_size_override_flag,
      render_and_frame_size_different,
      sb_width: width.align_power_of_two_and_shift(6),
      sb_height: height.align_power_of_two_and_shift(6),
      w_in_b,
      h_in_b,
      input_frameno: 0,
      order_hint: 0,
      show_frame: true,
      showable_frame: !sequence.reduced_still_picture_hdr,
      error_resilient: false,
      intra_only: true,
      allow_high_precision_mv: false,
      frame_type: FrameType::KEY,
      frame_to_show_map_idx: 0,
      use_reduced_tx_set,
      reference_mode: ReferenceMode::SINGLE,
      use_prev_frame_mvs: false,
      partition_range: config.speed_settings.partition.partition_range,
      globalmv_transformation_type: [GlobalMVMode::IDENTITY;
        INTER_REFS_PER_FRAME],
      num_tg: 1,
      large_scale_tile: false,
      disable_cdf_update: false,
      allow_screen_content_tools: sequence.force_screen_content_tools,
      force_integer_mv: 1,
      primary_ref_frame: PRIMARY_REF_NONE,
      refresh_frame_flags: ALL_REF_FRAMES_MASK,
      allow_intrabc: false,
      use_ref_frame_mvs: false,
      is_filter_switchable: false,
      is_motion_mode_switchable: false, // 0: only the SIMPLE motion mode will be used.
      disable_frame_end_update_cdf: sequence.reduced_still_picture_hdr,
      allow_warped_motion: false,
      cdef_search_method: CDEFSearchMethod::PickFromQ,
      cdef_damping: 3,
      cdef_bits: 0,
      cdef_y_strengths: [
        0 * 4 + 0,
        1 * 4 + 0,
        2 * 4 + 1,
        3 * 4 + 1,
        5 * 4 + 2,
        7 * 4 + 3,
        10 * 4 + 3,
        13 * 4 + 3,
      ],
      cdef_uv_strengths: [
        0 * 4 + 0,
        1 * 4 + 0,
        2 * 4 + 1,
        3 * 4 + 1,
        5 * 4 + 2,
        7 * 4 + 3,
        10 * 4 + 3,
        13 * 4 + 3,
      ],
      delta_q_present: false,
      ref_frames: [0; INTER_REFS_PER_FRAME],
      ref_frame_sign_bias: [false; INTER_REFS_PER_FRAME],
      rec_buffer: ReferenceFramesSet::new(),
      base_q_idx: config.quantizer as u8,
      dc_delta_q: [0; 3],
      ac_delta_q: [0; 3],
      lambda: 0.0,
      dist_scale: Default::default(),
      me_lambda: 0.0,
      me_range_scale: 1,
      use_tx_domain_distortion,
      use_tx_domain_rate,
      idx_in_group_output: 0,
      pyramid_level: 0,
      // prom_av1e008 harvest aid: RAV1E_NO_EARLY_EXIT=1 disables the split
      // trial's early exit so harvested split costs are COMPLETE (regret
      // analysis needs them). Unset = stock behaviour.
      enable_early_exit: std::env::var("RAV1E_NO_EARLY_EXIT")
        .map_or(true, |v| v.trim() != "1"),
      tx_mode_select: false,
      default_filter: FilterMode::REGULAR,
      cpu_feature_level: Default::default(),
      enable_segmentation: config.speed_settings.segmentation
        != SegmentationLevel::Disabled,
      enable_inter_txfm_split: config
        .speed_settings
        .transform
        .enable_inter_tx_split,
      t35_metadata: Box::new([]),
      sequence,
      config,
      coded_frame_data: None,
    }
  }

  pub fn new_key_frame(
    config: Arc<EncoderConfig>, sequence: Arc<Sequence>,
    gop_input_frameno_start: u64, t35_metadata: Box<[T35]>,
  ) -> Self {
    let tx_mode_select = config.speed_settings.transform.rdo_tx_decision;
    let mut fi = Self::new(config, sequence);
    fi.input_frameno = gop_input_frameno_start;
    fi.tx_mode_select = tx_mode_select;
    fi.coded_frame_data = Some(CodedFrameData::new(&fi));
    fi.t35_metadata = t35_metadata;
    fi
  }

  /// Returns the created `FrameInvariants`, or `None` if this should be
  /// a placeholder frame.
  pub(crate) fn new_inter_frame(
    previous_coded_fi: &Self, inter_cfg: &InterConfig,
    gop_input_frameno_start: u64, output_frameno_in_gop: u64,
    next_keyframe_input_frameno: u64, error_resilient: bool,
    t35_metadata: Box<[T35]>,
  ) -> Option<Self> {
    let input_frameno = inter_cfg
      .get_input_frameno(output_frameno_in_gop, gop_input_frameno_start);
    if input_frameno >= next_keyframe_input_frameno {
      // This is an invalid frame. We set it as a placeholder in the FI list.
      return None;
    }

    // We have this special thin clone method to avoid cloning the
    // quite large lookahead data for SEFs, when it is not needed.
    let mut fi = previous_coded_fi.clone_without_coded_data();
    fi.intra_only = false;
    fi.force_integer_mv = 0; // note: should be 1 if fi.intra_only is true
    fi.idx_in_group_output =
      inter_cfg.get_idx_in_group_output(output_frameno_in_gop);
    fi.tx_mode_select = fi.enable_inter_txfm_split;

    let show_existing_frame =
      inter_cfg.get_show_existing_frame(fi.idx_in_group_output);
    if !show_existing_frame {
      fi.coded_frame_data.clone_from(&previous_coded_fi.coded_frame_data);
    }

    fi.order_hint =
      inter_cfg.get_order_hint(output_frameno_in_gop, fi.idx_in_group_output);

    fi.pyramid_level = inter_cfg.get_level(fi.idx_in_group_output);

    fi.frame_type = if (inter_cfg.switch_frame_interval > 0)
      && (output_frameno_in_gop % inter_cfg.switch_frame_interval == 0)
      && (fi.pyramid_level == 0)
    {
      FrameType::SWITCH
    } else {
      FrameType::INTER
    };
    fi.error_resilient =
      if fi.frame_type == FrameType::SWITCH { true } else { error_resilient };

    fi.frame_size_override_flag = if fi.frame_type == FrameType::SWITCH {
      true
    } else if fi.sequence.reduced_still_picture_hdr {
      false
    } else if fi.frame_type == FrameType::INTER
      && !fi.error_resilient
      && fi.render_and_frame_size_different
    {
      // force frame_size_with_refs() code path if render size != frame size
      true
    } else {
      fi.width as u32 != fi.sequence.max_frame_width
        || fi.height as u32 != fi.sequence.max_frame_height
    };

    // this is the slot that the current frame is going to be saved into
    let slot_idx = inter_cfg.get_slot_idx(fi.pyramid_level, fi.order_hint);
    fi.show_frame = inter_cfg.get_show_frame(fi.idx_in_group_output);
    fi.t35_metadata = if fi.show_frame { t35_metadata } else { Box::new([]) };
    fi.frame_to_show_map_idx = slot_idx;
    fi.refresh_frame_flags = if fi.frame_type == FrameType::SWITCH {
      ALL_REF_FRAMES_MASK
    } else if fi.is_show_existing_frame() {
      0
    } else {
      1 << slot_idx
    };

    let second_ref_frame =
      if fi.idx_in_group_output == 0 { LAST2_FRAME } else { ALTREF_FRAME };
    let ref_in_previous_group = LAST3_FRAME;

    // reuse probability estimates from previous frames only in top level frames
    fi.primary_ref_frame = if fi.error_resilient || (fi.pyramid_level > 2) {
      PRIMARY_REF_NONE
    } else {
      (ref_in_previous_group.to_index()) as u32
    };

    if fi.pyramid_level == 0 {
      // level 0 has no forward references
      // default to last P frame
      fi.ref_frames = [
        // calculations done relative to the slot_idx for this frame.
        // the last four frames can be found by subtracting from the current slot_idx
        // add 4 to prevent underflow
        // TODO: maybe use order_hint here like in get_slot_idx?
        // this is the previous P frame
        (slot_idx + 4 - 1) as u8 % 4
          ; INTER_REFS_PER_FRAME];
      if inter_cfg.multiref {
        // use the second-previous p frame as a second reference frame
        fi.ref_frames[second_ref_frame.to_index()] =
          (slot_idx + 4 - 2) as u8 % 4;
      }
    } else {
      debug_assert!(inter_cfg.multiref);

      // fill in defaults
      // default to backwards reference in lower level
      fi.ref_frames = [{
        let oh = fi.order_hint
          - (inter_cfg.group_input_len as u32 >> fi.pyramid_level);
        let lvl1 = pos_to_lvl(oh as u64, inter_cfg.pyramid_depth);
        if lvl1 == 0 {
          ((oh >> inter_cfg.pyramid_depth) % 4) as u8
        } else {
          3 + lvl1 as u8
        }
      }; INTER_REFS_PER_FRAME];
      // use forward reference in lower level as a second reference frame
      fi.ref_frames[second_ref_frame.to_index()] = {
        let oh = fi.order_hint
          + (inter_cfg.group_input_len as u32 >> fi.pyramid_level);
        let lvl2 = pos_to_lvl(oh as u64, inter_cfg.pyramid_depth);
        if lvl2 == 0 {
          ((oh >> inter_cfg.pyramid_depth) % 4) as u8
        } else {
          3 + lvl2 as u8
        }
      };
      // use a reference to the previous frame in the same level
      // (horizontally) as a third reference
      fi.ref_frames[ref_in_previous_group.to_index()] = slot_idx as u8;
    }

    fi.set_ref_frame_sign_bias();

    fi.reference_mode = if inter_cfg.multiref && fi.idx_in_group_output != 0 {
      ReferenceMode::SELECT
    } else {
      ReferenceMode::SINGLE
    };
    fi.input_frameno = input_frameno;
    fi.me_range_scale = (inter_cfg.group_input_len >> fi.pyramid_level) as u8;

    if fi.show_frame || fi.showable_frame {
      let cur_frame_time = fi.frame_timestamp();
      // Increment the film grain seed for the next frame
      if let Some(params) =
        Arc::make_mut(&mut fi.config).get_film_grain_mut_at(cur_frame_time)
      {
        params.random_seed = params.random_seed.wrapping_add(3248);
        if params.random_seed == 0 {
          params.random_seed = DEFAULT_GRAIN_SEED;
        }
      }
    }

    Some(fi)
  }

  pub fn is_show_existing_frame(&self) -> bool {
    self.coded_frame_data.is_none()
  }

  pub fn clone_without_coded_data(&self) -> Self {
    Self {
      coded_frame_data: None,

      sequence: self.sequence.clone(),
      config: self.config.clone(),
      width: self.width,
      height: self.height,
      render_width: self.render_width,
      render_height: self.render_height,
      frame_size_override_flag: self.frame_size_override_flag,
      render_and_frame_size_different: self.render_and_frame_size_different,
      sb_width: self.sb_width,
      sb_height: self.sb_height,
      w_in_b: self.w_in_b,
      h_in_b: self.h_in_b,
      input_frameno: self.input_frameno,
      order_hint: self.order_hint,
      show_frame: self.show_frame,
      showable_frame: self.showable_frame,
      error_resilient: self.error_resilient,
      intra_only: self.intra_only,
      allow_high_precision_mv: self.allow_high_precision_mv,
      frame_type: self.frame_type,
      frame_to_show_map_idx: self.frame_to_show_map_idx,
      use_reduced_tx_set: self.use_reduced_tx_set,
      reference_mode: self.reference_mode,
      use_prev_frame_mvs: self.use_prev_frame_mvs,
      partition_range: self.partition_range,
      globalmv_transformation_type: self.globalmv_transformation_type,
      num_tg: self.num_tg,
      large_scale_tile: self.large_scale_tile,
      disable_cdf_update: self.disable_cdf_update,
      allow_screen_content_tools: self.allow_screen_content_tools,
      force_integer_mv: self.force_integer_mv,
      primary_ref_frame: self.primary_ref_frame,
      refresh_frame_flags: self.refresh_frame_flags,
      allow_intrabc: self.allow_intrabc,
      use_ref_frame_mvs: self.use_ref_frame_mvs,
      is_filter_switchable: self.is_filter_switchable,
      is_motion_mode_switchable: self.is_motion_mode_switchable,
      disable_frame_end_update_cdf: self.disable_frame_end_update_cdf,
      allow_warped_motion: self.allow_warped_motion,
      cdef_search_method: self.cdef_search_method,
      cdef_damping: self.cdef_damping,
      cdef_bits: self.cdef_bits,
      cdef_y_strengths: self.cdef_y_strengths,
      cdef_uv_strengths: self.cdef_uv_strengths,
      delta_q_present: self.delta_q_present,
      ref_frames: self.ref_frames,
      ref_frame_sign_bias: self.ref_frame_sign_bias,
      rec_buffer: self.rec_buffer.clone(),
      base_q_idx: self.base_q_idx,
      dc_delta_q: self.dc_delta_q,
      ac_delta_q: self.ac_delta_q,
      lambda: self.lambda,
      me_lambda: self.me_lambda,
      dist_scale: self.dist_scale,
      me_range_scale: self.me_range_scale,
      use_tx_domain_distortion: self.use_tx_domain_distortion,
      use_tx_domain_rate: self.use_tx_domain_rate,
      idx_in_group_output: self.idx_in_group_output,
      pyramid_level: self.pyramid_level,
      enable_early_exit: self.enable_early_exit,
      tx_mode_select: self.tx_mode_select,
      enable_inter_txfm_split: self.enable_inter_txfm_split,
      // prom_av1e044: interp-filter ceiling probe (whole-frame fixed filter).
      default_filter: match crate::harvest::filter_probe() {
        Some(1) => crate::mc::FilterMode::SMOOTH,
        Some(2) => crate::mc::FilterMode::SHARP,
        Some(_) => crate::mc::FilterMode::REGULAR,
        None => self.default_filter,
      },
      enable_segmentation: self.enable_segmentation,
      t35_metadata: self.t35_metadata.clone(),
      cpu_feature_level: self.cpu_feature_level,
    }
  }

  pub fn set_ref_frame_sign_bias(&mut self) {
    for i in 0..INTER_REFS_PER_FRAME {
      self.ref_frame_sign_bias[i] = if !self.sequence.enable_order_hint {
        false
      } else if let Some(ref rec) =
        self.rec_buffer.frames[self.ref_frames[i] as usize]
      {
        let hint = rec.order_hint;
        self.sequence.get_relative_dist(hint, self.order_hint) > 0
      } else {
        false
      };
    }
  }

  pub fn get_frame_subtype(&self) -> usize {
    if self.frame_type == FrameType::KEY {
      FRAME_SUBTYPE_I
    } else {
      FRAME_SUBTYPE_P + (self.pyramid_level as usize)
    }
  }

  fn pick_strength_from_q(&mut self, qps: &QuantizerParameters) {
    self.cdef_damping = 3 + (self.base_q_idx >> 6);
    let q = bexp64(qps.log_target_q + q57(QSCALE)) as f32;
    /* These coefficients were trained on libaom. */
    let (y_f1, y_f2, uv_f1, uv_f2) = if !self.intra_only {
      (
        poly2(q, -0.0000023593946_f32, 0.0068615186_f32, 0.02709886_f32, 15),
        poly2(q, -0.00000057629734_f32, 0.0013993345_f32, 0.03831067_f32, 3),
        poly2(q, -0.0000007095069_f32, 0.0034628846_f32, 0.00887099_f32, 15),
        poly2(q, 0.00000023874085_f32, 0.00028223585_f32, 0.05576307_f32, 3),
      )
    } else {
      (
        poly2(q, 0.0000033731974_f32, 0.008070594_f32, 0.0187634_f32, 15),
        poly2(q, 0.0000029167343_f32, 0.0027798624_f32, 0.0079405_f32, 3),
        poly2(q, -0.0000130790995_f32, 0.012892405_f32, -0.00748388_f32, 15),
        poly2(q, 0.0000032651783_f32, 0.00035520183_f32, 0.00228092_f32, 3),
      )
    };
    self.cdef_y_strengths[0] = (y_f1 * CDEF_SEC_STRENGTHS as i32 + y_f2) as u8;
    self.cdef_uv_strengths[0] =
      (uv_f1 * CDEF_SEC_STRENGTHS as i32 + uv_f2) as u8;
  }

  pub fn set_quantizers(&mut self, qps: &QuantizerParameters) {
    self.base_q_idx = qps.ac_qi[0];
    let base_q_idx = self.base_q_idx as i32;
    for pi in 0..3 {
      self.dc_delta_q[pi] = (qps.dc_qi[pi] as i32 - base_q_idx) as i8;
      self.ac_delta_q[pi] = (qps.ac_qi[pi] as i32 - base_q_idx) as i8;
    }
    self.lambda =
      qps.lambda * ((1 << (2 * (self.sequence.bit_depth - 8))) as f64);
    // prom_av1e002/009: experimental λ probe (env RAV1E_LAMBDA_MULT; unset =
    // baseline). me_lambda derives from the scaled value below.
    match crate::harvest::lambda_mult() {
      Some(crate::harvest::LambdaMult::Kind(m_intra, m_inter)) => {
        self.lambda *= if self.intra_only { m_intra } else { m_inter };
      }
      Some(crate::harvest::LambdaMult::Level(ms)) => {
        let lvl =
          if self.intra_only { 0 } else { (self.pyramid_level as usize).min(3) };
        self.lambda *= ms[lvl];
      }
      None => {}
    }
    self.me_lambda = self.lambda.sqrt();
    self.dist_scale = qps.dist_scale.map(DistortionScale::from);

    match self.cdef_search_method {
      CDEFSearchMethod::PickFromQ => {
        self.pick_strength_from_q(qps);
      }
      // TODO: implement FastSearch and FullSearch
      _ => unreachable!(),
    }
  }

  #[inline(always)]
  pub fn sb_size_log2(&self) -> usize {
    self.sequence.tiling.sb_size_log2
  }

  pub fn film_grain_params(&self) -> Option<&GrainTableSegment> {
    if !(self.show_frame || self.showable_frame) {
      return None;
    }
    let cur_frame_time = self.frame_timestamp();
    self.config.get_film_grain_at(cur_frame_time)
  }

  pub fn frame_timestamp(&self) -> u64 {
    // I don't know why this is the base unit for a timestamp but it is. 1/10000000 of a second.
    const TIMESTAMP_BASE_UNIT: u64 = 10_000_000;

    self.input_frameno * TIMESTAMP_BASE_UNIT * self.sequence.time_base.num
      / self.sequence.time_base.den
  }
}

impl<T: Pixel> fmt::Display for FrameInvariants<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "Input Frame {} - {}", self.input_frameno, self.frame_type)
  }
}

/// # Errors
///
/// - If the frame packet cannot be written to
pub fn write_temporal_delimiter(packet: &mut dyn io::Write) -> io::Result<()> {
  packet.write_all(&TEMPORAL_DELIMITER)?;
  Ok(())
}

fn write_key_frame_obus<T: Pixel>(
  packet: &mut dyn io::Write, fi: &FrameInvariants<T>, obu_extension: u32,
) -> io::Result<()> {
  let mut buf1 = Vec::new();
  let mut buf2 = Vec::new();
  {
    let mut bw2 = BitWriter::endian(&mut buf2, BigEndian);
    bw2.write_sequence_header_obu(fi)?;
    bw2.write_bit(true)?; // trailing bit
    bw2.byte_align()?;
  }

  {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_obu_header(ObuType::OBU_SEQUENCE_HEADER, obu_extension)?;
  }
  packet.write_all(&buf1).unwrap();
  buf1.clear();

  {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_uleb128(buf2.len() as u64)?;
  }

  packet.write_all(&buf1).unwrap();
  buf1.clear();

  packet.write_all(&buf2).unwrap();
  buf2.clear();

  if fi.sequence.content_light.is_some() {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_sequence_metadata_obu(
      ObuMetaType::OBU_META_HDR_CLL,
      &fi.sequence,
    )?;
    packet.write_all(&buf1).unwrap();
    buf1.clear();
  }

  if fi.sequence.mastering_display.is_some() {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_sequence_metadata_obu(
      ObuMetaType::OBU_META_HDR_MDCV,
      &fi.sequence,
    )?;
    packet.write_all(&buf1).unwrap();
    buf1.clear();
  }

  Ok(())
}

/// Write into `dst` the difference between the blocks at `src1` and `src2`
fn diff<T: Pixel>(
  dst: &mut [MaybeUninit<i16>], src1: &PlaneRegion<'_, T>,
  src2: &PlaneRegion<'_, T>,
) {
  debug_assert!(dst.len() % src1.rect().width == 0);
  debug_assert_eq!(src1.rows_iter().count(), src1.rect().height);

  let width = src1.rect().width;
  let height = src1.rect().height;

  if width == 0
    || width != src2.rect().width
    || height == 0
    || src1.rows_iter().len() != src2.rows_iter().len()
  {
    debug_assert!(false);
    return;
  }

  for ((l, s1), s2) in
    dst.chunks_exact_mut(width).zip(src1.rows_iter()).zip(src2.rows_iter())
  {
    for ((r, v1), v2) in l.iter_mut().zip(s1).zip(s2) {
      r.write(i16::cast_from(*v1) - i16::cast_from(*v2));
    }
  }
}

fn get_qidx<T: Pixel>(
  fi: &FrameInvariants<T>, ts: &TileStateMut<'_, T>, cw: &ContextWriter,
  tile_bo: TileBlockOffset,
) -> u8 {
  let mut qidx = fi.base_q_idx;
  let sidx = cw.bc.blocks[tile_bo].segmentation_idx as usize;
  if ts.segmentation.features[sidx][SegLvl::SEG_LVL_ALT_Q as usize] {
    let delta = ts.segmentation.data[sidx][SegLvl::SEG_LVL_ALT_Q as usize];
    qidx = clamp((qidx as i16) + delta, 0, 255) as u8;
  }
  qidx
}

/// prom_av1e039 — PD0 real cheap-RD block cost (SVT's PD0 screen, done with a
/// REAL RD cost instead of the SATD proxy that made the NONE-skip direction
/// catastrophic, av1e022). ONE NEARESTMV(LAST) prediction, the block's inter
/// tx tiling, LUMA only, DCT_DCT: predict → diff → forward_transform →
/// quantize → dequantize → tx-domain distortion + `estimate_rate` → RD.
/// Self-contained by design: touches only rec pixels (the real trials
/// overwrite them, exactly like `pd0_proxy_cost`) + `ts.qc` (re-set per real
/// tx block) + local buffers — NO CDF, NO writer, NO block-info writes, so it
/// is state-safe to call as a pre-pass in the partition search. Returns the
/// RD cost in `compute_rd_cost` units so node-vs-kids compares apples to
/// apples with the full search.
pub(crate) fn pd0_real_cost<T: Pixel>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, bsize: BlockSize, tile_bo: TileBlockOffset,
) -> f64 {
  if tile_bo.0.x >= ts.mi_width || tile_bo.0.y >= ts.mi_height {
    return 0.0;
  }
  let ref_frames = [LAST_FRAME, NONE_FRAME];
  let mut mv_stack = ArrayVec::<CandidateMV, 9>::new();
  let _ = cw.find_mvrefs(tile_bo, ref_frames, &mut mv_stack, bsize, fi, false);
  let mut pmv = [MotionVector::default(); 2];
  if !mv_stack.is_empty() {
    pmv[0] = mv_stack[0].this_mv;
  }
  if mv_stack.len() > 1 {
    pmv[1] = mv_stack[1].this_mv;
  }

  // Real (light) motion search — the ingredient SVT's PD0 has and the
  // NEARESTMV-only cost lacked (ceiling: coin-flip at 32/16 without it). This
  // is the expensive part of PD0; it makes the residual, hence the RD, faithful
  // to what the full search would find, so the NONE/SPLIT margin predicts.
  let me_res = estimate_motion(
    fi,
    ts,
    bsize.width(),
    bsize.height(),
    tile_bo,
    ref_frames[0],
    Some(pmv),
    MVSamplingMode::CORNER { right: true, bottom: true },
    false,
    0,
    None,
  );
  let (mode, mv) = match me_res {
    Some(r) => (PredictionMode::NEWMV, r.mv),
    None => (PredictionMode::NEARESTMV, pmv[0]),
  };
  // MV signaling + mode-flag rate (frac bits) — makes the split overhead (4
  // MVs/mode-flags vs 1) real in the margin, so NONE stays competitive.
  // get_mv_rate is whole bits; shift into OD_BITRES frac units. The +4-bit
  // constant approximates the per-block mode/ref/skip flag cost.
  let mv_bits = if mode == PredictionMode::NEWMV {
    u64::from(get_mv_rate(mv, pmv[0], fi.allow_high_precision_mv))
  } else {
    0
  };
  let block_rate = (mv_bits + 4) << crate::ec::OD_BITRES;

  let tile_rect = ts.tile_rect();
  {
    let rec = &mut ts.rec.planes[0];
    let po = tile_bo.plane_offset(rec.plane_cfg);
    let mut rec_region =
      rec.subregion_mut(Area::BlockStartingAt { bo: tile_bo.0 });
    mode.predict_inter(
      fi,
      tile_rect,
      0,
      po,
      &mut rec_region,
      bsize.width(),
      bsize.height(),
      ref_frames,
      [mv, MotionVector::default()],
      &mut ts.inter_compound_buffers,
    );
  }

  // Inter tx tiling, mirroring rdo_tx_size_type's inter path.
  let mut tx_size = max_txsize_rect_lookup[bsize as usize];
  if fi.enable_inter_txfm_split {
    tx_size = sub_tx_size_map[tx_size as usize];
  }
  let tx_type = TxType::DCT_DCT;
  let qidx = get_qidx(fi, ts, cw, tile_bo);
  ts.qc.update(
    qidx,
    tx_size,
    false,
    fi.sequence.bit_depth,
    fi.dc_delta_q[0],
    fi.ac_delta_q[0],
  );

  let bw = bsize.width_mi() / tx_size.width_mi();
  let bh = bsize.height_mi() / tx_size.height_mi();
  let coded_tx_area = av1_get_coded_tx_size(tx_size).area();
  let sb = 2 * (3 - get_log_tx_scale(tx_size));

  let mut residual = Aligned::<[MaybeUninit<i16>; 64 * 64]>::uninit_array();
  let mut coeffs = Aligned::<[MaybeUninit<T::Coeff>; 64 * 64]>::uninit_array();
  let mut qcoeffs =
    Aligned::<[MaybeUninit<T::Coeff>; 32 * 32]>::uninit_array();
  let mut rcoeffs =
    Aligned::<[MaybeUninit<T::Coeff>; 32 * 32]>::uninit_array();

  let mut dist = ScaledDistortion::zero();
  let mut rate = 0u64;

  for by in 0..bh {
    for bx in 0..bw {
      let tx_bo = TileBlockOffset(BlockOffset {
        x: tile_bo.0.x + bx * tx_size.width_mi(),
        y: tile_bo.0.y + by * tx_size.height_mi(),
      });
      if tx_bo.0.x >= ts.mi_width || tx_bo.0.y >= ts.mi_height {
        continue;
      }
      let area = Area::BlockRect {
        bo: tx_bo.0,
        width: tx_size.width(),
        height: tx_size.height(),
      };
      let residual = &mut residual.data[..tx_size.area()];
      let coeffs = &mut coeffs.data[..tx_size.area()];
      let qcoeffs = init_slice_repeat_mut(
        &mut qcoeffs.data[..coded_tx_area],
        T::Coeff::cast_from(0),
      );
      let rcoeffs = &mut rcoeffs.data[..coded_tx_area];

      {
        let rec = &ts.rec.planes[0];
        diff(
          residual,
          &ts.input_tile.planes[0].subregion(area),
          &rec.subregion(area),
        );
      }
      // SAFETY: diff inits tx_size.area() elements.
      let residual = unsafe { slice_assume_init_mut(residual) };
      forward_transform(
        residual,
        coeffs,
        tx_size.width(),
        tx_size,
        tx_type,
        fi.sequence.bit_depth,
        fi.cpu_feature_level,
      );
      // SAFETY: forward_transform initialized coeffs.
      let coeffs = unsafe { slice_assume_init_mut(coeffs) };
      let eob = ts.qc.quantize(coeffs, qcoeffs, tx_size, tx_type);
      dequantize(
        qidx,
        qcoeffs,
        eob,
        rcoeffs,
        tx_size,
        fi.sequence.bit_depth,
        fi.dc_delta_q[0],
        fi.ac_delta_q[0],
        fi.cpu_feature_level,
      );
      // SAFETY: dequantize initialized rcoeffs.
      let rcoeffs = unsafe { slice_assume_init_mut(rcoeffs) };

      let mut raw = coeffs
        .iter()
        .zip(rcoeffs.iter())
        .map(|(&a, &b)| {
          let c = i32::cast_from(a) - i32::cast_from(b);
          (c * c) as u64
        })
        .sum::<u64>()
        + coeffs[rcoeffs.len()..]
          .iter()
          .map(|&a| {
            let c = i32::cast_from(a);
            (c * c) as u64
          })
          .sum::<u64>();
      raw = (raw + (1 << (sb - 1))) >> sb;
      rate += estimate_rate(fi.base_q_idx, tx_size, raw);
      // spatiotemporal_scale (not distortion_scale) is the >8×8-safe perceptual
      // bias — matches the default Psychovisual pixel-domain weighting and does
      // not assert on large blocks.
      let bias =
        spatiotemporal_scale(fi, ts.to_frame_block_offset(tx_bo), bsize);
      dist += RawDistortion::new(raw) * bias * fi.dist_scale[0];
    }
  }

  compute_rd_cost(fi, (rate + block_rate) as u32, dist)
}

/// For a transform block,
/// predict, transform, quantize, write coefficients to a bitstream,
/// dequantize, inverse-transform.
///
/// # Panics
///
/// - If the block size is invalid for subsampling
/// - If a tx type other than DCT is used for 64x64 blocks
pub fn encode_tx_block<T: Pixel, W: Writer>(
  fi: &FrameInvariants<T>,
  ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter,
  w: &mut W,
  p: usize,
  // Offset in the luma plane of the partition enclosing this block.
  tile_partition_bo: TileBlockOffset,
  // tx block position within a partition, unit: tx block number
  bx: usize,
  by: usize,
  // Offset in the luma plane where this tx block is colocated. Note that for
  // a chroma block, this offset might be outside of the current partition.
  // For example in 4:2:0, four 4x4 luma partitions share one 4x4 chroma block,
  // this block is part of the last 4x4 partition, but its `tx_bo` offset
  // matches the offset of the first 4x4 partition.
  tx_bo: TileBlockOffset,
  mode: PredictionMode,
  tx_size: TxSize,
  tx_type: TxType,
  bsize: BlockSize,
  po: PlaneOffset,
  skip: bool,
  qidx: u8,
  ac: &[i16],
  pred_intra_param: IntraParam,
  rdo_type: RDOType,
  need_recon_pixel: bool,
) -> (bool, ScaledDistortion) {
  let PlaneConfig { xdec, ydec, .. } = ts.input.planes[p].cfg;
  let tile_rect = ts.tile_rect().decimated(xdec, ydec);
  let area = Area::BlockRect {
    bo: tx_bo.0,
    width: tx_size.width(),
    height: tx_size.height(),
  };

  if tx_bo.0.x >= ts.mi_width || tx_bo.0.y >= ts.mi_height {
    return (false, ScaledDistortion::zero());
  }

  debug_assert!(tx_bo.0.x < ts.mi_width);
  debug_assert!(tx_bo.0.y < ts.mi_height);

  debug_assert!(
    tx_size.sqr() <= TxSize::TX_32X32 || tx_type == TxType::DCT_DCT
  );

  let plane_bsize = bsize.subsampled_size(xdec, ydec).unwrap();

  debug_assert!(p != 0 || !mode.is_intra() || tx_size.block_size() == plane_bsize || need_recon_pixel,
    "mode.is_intra()={:#?}, plane={:#?}, tx_size.block_size()={:#?}, plane_bsize={:#?}, need_recon_pixel={:#?}",
    mode.is_intra(), p, tx_size.block_size(), plane_bsize, need_recon_pixel);

  let ief_params = if mode.is_directional()
    && fi.sequence.enable_intra_edge_filter
  {
    let (plane_xdec, plane_ydec) = if p == 0 { (0, 0) } else { (xdec, ydec) };
    let above_block_info =
      ts.above_block_info(tile_partition_bo, plane_xdec, plane_ydec);
    let left_block_info =
      ts.left_block_info(tile_partition_bo, plane_xdec, plane_ydec);
    Some(IntraEdgeFilterParameters::new(p, above_block_info, left_block_info))
  } else {
    None
  };

  let frame_bo = ts.to_frame_block_offset(tx_bo);
  let rec = &mut ts.rec.planes[p];

  if mode.is_intra() {
    let bit_depth = fi.sequence.bit_depth;
    let mut edge_buf = Aligned::uninit_array();
    let edge_buf = {
      let _s = crate::prof::scope(crate::prof::Stage::IntraEdges);
      get_intra_edges(
        &mut edge_buf,
        &rec.as_const(),
        tile_partition_bo,
        bx,
        by,
        bsize,
        po,
        tx_size,
        bit_depth,
        Some(mode),
        fi.sequence.enable_intra_edge_filter,
        pred_intra_param,
      )
    };

    mode.predict_intra(
      tile_rect,
      &mut rec.subregion_mut(area),
      tx_size,
      bit_depth,
      ac,
      pred_intra_param,
      ief_params,
      &edge_buf,
      fi.cpu_feature_level,
    );
  }

  if skip {
    return (false, ScaledDistortion::zero());
  }

  let coded_tx_area = av1_get_coded_tx_size(tx_size).area();
  let mut residual = Aligned::<[MaybeUninit<i16>; 64 * 64]>::uninit_array();
  let mut coeffs = Aligned::<[MaybeUninit<T::Coeff>; 64 * 64]>::uninit_array();
  let mut qcoeffs =
    Aligned::<[MaybeUninit<T::Coeff>; 32 * 32]>::uninit_array();
  let mut rcoeffs =
    Aligned::<[MaybeUninit<T::Coeff>; 32 * 32]>::uninit_array();
  let residual = &mut residual.data[..tx_size.area()];
  let coeffs = &mut coeffs.data[..tx_size.area()];
  let qcoeffs = {
    let _s = crate::prof::scope(crate::prof::Stage::QcoeffsZero);
    init_slice_repeat_mut(
      &mut qcoeffs.data[..coded_tx_area],
      T::Coeff::cast_from(0),
    )
  };
  let rcoeffs = &mut rcoeffs.data[..coded_tx_area];

  let (visible_tx_w, visible_tx_h) = clip_visible_bsize(
    (fi.width + xdec) >> xdec,
    (fi.height + ydec) >> ydec,
    tx_size.block_size(),
    (frame_bo.0.x << MI_SIZE_LOG2) >> xdec,
    (frame_bo.0.y << MI_SIZE_LOG2) >> ydec,
  );

  if visible_tx_w != 0 && visible_tx_h != 0 {
    let _s = crate::prof::scope(crate::prof::Stage::Diff);
    diff(
      residual,
      &ts.input_tile.planes[p].subregion(area),
      &rec.subregion(area),
    );
  } else {
    residual.fill(MaybeUninit::new(0));
  }
  // SAFETY: `diff()` inits `tx_size.area()` elements when it matches size of `subregion(area)`
  let residual = unsafe { slice_assume_init_mut(residual) };

  #[cfg(feature = "profile")]
  fwd_phase::count(W::COUNTS_ONLY);
  forward_transform(
    residual,
    coeffs,
    tx_size.width(),
    tx_size,
    tx_type,
    fi.sequence.bit_depth,
    fi.cpu_feature_level,
  );
  // SAFETY: forward_transform initialized coeffs
  let coeffs = unsafe { slice_assume_init_mut(coeffs) };

  let mut eob = ts.qc.quantize(coeffs, qcoeffs, tx_size, tx_type);

  // prom_av1e047: coefficient trellis (RDOQ) — final encode only, on the shared
  // qcoeffs before BOTH coding and recon so they stay consistent. Reads the
  // decoder's exact dequant + the coeff CDFs; RD units match compute_rd_cost.
  // ★ sign-flip → DISPATCH: force-on is +0.53% on flat / −0.39% on busy (mean
  // +0.055%, a mirage). Routed to BUSY SBs (the deep flag) it banks the busy
  // win and skips the flat loss. `deep::active()` when RAV1E_DEEP/DEEP_Q on;
  // RAV1E_TRELLIS_ALL=1 forces every block (the force-on ceiling).
  // trellis_gate::on() is set per-SB: true when the trellis is active AND this
  // SB is non-flat (absolute variance > threshold) or force-on.
  if trellis_gate::on() && !W::COUNTS_ONLY && !skip && eob > 2 {
    let acq =
      crate::quantize::ac_q(qidx, fi.ac_delta_q[p], fi.sequence.bit_depth)
        .get() as i32;
    let lts = get_log_tx_scale(tx_size) as i32;
    let sb = 2 * (3 - get_log_tx_scale(tx_size));
    // Include the per-block PERCEPTUAL bias (spatiotemporal_scale, the >8×8-safe
    // weight the pixel-domain RD uses) so the trellis matches the encoder's RD —
    // without it, important flat regions (akiyo's face) are under-weighted and
    // over-dropped.
    let bias = crate::rdo::spatiotemporal_scale(fi, frame_bo, bsize);
    let rd_scale = (f64::from(fi.dist_scale[p].0) / f64::from(1u32 << 14))
      * (f64::from(bias.0) / f64::from(1u32 << 14))
      / (1u64 << sb) as f64;
    eob = cw.trellis_optimize(
      w,
      coeffs,
      qcoeffs,
      eob,
      tx_size,
      tx_type,
      usize::from(p != 0),
      acq,
      lts,
      fi.lambda,
      rd_scale,
    );
  }

  // prom_av1e034 rate-table harvest: capture the REAL coefficient rate the
  // RDO uses, to refit estimate_rate's table. Tier at tune=Psnr already runs
  // both write_coeffs (rate) and the tx_dist below.
  let rate_harvest = crate::harvest::rateharvest();
  let rh_tell0 = if rate_harvest { w.tell_frac() } else { 0 };
  let has_coeff = if need_recon_pixel || rdo_type.needs_coeff_rate() {
    debug_assert!((((fi.w_in_b - frame_bo.0.x) << MI_SIZE_LOG2) >> xdec) >= 4);
    debug_assert!((((fi.h_in_b - frame_bo.0.y) << MI_SIZE_LOG2) >> ydec) >= 4);
    let frame_clipped_txw: usize =
      (((fi.w_in_b - frame_bo.0.x) << MI_SIZE_LOG2) >> xdec)
        .min(tx_size.width());
    let frame_clipped_txh: usize =
      (((fi.h_in_b - frame_bo.0.y) << MI_SIZE_LOG2) >> ydec)
        .min(tx_size.height());

    cw.write_coeffs_lv_map(
      w,
      p,
      tx_bo,
      qcoeffs,
      eob,
      mode,
      tx_size,
      tx_type,
      plane_bsize,
      xdec,
      ydec,
      fi.use_reduced_tx_set,
      frame_clipped_txw,
      frame_clipped_txh,
    )
  } else {
    true
  };

  // Reconstruct
  dequantize(
    qidx,
    qcoeffs,
    eob,
    rcoeffs,
    tx_size,
    fi.sequence.bit_depth,
    fi.dc_delta_q[p],
    fi.ac_delta_q[p],
    fi.cpu_feature_level,
  );
  // SAFETY: dequantize initialized rcoeffs
  let rcoeffs = unsafe { slice_assume_init_mut(rcoeffs) };

  if eob == 0 {
    // All zero coefficients is a no-op
  } else if !fi.use_tx_domain_distortion || need_recon_pixel {
    inverse_transform_add(
      rcoeffs,
      &mut rec.subregion_mut(area),
      eob,
      tx_size,
      tx_type,
      fi.sequence.bit_depth,
      fi.cpu_feature_level,
    );
  }

  let _txd = crate::prof::scope(crate::prof::Stage::TxDistLoop);
  let tx_dist =
    if rdo_type.needs_tx_dist() && visible_tx_w != 0 && visible_tx_h != 0 {
      // Store tx-domain distortion of this block
      // rcoeffs above 32 rows/cols aren't held in the array, because they are
      // always 0. The first 32x32 is stored first in coeffs so we can iterate
      // over coeffs and rcoeffs for the first 32 rows/cols. For the
      // coefficients above 32 rows/cols, we iterate over the rest of coeffs
      // with the assumption that rcoeff coefficients are zero.
      let mut raw_tx_dist = coeffs
        .iter()
        .zip(rcoeffs.iter())
        .map(|(&a, &b)| {
          let c = i32::cast_from(a) - i32::cast_from(b);
          (c * c) as u64
        })
        .sum::<u64>()
        + coeffs[rcoeffs.len()..]
          .iter()
          .map(|&a| {
            let c = i32::cast_from(a);
            (c * c) as u64
          })
          .sum::<u64>();

      let tx_dist_scale_bits = 2 * (3 - get_log_tx_scale(tx_size));
      let tx_dist_scale_rounding_offset = 1 << (tx_dist_scale_bits - 1);

      raw_tx_dist =
        (raw_tx_dist + tx_dist_scale_rounding_offset) >> tx_dist_scale_bits;

      if rdo_type == RDOType::TxDistEstRate {
        // look up rate and distortion in table
        let estimated_rate =
          estimate_rate(fi.base_q_idx, tx_size, raw_tx_dist);
        w.add_bits_frac(estimated_rate as u32);
      }

      if rate_harvest && W::COUNTS_ONLY {
        let real_rate = w.tell_frac().saturating_sub(rh_tell0);
        crate::harvest::emit(&format!(
          "RATE,{},{},{},{}",
          fi.base_q_idx, tx_size as usize, raw_tx_dist, real_rate
        ));
      }

      let bias = distortion_scale(fi, ts.to_frame_block_offset(tx_bo), bsize);
      RawDistortion::new(raw_tx_dist) * bias * fi.dist_scale[p]
    } else {
      ScaledDistortion::zero()
    };

  (has_coeff, tx_dist)
}

/// # Panics
///
/// - If the block size is invalid for subsampling
#[profiling::function]
pub fn motion_compensate<T: Pixel>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, luma_mode: PredictionMode, ref_frames: [RefType; 2],
  mvs: [MotionVector; 2], bsize: BlockSize, tile_bo: TileBlockOffset,
  luma_only: bool,
) {
  let _prof = crate::prof::scope(crate::prof::Stage::MotionCompensate);
  let _prof = crate::prof::scope(crate::prof::Stage::Predict);
  debug_assert!(!luma_mode.is_intra());

  let PlaneConfig { xdec: u_xdec, ydec: u_ydec, .. } = ts.input.planes[1].cfg;

  // Inter mode prediction can take place once for a whole partition,
  // instead of each tx-block.
  let num_planes = 1
    + if !luma_only
      && has_chroma(
        tile_bo,
        bsize,
        u_xdec,
        u_ydec,
        fi.sequence.chroma_sampling,
      ) {
      2
    } else {
      0
    };

  let luma_tile_rect = ts.tile_rect();
  let compound_buffer = &mut ts.inter_compound_buffers;
  for p in 0..num_planes {
    let plane_bsize = if p == 0 {
      bsize
    } else {
      bsize.subsampled_size(u_xdec, u_ydec).unwrap()
    };

    let rec = &mut ts.rec.planes[p];
    let po = tile_bo.plane_offset(rec.plane_cfg);
    let &PlaneConfig { xdec, ydec, .. } = rec.plane_cfg;
    let tile_rect = luma_tile_rect.decimated(xdec, ydec);

    let area = Area::BlockStartingAt { bo: tile_bo.0 };
    if p > 0 && bsize < BlockSize::BLOCK_8X8 {
      let mut some_use_intra = false;
      if bsize == BlockSize::BLOCK_4X4 || bsize == BlockSize::BLOCK_4X8 {
        some_use_intra |=
          cw.bc.blocks[tile_bo.with_offset(-1, 0)].mode.is_intra();
      };
      if !some_use_intra && bsize == BlockSize::BLOCK_4X4
        || bsize == BlockSize::BLOCK_8X4
      {
        some_use_intra |=
          cw.bc.blocks[tile_bo.with_offset(0, -1)].mode.is_intra();
      };
      if !some_use_intra && bsize == BlockSize::BLOCK_4X4 {
        some_use_intra |=
          cw.bc.blocks[tile_bo.with_offset(-1, -1)].mode.is_intra();
      };

      if some_use_intra {
        luma_mode.predict_inter(
          fi,
          tile_rect,
          p,
          po,
          &mut rec.subregion_mut(area),
          plane_bsize.width(),
          plane_bsize.height(),
          ref_frames,
          mvs,
          compound_buffer,
        );
      } else {
        assert!(u_xdec == 1 && u_ydec == 1);
        // TODO: these are absolutely only valid for 4:2:0
        if bsize == BlockSize::BLOCK_4X4 {
          let mv0 = cw.bc.blocks[tile_bo.with_offset(-1, -1)].mv;
          let rf0 = cw.bc.blocks[tile_bo.with_offset(-1, -1)].ref_frames;
          let mv1 = cw.bc.blocks[tile_bo.with_offset(0, -1)].mv;
          let rf1 = cw.bc.blocks[tile_bo.with_offset(0, -1)].ref_frames;
          let po1 = PlaneOffset { x: po.x + 2, y: po.y };
          let area1 = Area::StartingAt { x: po1.x, y: po1.y };
          let mv2 = cw.bc.blocks[tile_bo.with_offset(-1, 0)].mv;
          let rf2 = cw.bc.blocks[tile_bo.with_offset(-1, 0)].ref_frames;
          let po2 = PlaneOffset { x: po.x, y: po.y + 2 };
          let area2 = Area::StartingAt { x: po2.x, y: po2.y };
          let po3 = PlaneOffset { x: po.x + 2, y: po.y + 2 };
          let area3 = Area::StartingAt { x: po3.x, y: po3.y };
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po,
            &mut rec.subregion_mut(area),
            2,
            2,
            rf0,
            mv0,
            compound_buffer,
          );
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po1,
            &mut rec.subregion_mut(area1),
            2,
            2,
            rf1,
            mv1,
            compound_buffer,
          );
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po2,
            &mut rec.subregion_mut(area2),
            2,
            2,
            rf2,
            mv2,
            compound_buffer,
          );
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po3,
            &mut rec.subregion_mut(area3),
            2,
            2,
            ref_frames,
            mvs,
            compound_buffer,
          );
        }
        if bsize == BlockSize::BLOCK_8X4 {
          let mv1 = cw.bc.blocks[tile_bo.with_offset(0, -1)].mv;
          let rf1 = cw.bc.blocks[tile_bo.with_offset(0, -1)].ref_frames;
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po,
            &mut rec.subregion_mut(area),
            4,
            2,
            rf1,
            mv1,
            compound_buffer,
          );
          let po3 = PlaneOffset { x: po.x, y: po.y + 2 };
          let area3 = Area::StartingAt { x: po3.x, y: po3.y };
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po3,
            &mut rec.subregion_mut(area3),
            4,
            2,
            ref_frames,
            mvs,
            compound_buffer,
          );
        }
        if bsize == BlockSize::BLOCK_4X8 {
          let mv2 = cw.bc.blocks[tile_bo.with_offset(-1, 0)].mv;
          let rf2 = cw.bc.blocks[tile_bo.with_offset(-1, 0)].ref_frames;
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po,
            &mut rec.subregion_mut(area),
            2,
            4,
            rf2,
            mv2,
            compound_buffer,
          );
          let po3 = PlaneOffset { x: po.x + 2, y: po.y };
          let area3 = Area::StartingAt { x: po3.x, y: po3.y };
          luma_mode.predict_inter(
            fi,
            tile_rect,
            p,
            po3,
            &mut rec.subregion_mut(area3),
            2,
            4,
            ref_frames,
            mvs,
            compound_buffer,
          );
        }
      }
    } else {
      luma_mode.predict_inter(
        fi,
        tile_rect,
        p,
        po,
        &mut rec.subregion_mut(area),
        plane_bsize.width(),
        plane_bsize.height(),
        ref_frames,
        mvs,
        compound_buffer,
      );
    }
  }
}

pub fn save_block_motion<T: Pixel>(
  ts: &mut TileStateMut<'_, T>, bsize: BlockSize, tile_bo: TileBlockOffset,
  ref_frame: usize, mv: MotionVector,
) {
  let tile_me_stats = &mut ts.me_stats[ref_frame];
  let tile_bo_x_end = (tile_bo.0.x + bsize.width_mi()).min(ts.mi_width);
  let tile_bo_y_end = (tile_bo.0.y + bsize.height_mi()).min(ts.mi_height);
  for mi_y in tile_bo.0.y..tile_bo_y_end {
    for mi_x in tile_bo.0.x..tile_bo_x_end {
      tile_me_stats[mi_y][mi_x].mv = mv;
    }
  }
}

#[profiling::function]
pub fn encode_block_pre_cdef<T: Pixel, W: Writer>(
  seq: &Sequence, ts: &TileStateMut<'_, T>, cw: &mut ContextWriter, w: &mut W,
  bsize: BlockSize, tile_bo: TileBlockOffset, skip: bool,
) -> bool {
  // prom_av1e024: see the post_cdef fills — own-cell skip flags are not read
  // within a counting trial (write_skip's ctx reads neighbors; the symbol
  // takes the param).
  if !W::COUNTS_ONLY {
    cw.bc.blocks.set_skip(tile_bo, bsize, skip);
  }
  if ts.segmentation.enabled
    && ts.segmentation.update_map
    && ts.segmentation.preskip
  {
    cw.write_segmentation(
      w,
      tile_bo,
      bsize,
      false,
      ts.segmentation.last_active_segid,
    );
  }
  cw.write_skip(w, tile_bo, skip);
  if ts.segmentation.enabled
    && ts.segmentation.update_map
    && !ts.segmentation.preskip
  {
    cw.write_segmentation(
      w,
      tile_bo,
      bsize,
      skip,
      ts.segmentation.last_active_segid,
    );
  }
  if !skip && seq.enable_cdef {
    cw.bc.cdef_coded = true;
  }
  cw.bc.cdef_coded
}

/// prom_av1e026 trial audit (profile builds): the work-count × per-trial-tax
/// matrix. Buckets encode_block_post_cdef cycles and calls by block-size
/// class × writer class (counter trial / recorder encode). Dumped at each
/// tile end alongside SBSKIP.
#[cfg(feature = "profile")]
pub(crate) mod trial_audit {
  use std::sync::atomic::{AtomicU64, Ordering};
  pub static CY: [[AtomicU64; 2]; 4] =
    [const { [const { AtomicU64::new(0) }; 2] }; 4];
  pub static N: [[AtomicU64; 2]; 4] =
    [const { [const { AtomicU64::new(0) }; 2] }; 4];

  #[inline]
  pub fn bucket(dim: usize) -> usize {
    match dim {
      64.. => 0,
      32.. => 1,
      16.. => 2,
      _ => 3,
    }
  }

  pub fn record(b: usize, counts_only: bool, dt: u64) {
    let w = usize::from(!counts_only);
    CY[b][w].fetch_add(dt, Ordering::Relaxed);
    N[b][w].fetch_add(1, Ordering::Relaxed);
  }

  pub fn dump() {
    let names = ["64", "32", "16", "8-"];
    for b in 0..4 {
      let (tn, tc) =
        (N[b][0].load(Ordering::Relaxed), CY[b][0].load(Ordering::Relaxed));
      let (rn, rc) =
        (N[b][1].load(Ordering::Relaxed), CY[b][1].load(Ordering::Relaxed));
      eprintln!(
        "TRIALAUDIT bsize={} trials={} trial_cy={} enc={} enc_cy={}",
        names[b], tn, tc, rn, rc
      );
    }
  }
}

/// # Panics
///
/// - If chroma and luma do not match for inter modes
/// - If an invalid motion vector is found
#[profiling::function]
pub fn encode_block_post_cdef<T: Pixel, W: Writer>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, w: &mut W, luma_mode: PredictionMode,
  chroma_mode: PredictionMode, angle_delta: AngleDelta,
  ref_frames: [RefType; 2], mvs: [MotionVector; 2], bsize: BlockSize,
  tile_bo: TileBlockOffset, skip: bool, cfl: CFLParams, tx_size: TxSize,
  tx_type: TxType, mode_context: usize, mv_stack: &[CandidateMV],
  rdo_type: RDOType, need_recon_pixel: bool,
  enc_stats: Option<&mut EncoderStats>, luma_reuse: Option<&mut LumaReuse>,
) -> (bool, ScaledDistortion) {
  let _prof = crate::prof::scope(crate::prof::Stage::EncodeBlockPost);
  #[cfg(feature = "profile")]
  let audit_t0 = unsafe { core::arch::x86_64::_rdtsc() };
  #[cfg(feature = "profile")]
  let audit_guard = {
    struct AuditGuard {
      t0: u64,
      b: usize,
      counts_only: bool,
    }
    impl Drop for AuditGuard {
      fn drop(&mut self) {
        let dt = unsafe { core::arch::x86_64::_rdtsc() } - self.t0;
        trial_audit::record(self.b, self.counts_only, dt);
      }
    }
    AuditGuard {
      t0: audit_t0,
      b: trial_audit::bucket(bsize.width().max(bsize.height())),
      counts_only: W::COUNTS_ONLY,
    }
  };
  #[cfg(feature = "profile")]
  let _ = &audit_guard;
  let planes =
    if fi.sequence.chroma_sampling == ChromaSampling::Cs400 { 1 } else { 3 };
  let is_inter = !luma_mode.is_intra();
  if is_inter {
    assert!(luma_mode == chroma_mode);
  };
  let sb_size = if fi.sequence.use_128x128_superblock {
    BlockSize::BLOCK_128X128
  } else {
    BlockSize::BLOCK_64X64
  };
  let PlaneConfig { xdec, ydec, .. } = ts.input.planes[1].cfg;
  // prom_av1e024: a skip trial codes no coefficients, so the zeroed coeff
  // contexts are never read within the trial — final/recorder paths keep it.
  if skip && !W::COUNTS_ONLY {
    cw.bc.reset_skip_context(
      tile_bo,
      bsize,
      xdec,
      ydec,
      fi.sequence.chroma_sampling,
    );
  }
  // prom_av1e024 (RDO-glue): on counting writers these rectangle fills
  // (bsize_mi² cells each, per trial) are write-only — nothing in the trial
  // reads the block's OWN cells for these fields; context derivations read
  // NEIGHBOR cells (above_of/left_of) and the coded values travel as
  // parameters. The final/recorder paths still write them. set_ref_frames
  // stays: write_ref_frames reads the own-cell value (with
  // neighbors_ref_counts) during the trial. Byte-identical class — any
  // hidden in-trial reader would flip decisions and break the FNV gate.
  if !W::COUNTS_ONLY {
    cw.bc.blocks.set_block_size(tile_bo, bsize);
    cw.bc.blocks.set_mode(tile_bo, bsize, luma_mode);
    cw.bc.blocks.set_tx_size(tile_bo, bsize, tx_size);
    cw.bc.blocks.set_motion_vectors(tile_bo, bsize, mvs);
  }
  cw.bc.blocks.set_ref_frames(tile_bo, bsize, ref_frames);

  //write_q_deltas();
  if cw.bc.code_deltas
    && ts.deblock.block_deltas_enabled
    && (bsize < sb_size || !skip)
  {
    cw.write_block_deblock_deltas(
      w,
      tile_bo,
      ts.deblock.block_delta_multi,
      planes,
    );
  }
  cw.bc.code_deltas = false;

  if fi.frame_type.has_inter() {
    cw.write_is_inter(w, tile_bo, is_inter);
    if is_inter {
      cw.fill_neighbours_ref_counts(tile_bo);
      cw.write_ref_frames(w, fi, tile_bo);

      if luma_mode.is_compound() {
        cw.write_compound_mode(w, luma_mode, mode_context);
      } else {
        cw.write_inter_mode(w, luma_mode, mode_context);
      }

      let ref_mv_idx = 0;
      let num_mv_found = mv_stack.len();

      if luma_mode == PredictionMode::NEWMV
        || luma_mode == PredictionMode::NEW_NEWMV
      {
        if luma_mode == PredictionMode::NEW_NEWMV {
          assert!(num_mv_found >= 2);
        }
        for idx in 0..2 {
          if num_mv_found > idx + 1 {
            let drl_mode = ref_mv_idx > idx;
            let ctx: usize = (mv_stack[idx].weight < REF_CAT_LEVEL) as usize
              + (mv_stack[idx + 1].weight < REF_CAT_LEVEL) as usize;
            cw.write_drl_mode(w, drl_mode, ctx);
            if !drl_mode {
              break;
            }
          }
        }
      }

      let ref_mvs = if num_mv_found > 0 {
        [mv_stack[ref_mv_idx].this_mv, mv_stack[ref_mv_idx].comp_mv]
      } else {
        [MotionVector::default(); 2]
      };

      let mv_precision = if fi.force_integer_mv != 0 {
        MvSubpelPrecision::MV_SUBPEL_NONE
      } else if fi.allow_high_precision_mv {
        MvSubpelPrecision::MV_SUBPEL_HIGH_PRECISION
      } else {
        MvSubpelPrecision::MV_SUBPEL_LOW_PRECISION
      };

      if luma_mode == PredictionMode::NEWMV
        || luma_mode == PredictionMode::NEW_NEWMV
        || luma_mode == PredictionMode::NEW_NEARESTMV
      {
        cw.write_mv(w, mvs[0], ref_mvs[0], mv_precision);
      }
      if luma_mode == PredictionMode::NEW_NEWMV
        || luma_mode == PredictionMode::NEAREST_NEWMV
      {
        cw.write_mv(w, mvs[1], ref_mvs[1], mv_precision);
      }

      if luma_mode.has_nearmv() {
        let ref_mv_idx = luma_mode.ref_mv_idx();
        if luma_mode != PredictionMode::NEAR0MV {
          assert!(num_mv_found > ref_mv_idx);
        }

        for idx in 1..3 {
          if num_mv_found > idx + 1 {
            let drl_mode = ref_mv_idx > idx;
            let ctx: usize = (mv_stack[idx].weight < REF_CAT_LEVEL) as usize
              + (mv_stack[idx + 1].weight < REF_CAT_LEVEL) as usize;

            cw.write_drl_mode(w, drl_mode, ctx);
            if !drl_mode {
              break;
            }
          }
        }
        if mv_stack.len() > 1 {
          assert!(mv_stack[ref_mv_idx].this_mv.row == mvs[0].row);
          assert!(mv_stack[ref_mv_idx].this_mv.col == mvs[0].col);
        } else {
          assert!(0 == mvs[0].row);
          assert!(0 == mvs[0].col);
        }
      } else if luma_mode == PredictionMode::NEARESTMV {
        if mv_stack.is_empty() {
          assert_eq!(mvs[0].row, 0);
          assert_eq!(mvs[0].col, 0);
        } else {
          assert_eq!(mvs[0].row, mv_stack[0].this_mv.row);
          assert_eq!(mvs[0].col, mv_stack[0].this_mv.col);
        }
      }
    } else {
      cw.write_intra_mode(w, bsize, luma_mode);
    }
  } else {
    cw.write_intra_mode_kf(w, tile_bo, luma_mode);
  }

  if !is_inter {
    if luma_mode.is_directional() && bsize >= BlockSize::BLOCK_8X8 {
      cw.write_angle_delta(w, angle_delta.y, luma_mode);
    }
    if has_chroma(tile_bo, bsize, xdec, ydec, fi.sequence.chroma_sampling) {
      cw.write_intra_uv_mode(w, chroma_mode, luma_mode, bsize);
      if chroma_mode.is_cfl() {
        assert!(bsize.cfl_allowed());
        cw.write_cfl_alphas(w, cfl);
      }
      if chroma_mode.is_directional() && bsize >= BlockSize::BLOCK_8X8 {
        cw.write_angle_delta(w, angle_delta.uv, chroma_mode);
      }
    }

    if fi.allow_screen_content_tools > 0
      && bsize >= BlockSize::BLOCK_8X8
      && bsize.width() <= 64
      && bsize.height() <= 64
    {
      cw.write_use_palette_mode(
        w,
        false,
        bsize,
        tile_bo,
        luma_mode,
        chroma_mode,
        xdec,
        ydec,
        fi.sequence.chroma_sampling,
      );
    }

    if fi.sequence.enable_filter_intra
      && luma_mode == PredictionMode::DC_PRED
      && bsize.width() <= 32
      && bsize.height() <= 32
    {
      cw.write_use_filter_intra(w, false, bsize); // turn off FILTER_INTRA
    }
  }

  // write tx_size here
  if fi.tx_mode_select {
    if bsize > BlockSize::BLOCK_4X4 && (!is_inter || !skip) {
      if !is_inter {
        cw.write_tx_size_intra(w, tile_bo, bsize, tx_size);
        if !W::COUNTS_ONLY {
          cw.bc.update_tx_size_context(tile_bo, bsize, tx_size, false);
        }
      } else {
        // write var_tx_size
        // if here, bsize > BLOCK_4X4 && is_inter && !skip && !Lossless
        debug_assert!(fi.tx_mode_select);
        debug_assert!(bsize > BlockSize::BLOCK_4X4);
        debug_assert!(is_inter);
        debug_assert!(!skip);
        let max_tx_size = max_txsize_rect_lookup[bsize as usize];
        debug_assert!(max_tx_size.block_size() <= BlockSize::BLOCK_64X64);

        //TODO: "&& tx_size.block_size() < bsize" will be replaced with tx-split info for a partition
        //  once it is available.
        let txfm_split =
          fi.enable_inter_txfm_split && tx_size.block_size() < bsize;

        // TODO: Revise write_tx_size_inter() for txfm_split = true
        cw.write_tx_size_inter(
          w,
          tile_bo,
          bsize,
          max_tx_size,
          txfm_split,
          0,
          0,
          0,
        );
      }
    } else {
      debug_assert!(bsize == BlockSize::BLOCK_4X4 || (is_inter && skip));
      if !W::COUNTS_ONLY {
        cw.bc.update_tx_size_context(tile_bo, bsize, tx_size, is_inter && skip);
      }
    }
  }

  if let Some(enc_stats) = enc_stats {
    let pixels = tx_size.area();
    enc_stats.block_size_counts[bsize as usize] += pixels;
    enc_stats.tx_type_counts[tx_type as usize] += pixels;
    enc_stats.luma_pred_mode_counts[luma_mode as usize] += pixels;
    enc_stats.chroma_pred_mode_counts[chroma_mode as usize] += pixels;
    if skip {
      enc_stats.skip_block_count += pixels;
    }
  }

  // prom_av1e024: coded_block_info is only read by FUTURE blocks as their
  // neighbor (above/left_block_info) — a counter trial's own-cell writes are
  // never consumed. Skip the bsize_mi²×3 fill on counting writers.
  if fi.sequence.enable_intra_edge_filter && !W::COUNTS_ONLY {
    for y in 0..bsize.height_mi() {
      if tile_bo.0.y + y >= ts.mi_height {
        continue;
      }
      for x in 0..bsize.width_mi() {
        if tile_bo.0.x + x >= ts.mi_width {
          continue;
        }
        let bi = &mut ts.coded_block_info[tile_bo.0.y + y][tile_bo.0.x + x];
        bi.luma_mode = luma_mode;
        bi.chroma_mode = chroma_mode;
        bi.reference_types = ref_frames;
      }
    }
  }

  if is_inter {
    motion_compensate(
      fi, ts, cw, luma_mode, ref_frames, mvs, bsize, tile_bo, false,
    );
    write_tx_tree(
      fi,
      ts,
      cw,
      w,
      luma_mode,
      angle_delta.y,
      tile_bo,
      bsize,
      tx_size,
      tx_type,
      skip,
      false,
      rdo_type,
      need_recon_pixel,
    )
  } else {
    write_tx_blocks(
      fi,
      ts,
      cw,
      w,
      luma_mode,
      chroma_mode,
      angle_delta,
      tile_bo,
      bsize,
      tx_size,
      tx_type,
      skip,
      cfl,
      false,
      rdo_type,
      need_recon_pixel,
      luma_reuse,
    )
  }
}

/// # Panics
///
/// - If attempting to encode a lossless block (not yet supported)
/// prom_av1e017: cached luma tx-coding results for the intra chroma-mode
/// loop. Iteration 1 fills it; iterations 2+ skip plane-0 prediction and
/// coding entirely and re-inject the cached rate via the fake-bits ledger.
/// Legal in the trial estimator: the iterations differ only in chroma mode,
/// the luma tx section reads no chroma state, and its bc/CDF inputs are
/// identical after the per-iteration rollback (exact under frozen costing,
/// rng-epsilon-exact otherwise) — BD-gated like every estimator change.
#[derive(Default)]
pub struct LumaReuse {
  cached: Option<(u32, ScaledDistortion, bool)>,
}

impl LumaReuse {
  pub fn new() -> Self {
    Self::default()
  }
}

pub fn write_tx_blocks<T: Pixel, W: Writer>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, w: &mut W, luma_mode: PredictionMode,
  chroma_mode: PredictionMode, angle_delta: AngleDelta,
  tile_bo: TileBlockOffset, bsize: BlockSize, tx_size: TxSize,
  tx_type: TxType, skip: bool, cfl: CFLParams, luma_only: bool,
  rdo_type: RDOType, need_recon_pixel: bool,
  luma_reuse: Option<&mut LumaReuse>,
) -> (bool, ScaledDistortion) {
  let _prof = crate::prof::scope(crate::prof::Stage::WriteTxBlocks);
  let bw = bsize.width_mi() / tx_size.width_mi();
  let bh = bsize.height_mi() / tx_size.height_mi();
  let qidx = get_qidx(fi, ts, cw, tile_bo);

  // TODO: Lossless is not yet supported.
  if !skip {
    assert_ne!(qidx, 0);
  }

  let PlaneConfig { xdec, ydec, .. } = ts.input.planes[1].cfg;
  let mut ac = Aligned::<[MaybeUninit<i16>; 32 * 32]>::uninit_array();
  let mut partition_has_coeff: bool = false;
  let mut tx_dist = ScaledDistortion::zero();
  let do_chroma =
    has_chroma(tile_bo, bsize, xdec, ydec, fi.sequence.chroma_sampling);

  ts.qc.update(
    qidx,
    tx_size,
    luma_mode.is_intra(),
    fi.sequence.bit_depth,
    fi.dc_delta_q[0],
    0,
  );

  match luma_reuse {
    Some(reuse) if reuse.cached.is_some() => {
      // Skip the whole plane-0 section: account the cached rate through
      // the fake-bits ledger so the caller's tell_frac delta is unchanged.
      let (rate_frac, dist, has_coeff) = reuse.cached.unwrap();
      w.add_bits_frac(rate_frac);
      partition_has_coeff |= has_coeff;
      tx_dist += dist;
    }
    luma_reuse => {
      let tell = w.tell_frac();
      for by in 0..bh {
        for bx in 0..bw {
          let tx_bo = TileBlockOffset(BlockOffset {
            x: tile_bo.0.x + bx * tx_size.width_mi(),
            y: tile_bo.0.y + by * tx_size.height_mi(),
          });
          if tx_bo.0.x >= ts.mi_width || tx_bo.0.y >= ts.mi_height {
            continue;
          }
          let po = tx_bo.plane_offset(&ts.input.planes[0].cfg);
          let (has_coeff, dist) = encode_tx_block(
            fi,
            ts,
            cw,
            w,
            0,
            tile_bo,
            bx,
            by,
            tx_bo,
            luma_mode,
            tx_size,
            tx_type,
            bsize,
            po,
            skip,
            qidx,
            &[],
            IntraParam::AngleDelta(angle_delta.y),
            rdo_type,
            need_recon_pixel,
          );
          partition_has_coeff |= has_coeff;
          tx_dist += dist;
        }
      }
      if let Some(reuse) = luma_reuse {
        reuse.cached =
          Some((w.tell_frac() - tell, tx_dist, partition_has_coeff));
      }
    }
  }

  if !do_chroma
    || luma_only
    || fi.sequence.chroma_sampling == ChromaSampling::Cs400
  {
    return (partition_has_coeff, tx_dist);
  };
  debug_assert!(has_chroma(
    tile_bo,
    bsize,
    xdec,
    ydec,
    fi.sequence.chroma_sampling
  ));

  let uv_tx_size = bsize.largest_chroma_tx_size(xdec, ydec);

  let mut bw_uv = (bw * tx_size.width_mi()) >> xdec;
  let mut bh_uv = (bh * tx_size.height_mi()) >> ydec;

  if bw_uv == 0 || bh_uv == 0 {
    bw_uv = 1;
    bh_uv = 1;
  }

  bw_uv /= uv_tx_size.width_mi();
  bh_uv /= uv_tx_size.height_mi();

  let ac_data = if chroma_mode.is_cfl() {
    luma_ac(&mut ac.data, ts, tile_bo, bsize, tx_size, fi)
  } else {
    [].as_slice()
  };

  let uv_tx_type = if uv_tx_size.width() >= 32 || uv_tx_size.height() >= 32 {
    TxType::DCT_DCT
  } else {
    uv_intra_mode_to_tx_type_context(chroma_mode)
  };

  for p in 1..3 {
    ts.qc.update(
      qidx,
      uv_tx_size,
      true,
      fi.sequence.bit_depth,
      fi.dc_delta_q[p],
      fi.ac_delta_q[p],
    );
    let alpha = cfl.alpha(p - 1);
    for by in 0..bh_uv {
      for bx in 0..bw_uv {
        let tx_bo = TileBlockOffset(BlockOffset {
          x: tile_bo.0.x + ((bx * uv_tx_size.width_mi()) << xdec)
            - ((bw * tx_size.width_mi() == 1) as usize) * xdec,
          y: tile_bo.0.y + ((by * uv_tx_size.height_mi()) << ydec)
            - ((bh * tx_size.height_mi() == 1) as usize) * ydec,
        });

        let mut po = tile_bo.plane_offset(&ts.input.planes[p].cfg);
        po.x += (bx * uv_tx_size.width()) as isize;
        po.y += (by * uv_tx_size.height()) as isize;
        let (has_coeff, dist) = encode_tx_block(
          fi,
          ts,
          cw,
          w,
          p,
          tile_bo,
          bx,
          by,
          tx_bo,
          chroma_mode,
          uv_tx_size,
          uv_tx_type,
          bsize,
          po,
          skip,
          qidx,
          ac_data,
          if chroma_mode.is_cfl() {
            IntraParam::Alpha(alpha)
          } else {
            IntraParam::AngleDelta(angle_delta.uv)
          },
          rdo_type,
          need_recon_pixel,
        );
        partition_has_coeff |= has_coeff;
        tx_dist += dist;
      }
    }
  }

  (partition_has_coeff, tx_dist)
}

pub fn write_tx_tree<T: Pixel, W: Writer>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, w: &mut W, luma_mode: PredictionMode,
  angle_delta_y: i8, tile_bo: TileBlockOffset, bsize: BlockSize,
  tx_size: TxSize, tx_type: TxType, skip: bool, luma_only: bool,
  rdo_type: RDOType, need_recon_pixel: bool,
) -> (bool, ScaledDistortion) {
  if skip {
    return (false, ScaledDistortion::zero());
  }
  let _prof = crate::prof::scope(crate::prof::Stage::WriteTxTree);
  let bw = bsize.width_mi() / tx_size.width_mi();
  let bh = bsize.height_mi() / tx_size.height_mi();
  let qidx = get_qidx(fi, ts, cw, tile_bo);

  let PlaneConfig { xdec, ydec, .. } = ts.input.planes[1].cfg;
  let ac = &[0i16; 0];
  let mut partition_has_coeff: bool = false;
  let mut tx_dist = ScaledDistortion::zero();

  ts.qc.update(
    qidx,
    tx_size,
    luma_mode.is_intra(),
    fi.sequence.bit_depth,
    fi.dc_delta_q[0],
    0,
  );

  // TODO: If tx-parition more than only 1-level, this code does not work.
  // It should recursively traverse the tx block that are split recursivelty by calling write_tx_tree(),
  // as defined in https://aomediacodec.github.io/av1-spec/#transform-tree-syntax
  for by in 0..bh {
    for bx in 0..bw {
      let tx_bo = TileBlockOffset(BlockOffset {
        x: tile_bo.0.x + bx * tx_size.width_mi(),
        y: tile_bo.0.y + by * tx_size.height_mi(),
      });
      if tx_bo.0.x >= ts.mi_width || tx_bo.0.y >= ts.mi_height {
        continue;
      }

      let po = tx_bo.plane_offset(&ts.input.planes[0].cfg);
      let (has_coeff, dist) = encode_tx_block(
        fi,
        ts,
        cw,
        w,
        0,
        tile_bo,
        0,
        0,
        tx_bo,
        luma_mode,
        tx_size,
        tx_type,
        bsize,
        po,
        skip,
        qidx,
        ac,
        IntraParam::AngleDelta(angle_delta_y),
        rdo_type,
        need_recon_pixel,
      );
      partition_has_coeff |= has_coeff;
      tx_dist += dist;
    }
  }

  if !has_chroma(tile_bo, bsize, xdec, ydec, fi.sequence.chroma_sampling)
    || luma_only
    || fi.sequence.chroma_sampling == ChromaSampling::Cs400
  {
    return (partition_has_coeff, tx_dist);
  };
  debug_assert!(has_chroma(
    tile_bo,
    bsize,
    xdec,
    ydec,
    fi.sequence.chroma_sampling
  ));

  let max_tx_size = max_txsize_rect_lookup[bsize as usize];
  debug_assert!(max_tx_size.block_size() <= BlockSize::BLOCK_64X64);
  let uv_tx_size = bsize.largest_chroma_tx_size(xdec, ydec);

  let mut bw_uv = max_tx_size.width_mi() >> xdec;
  let mut bh_uv = max_tx_size.height_mi() >> ydec;

  if bw_uv == 0 || bh_uv == 0 {
    bw_uv = 1;
    bh_uv = 1;
  }

  bw_uv /= uv_tx_size.width_mi();
  bh_uv /= uv_tx_size.height_mi();

  let uv_tx_type = if partition_has_coeff {
    tx_type.uv_inter(uv_tx_size)
  } else {
    TxType::DCT_DCT
  };

  for p in 1..3 {
    ts.qc.update(
      qidx,
      uv_tx_size,
      false,
      fi.sequence.bit_depth,
      fi.dc_delta_q[p],
      fi.ac_delta_q[p],
    );

    for by in 0..bh_uv {
      for bx in 0..bw_uv {
        let tx_bo = TileBlockOffset(BlockOffset {
          x: tile_bo.0.x + ((bx * uv_tx_size.width_mi()) << xdec)
            - (max_tx_size.width_mi() == 1) as usize * xdec,
          y: tile_bo.0.y + ((by * uv_tx_size.height_mi()) << ydec)
            - (max_tx_size.height_mi() == 1) as usize * ydec,
        });

        let mut po = tile_bo.plane_offset(&ts.input.planes[p].cfg);
        po.x += (bx * uv_tx_size.width()) as isize;
        po.y += (by * uv_tx_size.height()) as isize;
        let (has_coeff, dist) = encode_tx_block(
          fi,
          ts,
          cw,
          w,
          p,
          tile_bo,
          bx,
          by,
          tx_bo,
          luma_mode,
          uv_tx_size,
          uv_tx_type,
          bsize,
          po,
          skip,
          qidx,
          ac,
          IntraParam::AngleDelta(angle_delta_y),
          rdo_type,
          need_recon_pixel,
        );
        partition_has_coeff |= has_coeff;
        tx_dist += dist;
      }
    }
  }

  (partition_has_coeff, tx_dist)
}

#[profiling::function]
pub fn encode_block_with_modes<T: Pixel, W: Writer>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, w_pre_cdef: &mut W, w_post_cdef: &mut W,
  bsize: BlockSize, tile_bo: TileBlockOffset,
  mode_decision: &PartitionParameters, rdo_type: RDOType,
  enc_stats: Option<&mut EncoderStats>,
) {
  let (mode_luma, mode_chroma) =
    (mode_decision.pred_mode_luma, mode_decision.pred_mode_chroma);
  let cfl = mode_decision.pred_cfl_params;
  let ref_frames = mode_decision.ref_frames;
  let mvs = mode_decision.mvs;
  let mut skip = mode_decision.skip;
  let mut cdef_coded = cw.bc.cdef_coded;

  // Set correct segmentation ID before encoding and before
  // rdo_tx_size_type().
  cw.bc.blocks.set_segmentation_idx(tile_bo, bsize, mode_decision.sidx);

  let mut mv_stack = ArrayVec::<CandidateMV, 9>::new();
  let is_compound = ref_frames[1] != NONE_FRAME;
  let mode_context =
    cw.find_mvrefs(tile_bo, ref_frames, &mut mv_stack, bsize, fi, is_compound);

  let (tx_size, tx_type) = if !mode_decision.skip && !mode_decision.has_coeff {
    skip = true;
    rdo_tx_size_type(
      fi, ts, cw, bsize, tile_bo, mode_luma, ref_frames, mvs, skip,
    )
  } else {
    (mode_decision.tx_size, mode_decision.tx_type)
  };

  cdef_coded = encode_block_pre_cdef(
    &fi.sequence,
    ts,
    cw,
    if cdef_coded { w_post_cdef } else { w_pre_cdef },
    bsize,
    tile_bo,
    skip,
  );
  encode_block_post_cdef(
    fi,
    ts,
    cw,
    if cdef_coded { w_post_cdef } else { w_pre_cdef },
    mode_luma,
    mode_chroma,
    mode_decision.angle_delta,
    ref_frames,
    mvs,
    bsize,
    tile_bo,
    skip,
    cfl,
    tx_size,
    tx_type,
    mode_context,
    &mv_stack,
    rdo_type,
    true,
    enc_stats,
  
    None,
  );
}

#[profiling::function]
fn encode_partition_bottomup<T: Pixel, W: Writer>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, w_pre_cdef: &mut W, w_post_cdef: &mut W,
  bsize: BlockSize, tile_bo: TileBlockOffset, ref_rd_cost: f64,
  inter_cfg: &InterConfig, enc_stats: &mut EncoderStats,
) -> PartitionGroupParameters {
  let rdo_type = RDOType::PixelDistRealRate;
  let mut rd_cost = f64::MAX;
  let mut best_rd = f64::MAX;
  let mut rdo_output = PartitionGroupParameters {
    rd_cost,
    part_type: PartitionType::PARTITION_INVALID,
    part_modes: ArrayVec::new(),
  };

  if tile_bo.0.x >= ts.mi_width || tile_bo.0.y >= ts.mi_height {
    return rdo_output;
  }

  let is_square = bsize.is_sqr();
  let hbs = bsize.width_mi() / 2;
  let has_cols = tile_bo.0.x + hbs < ts.mi_width;
  let has_rows = tile_bo.0.y + hbs < ts.mi_height;
  let is_straddle_x = tile_bo.0.x + bsize.width_mi() > ts.mi_width;
  let is_straddle_y = tile_bo.0.y + bsize.height_mi() > ts.mi_height;

  // TODO: Update for 128x128 superblocks
  assert!(fi.partition_range.max <= BlockSize::BLOCK_64X64);

  let must_split =
    is_square && (bsize > fi.partition_range.max || !has_cols || !has_rows);

  let can_split = // FIXME: sub-8x8 inter blocks not supported for non-4:2:0 sampling
    if fi.frame_type.has_inter() &&
      fi.sequence.chroma_sampling != ChromaSampling::Cs420 &&
      bsize <= BlockSize::BLOCK_8X8 {
      false
    } else {
      (bsize > fi.partition_range.min && is_square) || must_split
    };

  assert!(bsize >= BlockSize::BLOCK_8X8 || !can_split);

  let mut best_partition = PartitionType::PARTITION_INVALID;

  let cw_checkpoint = cw.checkpoint(&tile_bo, fi.sequence.chroma_sampling);
  let w_pre_checkpoint = w_pre_cdef.checkpoint();
  let w_post_checkpoint = w_post_cdef.checkpoint();

  // Code the whole block
  if !must_split {
    let cost = if bsize >= BlockSize::BLOCK_8X8 && is_square {
      let w: &mut W = if cw.bc.cdef_coded { w_post_cdef } else { w_pre_cdef };
      let tell = w.tell_frac();
      cw.write_partition(w, tile_bo, PartitionType::PARTITION_NONE, bsize);
      compute_rd_cost(fi, w.tell_frac() - tell, ScaledDistortion::zero())
    } else {
      0.0
    };

    let mode_decision =
      rdo_mode_decision(fi, ts, cw, bsize, tile_bo, inter_cfg);

    if !mode_decision.pred_mode_luma.is_intra() {
      // Fill the saved motion structure
      save_block_motion(
        ts,
        mode_decision.bsize,
        mode_decision.bo,
        mode_decision.ref_frames[0].to_index(),
        mode_decision.mvs[0],
      );
    }

    rd_cost = mode_decision.rd_cost + cost;

    best_partition = PartitionType::PARTITION_NONE;
    best_rd = rd_cost;
    rdo_output.part_modes.push(mode_decision.clone());

    if !can_split {
      encode_block_with_modes(
        fi,
        ts,
        cw,
        w_pre_cdef,
        w_post_cdef,
        bsize,
        tile_bo,
        &mode_decision,
        rdo_type,
        Some(enc_stats),
      );
    }
  } // if !must_split

  let mut early_exit = false;

  // Test all partition types other than PARTITION_NONE by comparing their RD costs
  if can_split {
    debug_assert!(is_square);

    let mut partition_types = ArrayVec::<PartitionType, 3>::new();
    if bsize
      <= fi.config.speed_settings.partition.non_square_partition_max_threshold
      || is_straddle_x
      || is_straddle_y
    {
      if has_cols {
        partition_types.push(PartitionType::PARTITION_HORZ);
      }
      if !(fi.sequence.chroma_sampling == ChromaSampling::Cs422) && has_rows {
        partition_types.push(PartitionType::PARTITION_VERT);
      }
    }
    partition_types.push(PartitionType::PARTITION_SPLIT);

    for partition in partition_types {
      // (!has_rows || !has_cols) --> must_split
      debug_assert!((has_rows && has_cols) || must_split);
      // (!has_rows && has_cols) --> partition != PartitionType::PARTITION_VERT
      debug_assert!(
        has_rows || !has_cols || (partition != PartitionType::PARTITION_VERT)
      );
      // (has_rows && !has_cols) --> partition != PartitionType::PARTITION_HORZ
      debug_assert!(
        !has_rows || has_cols || (partition != PartitionType::PARTITION_HORZ)
      );
      // (!has_rows && !has_cols) --> partition == PartitionType::PARTITION_SPLIT
      debug_assert!(
        has_rows || has_cols || (partition == PartitionType::PARTITION_SPLIT)
      );

      cw.rollback(&cw_checkpoint);
      w_pre_cdef.rollback(&w_pre_checkpoint);
      w_post_cdef.rollback(&w_post_checkpoint);

      let subsize = bsize.subsize(partition).unwrap();
      let hbsw = subsize.width_mi(); // Half the block size width in blocks
      let hbsh = subsize.height_mi(); // Half the block size height in blocks
      let mut child_modes = ArrayVec::<PartitionParameters, 4>::new();
      rd_cost = 0.0;

      if bsize >= BlockSize::BLOCK_8X8 {
        let w: &mut W =
          if cw.bc.cdef_coded { w_post_cdef } else { w_pre_cdef };
        let tell = w.tell_frac();
        cw.write_partition(w, tile_bo, partition, bsize);
        rd_cost =
          compute_rd_cost(fi, w.tell_frac() - tell, ScaledDistortion::zero());
      }

      let four_partitions = [
        tile_bo,
        TileBlockOffset(BlockOffset { x: tile_bo.0.x + hbsw, y: tile_bo.0.y }),
        TileBlockOffset(BlockOffset { x: tile_bo.0.x, y: tile_bo.0.y + hbsh }),
        TileBlockOffset(BlockOffset {
          x: tile_bo.0.x + hbsw,
          y: tile_bo.0.y + hbsh,
        }),
      ];
      let partitions = get_sub_partitions(&four_partitions, partition);

      early_exit = false;
      // If either of horz or vert partition types is being tested,
      // two partitioned rectangles, defined in 'partitions', of the current block
      // is passed to encode_partition_bottomup()
      for offset in partitions {
        if offset.0.x >= ts.mi_width || offset.0.y >= ts.mi_height {
          continue;
        }
        let child_rdo_output = encode_partition_bottomup(
          fi,
          ts,
          cw,
          w_pre_cdef,
          w_post_cdef,
          subsize,
          offset,
          best_rd,
          inter_cfg,
          enc_stats,
        );
        let cost = child_rdo_output.rd_cost;
        assert!(cost >= 0.0);

        if cost != f64::MAX {
          rd_cost += cost;
          if !must_split
            && fi.enable_early_exit
            && (rd_cost >= best_rd || rd_cost >= ref_rd_cost)
          {
            assert!(cost != f64::MAX);
            early_exit = true;
            break;
          } else if partition != PartitionType::PARTITION_SPLIT {
            child_modes.push(child_rdo_output.part_modes[0].clone());
          }
        }
      }

      if !early_exit && rd_cost < best_rd {
        best_rd = rd_cost;
        best_partition = partition;
        if partition != PartitionType::PARTITION_SPLIT {
          assert!(!child_modes.is_empty());
          rdo_output.part_modes = child_modes;
        }
      }
    }

    debug_assert!(
      early_exit || best_partition != PartitionType::PARTITION_INVALID
    );

    // If the best partition is not PARTITION_SPLIT, recode it
    if best_partition != PartitionType::PARTITION_SPLIT {
      assert!(!rdo_output.part_modes.is_empty());
      cw.rollback(&cw_checkpoint);
      w_pre_cdef.rollback(&w_pre_checkpoint);
      w_post_cdef.rollback(&w_post_checkpoint);

      assert!(best_partition != PartitionType::PARTITION_NONE || !must_split);
      let subsize = bsize.subsize(best_partition).unwrap();

      if bsize >= BlockSize::BLOCK_8X8 {
        let w: &mut W =
          if cw.bc.cdef_coded { w_post_cdef } else { w_pre_cdef };
        cw.write_partition(w, tile_bo, best_partition, bsize);
      }
      for mode in rdo_output.part_modes.clone() {
        assert!(subsize == mode.bsize);

        if !mode.pred_mode_luma.is_intra() {
          save_block_motion(
            ts,
            mode.bsize,
            mode.bo,
            mode.ref_frames[0].to_index(),
            mode.mvs[0],
          );
        }

        // FIXME: redundant block re-encode
        encode_block_with_modes(
          fi,
          ts,
          cw,
          w_pre_cdef,
          w_post_cdef,
          mode.bsize,
          mode.bo,
          &mode,
          rdo_type,
          Some(enc_stats),
        );
      }
    }
  } // if can_split {

  assert!(best_partition != PartitionType::PARTITION_INVALID);

  if is_square
    && bsize >= BlockSize::BLOCK_8X8
    && (bsize == BlockSize::BLOCK_8X8
      || best_partition != PartitionType::PARTITION_SPLIT)
  {
    cw.bc.update_partition_context(
      tile_bo,
      bsize.subsize(best_partition).unwrap(),
      bsize,
    );
  }

  rdo_output.rd_cost = best_rd;
  rdo_output.part_type = best_partition;

  if best_partition != PartitionType::PARTITION_NONE {
    rdo_output.part_modes.clear();
  }
  rdo_output
}

// prom_av1e033 (Brick 3): the per-tile dispatch threshold, set by the
// per-frame percentile pre-pass. `Some((thresh, route_hi))` ⇒ dispatch mode;
// `None` ⇒ not in dispatch mode (force-on or off). Thread-local so each tile
// thread carries its own frame threshold.
thread_local! {
  // (routing_thresh, route_hi, internal_split_t) when in dispatch mode.
  static VARPART_DISPATCH: std::cell::Cell<Option<(i64, bool, i64)>> =
    const { std::cell::Cell::new(None) };
  // prom_av1e041: variance cutoff below which an SB is DEEPENED (low-variance
  // fraction q). `Some(t)` in deep-dispatch mode; `None` off.
  // prom_av1e041: (cutoff, route_lo). route_lo=false ⇒ deepen var>=cutoff
  // (high-variance/busy fraction, the default). route_lo=true ⇒ var<=cutoff.
  static DEEP_CUTOFF: std::cell::Cell<Option<(i64, bool)>> =
    const { std::cell::Cell::new(None) };
}

/// The threshold for the routed SB's INTERNAL NONE/SPLIT decision — the
/// quantizer-derived value (dispatch mode) or the fixed `RAV1E_VARPART`
/// value (force-on). This is SEPARATE from the routing percentile: the
/// routing percentile decides WHICH SBs use varpart, this decides HOW they
/// partition (tuned to coding difficulty, not to the SB population).
#[inline]
fn varpart_split_threshold() -> i64 {
  VARPART_DISPATCH
    .with(|c| c.get())
    .map(|(_, _, t)| t)
    .or_else(crate::harvest::varpart)
    .unwrap_or(i64::MAX)
}

/// Whether any SB should build the variance tree at all (force-on or in
/// dispatch mode).
#[inline]
fn varpart_sb_active() -> bool {
  crate::harvest::varpart().is_some()
    || VARPART_DISPATCH.with(|c| c.get()).is_some()
}

/// Route THIS SB (whose root variance is `v64`) to the variance partition?
/// Force-on ⇒ always; dispatch ⇒ low-variance (or high, if `route_hi`) side
/// of the frame routing percentile.
#[inline]
fn varpart_route_sb(v64: i64) -> bool {
  VARPART_DISPATCH.with(|c| c.get()).map_or(true, |(thresh, route_hi, _)| {
    if route_hi {
      v64 >= thresh
    } else {
      v64 <= thresh
    }
  })
}

// prom_av1e036: attribute every forward_transform (the full-price kernel) to
// its RDO phase to find the 68×-vs-SVT redundancy. Phase set by the RDO entry
// points; counts split trial (counter) vs final (real coder).
#[cfg(feature = "profile")]
pub(crate) mod fwd_phase {
  use std::cell::Cell;
  use std::sync::atomic::{AtomicU64, Ordering};
  thread_local! { pub static PHASE: Cell<usize> = const { Cell::new(2) }; }
  // [phase][counts_only]: phase 0=mode-trial 1=tx-search 2=final/other
  pub static N: [[AtomicU64; 2]; 3] =
    [const { [const { AtomicU64::new(0) }; 2] }; 3];

  pub struct Guard(usize);
  pub fn enter(p: usize) -> Guard {
    Guard(PHASE.with(|c| c.replace(p)))
  }
  impl Drop for Guard {
    fn drop(&mut self) {
      PHASE.with(|c| c.set(self.0));
    }
  }
  pub fn count(counts_only: bool) {
    let p = PHASE.with(|c| c.get());
    N[p][usize::from(!counts_only)].fetch_add(1, Ordering::Relaxed);
  }
  pub fn dump() {
    let names = ["mode-trial", "tx-search ", "final/othr"];
    let mut tot = 0u64;
    for p in 0..3 {
      let t = N[p][0].load(Ordering::Relaxed);
      let f = N[p][1].load(Ordering::Relaxed);
      tot += t + f;
      eprintln!("FWDPHASE {} trial={} final={}", names[p], t, f);
    }
    eprintln!("FWDPHASE TOTAL {}", tot);
  }
}

// prom_av1e041: per-SB DEEP-search dispatch (content-adaptive quality ladder).
// A thread-local flag set around a chosen SB's encode; the deep levers (tx-type/
// size RDO, unlocked for inter too) consult it. The dispatcher deepens a
// content-adaptive fraction of SBs — low-variance first (2.5× more BD/sec, the
// av1e040 cost-effectiveness finding). Off ⇒ the fast tier, byte-identical.
pub(crate) mod deep {
  use std::cell::Cell;
  thread_local! { pub static ACTIVE: Cell<bool> = const { Cell::new(false) }; }
  #[inline]
  pub fn active() -> bool {
    ACTIVE.with(|c| c.get())
  }
  pub struct Guard(bool);
  #[inline]
  pub fn enter(v: bool) -> Guard {
    Guard(ACTIVE.with(|c| c.replace(v)))
  }
  impl Drop for Guard {
    fn drop(&mut self) {
      ACTIVE.with(|c| c.set(self.0));
    }
  }
}

// prom_av1e045: the adaptively-chosen per-frame interp filter (Some ⇒ overrides
// fi.default_filter for BOTH prediction and the header signal, so they stay
// consistent). Set once per frame before the SB loop; None ⇒ use fi's default.
pub(crate) mod frame_filter {
  use crate::mc::FilterMode;
  use std::cell::Cell;
  thread_local! { pub static F: Cell<Option<FilterMode>> = const { Cell::new(None) }; }
  #[inline]
  pub fn get() -> Option<FilterMode> {
    F.with(|c| c.get())
  }
  #[inline]
  pub fn set(v: Option<FilterMode>) {
    F.with(|c| c.set(v));
  }
}

// prom_av1e047: per-SB gate for the trellis — true when this SB is NOT flat
// (absolute residual variance above the threshold), so the trellis (which
// helps busy content, marginally hurts flat) is dispatched off akiyo-like SBs.
pub(crate) mod trellis_gate {
  use std::cell::Cell;
  thread_local! { pub static ON: Cell<bool> = const { Cell::new(false) }; }
  #[inline]
  pub fn on() -> bool {
    ON.with(|c| c.get())
  }
  #[inline]
  pub fn set(v: bool) {
    ON.with(|c| c.set(v));
  }
}

/// prom_av1e045: pick this inter frame's fixed interp filter by a cheap SATD
/// trial (the sign-flip → dispatch rule). Sample a coarse grid of SBs, ME to
/// LAST, predict with REGULAR vs SHARP, and take the lower-total-SATD filter.
/// Predicts into ts.rec (overwritten by the real encode, like pd0_proxy_cost);
/// reads only the LAST reference — no CDF/block-info touched.
fn pick_frame_filter<T: Pixel>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>, cw: &mut ContextWriter,
) -> crate::mc::FilterMode {
  use crate::mc::FilterMode;
  let ref_frame = LAST_FRAME;
  if fi.rec_buffer.frames[fi.ref_frames[ref_frame.to_index()] as usize].is_none()
  {
    return fi.default_filter;
  }
  let bsize = BlockSize::BLOCK_64X64;
  let (w, h) = (bsize.width(), bsize.height());
  let tile_rect = ts.tile_rect();
  let (mut satd_reg, mut satd_sharp) = (0u64, 0u64);

  let mut sby = 0;
  while sby < ts.sb_height {
    let mut sbx = 0;
    while sbx < ts.sb_width {
      let tile_bo =
        TileSuperBlockOffset(SuperBlockOffset { x: sbx, y: sby })
          .block_offset(0, 0);
      sbx += 2; // coarse grid — a frame-level estimate, not per-block
      if tile_bo.0.x + 16 > ts.mi_width || tile_bo.0.y + 16 > ts.mi_height {
        continue;
      }
      let mut mv_stack = ArrayVec::<CandidateMV, 9>::new();
      let _ = cw.find_mvrefs(
        tile_bo,
        [ref_frame, NONE_FRAME],
        &mut mv_stack,
        bsize,
        fi,
        false,
      );
      let pmv = [
        mv_stack.first().map_or(MotionVector::default(), |c| c.this_mv),
        MotionVector::default(),
      ];
      let mv = estimate_motion(
        fi,
        ts,
        w,
        h,
        tile_bo,
        ref_frame,
        Some(pmv),
        MVSamplingMode::CORNER { right: true, bottom: true },
        false,
        0,
        None,
      )
      .map_or(pmv[0], |r| r.mv);

      let ref_frames = [ref_frame, NONE_FRAME];
      let mvs = [mv, MotionVector::default()];
      let po = tile_bo.plane_offset(ts.rec.planes[0].plane_cfg);
      // Predict with each filter into rec (overwritten by the real encode, like
      // pd0_proxy_cost); reuse predict_inter, which reads frame_filter, so the
      // trial's prediction is exactly what the real encode would produce.
      for (fm, acc) in [
        (FilterMode::REGULAR, &mut satd_reg),
        (FilterMode::SHARP, &mut satd_sharp),
      ] {
        frame_filter::set(Some(fm));
        {
          let mut dst = ts.rec.planes[0]
            .subregion_mut(Area::BlockStartingAt { bo: tile_bo.0 });
          PredictionMode::NEWMV.predict_inter(
            fi,
            tile_rect,
            0,
            po,
            &mut dst,
            w,
            h,
            ref_frames,
            mvs,
            &mut ts.inter_compound_buffers,
          );
        }
        let input = ts.input_tile.planes[0]
          .subregion(Area::BlockStartingAt { bo: tile_bo.0 });
        let recr =
          ts.rec.planes[0].subregion(Area::BlockStartingAt { bo: tile_bo.0 });
        *acc += crate::dist::get_satd(
          &input,
          &recr,
          w,
          h,
          fi.sequence.bit_depth,
          fi.cpu_feature_level,
        ) as u64;
      }
    }
    sby += 2;
  }
  // reset so the winner (set by the caller) isn't polluted by the last probe
  frame_filter::set(None);

  if satd_sharp < satd_reg {
    FilterMode::SHARP
  } else {
    FilterMode::REGULAR
  }
}

/// prom_av1e031/032: build the 64×64 residual variance tree for the SB at
/// `tile_bo` — source vs the co-located LAST reference (zero-MV residual =
/// coding difficulty). Falls back to source variance when LAST is absent
/// (e.g. intra frames). Edge SBs clamp-fill to the visible extent.
fn build_sb_var_tree<T: Pixel>(
  fi: &FrameInvariants<T>, ts: &TileStateMut<'_, T>, tile_bo: TileBlockOffset,
) -> crate::varpart::VarTree {
  let vis_w = ((ts.mi_width - tile_bo.0.x) * MI_SIZE).min(64);
  let vis_h = ((ts.mi_height - tile_bo.0.y) * MI_SIZE).min(64);
  let src =
    ts.input_tile.planes[0].subregion(Area::BlockStartingAt { bo: tile_bo.0 });

  let last = fi.rec_buffer.frames
    [fi.ref_frames[LAST_FRAME.to_index()] as usize]
    .as_ref();
  if let Some(rec) = last {
    let frame_bo = ts.to_frame_block_offset(tile_bo);
    let po = frame_bo.to_luma_plane_offset();
    let refr =
      rec.frame.planes[0].region(Area::StartingAt { x: po.x, y: po.y });
    crate::varpart::build_var_tree(&src, Some(&refr), vis_w, vis_h)
  } else {
    crate::varpart::build_var_tree(&src, None, vis_w, vis_h)
  }
}

/// Root-only residual variance for the SB at `tile_bo` (the dispatch
/// pre-pass signal — cheaper than the full tree).
fn sb_root_var<T: Pixel>(
  fi: &FrameInvariants<T>, ts: &TileStateMut<'_, T>, tile_bo: TileBlockOffset,
) -> i64 {
  let vis_w = ((ts.mi_width - tile_bo.0.x) * MI_SIZE).min(64);
  let vis_h = ((ts.mi_height - tile_bo.0.y) * MI_SIZE).min(64);
  let src =
    ts.input_tile.planes[0].subregion(Area::BlockStartingAt { bo: tile_bo.0 });
  let last = fi.rec_buffer.frames
    [fi.ref_frames[LAST_FRAME.to_index()] as usize]
    .as_ref();
  if let Some(rec) = last {
    let frame_bo = ts.to_frame_block_offset(tile_bo);
    let po = frame_bo.to_luma_plane_offset();
    let refr =
      rec.frame.planes[0].region(Area::StartingAt { x: po.x, y: po.y });
    crate::varpart::sb_root_variance(&src, Some(&refr), vis_w, vis_h)
  } else {
    crate::varpart::sb_root_variance(&src, None, vis_w, vis_h)
  }
}

fn encode_partition_topdown<T: Pixel, W: Writer>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, w_pre_cdef: &mut W, w_post_cdef: &mut W,
  bsize: BlockSize, tile_bo: TileBlockOffset,
  block_output: &Option<PartitionGroupParameters>, inter_cfg: &InterConfig,
  enc_stats: &mut EncoderStats, vt: Option<&crate::varpart::VarTree>,
) {
  if tile_bo.0.x >= ts.mi_width || tile_bo.0.y >= ts.mi_height {
    return;
  }
  // prom_av1e031/032: content-adaptive dispatch. At the 64×64 root under
  // RAV1E_VARPART, build the residual variance tree once and thread it down
  // the recursion; each square node reads its NONE/SPLIT decision from it
  // instead of running rdo_partition_decision.
  let owned_vt;
  let vt = if vt.is_none()
    && bsize == BlockSize::BLOCK_64X64
    && varpart_sb_active()
  {
    owned_vt = build_sb_var_tree(fi, ts, tile_bo);
    // Route this SB by its root variance (dispatch) or always (force-on).
    if varpart_route_sb(owned_vt.v64.variance()) {
      Some(&owned_vt)
    } else {
      None
    }
  } else {
    vt
  };
  let is_square = bsize.is_sqr();
  let rdo_type = RDOType::PixelDistRealRate;
  let hbs = bsize.width_mi() / 2;
  let has_cols = tile_bo.0.x + hbs < ts.mi_width;
  let has_rows = tile_bo.0.y + hbs < ts.mi_height;

  // TODO: Update for 128x128 superblocks
  debug_assert!(fi.partition_range.max <= BlockSize::BLOCK_64X64);

  let must_split =
    is_square && (bsize > fi.partition_range.max || !has_cols || !has_rows);

  let can_split = // FIXME: sub-8x8 inter blocks not supported for non-4:2:0 sampling
    if fi.frame_type.has_inter() &&
      fi.sequence.chroma_sampling != ChromaSampling::Cs420 &&
      bsize <= BlockSize::BLOCK_8X8 {
      false
    } else {
      (bsize > fi.partition_range.min && is_square) || must_split
    };

  let mut rdo_output =
    block_output.clone().unwrap_or_else(|| PartitionGroupParameters {
      part_type: PartitionType::PARTITION_INVALID,
      rd_cost: f64::MAX,
      part_modes: ArrayVec::new(),
    });

  let partition = if must_split {
    PartitionType::PARTITION_SPLIT
  } else if let Some(vt) = vt {
    // prom_av1e031/032/033: variance-partition alternative — SPLIT iff this
    // node's residual variance exceeds the (percentile or fixed) threshold;
    // the 8×8 floor never splits. No RD search runs; part_modes stays empty
    // so the NONE arm falls through to rdo_mode_decision (the identical leaf).
    debug_assert!(bsize.is_sqr());
    let t = varpart_split_threshold();
    let dim = bsize.width();
    let ox = (tile_bo.0.x % 16) * MI_SIZE;
    let oy = (tile_bo.0.y % 16) * MI_SIZE;
    if bsize > BlockSize::BLOCK_8X8 && vt.node_variance(dim, ox, oy) > t {
      PartitionType::PARTITION_SPLIT
    } else {
      PartitionType::PARTITION_NONE
    }
  } else if can_split {
    debug_assert!(bsize.is_sqr());

    // prom_av1e023: SB early skip — at the 64×64 root, a near-zero
    // NEARESTMV proxy bypasses the whole partition/mode RDO below.
    let mut forced_skip = false;
    if bsize == BlockSize::BLOCK_64X64
      && fi.frame_type.has_inter()
      && rdo_output.part_modes.is_empty()
    {
      if let Some(k) = crate::harvest::sb_skip() {
        if let Some(forced) =
          crate::rdo::sb_skip_probe(fi, ts, cw, tile_bo, k)
        {
          rdo_output.part_modes.push(forced);
          rdo_output.part_type = PartitionType::PARTITION_NONE;
          rdo_output.rd_cost = 0.0;
          forced_skip = true;
        }
      }
    }
    if !forced_skip {
      // Blocks of sizes within the supported range are subjected to a partitioning decision
      rdo_output = rdo_partition_decision(
        fi,
        ts,
        cw,
        w_pre_cdef,
        w_post_cdef,
        bsize,
        tile_bo,
        &rdo_output,
        // prom_av1e005: NONE evaluated FIRST — its cost both feeds the
        // partition gate and gives rdo_partition_simple a real early-exit
        // bound. Order affects output only on exact RD ties (verified
        // byte-identical on the corpus + FNV clip).
        // prom_av1e042: rect partitions (HORZ/VERT) added to the deep set here
        // were REFUTED — greedy topdown rect can't refine (rect leaf ≠ square ⇒
        // no further split) so it locks worse structures: force-on −0.514% (vs
        // −2.47% tx-type alone), foreman +2.18%, +156% wall. Rect needs the
        // bottomup full-tree search, not a topdown add. Reverted.
        &[PartitionType::PARTITION_NONE, PartitionType::PARTITION_SPLIT],
        rdo_type,
        inter_cfg,
      );
    }
    rdo_output.part_type
  } else {
    // Blocks of sizes below the supported range are encoded directly
    PartitionType::PARTITION_NONE
  };

  debug_assert!(partition != PartitionType::PARTITION_INVALID);

  let subsize = bsize.subsize(partition).unwrap();

  if bsize >= BlockSize::BLOCK_8X8 && is_square {
    let w: &mut W = if cw.bc.cdef_coded { w_post_cdef } else { w_pre_cdef };
    cw.write_partition(w, tile_bo, partition, bsize);
  }

  match partition {
    PartitionType::PARTITION_NONE => {
      // prom_av1e007 (brick ②) measurement: the winner-recode tax — this arm
      // re-derives tx size/type and re-encodes the already-decided block.
      let _prof_fe = crate::prof::scope(crate::prof::Stage::FinalEncode);
      let rdo_decision;
      let part_decision =
        if let Some(part_mode) = rdo_output.part_modes.first() {
          // The optimal prediction mode is known from a previous iteration
          part_mode
        } else {
          // Make a prediction mode decision for blocks encoded with no rdo_partition_decision call (e.g. edges)
          rdo_decision =
            rdo_mode_decision(fi, ts, cw, bsize, tile_bo, inter_cfg);
          &rdo_decision
        };

      let mut mode_luma = part_decision.pred_mode_luma;
      let mut mode_chroma = part_decision.pred_mode_chroma;

      let cfl = part_decision.pred_cfl_params;
      let skip = part_decision.skip;
      let ref_frames = part_decision.ref_frames;
      let mvs = part_decision.mvs;
      let mut cdef_coded = cw.bc.cdef_coded;

      // Set correct segmentation ID before encoding and before
      // rdo_tx_size_type().
      cw.bc.blocks.set_segmentation_idx(tile_bo, bsize, part_decision.sidx);

      // NOTE: Cannot avoid calling rdo_tx_size_type() here again,
      // because, with top-down partition RDO, the neighboring contexts
      // of current partition can change, i.e. neighboring partitions can split down more.
      let (tx_size, tx_type) = rdo_tx_size_type(
        fi, ts, cw, bsize, tile_bo, mode_luma, ref_frames, mvs, skip,
      );

      let mut mv_stack = ArrayVec::<CandidateMV, 9>::new();
      let is_compound = ref_frames[1] != NONE_FRAME;
      let mode_context = cw.find_mvrefs(
        tile_bo,
        ref_frames,
        &mut mv_stack,
        bsize,
        fi,
        is_compound,
      );

      // TODO: proper remap when is_compound is true
      if !mode_luma.is_intra() {
        if is_compound && mode_luma != PredictionMode::GLOBAL_GLOBALMV {
          let match0 = mv_stack[0].this_mv.row == mvs[0].row
            && mv_stack[0].this_mv.col == mvs[0].col;
          let match1 = mv_stack[0].comp_mv.row == mvs[1].row
            && mv_stack[0].comp_mv.col == mvs[1].col;

          let match2 = mv_stack[1].this_mv.row == mvs[0].row
            && mv_stack[1].this_mv.col == mvs[0].col;
          let match3 = mv_stack[1].comp_mv.row == mvs[1].row
            && mv_stack[1].comp_mv.col == mvs[1].col;

          let match4 = mv_stack.len() > 2 && mv_stack[2].this_mv == mvs[0];
          let match5 = mv_stack.len() > 2 && mv_stack[2].comp_mv == mvs[1];

          let match6 = mv_stack.len() > 3 && mv_stack[3].this_mv == mvs[0];
          let match7 = mv_stack.len() > 3 && mv_stack[3].comp_mv == mvs[1];

          mode_luma = if match0 && match1 {
            PredictionMode::NEAREST_NEARESTMV
          } else if match2 && match3 {
            PredictionMode::NEAR_NEAR0MV
          } else if match4 && match5 {
            PredictionMode::NEAR_NEAR1MV
          } else if match6 && match7 {
            PredictionMode::NEAR_NEAR2MV
          } else if match0 {
            PredictionMode::NEAREST_NEWMV
          } else if match1 {
            PredictionMode::NEW_NEARESTMV
          } else {
            PredictionMode::NEW_NEWMV
          };

          if mode_luma != PredictionMode::NEAREST_NEARESTMV
            && mvs[0].row == 0
            && mvs[0].col == 0
            && mvs[1].row == 0
            && mvs[1].col == 0
          {
            mode_luma = PredictionMode::GLOBAL_GLOBALMV;
          }
          mode_chroma = mode_luma;
        } else if !is_compound && mode_luma != PredictionMode::GLOBALMV {
          mode_luma = PredictionMode::NEWMV;
          for (c, m) in mv_stack.iter().take(4).zip(
            [
              PredictionMode::NEARESTMV,
              PredictionMode::NEAR0MV,
              PredictionMode::NEAR1MV,
              PredictionMode::NEAR2MV,
            ]
            .iter(),
          ) {
            if c.this_mv.row == mvs[0].row && c.this_mv.col == mvs[0].col {
              mode_luma = *m;
            }
          }
          if mode_luma == PredictionMode::NEWMV
            && mvs[0].row == 0
            && mvs[0].col == 0
          {
            mode_luma = if mv_stack.is_empty() {
              PredictionMode::NEARESTMV
            } else if mv_stack.len() == 1 {
              PredictionMode::NEAR0MV
            } else {
              PredictionMode::GLOBALMV
            };
          }
          mode_chroma = mode_luma;
        }

        save_block_motion(
          ts,
          part_decision.bsize,
          part_decision.bo,
          part_decision.ref_frames[0].to_index(),
          part_decision.mvs[0],
        );
      }

      // FIXME: every final block that has gone through the RDO decision process is encoded twice
      cdef_coded = encode_block_pre_cdef(
        &fi.sequence,
        ts,
        cw,
        if cdef_coded { w_post_cdef } else { w_pre_cdef },
        bsize,
        tile_bo,
        skip,
      );
      encode_block_post_cdef(
        fi,
        ts,
        cw,
        if cdef_coded { w_post_cdef } else { w_pre_cdef },
        mode_luma,
        mode_chroma,
        part_decision.angle_delta,
        ref_frames,
        mvs,
        bsize,
        tile_bo,
        skip,
        cfl,
        tx_size,
        tx_type,
        mode_context,
        &mv_stack,
        RDOType::PixelDistRealRate,
        true,
        Some(enc_stats),
      
    None,
  );
    }
    PARTITION_SPLIT | PARTITION_HORZ | PARTITION_VERT => {
      if !rdo_output.part_modes.is_empty() {
        debug_assert!(can_split && !must_split);

        // The optimal prediction modes for each split block is known from an rdo_partition_decision() call
        for mode in rdo_output.part_modes {
          // Each block is subjected to a new splitting decision
          encode_partition_topdown(
            fi,
            ts,
            cw,
            w_pre_cdef,
            w_post_cdef,
            subsize,
            mode.bo,
            &Some(PartitionGroupParameters {
              rd_cost: mode.rd_cost,
              part_type: PartitionType::PARTITION_NONE,
              part_modes: [mode][..].try_into().unwrap(),
            }),
            inter_cfg,
            enc_stats,
            vt,
          );
        }
      } else {
        // varpart (prom_av1e031/032/033) reaches SPLIT with empty part_modes.
        debug_assert!(must_split || varpart_sb_active());
        let hbsw = subsize.width_mi(); // Half the block size width in blocks
        let hbsh = subsize.height_mi(); // Half the block size height in blocks
        let four_partitions = [
          tile_bo,
          TileBlockOffset(BlockOffset {
            x: tile_bo.0.x + hbsw,
            y: tile_bo.0.y,
          }),
          TileBlockOffset(BlockOffset {
            x: tile_bo.0.x,
            y: tile_bo.0.y + hbsh,
          }),
          TileBlockOffset(BlockOffset {
            x: tile_bo.0.x + hbsw,
            y: tile_bo.0.y + hbsh,
          }),
        ];
        let partitions = get_sub_partitions(&four_partitions, partition);

        partitions.iter().for_each(|&offset| {
          encode_partition_topdown(
            fi,
            ts,
            cw,
            w_pre_cdef,
            w_post_cdef,
            subsize,
            offset,
            &None,
            inter_cfg,
            enc_stats,
            vt,
          );
        });
      }
    }
    _ => unreachable!(),
  }

  if is_square
    && bsize >= BlockSize::BLOCK_8X8
    && (bsize == BlockSize::BLOCK_8X8
      || partition != PartitionType::PARTITION_SPLIT)
  {
    cw.bc.update_partition_context(tile_bo, subsize, bsize);
  }
}

fn get_initial_cdfcontext<T: Pixel>(fi: &FrameInvariants<T>) -> CDFContext {
  let cdf = if fi.primary_ref_frame == PRIMARY_REF_NONE {
    None
  } else {
    let ref_frame_idx = fi.ref_frames[fi.primary_ref_frame as usize] as usize;
    let ref_frame = fi.rec_buffer.frames[ref_frame_idx].as_ref();
    ref_frame.map(|rec| rec.cdfs)
  };

  // return the retrieved instance if any, a new one otherwise
  cdf.unwrap_or_else(|| CDFContext::new(fi.base_q_idx))
}

#[profiling::function]
fn encode_tile_group<T: Pixel>(
  fi: &FrameInvariants<T>, fs: &mut FrameState<T>, inter_cfg: &InterConfig,
) -> Vec<u8> {
  let planes =
    if fi.sequence.chroma_sampling == ChromaSampling::Cs400 { 1 } else { 3 };
  let mut blocks = FrameBlocks::new(fi.w_in_b, fi.h_in_b);
  let ti = &fi.sequence.tiling;

  let initial_cdf = get_initial_cdfcontext(fi);
  // dynamic allocation: once per frame
  let mut cdfs = vec![initial_cdf; ti.tile_count()];

  let (raw_tiles, stats): (Vec<_>, Vec<_>) = ti
    .tile_iter_mut(fs, &mut blocks)
    .zip(cdfs.iter_mut())
    .collect::<Vec<_>>()
    .into_par_iter()
    .map(|(mut ctx, cdf)| {
      encode_tile(fi, &mut ctx.ts, cdf, &mut ctx.tb, inter_cfg)
    })
    .unzip();

  for tile_stats in stats {
    fs.enc_stats += &tile_stats;
  }

  /* Frame deblocking operates over a single large tile wrapping the
   * frame rather than the frame itself so that deblocking is
   * available inside RDO when needed */
  /* TODO: Don't apply if lossless */
  let levels = {
    let _prof = crate::prof::scope(crate::prof::Stage::Deblock);
    fs.apply_tile_state_mut(|ts| {
      let rec = &mut ts.rec;
      deblock_filter_optimize(
        fi,
        &rec.as_const(),
        &ts.input.as_tile(),
        &blocks.as_tile_blocks(),
        fi.width,
        fi.height,
      )
    })
  };
  fs.deblock.levels = levels;

  if fs.deblock.levels[0] != 0 || fs.deblock.levels[1] != 0 {
    let _prof = crate::prof::scope(crate::prof::Stage::Deblock);
    fs.apply_tile_state_mut(|ts| {
      let rec = &mut ts.rec;
      deblock_filter_frame(
        ts.deblock,
        rec,
        &blocks.as_tile_blocks(),
        fi.width,
        fi.height,
        fi.sequence.bit_depth,
        planes,
      );
    });
  }

  if fi.sequence.enable_restoration {
    // Until the loop filters are better pipelined, we'll need to keep
    // around a copy of both the deblocked and cdeffed frame.
    let deblocked_frame = (*fs.rec).clone();

    /* TODO: Don't apply if lossless */
    if fi.sequence.enable_cdef {
      let _prof = crate::prof::scope(crate::prof::Stage::Cdef);
      fs.apply_tile_state_mut(|ts| {
        let rec = &mut ts.rec;
        cdef_filter_tile(fi, &deblocked_frame, &blocks.as_tile_blocks(), rec);
      });
    }
    /* TODO: Don't apply if lossless */
    {
      let _prof = crate::prof::scope(crate::prof::Stage::LoopRestoration);
      fs.restoration.lrf_filter_frame(
        Arc::get_mut(&mut fs.rec).unwrap(),
        &deblocked_frame,
        fi,
      );
    }
  } else {
    /* TODO: Don't apply if lossless */
    if fi.sequence.enable_cdef {
      let _prof = crate::prof::scope(crate::prof::Stage::Cdef);
      let deblocked_frame = (*fs.rec).clone();
      fs.apply_tile_state_mut(|ts| {
        let rec = &mut ts.rec;
        cdef_filter_tile(fi, &deblocked_frame, &blocks.as_tile_blocks(), rec);
      });
    }
  }

  let (idx_max, max_len) = raw_tiles
    .iter()
    .map(Vec::len)
    .enumerate()
    .max_by_key(|&(_, len)| len)
    .unwrap();

  if !fi.disable_frame_end_update_cdf {
    // use the biggest tile (in bytes) for CDF update
    fs.context_update_tile_id = idx_max;
    fs.cdfs = cdfs[idx_max];
    fs.cdfs.reset_counts();
  }

  let max_tile_size_bytes = ILog::ilog(max_len).div_ceil(8) as u32;
  debug_assert!(max_tile_size_bytes > 0 && max_tile_size_bytes <= 4);
  fs.max_tile_size_bytes = max_tile_size_bytes;

  build_raw_tile_group(ti, &raw_tiles, max_tile_size_bytes)
}

fn build_raw_tile_group(
  ti: &TilingInfo, raw_tiles: &[Vec<u8>], max_tile_size_bytes: u32,
) -> Vec<u8> {
  // <https://aomediacodec.github.io/av1-spec/#general-tile-group-obu-syntax>
  let mut raw = Vec::new();
  let mut bw = BitWriter::endian(&mut raw, BigEndian);
  if ti.cols * ti.rows > 1 {
    // tile_start_and_end_present_flag
    bw.write_bit(false).unwrap();
  }
  bw.byte_align().unwrap();
  for (i, raw_tile) in raw_tiles.iter().enumerate() {
    let last = raw_tiles.len() - 1;
    if i != last {
      let tile_size_minus_1 = raw_tile.len() - 1;
      bw.write_le(max_tile_size_bytes, tile_size_minus_1 as u64).unwrap();
    }
    bw.write_bytes(raw_tile).unwrap();
  }
  raw
}

pub struct SBSQueueEntry {
  pub sbo: TileSuperBlockOffset,
  pub lru_index: [i32; MAX_PLANES],
  pub cdef_coded: bool,
  pub w_pre_cdef: WriterBase<WriterRecorder>,
  pub w_post_cdef: WriterBase<WriterRecorder>,
}

#[profiling::function]
fn check_lf_queue<T: Pixel>(
  fi: &FrameInvariants<T>, ts: &mut TileStateMut<'_, T>,
  cw: &mut ContextWriter, w: &mut WriterBase<WriterEncoder>,
  sbs_q: &mut VecDeque<SBSQueueEntry>, last_lru_ready: &mut [i32; 3],
  last_lru_rdoed: &mut [i32; 3], last_lru_coded: &mut [i32; 3],
  deblock_p: bool,
) {
  let mut check_queue = true;
  let planes = if fi.sequence.chroma_sampling == ChromaSampling::Cs400 {
    1
  } else {
    MAX_PLANES
  };

  // Walk queue from the head, see if anything is ready for RDO and flush
  while check_queue {
    if let Some(qe) = sbs_q.front_mut() {
      for pli in 0..planes {
        if qe.lru_index[pli] > last_lru_ready[pli] {
          check_queue = false;
          break;
        }
      }
      if check_queue {
        // yes, this entry is ready
        if qe.cdef_coded || fi.sequence.enable_restoration {
          // only RDO once for a given LRU.

          // One quirk worth noting: LRUs in different planes
          // may be different sizes; eg, one chroma LRU may
          // cover four luma LRUs. However, we won't get here
          // until all are ready for RDO because the smaller
          // ones all fit inside the biggest, and the biggest
          // doesn't trigger until everything is done.

          // RDO happens on all LRUs within the confines of the
          // biggest, all together.  If any of this SB's planes'
          // LRUs are RDOed, in actuality they all are.

          // SBs tagged with a lru index of -1 are ignored in
          // LRU coding/rdoing decisions (but still need to rdo
          // for cdef).
          let mut already_rdoed = false;
          for pli in 0..planes {
            if qe.lru_index[pli] != -1
              && qe.lru_index[pli] <= last_lru_rdoed[pli]
            {
              already_rdoed = true;
              break;
            }
          }
          if !already_rdoed {
            rdo_loop_decision(qe.sbo, fi, ts, cw, w, deblock_p);
            for pli in 0..planes {
              if qe.lru_index[pli] != -1
                && last_lru_rdoed[pli] < qe.lru_index[pli]
              {
                last_lru_rdoed[pli] = qe.lru_index[pli];
              }
            }
          }
        }
        // write LRF information
        if !fi.allow_intrabc && fi.sequence.enable_restoration {
          // TODO: also disallow if lossless
          for pli in 0..planes {
            if qe.lru_index[pli] != -1
              && last_lru_coded[pli] < qe.lru_index[pli]
            {
              last_lru_coded[pli] = qe.lru_index[pli];
              cw.write_lrf(w, &mut ts.restoration, qe.sbo, pli);
            }
          }
        }
        // Now that loop restoration is coded, we can replay the initial block bits
        {
          let _prof = crate::prof::scope(crate::prof::Stage::Replay);
          qe.w_pre_cdef.replay(w);
        }
        // Now code CDEF into the middle of the block
        if qe.cdef_coded {
          let cdef_index = cw.bc.blocks.get_cdef(qe.sbo);
          cw.write_cdef(w, cdef_index, fi.cdef_bits);
          // Code queued symbols that come after the CDEF index
          {
            let _prof = crate::prof::scope(crate::prof::Stage::Replay);
            qe.w_post_cdef.replay(w);
          }
        }
        sbs_q.pop_front();
      }
    } else {
      check_queue = false;
    }
  }
}

#[profiling::function]
fn encode_tile<'a, T: Pixel>(
  fi: &FrameInvariants<T>, ts: &'a mut TileStateMut<'_, T>,
  fc: &'a mut CDFContext, blocks: &'a mut TileBlocksMut<'a>,
  inter_cfg: &InterConfig,
) -> (Vec<u8>, EncoderStats) {
  let _prof = crate::prof::scope(crate::prof::Stage::PartitionRdo);
  let mut enc_stats = EncoderStats::default();
  let mut w = WriterEncoder::new();
  let planes =
    if fi.sequence.chroma_sampling == ChromaSampling::Cs400 { 1 } else { 3 };

  let bc = BlockContext::new(blocks);
  let mut cw = ContextWriter::new(fc, bc);
  let mut sbs_q: VecDeque<SBSQueueEntry> = VecDeque::new();
  let mut last_lru_ready = [-1; 3];
  let mut last_lru_rdoed = [-1; 3];
  let mut last_lru_coded = [-1; 3];

  // prom_av1e033 (Brick 3): per-frame percentile dispatch. Pre-pass the
  // root residual variance of every SB in this tile, then set the cutoff at
  // the percentile that routes the target fraction q — so the routed
  // fraction (hence the RD/variance work split) is content-invariant. The
  // same threshold drives the routed SBs' internal NONE/SPLIT decision.
  if let Some(q) = crate::harvest::dispatch_q() {
    let mut vars = Vec::with_capacity(ts.sb_width * ts.sb_height);
    for sby in 0..ts.sb_height {
      for sbx in 0..ts.sb_width {
        let sbo = TileSuperBlockOffset(SuperBlockOffset { x: sbx, y: sby });
        vars.push(sb_root_var(fi, ts, sbo.block_offset(0, 0)));
      }
    }
    vars.sort_unstable();
    let route_hi = crate::harvest::dispatch_hi();
    let n = vars.len();
    let idx = if route_hi {
      (((1.0 - q) * n as f64) as usize).min(n.saturating_sub(1))
    } else {
      ((q * n as f64) as usize).min(n.saturating_sub(1))
    };
    let thresh = vars.get(idx).copied().unwrap_or(i64::MAX);
    // Internal split threshold: tuned to coding difficulty (quantizer²),
    // independent of the SB routing percentile. RAV1E_VARPART overrides.
    let q_idx = fi.base_q_idx as i64;
    let internal_t =
      crate::harvest::varpart().unwrap_or(2 * q_idx * q_idx);
    VARPART_DISPATCH.with(|c| c.set(Some((thresh, route_hi, internal_t))));
  } else {
    VARPART_DISPATCH.with(|c| c.set(None));
  }

  // prom_av1e041: per-frame percentile for the DEEP dispatch — deepen the
  // low-variance fraction q (av1e040: deep search buys 2.5× more BD/sec on
  // low-variance SBs). Cutoff at the q-th percentile of this frame's SB
  // variances ⇒ content-invariant deepened fraction.
  if let Some(q) = crate::harvest::deep_q() {
    let mut vars = Vec::with_capacity(ts.sb_width * ts.sb_height);
    for sby in 0..ts.sb_height {
      for sbx in 0..ts.sb_width {
        let sbo = TileSuperBlockOffset(SuperBlockOffset { x: sbx, y: sby });
        vars.push(sb_root_var(fi, ts, sbo.block_offset(0, 0)));
      }
    }
    vars.sort_unstable();
    let n = vars.len();
    let route_lo = crate::harvest::deep_lo();
    // route-high (default): cutoff at the (1-q) percentile ⇒ deepen the top-q
    // busiest SBs. route-low: cutoff at q ⇒ deepen the bottom-q flattest.
    let idx = if route_lo {
      ((q * n as f64) as usize).min(n.saturating_sub(1))
    } else {
      (((1.0 - q) * n as f64) as usize).min(n.saturating_sub(1))
    };
    let cutoff = vars.get(idx).copied().unwrap_or(i64::MAX);
    DEEP_CUTOFF.with(|c| c.set(Some((cutoff, route_lo))));
  } else {
    DEEP_CUTOFF.with(|c| c.set(None));
  }

  // prom_av1e045: adaptive interp filter — pick this frame's fixed filter by a
  // cheap SATD trial (sign-flip → dispatch). Runs before the SB loop so both
  // prediction (below) and the header (written after the tile) read the same
  // frame_filter. Inter frames only; off ⇒ None ⇒ fi.default_filter.
  // Single-tile only: the filter is signaled once per frame but chosen inside
  // the tile pass, so multi-tile frames could desync. (Guarded, not yet solved.)
  if crate::harvest::afilter()
    && fi.frame_type.has_inter()
    && fi.sequence.tiling.cols * fi.sequence.tiling.rows == 1
  {
    let f = pick_frame_filter(fi, ts, &mut cw);
    frame_filter::set(Some(f));
  } else {
    frame_filter::set(None);
  }

  // main loop
  for sby in 0..ts.sb_height {
    cw.bc.reset_left_contexts(planes);

    for sbx in 0..ts.sb_width {
      cw.fc_log.clear();

      let tile_sbo = TileSuperBlockOffset(SuperBlockOffset { x: sbx, y: sby });
      let mut sbs_qe = SBSQueueEntry {
        sbo: tile_sbo,
        lru_index: [-1; MAX_PLANES],
        cdef_coded: false,
        w_pre_cdef: WriterRecorder::new(),
        w_post_cdef: WriterRecorder::new(),
      };

      let tile_bo = tile_sbo.block_offset(0, 0);
      cw.bc.cdef_coded = false;
      cw.bc.code_deltas = fi.delta_q_present;

      let is_straddle_sbx =
        tile_bo.0.x + BlockSize::BLOCK_64X64.width_mi() > ts.mi_width;
      let is_straddle_sby =
        tile_bo.0.y + BlockSize::BLOCK_64X64.height_mi() > ts.mi_height;

      // brick-③ ceiling measure (profile builds only): how much SB encode
      // time lands on SBs whose FINAL outcome is all-skip — the population an
      // SVT-style depth-removal gate could shortcut. Measured BEFORE building
      // the gate (av1e007 law).
      #[cfg(feature = "profile")]
      let sb_t0 = unsafe { core::arch::x86_64::_rdtsc() };

      // prom_av1e041: deepen this SB's search? Force-on (the A/B) or the
      // low-variance-first content fraction (deep dispatch). Covers the whole
      // SB encode (mode-decision trials + winner recode).
      let deepen = crate::harvest::deep_force()
        || DEEP_CUTOFF.with(|c| c.get()).is_some_and(|(t, route_lo)| {
          let rv = sb_root_var(fi, ts, tile_bo);
          if route_lo {
            rv <= t
          } else {
            rv >= t
          }
        });
      let _deep_guard = deep::enter(deepen);

      // prom_av1e047: dispatch the trellis off FLAT SBs (absolute residual
      // variance below the threshold) — it wins on busy content, marginally
      // hurts flat. Only compute the signal when the trellis is active.
      trellis_gate::set(
        crate::harvest::trellis()
          && (crate::harvest::trellis_all()
            || sb_root_var(fi, ts, tile_bo)
              > crate::harvest::trellis_t()),
      );

      // Encode SuperBlock
      if fi.config.speed_settings.partition.encode_bottomup
        || is_straddle_sbx
        || is_straddle_sby
      {
        encode_partition_bottomup(
          fi,
          ts,
          &mut cw,
          &mut sbs_qe.w_pre_cdef,
          &mut sbs_qe.w_post_cdef,
          BlockSize::BLOCK_64X64,
          tile_bo,
          f64::MAX,
          inter_cfg,
          &mut enc_stats,
        );
      } else {
        encode_partition_topdown(
          fi,
          ts,
          &mut cw,
          &mut sbs_qe.w_pre_cdef,
          &mut sbs_qe.w_post_cdef,
          BlockSize::BLOCK_64X64,
          tile_bo,
          &None,
          inter_cfg,
          &mut enc_stats,
          None,
        );
      }

      // prom_av1e040: per-SB content segmentation — residual variance (dispatch
      // signal) × partition-depth outcome (area at each block size). Bin
      // offline by variance tier to map compute vs content.
      if crate::harvest::sbseg() {
        let rv = sb_root_var(fi, ts, tile_bo);
        // area[i] = MI count at width 64/32/16/8/<=4
        let mut area = [0u32; 5];
        for my in 0..16u32 {
          for mx in 0..16u32 {
            let bo = TileBlockOffset(BlockOffset {
              x: tile_bo.0.x + mx as usize,
              y: tile_bo.0.y + my as usize,
            });
            if bo.0.x >= ts.mi_width || bo.0.y >= ts.mi_height {
              continue;
            }
            let w = cw.bc.blocks[bo].bsize.width_mi();
            let idx = match w {
              16.. => 0,
              8 => 1,
              4 => 2,
              2 => 3,
              _ => 4,
            };
            area[idx] += 1;
          }
        }
        crate::harvest::emit(&format!(
          "SBSEG,{},{},{},{},{},{},{}",
          fi.input_frameno, rv, area[0], area[1], area[2], area[3], area[4]
        ));
      }

      #[cfg(feature = "profile")]
      {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SKIP_CY: AtomicU64 = AtomicU64::new(0);
        static REST_CY: AtomicU64 = AtomicU64::new(0);
        static SKIP_N: AtomicU64 = AtomicU64::new(0);
        static REST_N: AtomicU64 = AtomicU64::new(0);
        let dt = unsafe { core::arch::x86_64::_rdtsc() } - sb_t0;
        let mut all_skip = true;
        'chk: for y in tile_bo.0.y..(tile_bo.0.y + 16).min(ts.mi_height) {
          for x in tile_bo.0.x..(tile_bo.0.x + 16).min(ts.mi_width) {
            if !cw.bc.blocks[y][x].skip {
              all_skip = false;
              break 'chk;
            }
          }
        }
        if all_skip {
          SKIP_CY.fetch_add(dt, Ordering::Relaxed);
          SKIP_N.fetch_add(1, Ordering::Relaxed);
        } else {
          REST_CY.fetch_add(dt, Ordering::Relaxed);
          REST_N.fetch_add(1, Ordering::Relaxed);
        }
        if sby + 1 == ts.sb_height && sbx + 1 == ts.sb_width {
          let (sc, rc) = (SKIP_CY.load(Ordering::Relaxed), REST_CY.load(Ordering::Relaxed));
          eprintln!(
            "SBSKIP allskip_sbs={} rest_sbs={} allskip_cy_pct={:.2}",
            SKIP_N.load(Ordering::Relaxed),
            REST_N.load(Ordering::Relaxed),
            100.0 * sc as f64 / (sc + rc).max(1) as f64
          );
          trial_audit::dump();
          crate::rdo::fulltrial_audit::dump();
          fwd_phase::dump();
        }
      }

      {
        let mut check_queue = false;
        // queue our superblock for when the LRU is complete
        sbs_qe.cdef_coded = cw.bc.cdef_coded;
        for pli in 0..planes {
          if let Some((lru_x, lru_y)) =
            ts.restoration.planes[pli].restoration_unit_index(tile_sbo, false)
          {
            let lru_index = ts.restoration.planes[pli]
              .restoration_unit_countable(lru_x, lru_y)
              as i32;
            sbs_qe.lru_index[pli] = lru_index;
            if ts.restoration.planes[pli]
              .restoration_unit_last_sb_for_rdo(fi, ts.sbo, tile_sbo)
            {
              last_lru_ready[pli] = lru_index;
              check_queue = true;
            }
          } else {
            // we're likely in an area stretched into a new tile
            // tag this SB to be ignored in LRU decisions
            sbs_qe.lru_index[pli] = -1;
            check_queue = true;
          }
        }
        sbs_q.push_back(sbs_qe);

        if check_queue && !fi.sequence.enable_delayed_loopfilter_rdo {
          check_lf_queue(
            fi,
            ts,
            &mut cw,
            &mut w,
            &mut sbs_q,
            &mut last_lru_ready,
            &mut last_lru_rdoed,
            &mut last_lru_coded,
            true,
          );
        }
      }
    }
  }

  if fi.sequence.enable_delayed_loopfilter_rdo {
    // Solve deblocking for just this tile
    /* TODO: Don't apply if lossless */
    let deblock_levels = deblock_filter_optimize(
      fi,
      &ts.rec.as_const(),
      &ts.input_tile,
      &cw.bc.blocks.as_const(),
      fi.width,
      fi.height,
    );

    if deblock_levels[0] != 0 || deblock_levels[1] != 0 {
      // copy reconstruction to a temp frame to restore it later
      let rec_copy = if planes == 3 {
        vec![
          ts.rec.planes[0].scratch_copy(),
          ts.rec.planes[1].scratch_copy(),
          ts.rec.planes[2].scratch_copy(),
        ]
      } else {
        vec![ts.rec.planes[0].scratch_copy()]
      };

      // copy ts.deblock because we need to set some of our own values here
      let mut deblock_copy = *ts.deblock;
      deblock_copy.levels = deblock_levels;

      // temporarily deblock the reference
      deblock_filter_frame(
        &deblock_copy,
        &mut ts.rec,
        &cw.bc.blocks.as_const(),
        fi.width,
        fi.height,
        fi.sequence.bit_depth,
        planes,
      );

      // rdo lf and write
      check_lf_queue(
        fi,
        ts,
        &mut cw,
        &mut w,
        &mut sbs_q,
        &mut last_lru_ready,
        &mut last_lru_rdoed,
        &mut last_lru_coded,
        false,
      );

      // copy original reference back in
      for pli in 0..planes {
        let dst = &mut ts.rec.planes[pli];
        let src = &rec_copy[pli];
        for (dst_row, src_row) in dst.rows_iter_mut().zip(src.rows_iter()) {
          for (out, input) in dst_row.iter_mut().zip(src_row) {
            *out = *input;
          }
        }
      }
    } else {
      // rdo lf and write
      check_lf_queue(
        fi,
        ts,
        &mut cw,
        &mut w,
        &mut sbs_q,
        &mut last_lru_ready,
        &mut last_lru_rdoed,
        &mut last_lru_coded,
        false,
      );
    }
  }

  assert!(
    sbs_q.is_empty(),
    "Superblock queue not empty in tile at offset {}:{}",
    ts.sbo.0.x,
    ts.sbo.0.y
  );
  (w.done(), enc_stats)
}

#[allow(unused)]
fn write_tile_group_header(tile_start_and_end_present_flag: bool) -> Vec<u8> {
  let mut buf = Vec::new();
  {
    let mut bw = BitWriter::endian(&mut buf, BigEndian);
    bw.write_bit(tile_start_and_end_present_flag).unwrap();
    bw.byte_align().unwrap();
  }
  buf
}

/// Write a packet containing only the placeholder that tells the decoder
/// to present the already decoded frame present at `frame_to_show_map_idx`
///
/// See `av1-spec` Section 6.8.2 and 7.18.
///
/// # Panics
///
/// - If the frame packets cannot be written
#[profiling::function]
pub fn encode_show_existing_frame<T: Pixel>(
  fi: &FrameInvariants<T>, fs: &mut FrameState<T>, inter_cfg: &InterConfig,
) -> Vec<u8> {
  debug_assert!(fi.is_show_existing_frame());
  let obu_extension = 0;

  let mut packet = Vec::new();

  if fi.frame_type == FrameType::KEY {
    write_key_frame_obus(&mut packet, fi, obu_extension).unwrap();
  }

  for t35 in fi.t35_metadata.iter() {
    let mut t35_buf = Vec::new();
    let mut t35_bw = BitWriter::endian(&mut t35_buf, BigEndian);
    t35_bw.write_t35_metadata_obu(t35).unwrap();
    packet.write_all(&t35_buf).unwrap();
    t35_buf.clear();
  }

  let mut buf1 = Vec::new();
  let mut buf2 = Vec::new();
  {
    let mut bw2 = BitWriter::endian(&mut buf2, BigEndian);
    bw2.write_frame_header_obu(fi, fs, inter_cfg).unwrap();
  }

  {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_obu_header(ObuType::OBU_FRAME_HEADER, obu_extension).unwrap();
  }
  packet.write_all(&buf1).unwrap();
  buf1.clear();

  {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_uleb128(buf2.len() as u64).unwrap();
  }
  packet.write_all(&buf1).unwrap();
  buf1.clear();

  packet.write_all(&buf2).unwrap();
  buf2.clear();

  let map_idx = fi.frame_to_show_map_idx as usize;
  if let Some(ref rec) = fi.rec_buffer.frames[map_idx] {
    let fs_rec = Arc::get_mut(&mut fs.rec).unwrap();
    let planes =
      if fi.sequence.chroma_sampling == ChromaSampling::Cs400 { 1 } else { 3 };
    for p in 0..planes {
      fs_rec.planes[p].data.copy_from_slice(&rec.frame.planes[p].data);
    }
  }
  packet
}

fn get_initial_segmentation<T: Pixel>(
  fi: &FrameInvariants<T>,
) -> SegmentationState {
  let segmentation = if fi.primary_ref_frame == PRIMARY_REF_NONE {
    None
  } else {
    let ref_frame_idx = fi.ref_frames[fi.primary_ref_frame as usize] as usize;
    let ref_frame = fi.rec_buffer.frames[ref_frame_idx].as_ref();
    ref_frame.map(|rec| rec.segmentation)
  };

  // return the retrieved instance if any, a new one otherwise
  segmentation.unwrap_or_default()
}

/// # Panics
///
/// - If the frame packets cannot be written
#[profiling::function]
pub fn encode_frame<T: Pixel>(
  fi: &FrameInvariants<T>, fs: &mut FrameState<T>, inter_cfg: &InterConfig,
) -> Vec<u8> {
  debug_assert!(!fi.is_show_existing_frame());
  let obu_extension = 0;

  let mut packet = Vec::new();

  if fi.enable_segmentation {
    fs.segmentation = get_initial_segmentation(fi);
    segmentation_optimize(fi, fs);
  }
  let tile_group = encode_tile_group(fi, fs, inter_cfg);

  if fi.frame_type == FrameType::KEY {
    write_key_frame_obus(&mut packet, fi, obu_extension).unwrap();
  }

  for t35 in fi.t35_metadata.iter() {
    let mut t35_buf = Vec::new();
    let mut t35_bw = BitWriter::endian(&mut t35_buf, BigEndian);
    t35_bw.write_t35_metadata_obu(t35).unwrap();
    packet.write_all(&t35_buf).unwrap();
    t35_buf.clear();
  }

  let mut buf1 = Vec::new();
  let mut buf2 = Vec::new();
  {
    let mut bw2 = BitWriter::endian(&mut buf2, BigEndian);
    bw2.write_frame_header_obu(fi, fs, inter_cfg).unwrap();
  }

  {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_obu_header(ObuType::OBU_FRAME, obu_extension).unwrap();
  }
  packet.write_all(&buf1).unwrap();
  buf1.clear();

  {
    let mut bw1 = BitWriter::endian(&mut buf1, BigEndian);
    bw1.write_uleb128((buf2.len() + tile_group.len()) as u64).unwrap();
  }
  packet.write_all(&buf1).unwrap();
  buf1.clear();

  packet.write_all(&buf2).unwrap();
  buf2.clear();

  packet.write_all(&tile_group).unwrap();
  packet
}

pub fn update_rec_buffer<T: Pixel>(
  output_frameno: u64, fi: &mut FrameInvariants<T>, fs: &FrameState<T>,
) {
  let rfs = Arc::new(ReferenceFrame {
    order_hint: fi.order_hint,
    width: fi.width as u32,
    height: fi.height as u32,
    render_width: fi.render_width,
    render_height: fi.render_height,
    frame: fs.rec.clone(),
    input_hres: fs.input_hres.clone(),
    input_qres: fs.input_qres.clone(),
    cdfs: fs.cdfs,
    frame_me_stats: fs.frame_me_stats.clone(),
    output_frameno,
    segmentation: fs.segmentation,
  });
  for i in 0..REF_FRAMES {
    if (fi.refresh_frame_flags & (1 << i)) != 0 {
      fi.rec_buffer.frames[i] = Some(Arc::clone(&rfs));
      fi.rec_buffer.deblock[i] = fs.deblock;
    }
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn check_partition_types_order() {
    assert_eq!(
      RAV1E_PARTITION_TYPES[RAV1E_PARTITION_TYPES.len() - 1],
      PartitionType::PARTITION_SPLIT
    );
  }
}
