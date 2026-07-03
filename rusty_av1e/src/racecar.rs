// Copyright (c) 2026, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.

//! Racecar mode: a runtime switch between the kept performance bricks
//! (racecar, the default) and the original stock-rav1e code paths (normal).
//!
//! Both modes emit a byte-identical bitstream — the switch changes speed
//! only. It exists so a single binary can demonstrate/measure the brick
//! campaign (docs/entropy-bricks.md) without building a stock worktree.
//!
//! Toggle sites:
//! - `ContextWriter::get_nz_map_contexts` (B7a + B7a-SIMD nz-map kernel
//!   vs the stock per-scan-position stencil)
//! - `QuantizationContext::quantize` (Q2 branchless main loop vs the
//!   stock branchy loop)
//!
//! Resolution order: `set()` (the CLI's `--racecar` flag) wins if called
//! before the first read; otherwise the `RAV1E_RACECAR` env var (`0`/`off`
//! disables); otherwise on. The value is latched on first read.

use std::sync::OnceLock;

static MODE: OnceLock<bool> = OnceLock::new();

/// Latch the mode (CLI). A no-op if the mode has already been read.
pub fn set(on: bool) {
  let _ = MODE.set(on);
}

/// True in racecar mode (optimized kernels), false in normal mode
/// (stock rav1e paths). Cost per call: one atomic load + predictable
/// branch — noise even at 1.38M calls/encode.
#[inline]
pub fn on() -> bool {
  *MODE.get_or_init(|| {
    std::env::var("RAV1E_RACECAR")
      .map_or(true, |v| v != "0" && !v.eq_ignore_ascii_case("off"))
  })
}
