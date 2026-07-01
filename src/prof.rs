// rusty_av1e analyzer spine — feature-gated stage profiler.
//
// This is the "stage profiler" instrument from the `analyzer` skill, adapted to
// rav1e's encode pipeline (modelled on rusty_h264-common/src/prof.rs).
//
//  * OFF (default): `scope()` returns a zero-sized guard with no `Drop`, so the
//    optimizer elides every call site — the shipped/release build is
//    byte-identical to stock rav1e. `profile` is an opt-in cargo feature.
//  * ON (`--features profile`): each `scope(Stage)` times `rdtsc()..drop` into a
//    per-stage atomic cycle bucket (+ a call count). `dump()`/`snapshot()`
//    recover ns from a wall/cycle ratio captured between `reset()` and the dump,
//    so the *percentage* breakdown is calibration-free (ratios of cycles) and the
//    *absolute* ms is wall-anchored.
//
// The timer is `rdtsc` (~15 ns) rather than `Instant::now()` (QueryPerformanceCounter,
// ~30 ns on Windows) so per-call overhead — which inflates high-call stages and the
// residue — is as small as possible. See the analyzer skill's "cheapen the timer"
// learning. rav1e is not a `forbid(unsafe_code)` crate (it ships asm), so the
// `rdtsc` intrinsic needs no special gating.
//
// Read `dump()` top-down: each stage's ms + % of the `Total` scope, sorted, with the
// residue (`Total − Σ stages`) — the most important line. For a meaningful %/residue,
// run the breakdown SINGLE-THREADED (see tests/profile_encode.rs): with tile/frame
// threads the per-stage cycle sums across workers exceed wall time.

/// A pipeline stage. Discriminants are declaration-ordered `0..COUNT` so
/// `stage as usize` indexes the bucket arrays directly.
macro_rules! stages {
  ($($variant:ident => $name:literal),+ $(,)?) => {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[repr(usize)]
    pub enum Stage { $($variant),+ }

    impl Stage {
      /// Every stage, in declaration order.
      pub const ALL: &'static [Stage] = &[$(Stage::$variant),+];
      /// Number of stages (= bucket array length).
      pub const COUNT: usize = Stage::ALL.len();

      /// Short label used in `dump()`.
      pub const fn name(self) -> &'static str {
        match self { $(Stage::$variant => $name),+ }
      }

      #[inline(always)]
      pub const fn idx(self) -> usize { self as usize }
    }
  };
}

// The stage taxonomy. `Total` MUST be first — it is the denominator (the scope
// wrapping one whole frame encode) and the residue base. The top-level stages are
// the frame pipeline phases; the RDO-CHILD stages (see `is_rdo_child`) decompose
// the dominant `PartitionRdo` bucket and are reported as a nested sub-breakdown so
// they don't double-count against the top-level residue.
stages! {
  Total            => "TOTAL (receive_packet)",
  Lookahead        => "lookahead intra-cost",
  MotionEstimation => "motion est. (lookahead)",
  PartitionRdo     => "partition+mode RDO+recon",
  Deblock          => "deblock (opt+apply)",
  Cdef             => "cdef (apply)",
  LoopRestoration  => "loop restoration (apply)",
  // --- RDO children (nested inside PartitionRdo; inclusive leaf timings) ---
  // asm/pure-Rust tag in the comment marks hand-optimization ROI.
  MotionEstEnc     => "ME (search: rust, SAD/SATD: asm)",
  Predict          => "prediction+MC (asm)",
  FwdTransform     => "fwd transform (asm)",
  Quantize         => "quantize (PURE RUST)",
  Reconstruct      => "inv-transform recon (asm)",
  // glue suspects (all pure Rust):
  CtxSaveRestore   => "CDF checkpoint/rollback (rust)",
  EntropyRate      => "coeff entropy/rate (rust)",
  // --- info tier: nested inside other stages, displayed but never summed ---
  SymbolMachinery  => "symbol_with_update [info]",
  TellFrac         => "tell_frac [info]",
}

impl Stage {
  /// Info stages nest inside OTHER scoped stages (e.g. per-symbol machinery
  /// inside EntropyRate), so they are displayed for audits but excluded from
  /// every sum — including them would double-count and corrupt the residue.
  pub const fn is_info(self) -> bool {
    matches!(self, Stage::SymbolMachinery | Stage::TellFrac)
  }

