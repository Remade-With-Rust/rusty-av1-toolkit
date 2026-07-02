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

## Next steps (the brick hunt)

1. **Drill into `decode_coefs`** — add info sub-scopes splitting (a) the asm symbol reads
   from (b) the pure-Rust context derivation (nz-map/base/br) and (c) dequant + cf writes.
   That isolates the addressable pure-Rust ms inside the 531 ms.
2. **Hunt the B7a analog** — the coefficient context computation (`get_nz_map` / base /
   br contexts). If it is scattered/scalar like the encoder's was, restructure to
   column-contiguous + SIMD, gated on the decode md5.
3. **mode-symbol decode (170 ms)** — second target; per-block partition/mode/MV symbol
   reads in `decode_b`/`decode_sb` (pure Rust around asm MSAC).
4. Profile an **LR + film-grain** clip separately (this clip exercises neither).

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
