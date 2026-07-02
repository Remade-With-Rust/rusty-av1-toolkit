# The decode house — brick ledger for the coeff-decode / entropy path

**Why this path:** the analyzer (`src/prof.rs`, feature `profile`) measured `decode_coefs`
(src/recon.rs:506) at **54.4% of total decode** (531 ms / 977 ms, 305,082 calls,
1-thread, 900f 852×480), all pure Rust. It is the direct mirror of the encoder's
`write_coeffs_lv_map` (48.6%) — the same "entropy house" on the decode side.

Baseline throughput: **~885 fps / ~362 Mpx/s** single-thread (900f 852×480, `--muxer null
--threads 1`), matching the repo's stated ~870 fps. Decode is **deterministic** (1-thread
md5 == multi-thread md5 = `c823cce786105f5f1271b4b28c87d871`) — that md5 is the
**byte-identical gate** for every brick.

## First stage breakdown (opt-coeffs @ e34bedba, 1-thread, profile ON)

| stage | ms | % decode | nature |
|---|---|---|---|
| **coeff decode (`decode_coefs`)** | **531** | **54.4%** | **PURE RUST** — #1 target |
| mode-symbol decode + setup | 170 | 17.4% | PURE RUST (MSAC block/partition/mv symbols) |
| pixel recon (mc + itx + add) | ~117 | ~12% | asm (dispatched fn-ptrs) |
| deblock | 94 | 9.7% | asm |
| cdef | 60 | 6.1% | asm |
| loop restoration | ~0 | — | not exercised by this clip (need an LR clip to profile) |

**Structure:** `decode_coefs` nests inside `recon_b_inter` (620 ms) and `recon_b_intra`
(28 ms); of that 648 ms of recon, 531 ms (82%) is coeff decode and only ~117 ms is the
asm pixel work. Top-level `TileSbrow` (818 ms) − recon = 170 ms of mode/mv symbol decode.

**Verdict: the decoder is ~72% pure-Rust MSAC entropy decoding, ~28% asm kernels** — even
more entropy-dominated than the encoder. The asm kernels (mc, itx, deblock, cdef) are
already optimized; the ROI is the pure-Rust entropy path.

## The key nuance — MSAC core is already asm

rav1d's MSAC symbol readers (`symbol_adapt16/8/4`, `decode_bool`, …) dispatch to dav1d's
hand-written `msac.asm` on x86_64+sse2 (src/msac.rs:14-48, `extern "C"`). So the 531 ms
of `decode_coefs` is **(asm symbol reads) + (pure-Rust coefficient logic)**: nz-map / base
/ br context derivation, the token-class loop (`decode_coefs_class`, recon.rs:750), sign
+ dequant, and the `cf` buffer writes.

This is the **exact mirror of the encoder's win**: there, `symbol_with_update` was the
serial asm-free core (untouchable) but the *context stencil* (`get_nz_map_contexts`) was
the pure-Rust structural brick — B7a/B7a-SIMD, ~−12% encode. On the decoder, the MSAC
reads are the untouchable serial core (already asm), and the pure-Rust **coefficient
context derivation inside `decode_coefs` is the B7a analog** — the first brick to hunt.

## `decode_coefs` — full block-by-block teardown (07-01, measured)

Internal info-scopes (CoefClass / CoefLevelsFill / CoefDequant) + a paired-scope
differential on `get_lo_ctx`. `decode_coefs` = ~53% of decode (631 ms w/ scope overhead,
305,082 calls, 45.0M AC coefficients):

| block | ms | % of decode | nature | brick? |
|---|---|---|---|---|
| **token loop (`decode_coefs_class`)** | **523** | **44%** | SERIAL (loop-carried levels + MSAC CDF) | **NO** |
| — asm MSAC reads (adapt4/hi_tok) | ~480 | ~40% | serial range decode, **already asm** | no (asm) |
| — `get_lo_ctx` context | **~22** | ~2% | pure Rust, but **near-free** (0.5 ns/call, differential) | **NO** |
| — `levels[..end].fill(0)` memset | **~4** | 0.3% | pure Rust | **NO (refuted)** |
| — scan map + level writes + branchless rc | rem. | — | tight scalar, serial | no |
| **dequant + sign loop** | **79** | 6.6% | SERIAL (rc linked-list walk + asm sign) | no |
| header (skip + txtp + eob) | ~30 | ~2.5% | asm MSAC + small per-block ctx | no |

**Verdict: `decode_coefs` is at its serial floor — no byte-identical structural brick.**
This is the crucial asymmetry vs the encoder: the encoder's B7a win existed because
*encoding computes coefficient contexts in a vectorizable BATCH upfront* (all levels are
known), so `get_nz_map_contexts` was 14% of RDO and could be turned into a column-contiguous
SIMD stencil. *Decoding is inherently INCREMENTAL* — `levels[]` is written token-by-token and
each `get_lo_ctx` reads neighbours just written, a loop-carried dependency — so the context
derivation is per-coefficient and already near-free (~0.5 ns), with nothing to batch. The
523 ms is the serial MSAC arithmetic decode, and MSAC is already asm. **The decoder mirror of
B7a does not exist; measurement proved it rather than assuming it.**