  /// RDO-child stages nest *inside* `PartitionRdo`; reported as a sub-breakdown
  /// of it, not as top-level siblings (so the top-level residue stays honest).
  pub const fn is_rdo_child(self) -> bool {
    matches!(
      self,
      Stage::MotionEstEnc
        | Stage::Predict
        | Stage::FwdTransform
        | Stage::Quantize
        | Stage::Reconstruct
        | Stage::CtxSaveRestore
        | Stage::EntropyRate
    )
  }
}

#[cfg(feature = "profile")]
pub use imp::*;

#[cfg(not(feature = "profile"))]
pub use noop::*;

// ---------------------------------------------------------------------------
// Real implementation (feature = "profile")
// ---------------------------------------------------------------------------
#[cfg(feature = "profile")]
mod imp {
  use super::Stage;
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::sync::Mutex;
  use std::time::Instant;

  static CYCLES: [AtomicU64; Stage::COUNT] =
    [const { AtomicU64::new(0) }; Stage::COUNT];
  static CALLS: [AtomicU64; Stage::COUNT] =
    [const { AtomicU64::new(0) }; Stage::COUNT];

  // Wall/cycle calibration anchor, captured at `reset()`. Touched only on the
  // cold reset/dump paths, so a Mutex here never contends the hot scope path.
  static ANCHOR: Mutex<Option<(Instant, u64)>> = Mutex::new(None);

