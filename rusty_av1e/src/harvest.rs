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

// --- prom_av1e047: coefficient-level trellis (RDOQ) ------------------------
//
// `RAV1E_TRELLIS=1` runs a backward RD pass over each block's interior
// coefficients (final encode only), lowering a level when the exact rate saved
// (symbol_bits) beats the tx-domain distortion added. Off = the deadzone
// quantizer alone (byte-identical). Adaptive follow-up: route to busy SBs where
// the residual is (the deep flag), and check the per-clip sign-flip.
static TRELLIS: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn trellis() -> bool {
  *TRELLIS.get_or_init(|| match std::env::var("RAV1E_TRELLIS") {
    Ok(v) => v.trim() == "1",
    // tier fallback: a FREE quality win — dispatched to non-flat SBs it is
    // −0.506% mean BD (mobile −1.0%), every clip improves, ~0% wall (O(n)
    // symbol_bits rate, busy SBs only). RAV1E_TRELLIS=0 opts out.
    Err(_) => fast_tier() >= FastTier::Turbo,
  })
}

// RAV1E_TRELLIS_ALL=1 forces the trellis on EVERY block (the force-on ceiling);
// otherwise the trellis is dispatched to non-flat SBs (sign-flip → route).
static TRELLIS_ALL: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn trellis_all() -> bool {
  *TRELLIS_ALL.get_or_init(|| {
    matches!(
      std::env::var("RAV1E_TRELLIS_ALL").as_deref().map(str::trim),
      Ok("1")
    )
  })
}

// Absolute residual-variance threshold: SBs above it get the trellis (busy),
// flat SBs below it (akiyo) are skipped. `RAV1E_TRELLIS_T=N`; default 20000
// (the akiyo↔foreman boundary from the av1e040 segmentation).
static TRELLIS_T: OnceLock<i64> = OnceLock::new();
#[inline]
pub fn trellis_t() -> i64 {
  *TRELLIS_T.get_or_init(|| {
    std::env::var("RAV1E_TRELLIS_T")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .unwrap_or(20000)
  })
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

// prom_av1e053 T1: speed-tier ladder, ordered by quality effort (higher = more
// bricks = slower). Turbo = Fast minus the one expensive brick (OBMC, −1% BD /
// +12% compute); Quality = Fast plus the deep tx-type RDO (−1.7% / +36%). The
// near-free bricks (interp, trellis) ride at every rung ≥ Turbo.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FastTier {
  Off,
  Clean,
  Turbo,
  Fast,
  Quality,
}

static FAST_TIER: OnceLock<FastTier> = OnceLock::new();