Bricks refuted here (do-not-retry): levels memset (3.8 ms), get_lo_ctx restructure/SIMD
(near-free + loop-carried, cannot batch), dequant vectorization (serial linked-list + asm signs).

### Token loop — per-primitive brick audit (A·B·C·D, 07-01)

Iterated EVERY primitive of `decode_coefs_class`. Attempted the ones with any pure-Rust
surface (whole-decode A/B, md5-gated); documented the rest with their floor reason.

**A — per-block setup (225k calls):**
| prim | what | verdict |
|---|---|---|
| A1 | CDF pointer fetch (eob/lo/hi) | references, 0 cost — no brick |
| A2 | scan/stride/lo_ctx_offsets | `const TX_CLASS` generic → compile-time resolved — no brick |
| A3 | shift/mask/end constants | per-block integer math, bounds elided — no brick (cross-fn `tx2dszctx` recompute = 2 mins/block, sub-noise) |
| A4 | `levels[..end].fill(0)` | 3.8 ms; region already minimal per tx size; `.fill` is memset — no brick |

**B — EOB coefficient (once/block):**
| prim | what | verdict |
|---|---|---|
| B1 | eob → (rc,x,y) map | once/block — no brick |
| B2 | eob base token | **ASM** (`eob_base_tok` adapt4) — no brick |
| B3 | eob hi token | **ASM** (`br_tok` hi_tok) — no brick |
| B4 | record cf.set + levels write | 2 stores/block — no brick |

**C — main AC loop (45M calls — the only place a flip could show):**
| prim | what | verdict |
|---|---|---|
| C1 | scan→position (+ `%=32`) | table load + shift/mask, in MSAC shadow; `%=32` can't be safely elided (H/V OOB risk) — no brick |
| C2 | level_off | trivial arithmetic — no brick |
| C3 | **get_lo_ctx** | **TESTED: direct-index → −0.39% FLAT** (9-round A/B; measured ~0.5 ns/call, in shadow) — REVERTED |
| C4 | base token | **ASM** (`base_tok` adapt4) — the dominant cost — no brick |
| C5 | hi token (base-range) | **ASM** (`br_tok` hi_tok) — no brick |
| C6 | branchless pack + linked-list | already branchless (debug_assert-verified), in shadow — no brick |

**D — DC coefficient (once/block):**
| prim | what | verdict |
|---|---|---|
| D1 | DC lo_ctx | once/block — no brick |
| D2 | DC base token | **ASM** — no brick |
| D3 | DC hi token + mag ctx | **ASM** — no brick |

**Every primitive is at its floor.** Two structural facts, both now verified:
1. **All six MSAC readers are asm** (SSE2, dav1d `msac.asm`, `target_feature="sse2"` → always
   on x86_64, checkasm-verified). B2/B3/C4/C5/D2/D3 are off the pure-Rust table entirely.
2. **The pure-Rust primitives run in the asm-MSAC shadow.** C3 — the *biggest* per-coefficient
   pure-Rust op — went direct-index and moved whole-decode −0.39% (flat): out-of-order execution
   overlaps the bookkeeping with the serial range-decode, so it's off the critical path. C1/C2/C6
   are smaller than C3 ⟹ also flat. A/B/D run 225k× vs the loop's 45M× (~200× too small to move
   whole-decode).

The only lever left on the token loop is the **asm range decoder itself** (`msac.asm`) — a
serial SSE2 kernel that doesn't parallelize with SIMD and is already checkasm-verified dav1d
code. Outside the pure-Rust brick game; not attempted. Refuted do-not-retry (token loop): C3
get_lo_ctx direct-index (flat), plus the earlier get_lo_ctx SIMD / levels memset / dequant.

## Mode-symbol path — full teardown (07-01, measured)

The other pure-Rust budget: **mode-symbol decode + setup = 177 ms (~17% of decode)**. Scopes
on every named helper (`MvRefsFind`/`VartxTree`/`ResetCtx`), remainder = diffuse per-block reads:

| block | ms | % decode | nature | brick? |
|---|---|---|---|---|
| **`refmvs_find`** (MV predictor scan) | **30** | 3.0% | serial neighbour scan + dedup + sort | **no** (inherent serial, <noise ceiling, correctness-critical) |
| `read_vartx_tree` | 9 | 0.9% | recursion + CaseSet writes | no (tiny) |
| `reset_context` fills (~18× `.fill()`) | **0.18** | ~0% | per-SB-col context reset | **NOT A BRICK (refuted)** |
| diffuse mode-symbol reads + 74× CaseSet | ~138 | ~14% | asm MSAC + tiny per-block ctx + neighbour writes | no (no single fat block; encoder-glue pattern) |

