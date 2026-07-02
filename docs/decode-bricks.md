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

## Next target — mode-symbol decode (~17%)

With `decode_coefs` proven at floor, the remaining pure-Rust ROI is **mode-symbol decode +
setup (~170-198 ms, ~17% of decode)** — per-block partition/mode/MV/segment symbol reads in
`decode_b`/`decode_sb` (pure Rust around asm MSAC). Its context derivation is *per-block* (not
per-coef), so unlike `get_lo_ctx` there may be batchable neighbour-context computation worth a
teardown. Also open: profile an **LR + film-grain** clip (this clip exercises neither).

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
