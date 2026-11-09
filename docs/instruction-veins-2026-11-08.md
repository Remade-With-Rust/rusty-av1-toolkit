# Instruction-count veins — rusty-av1-toolkit (2026-11-08)

Fifteen places where the two codecs execute instructions they do not need, ranked by
expected return, every one gated byte-identical (decoder: `--muxer md5`; encoder: the FNV
bitstream hash). The instrument is **callgrind Ir**, exact to the instruction, which is
what makes the sub-1% items the July wall-clock audits had to call "flat" decidable.

> **Read this before acting on any row.** Ir counts instructions, not time. A block that is
> 26% of Ir can be partly hidden behind a serial range decoder in wall terms, and the July
> audits said exactly that for `decode_coefs`. The discipline (`codec-measurement` §15) is:
> the counter is the verdict for each brick, and the *batch* of bricks carries the clock
> verdict. Nothing below should be reverted because one brick is invisible on a stopwatch,
> and nothing should be declared a wall win until the batch is timed, pinned, ABBA, N >= 20.

## 1. What was measured

| profile | build | content | frames | total Ir |
|---|---|---|---:|---:|
| `dec_asm_vq011` | decoder, x86_64 asm (shipping) | AOM VQ suite 432x240, libaom-encoded | 333 | 448,519,254 |
| `dec_asm_rav1e60` | decoder, asm | our rav1e s6 q100, 640x480 testsrc2 | 60 | 524,013,133 |
| `dec_asm_aom1` | decoder, asm | AOM conformance 320x240, palette-heavy | 3 | 852,863,748 |
| `dec_noasm_vq011` | decoder, **pure Rust** (the wasm32 / no-nasm path) | VQ suite | 333 | 2,083,345,513 |
| `dec_noasm_rav1e60` | decoder, pure Rust | rav1e stream | 60 | 3,765,747,241 |
| `dec_noasm_aom1` | decoder, pure Rust | AOM 320x240 | 3 | 1,495,156,099 |
| `enc_s6_q100_10f` | encoder, asm, `--racecar off` default, shipping release profile | testsrc2 640x480, s6 q100, 1T, 1 tile | 10 | 17,769,134,711 |

Work-parity anchors: decoder md5 identical between asm and pure-Rust builds on every stream
(`98b8c18a74a27a4fc1436f08b73b9270` for the AOM stream); the encoder output is the ledger's
definitive artifact (262,180 B for 60 frames).

The pure-Rust decoder costs **4.6x to 7.2x** the instructions of the asm build on the same
streams. That path is what `wasm32` and every build without `nasm` run, and the crates.io
package advertises it, so its veins are ranked alongside the asm build's.

Instruments (WSL Ubuntu, valgrind 3.26, rustc 1.95 nightly): `~/av1kit` holds the Linux
build (decoder `release-with-debug`, asm in `target/` and pure Rust in `target-noasm/`;
encoder `release`), `~/clips` the streams, `~/ir/ir.sh` runs one callgrind pass, and
`~/ir/cg_census.py <file.cg> [--lines N --calls N --edges N --callee X --caller X]` prints
the self-line, call-count, edge, caller and callee censuses quoted below. Two static
censuses complement it: `objdump_census.py` over the linked binaries (post-LTO guard-branch
count per function) and `guard_census.py` over `--emit asm`. The scripts are in the session
scratchpad `ir/`; copy them into `rusty_av1d/tools/` when the campaign starts. Three
read-only code sweeps (decoder glue, encoder glue, ISA/reachability) supplied the file:line
inventory; their estimates are marked as such wherever a number is not from callgrind.

Static census highlights (post-LTO, shipped binaries): the decoder has 3,252 guard branches
in 444,607 instructions; `decode_b` alone carries 98 (59 bounds checks, 20 `unwrap`, 9
slice-index), `rav1d_recon_b_inter` 34 per bit depth, `decode_coefs` 21, `refmvs_find` 19,
the Rust fallback `inv_dct64_1d` 61. The encoder has only 430 guards in 893,093
instructions: its cost is not bounds checks but iterator adapters, runtime switches, call
overhead and work recomputed per trial.

## 2. Where the instructions go

Decoder, asm build, real content (vq011): asm MSAC readers 46%; pure-Rust `decode_coefs`
glue 26%; everything else 28% (asm pixel kernels ~18%, Rust glue ~10%). On our encoder's
inter stream the Rust glue outside `decode_coefs` is ~22%: lf_mask 5.0%, decode_b +
CaseSet 4.9%, mc glue 2.7%, refmvs 4.0%, recon_b_inter 2.0%, memcpy/memset 2.5%.

Decoder, pure-Rust build: fixed-size scratch `memset` 20-27%; Rust MSAC 17-33%; CDEF
fallback up to 23%; loop filter up to 16%; loop restoration 10%+; MC 11%; `DisjointMut`
range checks 2.5-4%; `iclip` 2-2.5%.

Encoder, default path, speed 6: per-symbol machinery (`symbol_with_update` + `bool` in the
RDO counter) 24%; `write_coeffs_lv_map` body and adapters 9.6%; `quantize` 8%; LRF search
7%; `compute_distortion` 6.2% inclusive; forward transform 4.6%; pure-Rust deblock 4.5%;
cdef_dist 2.6%; ME 2%+.

ISA facts for this box (i7-14650HX, Raptor Lake): both crates run at the **AVX2** rung;
every `avx512icl` kernel in both asm trees is unreachable here. In the decoder the
`cpu.rs:67-68` compile-time flag is spelled `target_feature = "sse3"` for `SSSE3`, so
SSSE3 is forced on, the SSE2 rung is dead code (five SSE2 kernels unreachable), and
`dav1d_set_cpu_flags_mask` cannot lower the decoder below SSSE3. No effect on speed here;
it matters for the ISA-rung A/B and for conformance coverage of the Rust tables, which
`checkasm` does not test.

## 3. The fifteen veins, ranked

Return = share of the relevant build's Ir x fraction a byte-identical brick removes,
weighted toward what users run: decoder asm build first, encoder default path second,
pure-Rust decoder third. Each vein names the skill whose procedure applies.

### V1. Encoder: the per-symbol machinery in the RDO counter
- **Where:** `rusty_av1e/src/ec.rs` `symbol_with_update` (l.628-658), `store` (l.231/280),
  `bool`/`bit`/`literal` (l.551-583), `lr_compute` (l.399-409); `src/context/cdf_context.rs`
  `CDFContextLog::push` (l.653-666); `src/harvest.rs` `OnceLock<bool>` knobs `fastsym`,
  `fastrate`, `frozen`; `Cargo.toml:221-224` `[profile.release] incremental = true`.
