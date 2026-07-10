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

static LAMBDA_MULT: OnceLock<Option<(f64, f64)>> = OnceLock::new();

/// (intra_mult, inter_mult) for the λ probe, or `None` when off.
pub fn lambda_mult() -> Option<(f64, f64)> {
  *LAMBDA_MULT.get_or_init(|| {
    let v = std::env::var("RAV1E_LAMBDA_MULT").ok()?;
    let s = v.trim();
    if s.is_empty() || s == "0" || s == "off" {
      return None;
    }
    if let Some(m) = s.strip_prefix("i:") {
      return Some((m.parse().ok()?, 1.0));
    }
    if let Some(m) = s.strip_prefix("p:") {
      return Some((1.0, m.parse().ok()?));
    }
    if let Some((mi, mp)) = s.split_once(':') {
      return Some((mi.parse().ok()?, mp.parse().ok()?));
    }
    let m: f64 = s.parse().ok()?;
    Some((m, m))
  })
}

// --- RAV1E_FAST: the Prometheus speed-tier bundle ---------------------------
//
// One switch for the campaign's kept knobs (individual envs still override):
//   RAV1E_FAST=clean → MODE_TOPK=6                       (+25.8% @ +0.024% BD)
//   RAV1E_FAST=fast  → + LRF gate + partition gate       (+29.6% @ +0.213% BD)
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
    let v = match std::env::var("RAV1E_PART_GATE") {
      Ok(v) => v,
      Err(_) if fast_tier() == FastTier::Fast => {
        return Some((300.0, -1.0, -1.0))
      }
      Err(_) => return None,
    };
    let v: Option<String> = Some(v);
    let v = v?;
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
    let v = match std::env::var("RAV1E_LRF_GATE") {
      Ok(v) => v,
      Err(_) if fast_tier() == FastTier::Fast => return Some((800.0, 200.0)),
      Err(_) => return None,
    };
    let v: Option<String> = Some(v);
    let v = v?;
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