  #[inline(always)]
  fn rdtsc() -> u64 {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
      #[cfg(target_arch = "x86_64")]
      // SAFETY: _rdtsc is always available on x86_64.
      unsafe {
        core::arch::x86_64::_rdtsc()
      }
      #[cfg(target_arch = "x86")]
      // SAFETY: _rdtsc is always available on x86.
      unsafe {
        core::arch::x86::_rdtsc()
      }
    }
    // Portable fallback: nanoseconds from a monotonic clock. The wall/cycle
    // ratio then works out to ~1 ns/"tick", so percentages and ms stay correct.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
      use std::time::Instant;
      static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
      BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
  }

  /// RAII timing guard: on drop, adds elapsed cycles + 1 call to `stage`'s bucket.
  #[must_use = "the scope guard must be held for the duration being timed"]
  pub struct Guard {
    idx: usize,
    start: u64,
  }

  impl Drop for Guard {
    #[inline(always)]
    fn drop(&mut self) {
      let elapsed = rdtsc().wrapping_sub(self.start);
      CYCLES[self.idx].fetch_add(elapsed, Ordering::Relaxed);
      CALLS[self.idx].fetch_add(1, Ordering::Relaxed);
    }
  }

  /// Open a timing scope for `stage`. Hold the returned guard for the region.
  #[inline(always)]
  pub fn scope(stage: Stage) -> Guard {
    Guard { idx: stage.idx(), start: rdtsc() }
  }

  /// Clear all buckets and (re)start the wall/cycle calibration clock.
  pub fn reset() {
    for b in CYCLES.iter() {
      b.store(0, Ordering::Relaxed);
    }
    for c in CALLS.iter() {
      c.store(0, Ordering::Relaxed);
    }
    *ANCHOR.lock().unwrap() = Some((Instant::now(), rdtsc()));
  }

  fn ns_per_cycle() -> f64 {
    match *ANCHOR.lock().unwrap() {
      Some((inst, tsc0)) => {
        let wall_ns = inst.elapsed().as_nanos() as f64;
        let cyc = rdtsc().wrapping_sub(tsc0) as f64;
        if cyc > 0.0 {
          wall_ns / cyc
        } else {
          0.0
        }
      }
      None => 0.0,
    }
  }

  /// `(stage, milliseconds, calls)` for every stage, calibrated. Feeds the
  /// median-of-N driver in tests/profile_encode.rs.
  pub fn snapshot() -> Vec<(Stage, f64, u64)> {
    let npc = ns_per_cycle();
    Stage::ALL
      .iter()
      .map(|&s| {
        let cyc = CYCLES[s.idx()].load(Ordering::Relaxed) as f64;
        let calls = CALLS[s.idx()].load(Ordering::Relaxed);
        (s, cyc * npc / 1.0e6, calls)
      })
      .collect()
  }

  /// Print the top-down breakdown: ms + % of `Total` + calls per stage, sorted,
  /// with the residue (`Total − Σ top-level stages`). The RDO-child stages are
  /// shown as a nested sub-breakdown of `PartitionRdo` (they live inside it), with
  /// a derived "RDO glue" line = `PartitionRdo − Σ children` — the pure-Rust
  /// search/context/entropy overhead that no kernel scope captures.
  pub fn dump(label: &str) {
    let snap = snapshot();
    let total_ms = snap[Stage::Total.idx()].1;
    let pdo_ms = snap[Stage::PartitionRdo.idx()].1;
    let pct = |ms: f64, denom: f64| if denom > 0.0 { 100.0 * ms / denom } else { 0.0 };

    // Top-level rows: everything that is neither Total, an RDO child, nor info.
    let mut top: Vec<(Stage, f64, u64)> = snap
      .iter()
      .copied()
      .filter(|(s, ..)| *s != Stage::Total && !s.is_rdo_child() && !s.is_info())
      .collect();
    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let sum_top: f64 = top.iter().map(|(_, ms, _)| ms).sum();
    let residue_ms = total_ms - sum_top;

    eprintln!("\n=== prof::dump [{label}] ===");
    eprintln!("  {:<26} {:>10} {:>8}  {:>12}", "stage", "ms", "% tot", "calls");
    eprintln!("  {}", "-".repeat(60));
    for (s, ms, calls) in &top {
      eprintln!("  {:<26} {:>10.3} {:>7.1}% {:>12}", s.name(), ms, pct(*ms, total_ms), calls);
    }
    eprintln!("  {}", "-".repeat(60));
    eprintln!("  {:<26} {:>10.3} {:>7.1}%", "RESIDUE (unnamed)", residue_ms, pct(residue_ms, total_ms));
    eprintln!("  {:<26} {:>10.3} {:>7.1}%", "TOTAL", total_ms, pct(total_ms, total_ms));

    // Nested sub-breakdown of the dominant PartitionRdo bucket.
    let mut kids: Vec<(Stage, f64, u64)> =
      snap.iter().copied().filter(|(s, ..)| s.is_rdo_child()).collect();
    if kids.iter().any(|(_, ms, c)| *ms > 0.0 || *c > 0) {
      kids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
      let sum_kids: f64 = kids.iter().map(|(_, ms, _)| ms).sum();
      let glue_ms = pdo_ms - sum_kids;
      eprintln!("\n  -- decompose 'partition+mode RDO+recon' ({pdo_ms:.1} ms, % of it) --");
      for (s, ms, calls) in &kids {
        eprintln!("  {:<26} {:>10.3} {:>7.1}% {:>12}", s.name(), ms, pct(*ms, pdo_ms), calls);
      }
      eprintln!("  {:<26} {:>10.3} {:>7.1}%", "RDO glue (pure-Rust)", glue_ms, pct(glue_ms, pdo_ms));
      eprintln!("  (children are inclusive leaf timings; ME includes its own SAD/SATD)");
    }

    // Info tier: nested inside other stages; raw (overhead-inflated) numbers.
    let infos: Vec<(Stage, f64, u64)> =
      snap.iter().copied().filter(|(s, _, c)| s.is_info() && *c > 0).collect();
    if !infos.is_empty() {
      eprintln!("\n  -- info tier (nested inside stages above; NOT summed; \
                 subtract ~calls x 2x timer cost for true size) --");
      for (s, ms, calls) in &infos {
        eprintln!("  {:<26} {:>10.3} {:>7.1}% {:>12}", s.name(), ms, pct(*ms, total_ms), calls);
      }
    }
    eprintln!(
      "  (top residue = Total − Σ top-level; RDO glue = PartitionRdo − Σ children)\n"
    );
  }
}

// ---------------------------------------------------------------------------
// No-op implementation (feature off) — every symbol elides to nothing.
// ---------------------------------------------------------------------------
#[cfg(not(feature = "profile"))]
mod noop {
  use super::Stage;

  /// Zero-sized, no `Drop` — the optimizer removes it entirely.
  pub struct Guard;

  #[inline(always)]
  pub fn scope(_stage: Stage) -> Guard {
    Guard
  }

  #[inline(always)]
  pub fn reset() {}

  #[inline(always)]
  pub fn snapshot() -> Vec<(Stage, f64, u64)> {
    Vec::new()
  }

  #[inline(always)]
  pub fn dump(_label: &str) {}
}