- **Measured:** 32.4M calls of `symbol_with_update::<4>` at 98 Ir each (3.18G, 17.9%) plus
  13.6M `bool` at 52 (0.71G, 4.0%). Inside: the three knob reads cost ~455M (2.6%: 98M +
  71M + 27M on the branch lines, 129M in `once_lock.rs`/`futex.rs`, 129M in `atomic.rs`);
  the function prologue/epilogue lines (l.628/658) cost 532M (3.0%) because the writer is a
  real call per symbol; the CDF undo-log push ~300M (l.654-664 and the `cdf_context.rs`
  attribution); `lr_compute`'s `if fl >= 32768` select 103M; renormalise 246M.
- **Why it costs:** three environment knobs are re-read on every symbol of a 192M-symbol
  encode; the writer carries a 16-instruction frame per symbol; the log push re-loads
  `len`/`cap`/`ptr` each time because the raw CDF stores may alias the `Vec` header.
- **Refuted probe (recorded so it is not retried):** the release profile ships with
  `incremental = true` (256 codegen units under ThinLTO), so the non-inlined writer looked
  like a build artifact. Rebuilt with `CARGO_PROFILE_RELEASE_INCREMENTAL=false
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`: output byte-identical, **18,124,666,142 Ir vs
  17,769,134,711 (+2.0%)**, and `symbol_with_update::<4>` still 32,351,681 calls at 98.4 Ir.
  LLVM declines the inline on size regardless of units, and one unit costs 2% elsewhere
  (the `rusty-compiler-leverage` A6 pattern). The lever is therefore an explicit inline
  decision at the writer, not the profile.
- **Bricks:** resolve `fastsym/fastrate/frozen/racecar` once per frame into `bool` fields
  on `WriterBase` (none can change inside a frame) and move the knob bodies to `#[cold]
  #[inline(never)]` helpers (today the frozen arm inlines a second copy of `symbol`+`store`
  into every site); `reserve` the log once per `write_coeffs_lv_map` (at most 1024 x 5 + 16
  entries) and push through a register-held cursor, writing `len` back at the end; apply
  the `fl >= 32768` select only on the `s == 0` arm (needs a `debug_assert` on the CDF
  invariant); then split body from symbol (`rusty-compiler-leverage` A4) and measure per
  call site, because the two instruments will disagree.
- **Expected:** 4-6% of encode Ir. FNV gate.
- **Skill:** `codec-eliminate-redundancy` #1 (runtime switch re-read), `rusty-compiler-leverage` A2-A4, A6.

### V2. Decoder: the pure-Rust token loop around the asm MSAC — `decode_coefs`
- **Where:** `rusty_av1d/src/recon.rs` `decode_coefs` / `decode_coefs_class` (l.751-1077,
  already const-generic over `TX_CLASS` and `BD`), `get_lo_ctx` (l.440-500), dequant loops
  (l.1225-1312), `read_golomb` (l.145), MSAC wrappers `src/msac.rs:533/610/628/660`,
  `src/internal.rs:1058-1073` `TaskContextScratch`.
- **Measured:** 115M Ir self = **25.7%** of the asm build on vq011 (82.4M in recon.rs +
  17.5M attributed to the msac.rs wrappers + 5.6M `core::cmp` + 3.7M `range` + 3.3M
  `uint_macros` + 2.2M `slice/index` + 0.7M `bitdepth`); 15% on the rav1e stream. The asm
  MSAC it wraps is 208M. Per AC coefficient that is ~115 Ir of Rust around ~200 Ir of asm.
  Lines: `get_lo_ctx` neighbour reads and `if mag > 512` 18M (4.0%); scan lookup + `x %=
  32; y %= 32` 8.8M (2.0%); the branchless pack (l.1014-1032) 7.5M; `tok == 3` + hi-token
  context 8.0M; dequant loop incl. `Cf::index` and the release `assert!(dq <= cf_max)`
  (l.1303, 1.17M) 7.8M; golomb 4.6M; Rust-to-asm call wrappers 17M (3.8%, ~4.5 Ir per
  MSAC call).
- **What is already at floor (do not re-litigate):** `get_lo_ctx`'s `levels` window,
  `lo_cdf[lo_ctx]`, `Cf::index` and the `levels[..end]` slice are proven by `InRange` /
  `assert_unchecked`; the `tx_class` matches fold at monomorphization; `%= 32` is what lets
  the neighbour reads elide their checks.
- **Removable, concretely:** the TwoD `scan[i]` bounds check per coefficient (`scan` is a
  `&'static [Scan]`; `let scan = &scan[..=eob as usize];` once after l.880 makes `i`
  provable: ~2 Ir x 40M coefficients on the 900-frame corpus); `assert!(dq <= cf_max)` on
  every nonzero AC and the DC twins at l.1186/1214 (true by table construction at 8/10/12
  bit: `debug_assert!`); the `TaskContextScratch` runtime alignment test per tx block
  (`#[repr(C, align(64))]` folds it); the `levels[..end].fill(0)` libc call for 24-1088 B
  (24,800 calls); `ret as u8 % 4`/`% 16` in the wrappers; the golomb path as a cold set.
  Then a line-level census of the loop body itself (`cg_census.py --lines 60 --only
  recon.rs`) to account for the remaining ~60 Ir per coefficient that no single line owns.
- **Expected:** 2-4% of decoder Ir certain; more only if the body census finds structure.
  Wall is uncertain (July probes read -0.4%/-0.8% on single bricks); judge the batch.
- **Skill:** `codec-eliminate-redundancy` #2/#4, `instruction-counting` §5(d) line census.

### V3. Encoder: `write_coeffs_lv_map` adapters, loop shape, and the shelved B1/B2/B8 fusions
- **Where:** `rusty_av1e/src/context/block_unit.rs` `write_coeffs_lv_map` (l.2163-2600),
  `get_txb_ctx` (l.479-560), `transform_unit.rs` `get_br_ctx` (l.1273-1310), nz-map scatter
  (l.1255-1256), `ctx_area` memset (l.1249).
- **Measured:** body 543M self (3.1%) plus 236M `core::cmp` (min/max per coefficient), 159M
  `num_traits::sign` (`abs` per coefficient), 154M `zip`, 113M `copied`, 117M `non_null`,
  81M slice iter, 58M `arrayvec`: ~0.92G (5.2%) of adapter overhead on top of the body. The
  loop guard `if c != 0 && c + 1 != eob && c > fast_hi` 168M (l.2537, 0.95%); B1 gather
  `coeffs.extend(scan.iter().map(..))` 88M plus its second `abs().sum()` pass; B8 zero-walk in
  sign coding 59M (l.2593-2594); `get_br_ctx` 2.5M calls x 48 Ir = 120M and not `#[inline]`;
  nz-map area-to-scan scatter 116M; `ctx_area` 1 KiB memset per tx block 18M (dead: both
  kernels write every position); `levels` 1,384 B memset 20M.
