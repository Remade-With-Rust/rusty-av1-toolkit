// rusty_av1e harvest tap — env-gated CSV telemetry for the Prometheus refinery.
//
// The refinery (remade_ffmpeg_rs/Prometheus) discovers closed-form formulas for
// the encoder's fitted heuristics from real telemetry. This module is the
// telemetry source: when `RAV1E_HARVEST=<path>` is set, decision sites append
// one CSV row per observation to that file; when unset, the only cost is one
// initialized-`OnceLock` load per call site. The tap only OBSERVES — it never
// changes an encoding decision, so default output is untouched.
//
// Sink is a plain `File` (line-at-a-time writeln, no buffer to lose on exit);
// harvest runs are single-threaded (`--threads 1`), the Mutex is for safety.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn sink() -> &'static Option<Mutex<File>> {
  SINK.get_or_init(|| {
    std::env::var("RAV1E_HARVEST").ok().map(|path| {
      Mutex::new(
        File::options()
          .create(true)
          .append(true)
          .open(&path)
          .unwrap_or_else(|e| panic!("RAV1E_HARVEST={path}: {e}")),
      )
    })
  })
}

/// Cheap gate for call sites that must compute harvest-only features.
#[inline]
pub fn enabled() -> bool {
  sink().is_some()
}

/// Append one CSV row (no trailing newline needed).
pub fn emit(line: &str) {
  if let Some(m) = sink() {
    let mut f = m.lock().unwrap();
    let _ = writeln!(f, "{line}");
  }
}

// --- prom_av1e002: experimental λ multiplier -------------------------------
//
// Probe arm for the RDO λ-calibration experiment: scales fi.lambda (and thus
// me_lambda, derived as sqrt) at the single site where set_quantizers sets it.
// `RAV1E_LAMBDA_MULT`: unset = baseline (byte-identical); `M` = global
// multiplier; `MI:MP` = separate intra/inter multipliers; `i:M` / `p:M` =
// scale only intra / only inter frames.

static LAMBDA_MULT: OnceLock<Option<LambdaMult>> = OnceLock::new();

/// λ probe configuration (prom_av1e002/009).
#[derive(Clone, Copy)]
pub enum LambdaMult {
  /// (intra_mult, inter_mult)
  Kind(f64, f64),
  /// Per-pyramid-level multipliers, indexed by `pyramid_level.min(3)`;
  /// intra frames use index 0. The frame-role axis (prom_av1e009) —
  /// SVT shapes λ by layer (~1.17-1.41× on high layers); rav1e's stock λ
  /// is role-uniform.
  Level([f64; 4]),
}

/// The λ probe, or `None` when off. `M` | `MI:MP` | `i:M` | `p:M` |
/// `l:M0:M1:M2:M3` (per pyramid level).
pub fn lambda_mult() -> Option<LambdaMult> {
  *LAMBDA_MULT.get_or_init(|| {
    let v = std::env::var("RAV1E_LAMBDA_MULT").ok()?;
    let s = v.trim();
    if s.is_empty() || s == "0" || s == "off" {
      return None;
    }
    if let Some(rest) = s.strip_prefix("l:") {
      let ms: Vec<f64> =
        rest.split(':').filter_map(|x| x.trim().parse().ok()).collect();
      if ms.len() == 4 {
        return Some(LambdaMult::Level([ms[0], ms[1], ms[2], ms[3]]));
      }
      return None;
    }
    if let Some(m) = s.strip_prefix("i:") {
      return Some(LambdaMult::Kind(m.parse().ok()?, 1.0));
    }
    if let Some(m) = s.strip_prefix("p:") {
      return Some(LambdaMult::Kind(1.0, m.parse().ok()?));
    }
    if let Some((mi, mp)) = s.split_once(':') {
      return Some(LambdaMult::Kind(mi.parse().ok()?, mp.parse().ok()?));
    }
    let m: f64 = s.parse().ok()?;
    Some(LambdaMult::Kind(m, m))
  })
}