`refmvs_find` / `scan_row` / `scan_col` are tight ports (dav1d's own FIXMEs remain) — no fat
waste, ~1% ceiling, high revert-risk. `reset_context`'s wall of `.fill()`s measured **0.18 ms**
(the B4 lesson again: small array fills are already fast). The 138 ms remainder is the
decoder's analog of the encoder's diffuse RDO glue: many small asm-MSAC symbol reads + tiny
contexts + `CaseSet` neighbour writes, none individually brickable.

## DECODE PATH — FINAL VERDICT (proven at floor)

Every pure-Rust block in the decode hot path has been measured and evaluated:

| budget | ms/900f | verdict |
|---|---|---|
| coeff decode (`decode_coefs`) | 531 (53%) | serial MSAC floor — no brick (get_lo_ctx ~free, memset ~free) |
| mode-symbol decode + setup | 177 (17%) | diffuse serial — no brick (refmvs_find 3%, reset_context refuted) |
| pixel recon (mc+itx+add) | ~117 (12%) | **asm** — not a pure-Rust brick |
| deblock + cdef | 154 (16%) | **asm** — not a pure-Rust brick |

**There is no fat byte-identical brick in the decode path — measured, not assumed.** Why the
decoder differs from the encoder (which yielded ~10% via B7a+Q2): rav1d inherits dav1d's
hand-written asm for *every* hot kernel (itx/mc/cdef/loopfilter/lr **and the MSAC core**), so
the pure-Rust surface is small and already tight; and decoding is *incremental-serial* (levels
written token-by-token) so there is no batch-parallel context computation to vectorize — the
one structural lever the encoder had. The decoder is a mature, asm-heavy codebase near its
floor. Baseline confirmed byte-identical + no regression: ~875 fps / ~357 Mpx/s 1-thread,
decode md5 `c823cce7…` unchanged.

**Kept "home" for the decoder:** the analyzer spine (`prof.rs` + CLI dump + the complete,
reusable stage/coef/mode instrumentation) and this full ROI map. Any future decode gain must be
**algorithmic** (decode fewer symbols — bitstream) or **asm** (improve an inherited kernel /
add a missing AVX-512 path) — both outside the byte-identical-pure-Rust brick game.

Refuted do-not-retry (decode): levels memset, get_lo_ctx SIMD, dequant vectorize,
reset_context fills, refmvs_find micro-opt (all <noise / inherent-serial / already-tight).

## Still open (not yet profiled)

Profile an **LR + film-grain + 10-bit** clip separately (this 8-bit clip exercises no loop
restoration and no film grain — those asm kernels are untested here, though asm regardless).

## Gate & method (same discipline as the encoder)

- **Byte-identical gate:** decode md5 `c823cce7…` unchanged (`dav1d --muxer md5 -o x.md5
  --threads 1`), plus the lib test suite.
- **Run single-threaded** for the breakdown — frame/tile task threads sum per-worker
  cycles past wall time (the decode is deterministic, so 1-thread loses no fidelity).
- One brick per commit; revert if not faster; stage-median verdict when whole-decode is
  noise-bound. Ledger every attempt (kept or *do-not-retry*) below.

| date | brick | change | stage Δ | md5 gate | verdict |
|---|---|---|---|---|---|
| 07-01 | analyzer spine | prof.rs + scopes + CLI reset/dump | n/a | `c823cce7` ✔ (OFF byte-identical) | **KEPT** — commit e34bedba |
| 07-01 | decode_coefs audit | CoefClass/CoefLevelsFill/CoefDequant scopes + get_lo_ctx differential | token loop 523 ms (44%), dequant 79 ms, get_lo_ctx ~22 ms, memset 3.8 ms | `c823cce7` ✔ | **KEPT (instrument)** — teardown above; **no brick in decode_coefs (serial floor)** |
| 07-01 | levels memset | (hypothesised: skip zeroing for sparse blocks) | measured 3.8 ms / 0.3% | n/a | **NOT A BRICK** (refuted by measurement) |
| 07-01 | get_lo_ctx SIMD | (hypothesised: B7a-analog context stencil) | ~22 ms, ~0.5 ns/call; loop-carried (levels written incrementally) | n/a | **NOT A BRICK** — near-free + un-batchable; the decoder is not the encoder |
| 07-01 | mode-symbol audit | MvRefsFind/VartxTree/ResetCtx scopes | refmvs_find 30 ms, vartx 9 ms, reset_context 0.18 ms | `c823cce7` ✔ | **KEPT (instrument)** — no brick; reset_context refuted |
| 07-01 | **DQ1** | branchless conditional-negate on dequant sign (both AC loops) | **interleaved whole-decode A/B: −0.78% = FLAT** (9 rounds, base 1.0091 vs brick 1.0170, overlapping) | `c823cce7` ✔ | **REVERTED** — LLVM already emits the optimal negate (the F1 lesson); loop is bound by serial cf-read + asm sign-read, not sign application |
| 07-01 | token-loop per-primitive audit | iterated A/B/C/D (all 18 primitives); see table above | — | `c823cce7` ✔ | every primitive at floor; MSAC readers all asm; pure-Rust in the asm shadow |
| 07-01 | **C3** | get_lo_ctx direct-index (drop `\|y,x\| y*stride+x` closure) | **9-round A/B: −0.39% = FLAT** (base 1.0255 vs 1.0295) | `c823cce7` ✔ | **REVERTED** — biggest per-coef pure-Rust prim, in MSAC shadow; LLVM already inlines the closure optimally |