pub fn fast_tier() -> FastTier {
  *FAST_TIER.get_or_init(|| match std::env::var("RAV1E_FAST") {
    Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
      "clean" => FastTier::Clean,
      "turbo" => FastTier::Turbo,
      "1" | "on" | "fast" => FastTier::Fast,
      "quality" | "q" => FastTier::Quality,
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
    let v = match std::env::var("RAV1E_PART_GATE") {
      Ok(v) => v,
      // tier fallback (prom_av1e038): the MULTI-LEVEL accurate NONE-cost gate.
      // The av1e005 keeper gated only 64×64 (believing 32/16 leaked); the
      // av1e038 ladder disproved that — conservative per-level thresholds
      // 300:150:75 skip the split subtree at 64/32/16 for −12.2% forward
      // transforms at +0.126% mean BD (akiyo/foreman/mobile actually IMPROVE),
      // additive on top of the fast tier's pd0 gate. Unlike the refuted
      // variance/SATD partition PROXIES, the ACCURATE none_rd cost stays the
      // arbiter, so the prune only fires where full RD would also pick NONE.
      Err(_) if fast_tier() >= FastTier::Turbo => {
        return Some((300.0, 150.0, 75.0))
      }
      Err(_) => return None,
    };
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
      Err(_) => fast_tier() >= FastTier::Turbo,
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
      Err(_) => fast_tier() >= FastTier::Turbo,
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

// --- prom_av1e034: estimated-rate RDO trials (2-tier funnel) ------------------
//
// `RAV1E_TXRATE=1` makes mode-decision trials cost coefficient rate from a
// table (estimate_rate on tx-domain distortion) instead of running the full
// range coder per candidate — the SVT MDS0/MDS1 "fast cost". The winner is
// re-coded exactly at final encode, so only the ranking uses estimated rate.
// Decision-space change (estimate vs exact rate) ⇒ BD-gated.

// --- prom_av1e035: SVT-style subsampled coefficient cost ---------------------
//
// SVT's coefficient-structure-aware fast cost walks the real quantized levels
// but SUBSAMPLES: it costs the DC, the last-eob coefficient, and the first
// eob/N low-frequency coefficients in full (base + range), and skips the
// base/range of the high-frequency middle (their tiny levels contribute
// little rate). `RAV1E_FASTCOEFF=N` applies this to COUNTER trials only —
// the real coder always codes every coefficient, so the bitstream is
// unchanged; only the RDO rate estimate is subsampled ⇒ BD-gated.

// --- prom_av1e037: adaptive-K mode full-trials -------------------------------
//
// The SATD screen ranks all candidates; the tier then full-transforms the top
// K (=6). But when the best screen cost is confidently below the rest, the
// winner is nearly certain and full-trialing 6 wastes ~5 transforms.
// `RAV1E_ADAPTK=M` (percent) full-trials only the candidates whose screen key
// is within +M% of the best (clamped to [1, K]) — content-adaptive work.
// Decision-space change ⇒ BD-gated.

static ADAPTK: OnceLock<Option<u64>> = OnceLock::new();

/// Some(M) enables adaptive-K with a +M% screen-cost margin.
#[inline]
pub fn adaptk() -> Option<u64> {
  *ADAPTK.get_or_init(|| {
    std::env::var("RAV1E_ADAPTK").ok().and_then(|v| v.trim().parse().ok())
  })
}

static FASTCOEFF: OnceLock<Option<usize>> = OnceLock::new();

/// Some(N) subsamples the counter-trial coefficient cost to ~eob/N coeffs.
#[inline]
pub fn fastcoeff() -> Option<usize> {
  *FASTCOEFF.get_or_init(|| {
    std::env::var("RAV1E_FASTCOEFF")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .filter(|&n: &usize| n >= 1)
  })
}

static TXRATE: OnceLock<bool> = OnceLock::new();

/// True when RDO trials cost rate by table instead of the coefficient coder.
#[inline]
pub fn txrate() -> bool {
  *TXRATE.get_or_init(|| {
    matches!(std::env::var("RAV1E_TXRATE").as_deref().map(str::trim), Ok("1"))
  })
}

static RATEHARVEST: OnceLock<bool> = OnceLock::new();

/// True when encode_tx_block emits (q, tx_size, tx_dist, real_rate) pairs to
/// the harvest sink for refitting estimate_rate's table.
#[inline]
pub fn rateharvest() -> bool {
  *RATEHARVEST.get_or_init(|| {
    matches!(std::env::var("RAV1E_RATEHARVEST").as_deref().map(str::trim), Ok("1"))
      && enabled()
  })
}

static TXRATE_MUL: OnceLock<Option<u64>> = OnceLock::new();

/// Some(percent) scales the estimate_rate output (empirical recalibration).
#[inline]
pub fn txrate_mul() -> Option<u64> {
  *TXRATE_MUL.get_or_init(|| {
    std::env::var("RAV1E_TXRATE_MUL").ok().and_then(|v| v.trim().parse().ok())
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
      Err(_) => fast_tier() >= FastTier::Turbo,
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
      Err(_) => (fast_tier() >= FastTier::Turbo).then_some(8),
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
      Err(_) if fast_tier() >= FastTier::Turbo => {
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

// --- prom_av1e039: PD0 real-cost partition screen ---------------------------
//
// SVT's PD0 predicts the partition depth with a fast REAL RD cost (1 candidate
// + transform + quant + coeff-rate), then full RD refines within a band. Our
// prior screens (SATD pd0_gate av1e010, variance flatskip av1e038) used cheap
// PROXIES as the arbiter and leaked BD. pd0_real_cost is the real screen.
// `RAV1E_PD0_CEIL=1` = isolation instrument only: at each square node emit the
// PD0 node/kids real RD + the actual full-RD decision, for offline agreement.
static PD0_CEIL: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn pd0_ceil() -> bool {
  *PD0_CEIL.get_or_init(|| {
    matches!(std::env::var("RAV1E_PD0_CEIL").as_deref().map(str::trim), Ok("1"))
  })
}

// `RAV1E_PD0_REAL=TN:TS` = the SHIPPING gate: skip the split subtree when the
// PD0 real-cost margin m=(node-kids)/node < TN (node-confident), skip the
// fresh NONE full trial when m > TS (split-confident). Unset = off.
static PD0_REAL: OnceLock<Option<(f64, f64)>> = OnceLock::new();
#[inline]
pub fn pd0_real() -> Option<(f64, f64)> {
  *PD0_REAL.get_or_init(|| {
    let v = std::env::var("RAV1E_PD0_REAL").ok()?;
    let s = v.trim();
    if s.is_empty() || s == "0" || s == "off" {
      return None;
    }
    let (tn, ts) = s.split_once(':')?;
    Some((tn.trim().parse().ok()?, ts.trim().parse().ok()?))
  })
}

// --- prom_av1e040: per-SB content segmentation instrument ------------------
//
// Break open the RD partition+mode search BY CONTENT. `RAV1E_SBSEG=1` emits one
// row per SB: residual variance (the dispatch signal) + the partition-depth
// outcome (area at each block size = the search-cost/content map). Offline
// binning by variance tier reveals which segment is over-served (cheapen) vs
// under-served (deepen) — the input to the per-segment algorithm choice.
static SBSEG: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn sbseg() -> bool {
  *SBSEG.get_or_init(|| {
    matches!(std::env::var("RAV1E_SBSEG").as_deref().map(str::trim), Ok("1"))
  })
}

// --- prom_av1e041: per-SB DEEP-search dispatch (content-adaptive quality ladder)
//
// `RAV1E_DEEP=1` forces the deep alternative ON for every SB (the force-on A/B —
// measure the ceiling before wiring the dispatcher). `RAV1E_DEEP_Q=q` deepens
// the fraction q of SBs by residual variance, LOW-variance first (av1e040: deep
// search is 2.5× more BD/sec on low-variance SBs). Unset = fast tier.
static DEEP_FORCE: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn deep_force() -> bool {
  *DEEP_FORCE.get_or_init(|| {
    matches!(std::env::var("RAV1E_DEEP").as_deref().map(str::trim), Ok("1"))
  })
}

static DEEP_Q: OnceLock<Option<f64>> = OnceLock::new();
#[inline]
pub fn deep_q() -> Option<f64> {
  *DEEP_Q.get_or_init(|| {
    std::env::var("RAV1E_DEEP_Q")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .filter(|q: &f64| *q > 0.0 && *q <= 1.0)
      // prom_av1e053 T1: the Quality rung enables the deep tx-type RDO at its
      // concave sweet spot (q0.5 = 68% of the force-on gain for 58% of the cost,
      // av1e041). Fast/Turbo leave it off (it costs +36% compute).
      .or_else(|| (fast_tier() >= FastTier::Quality).then_some(0.5))
  })
}

// Direction: default routes the HIGH-variance (busy) fraction to deep search —
// inter tx-type RDO pays where there is RESIDUAL to re-transform (av1e041
// ladder: deepening low-variance/flat SBs found nothing and lost bits).
// `RAV1E_DEEP_LO=1` routes the low-variance fraction instead (probe).
static DEEP_LO: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn deep_lo() -> bool {
  *DEEP_LO.get_or_init(|| {
    matches!(std::env::var("RAV1E_DEEP_LO").as_deref().map(str::trim), Ok("1"))
  })
}

// --- prom_av1e045: ADAPTIVE interp filter (the sign-flip → dispatch rule) ---
//
// SHARP vs REGULAR flips sign by content (mobile −1.8%, bus +2.7%), so the
// filter is not one choice — it's a dispatch. `RAV1E_AFILTER=1` picks each
// inter frame's fixed filter by a cheap SATD trial (predict a sample grid with
// REGULAR vs SHARP at their ME MVs, take the lower-SATD filter). Off = REGULAR
// (byte-identical). The chosen filter is BOTH predicted and header-signaled, so
// encoder/decoder stay consistent (single fixed filter per frame, no switchable
// syntax needed).
static AFILTER: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn afilter() -> bool {
  *AFILTER.get_or_init(|| match std::env::var("RAV1E_AFILTER") {
    Ok(v) => v.trim() == "1",
    // tier fallback: a FREE quality win (−0.859% mean BD, mobile −3.4%, ~0%
    // wall — the per-frame SATD trial is a coarse grid) ⇒ folded into the fast
    // tier. RAV1E_AFILTER=0 opts out; multi-tile frames fall back to REGULAR.
    Err(_) => fast_tier() >= FastTier::Turbo,
  })
}

// prom_av1e048c: PER-BLOCK switchable interpolation filter. Default OFF
// (bitstream change; default path stays byte-identical). RAV1E_PBINTERP=1 on.
static PBINTERP: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn pbinterp() -> bool {
  *PBINTERP.get_or_init(|| match std::env::var("RAV1E_PBINTERP") {
    Ok(v) => v.trim() == "1",
    Err(_) => false,
  })
}

// prom_av1e050/051: OBMC (overlapped block motion compensation) motion_mode.
// FOLDED INTO THE FAST TIER (prom_av1e051): with the per-clip motion gate
// (obmc_mgate) it is −0.998% mean BD on Derf-CIF with WORST clip 0.00% (a clean
// monotone non-regression). RAV1E_OBMC=1 forces it on (per-block RD unless
// obmc_mgate also set); RAV1E_OBMC=0 opts out. Unset = fast tier only, so the
// plain default (RAV1E_FAST unset) stays byte-identical.
static OBMC: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn obmc() -> bool {
  *OBMC.get_or_init(|| match std::env::var("RAV1E_OBMC") {
    Ok(v) => v.trim() == "1",
    // prom_av1e053 T1: OBMC is the one expensive brick — on at Fast+, OFF at
    // Turbo (the speed rung that drops it for +12% encode / +1% BD).
    Err(_) => fast_tier() >= FastTier::Fast,
  })
}

// prom_av1e050: per-frame OBMC switchable-dispatch threshold on the PREVIOUS
// frame's OBMC-adoption fraction. A frame signals switchable (pays the per-block
// motion_mode tax) only when the prior frame's adoption cleared this — high on
// coherent-motion content where OBMC broadly helps, low where the RD only
// over-picks noisy blocks (the sign-flip → dispatch).
static OBMC_T: OnceLock<f64> = OnceLock::new();
#[inline]
pub fn obmc_t() -> f64 {
  *OBMC_T.get_or_init(|| {
    std::env::var("RAV1E_OBMC_T")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .unwrap_or(0.88)
  })
}

// prom_av1e050: force OBMC on for every eligible block (bring-up A/B, before the
// RD search) — validates the prediction + signaling bit-exact vs the decoder.
static OBMC_FORCE: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn obmc_force() -> bool {
  *OBMC_FORCE.get_or_init(|| match std::env::var("RAV1E_OBMC_FORCE") {
    Ok(v) => v.trim() == "1",
    Err(_) => false,
  })
}

// prom_av1e053 T1 PRUNED: a per-block OBMC blend-coherence gate (neighbour-MV
// deviation band) LOSES both ways — within motion-gate-enabled clips OBMC helps
// at every coherence level; the per-clip motion-gate is the right granularity.

// prom_av1e051: per-CLIP MOTION-GATED force-on OBMC. Derf-CIF deployment showed
// the per-block-RD dispatch is CONTAMINATED — it can't see OBMC's temporal-
// reference-chain benefit, so it under-adopts (halves the win vs force-on) and
// even mis-picks. Force-on beats it 2× but LOSES on 2/8 low-motion clips
// (container +1.13, news +0.91). The true dispatch is per-CLIP: force-on where
// motion clears a floor, byte-identical-off below it (the loss band = slight
// incoherent motion over flat regions). Latched once on the first inter frame.
static OBMC_MGATE: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn obmc_mgate() -> bool {
  *OBMC_MGATE.get_or_init(|| match std::env::var("RAV1E_OBMC_MGATE") {
    Ok(v) => v.trim() == "1",
    // tier fallback: the motion gate is the SHIPPING OBMC dispatch. Fast+ only
    // (Turbo drops OBMC) — must track obmc() above.
    Err(_) => fast_tier() >= FastTier::Fast,
  })
}

// Motion floor (mean per-pixel zero-MV SAD over a coarse first-inter-frame grid)
// above which the clip force-ons OBMC. Wide margin on the corpus: losers ~1.3-1.6,
// winners ~5.8-21 in source-SAD terms; tuned on the in-encoder (recon-ref) value.
static OBMC_MT: OnceLock<f64> = OnceLock::new();
#[inline]
pub fn obmc_mt() -> f64 {
  *OBMC_MT.get_or_init(|| {
    std::env::var("RAV1E_OBMC_MT")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .unwrap_or(6.0)
  })
}

// prom_av1e052: WARP (local warped motion) — the OTHER motion_mode, the twin of
// OBMC. M1 = signaling only (3-way motion_mode_cdf + find_matching_ref
// eligibility), M2 = the affine prediction (bit-exact vs dav1d).
// prom_av1e053 T3b: DEFAULT ON at the Fast rung. Warp is only ever applied when
// the pre-pass shear gate fires, and that gate is calibrated against per-clip
// forced-warp ground truth over 18 clips: every firing clip is a real BD win,
// and every non-firing clip is BYTE-IDENTICAL to warp-off. So the default costs
// nothing where it does not help and pays -1.5%..-21.3% where it does.
//
// It rides the SAME rung as OBMC because it structurally depends on it: a warp
// block is selected through the motion_mode symbol, which is only coded when
// `obmc_frame::on()`. Below Fast the gate could still fire and set the header
// flag, spending a bit per inter frame on a tool that can never engage —
// measured as a same-size-but-different bitstream at stock/clean/turbo.
// RAV1E_WARP=1/0 overrides in either direction.
static WARP: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn warp() -> bool {
  *WARP.get_or_init(|| match std::env::var("RAV1E_WARP") {
    Ok(v) => v.trim() == "1",
    Err(_) => fast_tier() >= FastTier::Fast,
  })
}

// prom_av1e052 WARP M2: force LOCALWARP on every warp-eligible block (the
// prediction bring-up A/B, before RD). Requires RAV1E_WARP=1 too.
static WARP_FORCE: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn warp_force() -> bool {
  *WARP_FORCE.get_or_init(|| match std::env::var("RAV1E_WARP_FORCE") {
    Ok(v) => v.trim() == "1",
    Err(_) => false,
  })
}

// prom_av1e053 T3: per-CLIP WARP dispatch — latch force-warp for the clip when
// the first inter frame's mean shear magnitude clears a floor (affine content:
// rotation/zoom). Off on translational content (no regression). Requires
// RAV1E_WARP=1. RAV1E_WARP_SHEAR_T = the shear floor (max|abcd| units).
static WARP_GATE: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn warp_gate() -> bool {
  *WARP_GATE.get_or_init(|| match std::env::var("RAV1E_WARP_GATE") {
    Ok(v) => v.trim() == "1",
    Err(_) => false,
  })
}
static WARP_SHEAR_T: OnceLock<u64> = OnceLock::new();
#[inline]
pub fn warp_shear_t() -> u64 {
  *WARP_SHEAR_T.get_or_init(|| {
    // Calibrated: rot (affine) first-frame mean shear ≈ 4871 (WARP −21%); the
    // busiest translational clip (bus) ≈ 3625 (WARP loses). Floor between them.
    std::env::var("RAV1E_WARP_SHEAR_T").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(4200)
  })
}

// prom_av1e053 T3b: PRE-PASS WARP dispatch. The T3 latch has to measure on the
// first inter frame, so `allow_warped_motion` must already be committed in the
// header by then (it is a per-clip constant — a dynamic 2-way↔3-way motion_mode
// flip desyncs the CDF context), costing translational clips the ~+0.3% 3-way
// signaling overhead for nothing. The pre-pass instead fits a GLOBAL affine to
// rav1e's LOOKAHEAD motion field — computed on ORIGINAL frame contents before
// any frame is encoded — so the flag is decided BEFORE frame 0 emits: off ⇒
// byte-identical to warp-off (a clean zero), on ⇒ warp from the very first
// inter frame (no measuring frame wasted).
// DEFAULT ON — it is what makes warp safe to enable by default, and it strictly
// dominates the T3 in-encoder latch (RAV1E_WARP_GATE): better BD on affine
// content (no measuring frame) and an exact zero elsewhere (the flag never
// enters the bitstream). RAV1E_WARP_PRE=0 falls back to the T3 latch behaviour.
static WARP_PRE: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn warp_prepass_on() -> bool {
  *WARP_PRE.get_or_init(|| match std::env::var("RAV1E_WARP_PRE") {
    Ok(v) => v.trim() != "0",
    Err(_) => true,
  })
}
// Shear floor — the ONE gate term. Calibrated against per-clip FORCED-warp
// ground truth over 18 clips (8 Derf + 8 synthesized affine/boundary + rot/zoom):
//   winners  shear 1200..6982  (BD -1.5% .. -21.3%)   min winner 1200
//   losers   shear    8.. 976  (BD +0.02% .. +4.6%)   max loser   976
// A single floor between them classifies all 18 correctly; 1100 sits mid-gap.
static WARP_PRE_T: OnceLock<u64> = OnceLock::new();
#[inline]
pub fn warp_pre_t() -> u64 {
  *WARP_PRE_T.get_or_init(|| {
    std::env::var("RAV1E_WARP_PRE_T").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1100)
  })
}
// The affine-explanatory gain (linear-part R²) is COMPUTED and reported under
// RAV1E_WARP_PRE_DBG, but deliberately does NOT gate. It was added on the
// premise that `bus` — a pan whose depth parallax reads as a motion gradient —
// was a warp LOSER that shear alone would wrongly admit. Per-clip forced-warp
// ground truth refuted that: bus is the LARGEST real-content win (-6.481%),
// because LOCAL warped motion models exactly that depth-varying motion even
// though no single GLOBAL affine explains the field (bus gain = 90). Gating on
// gain therefore costs -6.5% and buys nothing shear does not already give.
// Kept as a diagnostic, and as the record of a refuted hypothesis.
// Default 0 = disabled; set RAV1E_WARP_PRE_GAIN_T to re-enable for experiments.
static WARP_PRE_GAIN_T: OnceLock<u64> = OnceLock::new();
#[inline]
pub fn warp_pre_gain_t() -> u64 {
  *WARP_PRE_GAIN_T.get_or_init(|| {
    std::env::var("RAV1E_WARP_PRE_GAIN_T").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
  })
}

// prom_av1e048c: extend the av1e045 per-frame filter trial from best-of-2
// (REGULAR/SHARP) to best-of-3 (add SMOOTH). Clean win on SMOOTH-dominant
// content, neutral elsewhere, zero syntax cost. Default ON (folds into fast).
static AFILTER3: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn afilter3() -> bool {
  *AFILTER3.get_or_init(|| match std::env::var("RAV1E_AFILTER3") {
    Ok(v) => v.trim() != "0",
    Err(_) => true,
  })
}

// prom_av1e048c: run the per-block filter search inside RDO trials too (mode/tx
// co-optimise with the filter) instead of final-encode only. Costlier.
static PBINTERP_RDO: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn pbinterp_rdo() -> bool {
  *PBINTERP_RDO.get_or_init(|| match std::env::var("RAV1E_PBINTERP_RDO") {
    Ok(v) => v.trim() == "1",
    Err(_) => false,
  })
}

// prom_av1e048c: per-frame switchable-vs-fixed dispatch threshold on filter-
// choice DIVERSITY (fraction of sampled blocks not choosing the plurality
// filter). A frame uses SWITCHABLE (per-block filter) only when diversity
// exceeds this — i.e. blocks genuinely want different filters. Tuned on the
// sign-flip: corr627 (WIN, div 0.42) → switchable, al12 (LOSS, div 0.17) and
// perf (wash, div 0.25) → fixed.
static PBINTERP_T: OnceLock<f64> = OnceLock::new();
#[inline]
pub fn pbinterp_t() -> f64 {
  *PBINTERP_T.get_or_init(|| {
    std::env::var("RAV1E_PBINTERP_T")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .unwrap_or(0.33)
  })
}

// --- prom_av1e044: interp-filter ceiling probe -----------------------------
//
// Before building a per-block filter SEARCH, prove the filters differ on our
// content (codec-experimental: prove the ceiling first). `RAV1E_FILTER=0|1|2`
// forces the whole-frame fixed filter to REGULAR/SMOOTH/SHARP (default_filter,
// which is both predicted AND signaled, so encoder/decoder stay consistent).
// If a fixed non-REGULAR filter beats REGULAR on some content, a per-block
// search has headroom; if all three tie, it does not.
static FILTER_PROBE: OnceLock<Option<u8>> = OnceLock::new();
#[inline]
pub fn filter_probe() -> Option<u8> {
  *FILTER_PROBE.get_or_init(|| {
    std::env::var("RAV1E_FILTER")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .filter(|f: &u8| *f <= 2)
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

// prom_av1e056: re-ordering PYRAMID DEPTH. depth 2 = a 4-frame group (the
// long-standing default, previously hard-coded behind a "only works for
// pyramid_depth <= 2" TODO); 3 = 8 frames; 4 = 16, libaom's default shape.
// The pyramid is the single biggest measured lever on these clips — turning it
// OFF entirely (--low-latency, depth 0) costs +5.5pp on bus and +38.6pp on
// tempete against libaom — so deeper groups are the natural next rung.
// Default stays 2: the shipped bitstream is unchanged until deeper rungs gate.
static PYRAMID: OnceLock<u64> = OnceLock::new();
static PYRAMID_OVERRIDE: OnceLock<u64> = OnceLock::new();

/// prom_av1e058: `RAV1E_PYRAMID=auto` asks the CLI to choose the depth from a
/// source probe. The CLI computes it and calls this BEFORE the first
/// `pyramid_depth()` (which caches), because InterConfig is built from it.
pub fn set_pyramid_override(d: u64) {
  let _ = PYRAMID_OVERRIDE.set(d.clamp(1, 4));
}
/// prom_av1e058b: the per-clip probe is now the DEFAULT at the Fast rung and
/// above. It is strictly safe to default: on every clip it declines, the output
/// is BYTE-IDENTICAL to the previous default (verified), and on the clips it
/// routes it is a measured win (mobile −4.07%, shake −2.94%). Stock rav1e (no
/// RAV1E_FAST) is left untouched, as with every other brick in this campaign.
/// `RAV1E_PYRAMID=auto` forces it on at any tier; a numeric value pins the
/// depth and skips the probe entirely.
#[inline]
pub fn pyramid_auto() -> bool {
  match std::env::var("RAV1E_PYRAMID") {
    Ok(v) => v.trim().eq_ignore_ascii_case("auto"),
    Err(_) => fast_tier() >= FastTier::Fast,
  }
}
/// Motion-compensated decay ratio (mc8/mc1) below which a deeper pyramid is
/// selected. Deliberately CONSERVATIVE: ordering 18 measured clips by this
/// ratio, the first clip a deeper pyramid HURTS sits at 1.272, so a floor of
/// 1.2 routes only clips comfortably inside the winning region. That captures
/// 2 of the 9 available winners and — the point of the exercise — cannot make
/// any clip worse. A separating threshold of 2.17 would route 8 winners but
/// regresses one clip by +1.81%, which fails monotone non-regression.
static PYRAMID_T: OnceLock<f64> = OnceLock::new();
#[inline]
pub fn pyramid_ratio_t() -> f64 {
  *PYRAMID_T.get_or_init(|| {
    std::env::var("RAV1E_PYRAMID_T")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .unwrap_or(1.2)
  })
}
#[inline]
pub fn pyramid_depth() -> u64 {
  *PYRAMID.get_or_init(|| {
    if let Some(d) = PYRAMID_OVERRIDE.get() {
      return *d;
    }
    std::env::var("RAV1E_PYRAMID")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .map(|d: u64| d.clamp(1, 4))
      .unwrap_or(2)
  })
}

// prom_av1e056: maximum pyramid level still allowed to inherit CDFs from a
// reference (above it, primary_ref_frame = PRIMARY_REF_NONE and the frame
// starts from default CDFs). The historical value is 2, which was unreachable
// while the pyramid was capped at depth 2 — at depth 3 it silently disqualifies
// every level-3 frame, i.e. half the group.
//
// prom_av1e057: default raised 2 -> 4. This is a NO-OP at the shipped depth
// (levels only reach 2 there and the test is `level > max`), verified
// byte-identical, and worth -1.49pp on bus / -1.27pp on tempete at depth 3 —
// so enabling a deeper pyramid does not also require setting a second knob.
static PRIMREF_LVL: OnceLock<u64> = OnceLock::new();
#[inline]
pub fn primref_max_level() -> u64 {
  *PRIMREF_LVL.get_or_init(|| {
    std::env::var("RAV1E_PRIMREF_LVL")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .unwrap_or(4)
  })
}

// prom_av1e060: GLOBAL MOTION. Per-tool pricing against libaom put this at
// +4.10% mean / +6.71% on bus — 10x masked-compound wedge, the brick the
// campaign had queued. Default OFF during bring-up: signalling a non-identity
// model changes GLOBALMV's meaning everywhere the decoder derives it, so it
// stays opt-in until conformance is proven.
static GLOBAL_MOTION: OnceLock<bool> = OnceLock::new();
#[inline]
pub fn global_motion() -> bool {
  *GLOBAL_MOTION.get_or_init(|| match std::env::var("RAV1E_GM") {
    Ok(v) => v.trim() == "1",
    Err(_) => false,
  })
}

// prom_av1e060 M2: minimum fraction of the motion field that must agree with
// the global model before it is signalled. Signalling rewrites the MV predictor
// for every block, so an incoherent field pays the cost with no takers.
static GM_COH: OnceLock<f64> = OnceLock::new();
#[inline]
pub fn gm_coherence() -> f64 {
  *GM_COH.get_or_init(|| {
    std::env::var("RAV1E_GM_COH")
      .ok()
      .and_then(|v| v.trim().parse().ok())
      .unwrap_or(0.30)
  })
}

// --- prom_av1e061: activity-mask isolation knob (diagnostic, default ON) -----
//
// `--tune Psychovisual` (the CLI default) turns on TWO content-adaptive
// mechanisms at once — the psychovisual ACTIVITY mask (ssim_boost over 8x8
// variance) and, via tx_domain_distortion=false, the whole TPL/mbtree
// propagation. Measuring `--tune Psnr` therefore prices them TOGETHER and also
// swaps the RD distortion domain, so it cannot attribute a result to either.
// `RAV1E_AQ=0` drops ONLY the activity mask, leaving TPL and the distortion
// domain exactly as they were — which is what makes the split clean.
// Unset (or any value but 0/off) = stock behaviour, byte-identical.

static ACTIVITY_MASK: OnceLock<bool> = OnceLock::new();

/// False only when `RAV1E_AQ=0` explicitly disables the psychovisual
/// activity mask; the default is unconditionally stock.
#[inline]
pub fn activity_mask() -> bool {
  *ACTIVITY_MASK.get_or_init(|| match std::env::var("RAV1E_AQ") {
    Ok(v) => !(v.trim() == "0" || v.trim().eq_ignore_ascii_case("off")),
    Err(_) => true,
  })
}

// --- prom_av1e061: TPL isolation knob (diagnostic, default ON) ---------------
//
// `RAV1E_TPL=0` routes `EncoderConfig::temporal_rdo()` to false, which is a
// path the encoder already supports and tests (it is what `--tune Psnr` reaches
// via tx_domain_distortion). Using it directly leaves the RD distortion domain
// untouched, so the BD delta is the TPL's price ALONE.
// Unset (or any value but 0/off) = stock behaviour, byte-identical.

static TPL: OnceLock<bool> = OnceLock::new();

/// False only when `RAV1E_TPL=0` explicitly disables temporal RDO.
#[inline]
pub fn tpl() -> bool {
  *TPL.get_or_init(|| match std::env::var("RAV1E_TPL") {
    Ok(v) => !(v.trim() == "0" || v.trim().eq_ignore_ascii_case("off")),
    Err(_) => true,
  })
}