// --- RAV1E_FAST: the Prometheus speed-tier bundle ---------------------------
//
// One switch for the campaign's kept knobs (individual envs still override):
//   RAV1E_FAST=clean → MODE_TOPK=6                       (+25.8% @ +0.024% BD)
//   RAV1E_FAST=fast  → + PD0 gate + FASTRATE             (~+38%  @ +0.099% BD)
// (Full-length 6-clip × 4-QP ladders. The earlier LRF+partition-gate bundle
// is DOMINATED by pd0+topk — those knobs remain as individual levers.)
// Unset = stock rav1e (byte-identical; FNV-proven). Deliberately NOT part of
// --racecar, whose contract is byte-identical kernel swaps only.

#[derive(Clone, Copy, PartialEq)]
pub enum FastTier {
  Off,
  Clean,
  Fast,
}

static FAST_TIER: OnceLock<FastTier> = OnceLock::new();

pub fn fast_tier() -> FastTier {
  *FAST_TIER.get_or_init(|| match std::env::var("RAV1E_FAST") {
    Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
      "clean" => FastTier::Clean,
      "1" | "on" | "fast" => FastTier::Fast,
      _ => FastTier::Off,
    },
    Err(_) => FastTier::Off,
  })
}

// --- prom_av1e006: inter mode-RDO top-K cap (the MDS0-funnel direction) ----
//
// rav1e already SATD-screens and sorts its inter candidates, then full-trials
// `num_modes_rdo` of them. This knob caps the full trials at K (in screen
// order). `RAV1E_MODE_TOPK`: unset/`0`/`off` = baseline; `K` = cap.

static MODE_TOPK: OnceLock<Option<[usize; 4]>> = OnceLock::new();