- **Why it costs:** the ledger shelved B1/B2/B8 as "~0.5% each, sub-noise"; at 0.5-1% each
  they are what Ir resolves. The `fast_hi` test is a constant `usize::MAX` on recorders and
  only matters on counters, yet is evaluated last on every coefficient; the eob coefficient
  and DC are special-cased inside the loop rather than peeled; `coeff_ctx` is an `i8`
  sign-extended into a 42-entry index.
- **Bricks:** index loops over `&scan[..eob]` into fixed `[T; 1024]` buffers (the ArrayVec
  capacity check per element goes); fuse gather + `cul_level` + `levels` fill (B1+B4
  shape); nonzero-only sign loop with DC peeled (B8); single-pass `get_txb_ctx` with a
  padded 4-entry sign table (B2); unswitch on `fast_hi` and peel the eob coefficient;
  `coeff_contexts` as `u8` with the row base hoisted; `#[inline]` `get_br_ctx` and
  specialise it on `tx_class`; drop the `ctx_area` memset (`MaybeUninit`); an `eob == 1`
  fast path (context 0, range context 0, no levels, no kernel: 150-1000 Ir per DC-only
  block, sweep-estimated).
- **Expected:** 3-5% of encode Ir. FNV gate.
- **Skill:** `codec-eliminate-redundancy` #3/#4, `rusty-blazing-fast` §1-2.

### V4. Pure-Rust decoder: fixed-size scratch arrays zeroed per call
- **Where:** `rusty_av1d/src/itx.rs:98` `let mut tmp = [0; 64 * 64];` in `inv_txfm_add`
  (the const-generic sibling at l.307 already sizes `[0; W * H]`); `src/looprestoration.rs`
  `:653/655` `sumsq`/`sum` `[_; 68*390]` in `selfguided_filter` (called twice for mix),
  `:394/400` wiener `tmp`/`hor`, `:806/810/870/871/931-933` sgr `tmp`/`dst`; `src/mc.rs:162,
  287, 420, 526, 226, 347, 482, 584` the 2-D subpel `mid` scratch (34-67 KB); scaled MC
  `put/prep_c_erased::<_, 9>` 33 KB per call.
- **Measured:** `inv_txfm_add -> memset` 410M (10.9%) on the rav1e stream and 354M (17.0%)
  on vq011: 8,210 Ir per call, a 16 KB zero fill per transform block including 4x4s;
  `selfguided_filter -> memset` 414M (11.0%), 79,579 Ir per call (~318 KB), `sgr_mix/3x3/5x5`
  174M more; scaled MC 35M. Together **20-27% of the pure-Rust build**. The `mid` arrays do
  not appear as libc memset calls in the linked binary (verify: the zero fill may be inlined
  stores, and `mc.rs` carries 69.6M of unattributed Ir on the rav1e stream). The asm build
  has none of this: its kernels own their scratch, and dav1d's C leaves all of these
  uninitialised.
- **Bricks:** size `tmp` to the transform (`W*H` const generic as l.307 does, dispatched
  once per tx size), or `MaybeUninit` with a write-before-read proof; move the LR and MC
  scratch into `TaskContextScratch`; stripe scratch sized to the restoration unit.
- **Expected:** 20-30% of wasm32 / no-nasm decoder Ir, byte-identical trivially. The largest
  single win in the repo for those users.