/// Full-trial caps for the inter mode RDO as per-bsize (64, 32, 16, ≤8)
/// values, or `None` for baseline. `RAV1E_MODE_TOPK=K` = one global cap;
/// `K64:K32:K16:K8` = per-bsize (SATD is a far better oracle at big blocks —
/// prom_av1e008b ceiling: K=3@64 costs 0.32% regret vs 9.5% for K=3@8).
pub fn mode_topk() -> Option<[usize; 4]> {
  *MODE_TOPK.get_or_init(|| {
    let v = match std::env::var("RAV1E_MODE_TOPK") {
      Ok(v) => v,
      // tier fallback: both tiers enable the K=6 cap
      Err(_) if fast_tier() != FastTier::Off => return Some([6; 4]),
      Err(_) => return None,
    };
    let v: Option<String> = Some(v);
    let v = v?;
    let s = v.trim();
    if s.is_empty() || s == "0" || s == "off" {
      return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    let p = |x: &str| x.trim().parse::<usize>().ok().filter(|&k| k >= 1);
    match parts.as_slice() {
      [k] => Some([p(k)?; 4]),
      [a, b, c, d] => Some([p(a)?, p(b)?, p(c)?, p(d)?]),
      _ => None,
    }
  })
}

// --- prom_av1e005: partition-split gate (the VP9 G1 pattern on rav1e) ------
//
// In topdown partition RDO, NONE is evaluated first; when its RD cost,
// rate-normalized (none_rd/λ), is below a per-bsize threshold, the SPLIT
// subtree trials are skipped — the expensive arm of the whole encoder.
// `RAV1E_PART_GATE`: unset/`off` = disabled (baseline); `T64:T32:T16` =
// per-bsize thresholds in cost/λ units (a level with no useful gate gets 0;
// negative disables a level).

static PART_GATE: OnceLock<Option<(f64, f64, f64)>> = OnceLock::new();

/// Per-bsize (64, 32, 16) thresholds for the partition-split gate.
pub fn part_gate() -> Option<(f64, f64, f64)> {
  *PART_GATE.get_or_init(|| {
    let v = std::env::var("RAV1E_PART_GATE").ok()?;
    let s = v.trim();
    if s.is_empty() || s == "0" || s == "off" {
      return None;
    }
    let mut it = s.split(':');
    let t64 = it.next()?.trim().parse().ok()?;
    let t32 = it.next()?.trim().parse().ok()?;
    let t16 = it.next()?.trim().parse().ok()?;
    Some((t64, t32, t16))
  })
}

// --- prom_av1e011: fast rate accounting (B8 counter path) -------------------
//
// On counting writers, AC-sign and golomb bits are flat-probability — their
// exact LENGTH is known without running the range coder. `RAV1E_FASTRATE=1`
// accounts them via fake-bits (tell_frac-visible) instead of per-bit EC
// stores. Rate totals are exact in bits; only the counter's internal rng
// drift differs (rounding epsilons on later symbols) ⇒ BD-gated.

static FASTRATE: OnceLock<bool> = OnceLock::new();

/// True when the B8 counter fast path is enabled.
#[inline]
pub fn fastrate() -> bool {
  *FASTRATE.get_or_init(|| {
    match std::env::var("RAV1E_FASTRATE") {
      Ok(v) => v.trim() == "1" || v.trim().eq_ignore_ascii_case("on"),
      // tier fallback: fast tier includes fastrate (BD-negative standalone:
      // -0.117% mean; stack-measured +0.099% total)
      Err(_) => fast_tier() == FastTier::Fast,
    }
  })
}

// --- prom_av1e015: table-costed counter symbols ------------------------------
//
// libaom/SVT never run the range coder for RDO costing: adaptive symbols are
// costed by a prob→bits table (av1_prob_cost). Our WriterCounter instead runs
// lr_compute + lzcnt + rng tracking per symbol (192M calls/10f) purely to
// count bits. `RAV1E_FASTSYM=1` replaces the counter's store() with an ideal
// −log2(width) lookup in tell_frac units (1/8 bit — the same resolution RDO
// already consumes). CDF adaptation (log.push + update_cdf) is untouched, so
// context evolution is preserved; only the coder-state rounding epsilons
// differ from true bits ⇒ decisions can drift ⇒ BD-gated.

static FASTSYM: OnceLock<bool> = OnceLock::new();

/// True when counter symbols are costed by table instead of the range coder.
#[inline]
pub fn fastsym() -> bool {
  *FASTSYM.get_or_init(|| {
    matches!(std::env::var("RAV1E_FASTSYM").as_deref().map(str::trim), Ok("1"))
  })
}

/// Cost in 1/8-bit units of a symbol whose bracket width is `w` in the
/// 9-bit (Q15 >> EC_PROB_SHIFT) domain, 0..=512. Ideal −log2(w/512); w = 0
/// (possible after the >>6 quantization) costs as the EC_MIN_PROB floor
/// (4/32768 ⇒ 13 bits).
#[inline]
pub fn sym_cost_frac(w: usize) -> u32 {
  static TABLE: OnceLock<[u16; 513]> = OnceLock::new();
  let t = TABLE.get_or_init(|| {
    let mut t = [104u16; 513]; // 13 bits × 8 — the w=0 / EC_MIN_PROB floor
    for w in 1..=512usize {
      t[w] = ((9.0 - (w as f64).log2()) * 8.0).round() as u16;
    }
    t
  });
  u32::from(t[w])
}

// --- prom_av1e016: frozen-CDF trial costing ----------------------------------
//
// SVT's MD stages cost from per-frame rate tables and never touch CDFs;
// only the final encode adapts them. Our counter trials instead run
// log.push (a 10-34B undo-log snapshot per symbol, no dedup) + update_cdf
// per symbol, then roll every update back — the net effect on the CDF
// context is zero BY CONSTRUCTION (rollback is what makes trials legal),
// so skipping both changes only the cost estimates. `RAV1E_FROZEN=1`
// freezes COUNTER (CountsOnly) writers only: recorders MUST stay adaptive —
// their tokens are replayed into the real bitstream and stale brackets
// would desync the decoder. Costs drift within a block (no intra-trial
// adaptation) ⇒ BD-gated.

static FROZEN: OnceLock<bool> = OnceLock::new();

/// True when counter trials neither log nor update CDFs (frozen costing).
#[inline]
pub fn frozen() -> bool {
  *FROZEN.get_or_init(|| {
    match std::env::var("RAV1E_FROZEN") {
      Ok(v) => v.trim() == "1" || v.trim().eq_ignore_ascii_case("on"),
      // tier fallback: frozen composes cleanly with the fast tier — BD
      // 0.099% → 0.056% vs off while adding ~15% same-ladder speed
      // (349.9s vs 419.8s; trial14). NOTE fastsym does NOT get this
      // fallback: it leaks with both frozen (+0.201%) and the tier
      // (+0.313%) — approximations compose sub-additively in BD.
      Err(_) => fast_tier() == FastTier::Fast,
    }
  })
}

// --- prom_av1e017: luma reuse across intra chroma-mode trials ----------------
//
// The intra chroma loop re-codes the ENTIRE block (all luma planes) per
// chroma mode — 13.3% of tier RDO measured for the 2nd iteration alone.
// Only the chroma coding differs between iterations, so the luma tx section
// is cached from iteration 1 and skipped afterwards (rate re-injected via
// fake-bits). Exact under frozen costing up to counter-rng epsilons ⇒
// BD-gated. `RAV1E_LUMA_REUSE=1`; unset = off (tier fold pending gates).

static LUMA_REUSE: OnceLock<bool> = OnceLock::new();

/// True when the intra chroma loop reuses iteration-1 luma results.
#[inline]
pub fn luma_reuse() -> bool {
  *LUMA_REUSE.get_or_init(|| {
    matches!(
      std::env::var("RAV1E_LUMA_REUSE").as_deref().map(str::trim),
      Ok("1")
    )
  })
}

// --- prom_av1e031/032: content-adaptive dispatch (variance partition) --------
//
// `RAV1E_VARPART=T` forces the variance-partition alternative ON for every SB
// (Brick 2 force-on): at each square NONE/SPLIT node, SPLIT iff the node's
// 256·per-sample residual variance exceeds T, else NONE — no RD search. The
// leaf (rdo_mode_decision + encode) is reused verbatim, so the stream is
// decodable by construction. Decision-space swap ⇒ BD-gated. Brick 3 replaces
// the fixed T with a per-frame percentile.

static VARPART: OnceLock<Option<i64>> = OnceLock::new();

/// Some(T) forces the variance partition with split-threshold T.
#[inline]
pub fn varpart() -> Option<i64> {
  *VARPART.get_or_init(|| {
    std::env::var("RAV1E_VARPART").ok().and_then(|v| v.trim().parse().ok())
  })
}

// Brick 3: the per-frame PERCENTILE dispatcher. `RAV1E_DISPATCH_Q=q` routes
// the fraction q of SBs (by root residual variance) to the variance
// partition; the rest keep the full RD search. Direction: default routes the
// LOW-variance (easy) SBs to varpart (quality-optimal — keeps RD where it
// helps); `RAV1E_DISPATCH_HI=1` routes the HIGH-variance (most-expensive) SBs
// instead (time-capping). The same percentile threshold drives both routing
// and the routed SB's internal NONE/SPLIT decision (content-invariant dial).

static DISPATCH_Q: OnceLock<Option<f64>> = OnceLock::new();
static DISPATCH_HI: OnceLock<bool> = OnceLock::new();

/// Some(q) enables the percentile dispatcher with routed fraction q∈[0,1].
#[inline]
pub fn dispatch_q() -> Option<f64> {
  *DISPATCH_Q.get_or_init(|| {
    std::env::var("RAV1E_DISPATCH_Q")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .filter(|q: &f64| *q > 0.0 && *q <= 1.0)
  })
}

/// True routes HIGH-variance SBs to varpart (time-capping) instead of low.
#[inline]
pub fn dispatch_hi() -> bool {
  *DISPATCH_HI.get_or_init(|| {
    matches!(std::env::var("RAV1E_DISPATCH_HI").as_deref().map(str::trim), Ok("1"))
  })
}

// --- prom_av1e029: intra full-trial cap --------------------------------------
//
// The aggressive MODE_TOPK knob caps INTER mode trials but the intra path
// full-trials num_modes_rdo (3-7) uncapped — measured 14% of full-price work
// on motion content where intra rarely wins. `RAV1E_INTRA_TOPK=K` caps the
// intra full-trial loop to the top-K CDF-prob/SATD-ranked modes. Decision-
// space reduction ⇒ BD-gated.

static INTRA_TOPK: OnceLock<Option<usize>> = OnceLock::new();

/// Some(K) caps intra mode full-trials to the top-K ranked candidates.
#[inline]
pub fn intra_topk() -> Option<usize> {
  *INTRA_TOPK.get_or_init(|| {
    std::env::var("RAV1E_INTRA_TOPK").ok().and_then(|v| v.trim().parse().ok())
  })
}

// --- prom_av1e028: rate-aware mode screen ------------------------------------
//
// THE FRONTIER FIX. Our inter-mode screen ranked candidates by SATD alone —
// blind to the mv-residual rate that separates NEWMV (cheap SATD, dear bits)
// from NEAREST/NEAR (dearer SATD, ~free bits). av1e027 proved cutting K on a
// distortion-only ranking blows BD up (+2.2%). `RAV1E_FASTRD=1` adds the
// mv-rate term to the screen key using ME's own calibration
// (cost/256 = SATD + rate·me_lambda·0.5), so the top-K after sorting are the
// real RD leaders and K can drop toward the ~2 frontier. Decision-space
// reduction ⇒ BD-gated; also forces the screen on like MODE_TOPK.

static FASTRD: OnceLock<bool> = OnceLock::new();

/// True when the inter-mode screen ranks by SATD + mv-rate instead of SATD.
#[inline]
pub fn fastrd() -> bool {
  *FASTRD.get_or_init(|| {
    match std::env::var("RAV1E_FASTRD") {
      Ok(v) => v.trim() == "1" || v.trim().eq_ignore_ascii_case("on"),
      // tier fallback: rate-aware ranking is the CORRECT screen (ME has
      // always ranked SATD+rate); at the tier's K=6 it is a free BD win
      // (−0.039% mean, ~0 speed; trial22) ⇒ folded on. RAV1E_FASTRD=0 opts
      // out.
      Err(_) => fast_tier() == FastTier::Fast,
    }
  })
}

// --- prom_av1e020: chroma-mode SATD pre-selection ----------------------------
//
// The intra chroma-mode loop full-codes the whole block per candidate;
// `RAV1E_CHROMA_PRESEL=1` collapses the set to the per-plane prediction-SATD
// winner before the loop (decision-space reduction, the TOPK class).

static CHROMA_PRESEL: OnceLock<bool> = OnceLock::new();

/// True when the intra chroma-mode set is pre-selected by SATD.
#[inline]
pub fn chroma_presel() -> bool {
  *CHROMA_PRESEL.get_or_init(|| {
    matches!(
      std::env::var("RAV1E_CHROMA_PRESEL").as_deref().map(str::trim),
      Ok("1")
    )
  })
}

// --- prom_av1e023: SB-level early skip ---------------------------------------
//
// `RAV1E_SB_SKIP=k` bypasses the whole 64×64 partition/mode RDO when the
// NEARESTMV(LAST) proxy SATD < k/256 quantizer steps per pixel — the SVT
// depth-removal analog for static/smooth superblocks. Unset = off.

static SB_SKIP: OnceLock<Option<u64>> = OnceLock::new();

/// Some(k) when the SB early-skip gate is enabled.
#[inline]
pub fn sb_skip() -> Option<u64> {
  *SB_SKIP.get_or_init(|| {
    match std::env::var("RAV1E_SB_SKIP") {
      Ok(v) => match v.trim() {
        "0" | "off" => None,
        s => s.parse().ok(),
      },
      // tier fallback: k=8 passed the composition gate CLEAN on both
      // resolutions — 1080p BD −0.001% @ +12.8% wall, CIF +0.000% @ +2.2%
      // (trial18/19; rotated arms). First clean tier leg since frozen.
      Err(_) => (fast_tier() == FastTier::Fast).then_some(8),
    }
  })
}

// --- prom_av1e010: PD0 proxy margin gates -----------------------------------
//
// A cheap SATD proxy tree (node vs 4 children, one NEARESTMV prediction each)
// gates BOTH partition arms by its dimensionless margin m = (node−kids)/node:
// m < TN ⇒ NONE-confident, skip the split subtree; m > TS ⇒ split-confident,
// skip the fresh 64×64 NONE full trial. `RAV1E_PD0_GATE=TN:TS`; unset = off.

static PD0_GATE: OnceLock<Option<(f64, f64)>> = OnceLock::new();

/// (TN, TS) margin thresholds for the PD0 proxy gates, or `None` when off.
pub fn pd0_gate() -> Option<(f64, f64)> {
  *PD0_GATE.get_or_init(|| {
    let v = match std::env::var("RAV1E_PD0_GATE") {
      Ok(v) => v,
      Err(_) if fast_tier() == FastTier::Fast => {
        return Some((-999.0, 0.134))
      }
      Err(_) => return None,
    };
    let v: Option<String> = Some(v);
    let v = v?;
    let s = v.trim();
    if s.is_empty() || s == "0" || s == "off" {
      return None;
    }
    let (tn, ts) = s.split_once(':')?;
    Some((tn.trim().parse().ok()?, ts.trim().parse().ok()?))
  })
}

// --- prom_av1e004: calibrated MV-cost model for the motion search ----------
//
// me.rs::get_mv_rate prices an MV diff at 2·ilog(|d|) bits/axis; the harvested
// truth (3.58M write_mv observations, tell_frac-exact) is
//   bits ≈ 5.71 − 2.85·(active axes) + 2.06·Σ ilog(|d'|)    (RMS 1.08 vs 3.26)
// i.e. the stock model misses a ~5.7-bit magnitude-independent floor and the
// per-axis structure. `RAV1E_MVCOST`: unset = stock (byte-identical); `1` =
// fitted constants below; `C0:CNZ:CIL` = custom, all in EIGHTH-bit units.

static MVCOST: OnceLock<Option<(i32, i32, i32)>> = OnceLock::new();

/// (C0, C_nonzero_axis, C_per_ilog) in eighth-bits, or `None` for stock.
pub fn mvcost() -> Option<(i32, i32, i32)> {
  *MVCOST.get_or_init(|| {
    let v = std::env::var("RAV1E_MVCOST").ok()?;
    let s = v.trim();
    match s {
      "" | "0" | "off" => None,
      "1" | "on" => Some((46, -23, 16)),
      _ => {
        let mut it = s.split(':');
        let c0 = it.next()?.trim().parse().ok()?;
        let cnz = it.next()?.trim().parse().ok()?;
        let cil = it.next()?.trim().parse().ok()?;
        Some((c0, cnz, cil))
      }
    }
  })
}

// --- prom_av1e001: LRF sgrproj solve gate ---------------------------------
//
// Discovered from the harvest corpus (4 Derf CIF clips × q60..200, 23 779 LRU
// decisions): 83.8% of sgrproj solves end in RestorationFilter::None anyway,
// and the None-arm RD cost normalized by λ separates them — below a per-plane
// threshold, the solve almost never pays. Tuned operating point: Y=800 /
// UV=200 ≈ 45% of solves skipped while keeping ~99% of the total LRF gain.
//
// `RAV1E_LRF_GATE`: unset/`0`/`off` = disabled (bitstream identical to
// baseline); `1`/`on` = tuned defaults; `<TY>:<TUV>` = explicit thresholds.

static LRF_GATE: OnceLock<Option<(f64, f64)>> = OnceLock::new();

/// Per-plane (Y, UV) thresholds for the LRF solve gate, or `None` when off.
pub fn lrf_gate() -> Option<(f64, f64)> {
  *LRF_GATE.get_or_init(|| {
    let v = std::env::var("RAV1E_LRF_GATE").ok()?;
    match v.as_str() {
      "" | "0" | "off" => None,
      "1" | "on" => Some((800.0, 200.0)),
      s => {
        let (ty, tuv) = s.split_once(':')?;
        Some((ty.trim().parse().ok()?, tuv.trim().parse().ok()?))
      }
    }
  })
}