- **Skill:** `codec-eliminate-redundancy` ("a fixed-size stack array is a hidden per-call
  memset"), `rusty-unsafe-optimizations` pattern 2 if `MaybeUninit` is chosen.

### V5. Encoder: work repeated across RDO trials of the same block (count first)
- **Where:** `rusty_av1e/src/rdo.rs:1178-1198` (CFL luma-recon pass), `rdo.rs:1105-1110` +
  `encoder.rs:2994` (`motion_compensate` in both the skip and non-skip trial),
  `rdo.rs:1003-1004` (`select_segment` + `set_segmentation_idx` per candidate),
  `encoder.rs:3186-3193/3348` (`ts.qc.update` for p = 2 identical to p = 1),
  `encoder.rs:1969-1979` (`dequantize` when `eob == 0`), `rdo.rs:2748-2752`
  (`distortion_scale` per 8x8 x plane x cdef index), `rdo.rs:3252-3260` (re-filter of the
  winning CDEF index).
- **Measured so far:** `select_segment` 193,742 calls x 153 Ir = 29.6M plus
  `write_segmentation` 56M (0.5% together); `QuantizationContext::update` 412,801 calls x
  150 Ir = 62M (0.35%, two 64-bit divisions each); `rdo_loop_decision` memsets 2.1M. The two
  largest items are not yet counted: the CFL pass runs a full luma `write_coeffs_lv_map`
  whose rate is discarded and rolled back (`need_recon_pixel` forces it at
  `encoder.rs:1938`), and the duplicate 3-plane motion compensation per inter full trial.
- **Why it is byte-identical:** segmentation is Simple so `sidx` depends only on
  `(tile_bo, bsize)`; the CFL pass's rate is unused and its CDF/context writes are rolled
  back; the dequantized block is unused when the inverse transform is skipped and
  `!needs_tx_dist()`; the MC inputs (mode, MVs, refs) are the same in both trials provided
  nothing writes `ts.rec` between them (prove it; the skip trial returns at
  `encoder.rs:3249`).
- **Bricks:** a "recon-only" flag on `encode_tx_block` that skips coefficient coding;
  compute segment once per `rdo_mode_decision`; cache `qc.update` on its inputs; skip
  `dequantize` for `eob == 0`; precompute a 64-entry bias table per SB; double-buffer the
  CDEF trials. **Each one starts with an atomic counter and one run** (`codec-measurement`
  §15): the removable Ir is calls x per-call cost, and only the count says how many calls.
- **Expected:** 2-5% of encode Ir; the CFL and MC items decide which end.
- **Skill:** `codec-eliminate-redundancy` #3 ("stop recomputing what you already computed"),
  with `codec-six-whys-unknowns`' count-before-time rule.

### V6. Encoder: `compute_distortion` plumbing, `BlockSize` re-derivation, and a knob per 8x8
- **Where:** `rusty_av1e/src/rdo.rs` `compute_distortion` (l.420-460), `distortion_scale`
  l.472-484, `api/config/encoder.rs:222` `temporal_rdo()` (reads `harvest::tpl()` every
  call), `tiling/plane_region.rs:295-320` `subregion`, `partition.rs:193`
  `BlockSize::from_width_and_height(_opt)`, `largest_chroma_tx_size`, `clip_visible_bsize`,
  `asm/x86/dist/cdef_dist.rs` + `activity.rs` wrapper.
- **Measured:** 1.106G inclusive (6.2%) over 210,894 calls = 5,244 Ir per call. Kernels:
  `cdef_dist_kernel` 430M (SSE2 asm `rav1e_cdef_dist_kernel_8x8_sse2` 333M at 175 Ir per
  8x8 + 97M Rust wrapper), weighted-SSE asm ~38M. Glue: `PlaneRegion` construction with two
  `assert!`s, three times per call, 226M (1.3%); `rdo.rs` own arithmetic 169M; `BlockSize`
  from (w,h) 21M; `clip_visible_bsize` 3.8M; memset 6M; `temporal_rdo()` + the
  `distortion_scales` index 70M. Across the encoder `from_width_and_height(_opt)` +
  `largest_chroma_tx_size` + `partition.rs:193` total ~110M (0.6%) over 1.86M + 1.23M calls
  whose callers already hold the `BlockSize`.
- **Bricks:** pass `(ptr, stride)` or one `PlaneRegion` per block to the SSE kernels and
  slice the `distortion_scales` row once per call; hoist `temporal_rdo()` to a `bool` on
  `FrameInvariants` (it is a `OnceLock` plus two config loads per 8x8 today); pass
  `BlockSize` down to `compute_distortion`, `get_fullpel_mv_rd`, `rdo_loop_plane_error`
  and the deblock edge functions; thin the `cdef_dist` wrapper; for the 4x4 chroma
  chunks in `rdo_loop_plane_error`/`rdo_cfl_alpha` call `get_weighted_sse` directly instead
  of the `sse_wxh` setup (do not batch: the final rounding would change bytes).
- **Expected:** 2.5-3% of encode Ir.
- **Skill:** `codec-eliminate-redundancy` (the "producer already knew" redundancy);
  `codec-measurement` (glue is ~45% of this stage).

### V7. Decoder: `decode_b` and the `CaseSet` neighbour-context writes
- **Where:** `rusty_av1d/src/decode.rs` `decode_b` (l.1045-3334, 8,044 instructions, 98
  guard branches post-LTO), `src/ctx.rs:56-73` `small_memset` `match buf.len()` and `:96`
  `index_mut(offset..offset+len)`, `decode_sb` (26 guards, 8 `expect_failed`),
  `decode.rs:3398` `BlockPartition::from_repr(..).expect`, `recon.rs:294`
  `InRange::new(..).unwrap()`.
- **Measured:** `decode_b` self 5.4M + ctx.rs 1.85M + option.rs 0.6M + disjoint_mut 0.56M =
  8.4M (1.9%) on vq011; **25.7M (4.9%)** on the rav1e stream (decode_b 18.0M + ctx.rs
  7.7M). The `match buf.len()` dispatch alone is 1.05% there: each of the ~13 fields per
  direction in a CaseSet closure re-dispatches on a length the block already knows, and
  each carries two compares for `offset + len <= 32`.
- **Bricks:** dispatch once per direction in `CaseSet::one` (`match len { 1 => f(&Setter::<1>),
  2 => .. }`) with const-N array stores for every field, as dav1d's `case_set` does; move
  palette, intrabc, segmentation, warp and delta-lf arms out of the 2,290-line frame into
  `#[cold] #[inline(never)]` helpers (price the set, not one); replace the 20 `unwrap`s and
  8 `expect`s with table-typed values (`InRange`) or proofs.
- **Expected:** 2-3% of decoder Ir on inter content (verify with the guard-branch census
  that LLVM has not already jump-threaded the length matches).
- **Skill:** `codec-eliminate-redundancy` #1/#2/#4; `codec-measurement` 2b/2c for the 98 guards.

### V8. Decoder: the loop-filter mask builder `lf_mask`
- **Where:** `rusty_av1d/src/lf_mask.rs` `rav1d_create_lf_mask_inter`, `mask_edges_chroma`,
  `decomp_tx`, l.553-562 (`level_cache.index_mut((idx.., ..2))` + `u16::mut_from(lvl).unwrap()`
  per 4x4), l.139 `case.set(&mut txa[0][0][y0 + y], MaybeUninit::new(lw))`.
- **Measured:** 13.4M + 5.3M (`cmp`) + 2.9M (`range`) + 4.4M + 0.6M = **26M (5.0%)** on the
  rav1e stream; 4.3M (1.0%) on vq011. Per-4x4 zerocopy conversions 2.2M.
- **Bricks:** write the level word as `[u8; 2]` directly; one `DisjointMut` slice per block
  row instead of per 4x4; span fills for runs of equal levels; bound `decomp_tx` by tx-size
  tables.
- **Expected:** 2-4% on inter content from encoders that deblock per block (ours does).
- **Skill:** `codec-eliminate-redundancy` #3.

### V9. Pure-Rust decoder: the Rust MSAC fallback
- **Where:** `rusty_av1d/src/msac.rs:391-429` `rav1d_msac_decode_symbol_adapt_rust` (a real
  symbol with 130 call sites, not inlined, `n_symbols` runtime), `:300-335` refill and
  `ctx_norm` (byte-at-a-time refill where current dav1d does an 8-byte bswap), live
  `assert!`s at l.327, 344, 361, 397, 398, 409.
- **Measured:** 150 Ir per adapt call vs 59 for the SSE2 asm; 351M (17%) on vq011 and 498M
  (33%) on the palette stream, plus `bool_equi` 62M and `bool` 7.8M. Lines: the linear CDF
  search `v = r * (cdf[val] >> EC_PROB_SHIFT)` 45M; the CDF update loops (l.420/423/429) 64M
  (3.1%); the five release asserts ~2.3%; `allow_update_cdf()` 4.7M.
- **Bricks:** `fn adapt<const N: usize>(cdf: &mut [u16; N+1])` behind the existing
  `adapt4/8/16` wrappers so the search and the two update loops unroll; asserts the array
  type proves become `debug_assert!`; 8-byte refill; branchless update only if the `.s`
  shows LLVM did not already cmov (F1 on the encoder was flat for that reason). The asm
  path is untouched.
- **Expected:** 5-10% of pure-Rust decoder Ir.
- **Skill:** `codec-eliminate-redundancy` #5, `instruction-counting` §6.

### V10. Pure-Rust decoder: per-pixel checked accessors and erased const generics in the fallback kernels
- **Where:** `rusty_av1d/src/mc.rs:117-130` `filter_8tap` (8 `DisjointMut::element_as`
  calls per output pixel), `:393-404` bilinear, `:633/657/683` `tmp[y*w+x]` in avg/w_avg/mask;
  `src/loopfilter.rs:91` `loop_filter` (runtime `wd`, `Cursor` accessor `cursor.rs:95/97`,
  `get_dst` closure l.108); `src/cdef.rs:242/403-418` (`cdef_filter_block_rust` takes `w, h`
  at runtime although the caller is const-generic; `tmp_index` + `constrain` per tap);
  `src/looprestoration.rs:640-760` (`CursorMut` per access, `eight_neighbors` as a call per
  pixel); `src/itx.rs:41-122` (`inv_txfm_add` erases `W, H` and calls the 1-D transforms
  through fn pointers with a runtime stride); `ipred.rs` z3/filter/cfl_ac per-pixel
  `.index_mut::<BD>()`; the per-row `slice_mut::<BD>` zerocopy cast (6-10 Ir per row).
- **Measured:** CDEF 10,114 Ir per 8x8 vs 601 for the AVX2 asm (17x); 486M = **23%** of
  vq011 pure-Rust, `constrain` alone 74M (3.6%). `loop_filter` 1,223 Ir per call, 598M =
  **16%** of the rav1e stream pure-Rust; `Cursor` 148M (3.9%). MC: `put_8tap_rust` 63.8M +
  33.5M `disjoint_mut` + `prep` 36.3M + 19.2M = 11% on the palette stream, with the range
  checks a third of it. `eight_neighbors` 3.9M calls, 176M (4.7%); `iclip` 44-93M; 1-D DCTs
  ~100-150M with 38-61 bounds checks each in the static census.
- **Bricks (instruction-level, no SIMD):** load the taps into a local `[i32; N]` once per
  output row/edge and index it; make `inv_txfm_add`, `cdef_filter_block_rust`,
  `loop_filter` (`WD` in {4,6,8,16}) and `cfl_ac` const-generic so trip counts and strides
  are constants; call the 1-D transforms directly; index the fixed `[i16; 144]` CDEF temp
  with a single `dir < 8` assert; row slices + `chunks_exact` in SGR/avg paths. Scalar oracle
  stays; check the `.s` for packed ops afterwards before any SIMD.
- **Expected:** 15-25% of pure-Rust Ir across content.
- **Skill:** `codec-eliminate-redundancy` #5, then `codec-vectorize-kernel` only if the `.s` says so.

### V11. Decoder: inter reconstruction glue — `recon::mc` and `rav1d_recon_b_inter`
- **Where:** `rusty_av1d/src/recon.rs` `mc` (l.1778-1940; emu-edge decision l.1807-1820),
  `rav1d_recon_b_inter` (3,099 instructions, 34 guards per bit depth), `internal.rs:786-789`
  `bd_fn()` select per block, `ipred_prepare.rs:236/265` left-edge per-pixel gather.
- **Measured:** `mc` 14.3M (2.7%) + `recon_b_inter` 10.4M (2.0%) on the rav1e stream; 3.1M +
  4.1M on vq011; `prepare_intra_edges` 1.5M (0.33%).
- **Bricks:** compute block geometry once per block and pass to the three planes; fixed
  arrays for the 25 bounds checks per bit depth; cold-outline the emu-edge path as a set;
  cache `bd_fn` in `f`; row-slice the left-edge gather.
- **Expected:** 1.5-2.5%.
- **Skill:** `codec-eliminate-redundancy`; `codec-vectorize-kernel` Step 0 law "the entry
  path, not the kernel".

### V12. Encoder: loop-restoration search scratch and the `step_by` AVX2 wrappers
- **Where:** `rusty_av1e/src/lrf.rs:847` `sgrproj_solve` and `sgrproj_stripe_filter` (seven
  `[u32; IMAGE_WIDTH_MAX(+2)]` arrays zeroed per call, the `bit_depth` match at l.871),
  `src/asm/x86/lrf.rs:498/620` `sgrproj_box_f_r1_avx2` `for x in (0..w).step_by(8)`;
  `rdo.rs:2762` `sse_wxh` setup around a single 4x4 chroma SSE.
- **Measured:** `sgrproj_solve` 294M self + `box_f_r1_avx2` 496M inclusive (459k calls,
  1,081 Ir each: 237M `uint_macros`, 104M `step_by`) + `box_ab_r1` 220M + `stripe_filter`
  93M + **175M memset** (1,800 Ir per call, ~7 KB zeroed per unit): ~1.2G (7%) in the
  loop-filter RDO's restoration search.
- **Bricks:** size the scratch to `cdef_w`; `chunks_exact(8)` with a scalar tail instead of
  `step_by(8)` plus per-iteration `min`; hoist the `bit_depth` match and the `s_r1/s_r2`
  table reads; direct `get_weighted_sse` for the 4x4 chroma chunks.
- **Expected:** 1.5-2.5% of encode Ir.
- **Skill:** `codec-eliminate-redundancy`; `codec-vectorize-kernel` ("a mature vectorised
  stage is mostly not its vector loop").

### V13. Decoder: `refmvs_find` and its neighbour scans
- **Where:** `rusty_av1d/src/refmvs.rs` `rav1d_refmvs_find` (1,410 instructions, 19 guards),
  `scan_row`/`scan_col` (l.606-690), `add_spatial_candidate`; slice parameters `mvstack: &mut
  [RefMvsCandidate]` (l.463/492/500/871) and `b: &[usize]` (l.620/676); live asserts at
  l.608, 678, 689-690, 917, 1098, 1155, 1310.
- **Measured:** 10.9M + 4.4M + 3.0M + ~2.9M = **~21M (4.0%)** on the rav1e stream; 5.8M
  (1.3%) on vq011.
- **Bricks:** `&mut [_; 8]` candidate array with `cnt` as a `u8 < 8` invariant; `&[usize;
  37]` for `b`; carry the stride once; `debug_assert!` where the spec guarantees the bound
  (keep `den < 32` for the table index); `#[inline]` the candidate push into the scans.
- **Expected:** 1-2%.
- **Skill:** `codec-eliminate-redundancy` #2/#4.

### V14. Decoder, both builds: `DisjointMut` range checks, per-frame allocation, and filter-stage glue
- **Where:** `rusty_av1d/src/disjoint_mut.rs:732` `if start <= end && end <= len` (every
  `index`/`index_mut`); `src/decode.rs:5134-5138/5200` `f.mvs`/`cur_segmap` collect + init
  per frame; `src/internal.rs:1058-1073` `TaskContextScratch` (align 1, so every
  `mut_from_prefix` tests `ptr & 63`); `src/cdef_apply.rs` `rav1d_cdef_brow`; `loopfilter.rs:65`
  and `cdef.rs:98` fn-pointer wrappers; `pixels.rs:45`; `picture.rs:232` stride read;
  `headers.rs:384` derived `PartialEq` on `Rav1dPixelLayout`; per-SB `ts.context.try_lock()`
  and `c.flush.load(Acquire)` (`decode.rs:4093/4178/4229`).
- **Measured (asm build, rav1e stream):** DisjointMut 6.2M (1.2%); `rav1d_submit_frame ->
  memset` 3.4M + `decode_frame_init -> memset` 1.9M = **1.0%** (479 calls x 7,088 Ir: the
  per-frame mvs/segmap re-allocation that dav1d pools without init); loopfilter wrapper
  4.0M (0.76%); cdef_brow glue 6.5M + 1.3M wrapper against ~19.6M of CDEF asm (glue is ~28%
  of the stage); pixels.rs 1.2M; picture.rs 1.1M; PixelLayout compare 1.8M (0.35%); memcpy
  6.7M. **Pure-Rust build:** DisjointMut 149M (4.0%) and 52M (2.5%).
- **Bricks:** one slice per row/stripe instead of per element; pool `mvs`/`segmap` buffers
  (keep the zero fill); `#[repr(C, align(64))]` on the scratch; pass layout facts as bools
  computed per frame; marshal fn-pointer arguments once per sbrow; hoist the lock to once
  per tile-sbrow.
- **Expected:** 2-3% asm build, 4-7% pure-Rust build.
- **Skill:** `codec-eliminate-redundancy` #1/#2; `codec-memory-copies` §2 (per-frame allocs).

### V15. Encoder: per-candidate and per-coefficient glue in motion estimation and quantize
- **Where:** `rusty_av1e/src/me.rs` `get_fullpel_mv_rd` (1.19M calls, 303 Ir each),
  `me.rs:1403-1406` (`p_ref.region(Area::StartingAt{..})` with a signed `/ 8` per
  candidate), `me.rs:1452-1461` (SAD dispatch + `harvest::mvcost()` OnceLock per candidate),
  `me.rs:705-707` (`frame_me_stats.read()` RwLock per `estimate_motion`), `me.rs:894-904`
  (up to 11 predictors, duplicates searched), `me.rs:969-992/1344-1381` (diamond re-tests
  the previous centre), `asm/x86/dist/mod.rs:291/301` (`from_width_and_height_opt` + table
  lookup per SAD/SATD call); `src/quantize/mod.rs:333-348` (`scan.iter().take(eob).skip(1)`,
  `coeffs[pos]`/`qcoeffs[pos]` bounds per coefficient).
- **Measured:** ME glue ~230M (1.3%) around 63M of SAD (`rav1e_sad16x16_sse2`, 74 Ir per
  call); `BlockSize::from_width_and_height_opt` 26 Ir per candidate (30.8M); `me.rs` self
  99.6M. Quantize 1.13G self (6.4%) at 3,400 Ir per call, of which `take.rs` 146M (0.8%) and
  `int_macros` 88M are adapter overhead; `copysign` 87M; the serial body is already
  branchless (Q2).
- **Bricks:** pass `BlockSize` and a resolved SAD fn pointer into the search; prebuilt
  reference region offset by pointer, `>> 3` on the fullpel path; skip a predictor already
  tested (strict `<` keeps the first occurrence, so identical bytes) and the diamond's
  previous centre (its cost is strictly above the current best); hoist `mvcost()` and the
  RwLock read per tile. Quantize: plain index loop over `&scan[1..eob]`, `pos & 1023` into
  `[T; 1024]` buffers, hoist `log_tx_scale`. The dedup bricks are count-gated (SAD calls
  before/after).
- **Expected:** 2-3% of encode Ir.
- **Skill:** `codec-eliminate-redundancy`, `rusty-blazing-fast` §1.

### Honourable mentions (each a single brick, or a kernel job outside this list)
- **Encoder deblocking is scalar Rust end to end** (`deblock.rs`, 0.8G = 4.5%; `loopfilter*.asm`
  sits in `src/x86` but is not in `build.rs`). Instruction-level: derive `deblock_size` /
  `largest_chroma_tx_size` once per block edge (13M). The other 3% is a `codec-vectorize-kernel`
  job with the scalar twin as oracle.
- **Encoder CDEF 8-bit filters through a per-block 16-bit pad copy** (`cdef.rs:151-182`,
  `pad_into_tmp16`) into rav1e's own AVX2 kernel, while dav1d's `cdef_filter_*` SSE2-AVX512
  kernels are linked and never bound; HBD has no CDEF asm at all. A REACHABILITY-class finding.
- `cdef_dist_kernel_8x8` is SSE2-only at 1.9M calls (`asm/x86/dist/cdef_dist.rs`).
- `sad*_avg` compound SAD asm (all 22 sizes) is compiled and unbound.
- `tempfilter.rs:95` re-runs `CpuFeatureLevel::default()` (15 cpuid checks + `env::var`) per
  block per subpel step when ARNR is on, which it is in the fast tier.
- Decoder `cpu.rs:67-68` `sse3` vs `SSSE3` typo (dead SSE2 rung; mask floor is SSSE3).
- The AVX2 nz-map twin does not honour `RAV1E_CPU_TARGET` (`transform_unit.rs:1192` probes
  `is_x86_feature_detected!` directly), contrary to its comment at l.1175.
- Palette decode (`read_pal_indices`: 1.25% on vq011, 5.9% on screen content; `zip`/`range`
  adapters, `pal_tmp[(i-j)*stride+j]` scatter); forward-transform wrapper bounds (63M) and
  `daala_fdct64` memset (14M); the residual `diff` loop `encoder.rs:1505` (42M); `replay`'s
  `dyn StorageBackend` call per final token (`ec.rs:482`); two `WriterRecorder::new()` per
  superblock (`encoder.rs:5553`); the md5 muxer's per-64-byte memcpy.

## 4. How the fifteen interact with quality

Every vein above is byte-identical, so quality is protected by construction. The quality
return is indirect and real:

- **The RDO counter path (V1, V3, V5, V6, V15) prices every trial.** Cheaper trials let the
  fast tier afford more candidates at the same wall clock, or let the default path keep its
  byte-identical output while closing the speed gap the tier was built to buy. Record the
  Ir per trial before and after; it is the budget the quality skills spend.
- **The loop-filter searches (V12, the CDEF/LRF trials) are where the fast tier's BD-rate
  gates already skip work** (`prom_av1e001`). Lowering the per-trial cost raises the
  break-even for keeping a search on, so the `codec-search-skip-gate` thresholds should be
  re-swept after V12.
- **The tx-domain-rate lever (1.62x, parked because its estimator loses 1 dB)** is the one
  bitstream-changing speed vein in the ledger. V1/V3 shrink the exact rate it approximates,
  which lowers its ceiling: price it again after they land, before investing in the
  estimator retrain (`codec-experimental`).
- **The decoder veins are pure speed**; its output is fixed. Their quality value is the
  conformance side-effect: every new stream decoded for Ir is a conformance test, and the
  reference-encoder VQ suite decodes bit-exact here already. The `cpu.rs` typo and the
  absence of Rust-table `checkasm` coverage are the two gaps that finding exposed.

## 5. Running the campaign

1. Re-baseline every profile in one invocation at the start of a session
   (`instruction-counting` §2: same binary 0, rebuild 6-10, a different shell ~950 Ir).
2. One brick per commit; the commit body carries the Ir delta on at least two profiles
   (decoder: `vq011` and `rav1e60`; encoder: the 10-frame profile and a second clip), the
   guard-branch delta where relevant, and the md5/FNV gate result. Where the brick removes
   calls rather than straight-line code (V5, V15 dedup), the commit carries the call count,
   not the static instruction count, which systematically mis-scores fast paths.
3. Batch the sub-1% bricks behind one switch for the clock verdict (`codec-measurement`
   §15), pinned, ABBA, N >= 20, both arms reporting frame counts; re-run the null arm per
   session.
4. Record refutations with their numbers in the ledgers (`rusty_av1d/docs/decode-bricks.md`,
   `rusty_av1e/docs/entropy-bricks.md`) so they are not re-litigated.
5. For the pure-Rust path, add a `wasm32-unknown-unknown` `cargo check` to the gate for
   anything touching `#[cfg]`-selected kernels, and keep the no-asm md5 run next to the asm
   one: the two builds agreeing is the whole contract.

## 6. Outcome (2026-11-08)

All fifteen veins were mined to five accepted bricks each: 75 bricks, every one byte-identical
on its gates. Decoder gates: md5 of three streams on both builds plus the 792-stream
conformance suite. Encoder gates: six encodes. Each brick is one commit in the measurement
clone with its Ir delta in the message; the working tree carries the same changes,
uncommitted.

| Profile | Start Ir | End Ir | Change | % |
|---|---:|---:|---:|---:|
| Encoder, sh profile | 8,214,847,720 | 6,579,245,625 | -1,635,602,095 | -19.91% |
| Encoder, t2 profile | 6,840,558,040 | 5,359,732,181 | -1,480,825,859 | -21.65% |
| Asm decoder, vq011 | 448,519,254 | 437,075,774 | -11,443,480 | -2.55% |
| Asm decoder, rav1e60 | 524,013,133 | 476,612,080 | -47,401,053 | -9.05% |
| Pure-Rust decoder, vq011 | 2,083,345,513 | 1,451,955,483 | -631,390,030 | -30.31% |
| Pure-Rust decoder, rav1e60 | 3,765,747,241 | 2,159,078,934 | -1,606,668,307 | -42.67% |

Read these with two cautions:

- **Ir is not time.** callgrind counts every iteration of `rep stosb` and `rep movsb`, so the
  bricks that stopped zeroing large scratch buffers per call (V4.1, V4.2, V4.4, V12.1) score
  far higher in Ir than they will on a clock. The batch clock verdict of section 5.3 is
  still owed.
- **Shared-code bricks also moved the pure-Rust build.** The V4, V9 and V10 sums below do not
  add up to its total; the asm-decoder and encoder sums do reconcile with their totals.

### Per vein

Sums of the five bricks, in millions of Ir, on the two profiles each vein was gated on.

| Vein | Build | Profiles | Sum (M Ir) | The five bricks |
|---|---|---|---:|---|
| V1 | encoder | sh / t2 | -536.2 / -514.1 | knobs resolved once per writer; `Writer::bool`/`bit` inlined so flat probabilities fold; always-inline symbol copy at the three coefficient sites; knob-free symbol path chosen per transform block; first symbol stored through its own call site |
| V2 | asm decoder | vq011 / rav1e60 | -4.7 / -2.7 | asm bit as an integer in the Golomb reader and in `decode_bools`/`decode_uniform`; table-bounded dequant asserts debug-only; `TaskContextScratch` aligned to 64; `get_skip_ctx` chroma helper inlined |
| V3 | encoder | sh / t2 | -208.3 / -196.8 | `fast_hi` tested first; `get_br_ctx` always-inline; default instantiation drops the `fast_hi` test; last coefficient peeled off the reverse scan; masked gather index |
| V4 | pure-Rust decoder | vq011 / rav1e60 | -443.6 / -1036.2 | per-thread scratch instead of per-call zeroing for the inverse transform, self-guided box sums, Wiener buffers, self-guided driver buffers and the 2-D subpel mid buffers |
| V5 | encoder | sh / t2 | -187.3 / -113.7 | no dequantize for `eob == 0`; segments selected once per block; the skip trial prediction reused by the next trial; the default transform returned without the search setup; quantizers cached per plane |
| V6 | encoder | sh / t2 | -75.2 / -64.1 | plain loops in `cdef_dist_wxh`; bias offset without building a subregion; `BlockSize` from a log2 table; `cdef_dist_kernel` inlined; size assert debug-only |
| V7 | asm decoder | vq011 / rav1e60 | -2.1 / -4.9 | palette asserts debug-only; `CaseSet` width tag classified once; two-direction loop without spilled iterators; frame header bound once in `decode_b`; block payloads built in place |
| V8 | asm decoder | vq011 / rav1e60 | -0.5 / -11.8 | edge masks accumulated per level for inter, chroma and intra; pre-sliced level iterators; the level cache written through `DisjointMut::write_strided` |
| V9 | pure-Rust decoder | vq011 / rav1e60 | -93.6 / -39.7 | range-coder asserts debug-only; symbol decoder inlined at the AC token site; first hi-token decode inlined with a cold tail; bool decoder inlined at the sign reads; word-at-a-time refill |
| V10 | pure-Rust decoder | vq011 / rav1e60 | -86.4 / -486.1 | self-guided final pass and A/B inversion over row slices; loop-filter width as a const generic; horizontal loop filter over a borrowed window; 8-tap horizontal passes over row slices |
| V11 | asm decoder | vq011 / rav1e60 | -1.4 / -7.1 | `mc` specialized for luma, for constant subsampling, and for paired chroma planes; mask layout indices derived where used; intrabc identified by the reference |
| V12 | encoder | sh / t2 | -485.0 / -474.2 | thread-local sgrproj scratch; plain loops instead of `step_by(8)`; AVX2-compiled twins of the solve accumulator and the stripe apply loop; integral-image rows walked as three runs |
| V13 | asm decoder | vq011 / rav1e60 | -1.8 / -6.4 | `add_spatial_candidate`, `scan_row`/`scan_col`, the `get_gmv_2d` fast cases and the extended-candidate helpers inlined; an inline stable insertion sort |
| V14 | asm decoder | vq011 / rav1e60 | -0.9 / -14.5 | deblock calls with all-zero masks skipped; `backup2x8` gathered locally, unrolled on a constant row count, and read with one bounds check; tile context locked once per superblock row |
| V15 | encoder | sh / t2 | -143.5 / -117.9 | AC scan as a plain slice; AVX2-compiled eob search; one shift in `divu_pair`; `get_fullpel_mv_rd` inlined; masked scan positions fold the bounds checks |

### What the campaign taught

- **Loop shape and memory traffic beat arithmetic.** The largest bricks removed a runtime-step
  `step_by` (V12.2, -238M on sh), per-call zeroing of large scratch (V4, -1.04G on the
  pure-Rust rav1e60 profile), and checked per-element accessors (V10).
- **Inline small helpers, not big bodies.** `#[inline(always)]` on small helpers with many
  calls won repeatedly (V3.2, V13). A plain `#[inline]` was often declined. Forcing inlining
  into large functions re-rolled their register allocation and lost (V9.3a, V9.5a).
- **Const-generic specialization of glue won** (V7.3, V10.3, V11.1 to V11.3).
- **AVX2-compiled twins of the same safe Rust won** where baseline SSE2 lacks widening or
  32-bit multiplies (V12.3, V12.4, V15.2).
- **Reuse needs proof.** V5.3 still runs motion compensation in debug builds and asserts that
  the reused prediction matches; V12.1 was validated by poisoning the reused scratch in a
  release build and re-running the gates.
- **31 refutations** are recorded with their numbers in the harness notes, so they need not
  be re-tried.

### Verification of the end state

- Encoder: six encodes byte-identical. Decoder: md5 on both builds, and 792/792 conformance
  on both builds.
- `cargo check --target wasm32-unknown-unknown` is clean for both libraries (decoder with the
  pure-Rust features, encoder without default features).
- Decoder workspace tests pass.
- Encoder library tests: 538 passed and 17 failed at the end of the campaign, the same 17 as
  at the campaign base (b573de0). All 17 stopped at the `debug_assert!` in
  `write_tx_size_inter` (`rusty_av1e/src/context/transform_unit.rs:734`), which reads the mode
  of the block itself, and `prom_av1e024` (2fcb598) had stopped writing that mode on counting
  writers. With only that assertion disabled in a throwaway copy, all 75 API tests passed in a
  debug build and the V5.3 reuse check held across more than 28,000 reuses. Fixed afterwards
  (below): 555/555.

### The clock verdict (2026-11-09)

The whole batch, timed once on its release binaries: the campaign base (b573de0) against the
final tree, both built in release with the CLI defaults and rusty_alloc 2.4.0 in both arms
(dependency parity checked in the build logs). Each run is one process pinned to one core
(`taskset -c 6`, `nice -n -10`); the arms alternate with the leading arm alternating, after two
untimed warm-up runs each; wall time comes from a nanosecond clock, and CPU/wall stays
between 0.97 and 1.00 in every reported row. Before any timing, both arms produced identical output on every
workload: the same IVF bytes from the encoder, and the same md5 and frame count from the
decoder. Short decoder streams are looped (`loop_ivf.py`) so a run lasts seconds.

| Comparison | n | Best of n, before -> after | Speedup, best | Paired median | Wins | z |
|---|---:|---|---:|---:|---:|---:|
| null: encoder against itself | 20 | 1.332 -> 1.319 s | 1.010x | 1.015x | 13/20 | +1.34 |
| encoder, shields 10 frames, speed 6 | 30 | 1.514 -> 1.288 s | **1.176x** | **1.161x** | 30/30 | +5.48 |
| encoder, testsrc2 30 frames, speed 6 | 20 | 4.012 -> 3.707 s | **1.083x** | **1.139x** | 20/20 | +4.47 |
| encoder, shields 10 frames, speed 2 | 20 | 7.004 -> 6.371 s | **1.099x** | **1.116x** | 20/20 | +4.47 |
| null: asm decoder against itself, rav1e60 | 20 | 2.571 -> 2.577 s | 0.998x | 1.005x | 12/20 | +0.89 |
| null: asm decoder against itself, vq011 | 30 | 2.610 -> 2.597 s | 1.005x | 1.011x | 16/30 | +0.37 |
| asm decoder, rav1e60 x50 | 20 | 2.677 -> 2.578 s | **1.038x** | **1.044x** | 18/20 | +3.58 |
| asm decoder, aom1 x30 | 30 | 2.510 -> 2.425 s | **1.035x** | **1.026x** | 25/30 | +3.65 |
| asm decoder, vq011 x50 | 30 | 2.749 -> 2.755 s | 0.998x | 1.021x | 18/30 | +1.10 |
| pure-Rust decoder, vq011 x50 | 20 | 11.291 -> 10.535 s | **1.072x** | **1.080x** | 16/20 | +2.68 |
| pure-Rust decoder, rav1e60 x50 | 20 | 9.701 -> 7.855 s | **1.235x** | **1.232x** | 18/20 | +3.58 |
| pure-Rust decoder, aom1 x30 | 20 | 3.831 -> 3.263 s | **1.174x** | **1.159x** | 20/20 | +4.47 |
| rusty_alloc 2.2.6 -> 2.4.0, encoder | 20 | 1.263 -> 1.258 s | 1.004x | 1.001x | 11/20 | +0.45 |
| rusty_alloc 2.2.6 -> 2.4.0, asm decoder | 20 | 2.338 -> 2.360 s | 0.991x | 0.997x | 9/20 | -0.45 |
| rusty_alloc 2.2.6 -> 2.4.0, pure-Rust decoder | 30 | 3.176 -> 3.196 s | 0.994x | 0.997x | 12/30 | -1.10 |

The asm-decoder vq011 and aom1 rows and the pure-Rust allocator row are re-runs at n = 30 on a
quiet machine: during the first pass the WSL load average reached about 14 from a source that
was gone by the time it was checked, which left those three unresolved (|z| < 2). The asm-decoder vq011 effect stays inside
the null floor, so it is reported as within noise. The rusty_alloc bump is neutral on all three
workloads.

Read against the instruction counts, the shape is the one the campaign predicts. The encoder,
where the work removed was compute, gains 8% to 18% of wall for 20% to 22% of instructions. The
pure-Rust decoder gains less in wall (7% to 24%) than in instructions (30% to 43%) because its
largest bricks (V4) removed per-call buffer zeroing, which callgrind counts per byte. The asm
decoder moves least, as predicted, and most on the stream with the most loop-filter work
(rav1e60).

### Resolved after the campaign

- The `prom_av1e024` assertion: it now applies only on writers that keep the block context
  (`W::COUNTS_ONLY || ...`); the caller already asserts the coded mode is inter on every writer.
  Debug `cargo test --lib`: 555/555. Release: byte-identical, Ir null.
- rusty_alloc-api 2.2 -> 2.4 (rusty_alloc 2.4.0) for both CLIs: byte-identical, timed neutral.
- `rusty_av1d/src/scratch.rs`, which V4.2 introduced, had stayed untracked because the accept
  step staged with `git add -u`, so no commit from V4.2 on built from a clean checkout. The
  history was replayed with the file present from V4.2 on, and V4.1 to V4.5 were build-checked
  on both feature sets.
- Released as rusty_av1e 0.8.1 and rusty_av1d 1.3.0.
