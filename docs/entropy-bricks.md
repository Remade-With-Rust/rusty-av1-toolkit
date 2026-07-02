# The entropy house — brick ledger for the coeff rate/entropy path

**Why this path:** the analyzer measured `write_coeffs_lv_map` at **48.6% of total
encode** (3671 ms / 7546 ms RDO, 1,376,974 calls, speed 6, 640×480×10f, 1 thread),
all pure Rust. Quantize (12.3%, same 1.38M call count) is its upstream sibling and
shares data with it. This ledger maps **every brick** — each primitive/function on
the path — so we can optimize one at a time under the `optimize-codec` discipline:
*one brick per commit, byte-identical gate (bitstream MD5 + `bench_encode` A/B),
revert if flat.*

Average cost: 3671 ms / 1.38M calls ≈ **2.66 µs per tx-block call**.

Verification gate for every brick: `stage_breakdown` (per-stage median — the honest
kernel verdict) + `bench_encode` best-of-N + **byte-identical output** (same encoded
bytes, e.g. hash the packet stream in the harness) + `cargo test`.

---

## Floor plan (the call tree)

```
encode_tx_block (encoder.rs:1556-1586)
├─ quantize()                    [12.3%, upstream sibling — brick Q1..Q3]
└─ write_coeffs_lv_map (block_unit.rs:1783)   [48.6% — the house]
   ├─ B1  scan-order gather + cul_level sum      (per-coeff ×2 passes)
   ├─ B2  get_txb_ctx (block_unit.rs:442)        (per-call, 2-pass neighbour folds)
   ├─ B3  txb_skip symbol                        (per-call, 1 symbol)
   │      └─ eob==0 → set_coeff_context, return  (the skip fast path)
   ├─ B4  levels_buf zero + txb_init_levels (transform_unit.rs:780)
   │                                             (per-call memset + full-AREA fill)
   ├─ B5  write_tx_type (transform_unit.rs:530)  (per-luma-block, 1 symbol)
   ├─ B6  encode_eob (block_unit.rs:1862)        (per-call: eob_pt token + extra bits)
   │      └─ get_eob_pos_token (transform_unit.rs:808)  (table lookups)
   ├─ B7  encode_coeffs (block_unit.rs:1917)
   │      ├─ B7a get_nz_map_contexts (transform_unit.rs:911)   (per-COEFF stencil)
   │      │      └─ get_nz_map_ctx → get_nz_mag (5 neighbour reads + mins)
   │      │                        → get_nz_map_ctx_from_stats (3-D table)
   │      ├─ B7b reverse-scan base-level loop    (per-COEFF: 1×4-ary symbol)
   │      └─ B7c BR loop for level>2             (per big coeff: get_br_ctx + ≤4 symbols)
   ├─ B8  encode_coeff_signs (block_unit.rs:1984) (per-nonzero: DC symbol / raw bit
   │                                              + golomb for level>14)
   └─ B9  set_coeff_context                       (per-call neighbour-array write)

every `symbol_with_update!` above bottoms out in:
   └─ F*  ec.rs Writer machinery                  [FOUNDATION — see §Foundation]
          WriterCounter rate path · CDF adapt · fc_log append (rollback support)
```

---

## The bricks (numbered, with observed redundancy)

### B1 — scan gather + cul_level (block_unit.rs:1802-1807)
`coeffs.extend(scan.iter().map(|&i| coeffs_in[i]))` then a **second pass**
`coeffs.iter().map(abs).sum()`. Two passes over the same data; the gather itself
re-derives what `quantize` just produced while walking the same scan order.
- **Lever (cheap):** fuse the sum into the gather (single pass).
- **Lever (structural, later):** quantize iterates scan positions already — it could
  emit scan-ordered levels/cul_level as by-products (cross-brick fusion with Q*).
- Class: eliminate-redundancy. Risk: low → medium (fusion).

### B2 — get_txb_ctx (block_unit.rs:442)
Derives `txb_skip_ctx` + `dc_sign_ctx` from above/left context rows. Reads the SAME
two slices **twice** (dc_sign fold, then OR folds). `dc_sign_ctx` is only consumed
when a nonzero DC gets its sign coded (B8), yet computed on every call including
eob==0 skips.
- **Lever:** single fused pass; and/or compute `dc_sign_ctx` lazily.
- Class: eliminate-redundancy. Risk: low.

### B3 — txb_skip symbol + the eob==0 fast path
One adaptive symbol; the skip path is already short. Healthy brick — audit only
after B2 (its ctx feed) is tightened.

### B4 — levels_buf + txb_init_levels (block_unit.rs:1831-1835, transform_unit.rs:780)
`[0u8; TX_PAD_2D]` zeroed per call, then |coeff|.min(127) filled for the **full
coded area** (height × width) — even when eob is tiny (common at high QP). Work is
area-proportional, not eob-proportional. Only the stencil neighbourhood of coded
positions is ever read (B7a/B7c read levels at scan positions + padded neighbours).
- **Lever:** zero/fill only rows up to the highest coded row (derivable from eob and
  scan geometry), or fill from the scan-gathered coeffs (eob-proportional).
- Class: eliminate-redundancy. Risk: medium (padding reads must stay valid — the
  stencil reads +1/+2 beyond coded positions; keep the pad rows zeroed).

### B5 — write_tx_type (transform_unit.rs:530)
Per-luma-block symbol from inter/intra tx-set CDFs. Small. Audit later.

### B6 — encode_eob + get_eob_pos_token
Table lookups + one multi-symbol + offset bits. Small per call, once per block.
The 7-way `match` on tx-area picking `eob_flag_cdf{16..1024}` is branchy but cold.

### B7a — get_nz_map_contexts (transform_unit.rs:911) — **the per-coefficient stencil**
For each of the eob coefficients: padded-index math, `get_nz_mag` (5 neighbour
reads + 5 min(3,·) + adds), `get_nz_map_ctx_from_stats` (shift/min + 3-D table).
This is the scalar workhorse; **libaom SIMDs exactly this**
(`av1_get_nz_map_contexts_sse2/avx2`) — strong precedent that the stencil
vectorizes (process a column of positions at once; the levels layout is already
transposed/padded for it).
- **Lever 1 (eliminate-redundancy):** hoist per-call invariants (bhl, area, tx_class
  branches) out of the per-coeff closure — check codegen first.
- **Lever 2 (vectorize-kernel):** SIMD the stencil à la libaom, scalar twin as oracle.
- Class: both, in order. Risk: low / medium.

### B7b — the reverse-scan base-level loop (block_unit.rs:1939-1960)
Per coefficient: min(level,3) → one 4-ary `symbol_with_update!`. The loop itself is
lean; its cost is ~all in the Foundation (per-symbol CDF walk + adapt + log).
Sequential by construction (context adaptation) — **not** SIMD-able. Optimize via F*.

### B7c — the BR loop (block_unit.rs:1962-1979)
For each coeff with level>2: `get_br_ctx` (3 neighbour reads) + up to 4 4-ary
symbols. Data-dependent; bounded. Cost lives in F*.

### B8 — encode_coeff_signs (block_unit.rs:1984)
Walks ALL scan-ordered coeffs (including zeros — `continue` per zero) to code signs
of the nonzero ones; golomb (raw bits) for level>14.
- **Lever:** iterate only nonzeros (they're knowable from the gather in B1);
  micro — measure first.
- Class: eliminate-redundancy. Risk: low.

### B9 — set_coeff_context
Neighbour-array writes (cul_level | dc_sign). Tiny. Leave.

---

## Foundation — the per-symbol machinery (ec.rs)  ⟨AUDITED 2026-07-01⟩

**What one `symbol_with_update!` costs (verified in source, all writers incl. the
RDO `WriterCounter`):**
1. `fc.offset(cdf)` — offset math (free).
2. `CDFContextLog::push` (cdf_context.rs:598) — raw-ptr copy of the CDF row
   (8 B small / 32 B large partition) + offset tag + deferred-capacity bookkeeping.
   Already unsafe-optimized upstream.
3. `symbol → store` — Counter (ec.rs:195): `lr_compute` (2 mults) + CLZ + add.
4. `update_cdf` (ec.rs:935) — const-generic ≤15-iteration shift-add adapt loop.

**Measured (info-scope audit, 640×480×10f speed 6):**
- **192,251,661 calls** ≈ 140 symbols per `write_coeffs_lv_map` call.
- Overhead identity confirmed: EntropyRate 3671→8071 ms with the scope on
  ≈ 192M × ~23 ns — the audit instrument itself. True pool ≈ **2.0-2.9 s of the
  3671 ms EntropyRate stage (majority)** at **~13-19 ns/symbol**.
- `tell_frac`: 1.3M calls, **11.7 ms** — negligible. Cost-table hypothesis: dead.

**Verdict:** the foundation is per-call LEAN. Micro-bricks (branchless update_cdf,
cheaper push) target ≤20% of the pool ≈ ≤5% total encode — attempt ONE brick (F1),
measure honestly, revert if flat. The real levers are the B-bricks above it (fewer
per-call passes) and Q-bricks (12.3% pool). Symbol COUNT is decision-determined —
cutting it byte-identically is not possible; count reduction = `experimental` land.

---

## Upstream sibling — quantize (quantize/mod.rs:269) [12.3%]

### Q1 — full-area EOB scan (lines 293-306)
`iscan.iter().zip(coeffs).map(select).max()` over the ENTIRE tx area to find eob.
Vectorizable select+max; verify codegen, consider early structure.

### Q-split MEASURED 2026-07-01 (info scopes, s6 640×480×10f, post-B7a)
quantize = **16.5% of RDO** (grew from 12.3% as B7a shrank the RDO denominator):
- **Q1 eob-scan = 0.9%** (42-45 ms) — raster-order independent masked-max, already
  efficient (auto-vec-shaped). **SKIP — not a brick** regardless of codegen.
- **Q2 main loop = ~14% of RDO ≈ ~11% encode** (676-730 ms) — **the whole prize.**
- DC + tail = ~1.5% (negligible).
Q2 is provably NOT auto-vectorizable: loop-carried `level_mode` + scan-order
gather/scatter. The two-pass restructure below is the byte-identical lever.

### Q2 — the main quant loop's serial `level_mode` (lines 318-340)
`level_mode` feeds each iteration from the previous → **blocks auto-vectorization**
(the classic pattern from the playbook). Byte-identical restructure candidates:
compute `level0 = divu_pair(...)` for all positions vectorized (pass 1), then run
the cheap serial mode/rounding fix-up over level0 (pass 2).
- Class: eliminate-redundancy (unblock autovec) → vectorize-kernel if needed.

### Q3 — tail zeroing (line 342+)
Not yet read in full; audit when Q1/Q2 land.

---

## Build order (the scaffolding plan)

Sequenced by (expected win × certainty) / risk, per the playbook — biggest,
safest, most-informative first; each independently revertible:

| # | Brick | Class | Status / Expected |
|---|---|---|---|
| 1 | **F-audit** | analyzer | ✅ 192M symbols ~13-19ns; lean per-call (see Foundation §) |
| 2 | **B4** eob-proportional levels init | elim-redundancy | ❌ REVERTED flat — fill already autovec'd |
| 3 | **F1** update_cdf split-loop | elim-redundancy | ❌ REVERTED flat — already cmov |
| 4 | **B7a** full-area ctx kernel | prep+autovec | ✅ **KEPT: −64% stage, −8.3% whole encode** |
| — | — remaining queue — | | |
| 5 | ~~**F2** fc_log dedup~~ | elim-redundancy | ❌ **REVERTED — regression** (see ledger). 88.9% skippable but the tag-table load costs more than the streaming copy it saves. Closed. |
| 6 | ~~**B7a-SIMD**~~ | vectorize-kernel | ✅ **KEPT: stencil 320→97 ms (−70%), −3.5-4% whole encode** — asm inspection showed the "autovec" never happened; one ymm covers a whole column |
| 7 | ~~**Q2** quantize~~ | elim-redundancy | ✅ **KEPT (branchless, −35% stage): commit 560e8e52.** Two-pass variant reverted (regression). |
| 8 | B1/B2/B8 micro-fusions | elim-redundancy | ~0.5% each — likely sub-noise (B4 lesson) |
| 9 | NEON mirror of B7a-SIMD (aarch64) | vectorize-kernel | when an ARM target matters; same oracle |

**Not on the table (bitstream-changing, → `experimental` skill):** frozen-CDF rate
estimation, tx-domain rate (`use_tx_domain_rate`), candidate pruning. Those change
decisions; this house is built byte-identical.

---

## Ledger (results — append per brick, measured, before/after)

Baseline (640×480×10f, speed 6, QP 100, 1 thread, profile build):
**bitstream FNV = `688d5eeaee94d95e`** · EntropyRate ≈ 3671 ms · quantize ≈ 898-931 ms.
Honest throughput baseline (bench_encode, all threads, 20f): 3.44 Mpx/s best-of-3.

| date | brick | change | stage Δ (median) | bench Δ | gate | verdict |
|---|---|---|---|---|---|---|
| 07-01 | F-audit | info scope on symbol_with_update + tell_frac (removed after) | n/a | n/a | hash `688d…95e` ✔ | 192M symbols ~13-19ns; tell_frac dead |
| 07-01 | B4 | eob-proportional levels scatter instead of full-area fill | share 50.5%→50.5% FLAT | n/a | hash ✔ | **REVERTED** — old fill already autovec'd; per-call fixed cost (~15ns) is noise vs 2.5µs call. Small-array data movement ≠ redundancy (3rd codec confirming) |
| 07-01 | F1 | update_cdf split-loop (branch-free at monotonic `i>=val`) | share 50.5%→49.7%, <noise | n/a | hash ✔ | **REVERTED** — LLVM already emits cmov; ≤1% unresolvable on ±10% thermal box |
| 07-01 | B7a-audit | info scope on get_nz_map_contexts | n/a | n/a | hash ✔ | stencil pool = **864-892 ms = 14.2% of RDO** (1.24M calls, ~700ns ea) |
| 07-01 | **B7a** | full-area column-contiguous ctx kernel, safe-Rust autovec (the TX_PAD layout was designed for this SIMD pattern) | **878→449 ms (−49%), non-overlapping** | — | hash ✔ · oracle ✔ (19 sizes × 3 classes, every raster pos) | **KEPT** — commit 434d9202 |
| 07-01 | **B7a-tune** | drop the density cutoff — kernel wins at every density (sweep K=8/16/64/∞: 449/338/311/320 ms) | stencil **878→320 ms (−64%)** | **553.6→507.8 ms/frame = −8.3% whole encode** (interleaved 1T A/B, non-overlapping) | hash ✔ · 547/547 lib tests ✔ | **KEPT** — commit df2ab0e6 |
| 07-01 | F2-verify | workflow: call-site map (5 checkpoint sites, strict LIFO proven; double-rollback dominant; clear per-SB; no out-of-band CDF writes) + adversarial review (killed shared-base & no-invalidation variants; prescribed monotone-clock: tags=log-event counter, base:=clock at checkpoint/rollback/clear, skip iff tag>base) | n/a | n/a | n/a | scheme provably safe as prescribed |
| 07-01 | F2-probe | monotone-semantics ceiling probe (profile-gated counters) | n/a | n/a | hash ✔ | **88.9% of 192M pushes dedup-skippable** (deterministic 170,826,849 across runs); naive probe said 98.9% (stale-tag overcount, as review predicted) |
| 07-01 | **F2** | monotone-clock fc_log dedup (22 KB tag table, skip path in push) | CtxSaveRestore 94-102→33.5-34.6 ms (−65%); EntropyRate muddied by probe | **REGRESSION: A 483-489 vs B 519-532 ms/frame (~+6-7%), 2 interleaved rounds consistent** | hash ✔ all runs · 547/547 ✔ · skip counts deterministic | **REVERTED** — the copy was never the cost: the old push is a streaming L1 copy (~free); the dedup adds a dependent RANDOM tag-table load + branch to ALL 192M pushes ≈ 2ns×192M ≈ the 400ms lost. Correctness held; economics didn't. Do-not-retry with any side-table variant; only a same-cache-line tag could work, and there is no spare space in coded rows |
| 07-01 | **B7a-SIMD** | hand-AVX2 kernel: one ymm per ≤32-row column (5× vpminub/vpaddb stencil, vpavgb ≡ (mag+1)>>1, per-tx offset vectors in a lazy static); cached CpuFeatureLevel dispatch + hard release-mode bounds guard | stencil **320→96-97 ms (−70%; −89% cumulative vs 878 pre-B7a)**, ±1 ms across runs | **~−3.5-4% whole encode** (457-483 vs 474-499 ms/frame, 2 interleaved rounds consistent) | hash ✔ · oracle scalar==AVX2==dispatch every position ✔ · 547/547 ✔ · adversarial bounds review: SOUND ×9 attacks (2 fixes applied) | **KEPT** — commit b7bb106b. Asm inspection first: the scalar kernel had ZERO SIMD — "the compiler already vectorized it" was FALSE here; Step-0 inspection beats assumption |
| 07-01 | Q-audit | info scopes split quantize | n/a | n/a | hash ✔ | Q1 eob-scan 0.9% (skip); **Q2 main loop 14% of RDO = the prize**; provably non-vectorizable (loop-carried level_mode + scan gather) |
| 07-01 | Q2-twopass | hoist divu_pair + both bias candidates to a raster vectorizable pass; serial pick pass | main loop 676-730 → **800-818 (+15% REGRESSION)** | n/a | hash ✔ · 547/547 ✔ | **REVERTED** — the division was NOT the bottleneck; area-vs-eob waste + 12KB scratch traffic cost more than any vectorization |
| 07-01 | **Q2-branchless** | offset-select + level_mode-update rewritten branchless (byte-identical case analysis) — kills the two data-dependent mispredicting branches in the serial loop | main loop **676-730 → 440-466 ms (−35%)**, non-overlapping (baseline min 676 > branchless max 496); quantize 16.5%→~11% of RDO | within noise (~4% of encode < ~5% box) — per-stage median is the verdict | hash ✔ · 547/547 ✔ | **KEPT** — commit 560e8e52. The bottleneck was BRANCHES, not the division (twopass proved it); LLVM did NOT auto-cmov here (unlike F1) |
| 07-01 | **GLUE decomposition** | info scopes on the 24.7% RDO-glue residue | loop-filter RDO search **8.3%** (biggest named); compute_distortion 2.6%; compute_rd_cost 0.1%; CtxSaveRestore ~1.4% (already a child); ~16% still diffuse | n/a | hash ✔ | commit f70b7f63. An agent's code-size survey proposed checkpoint-caching (est 20-30% of glue) + distortion-scale (10-15%) as the top bricks — BOTH REFUTED by the scopes (1.4% / 2.6%). rav1e's glue is TIGHT (stack ArrayVecs, cheap checkpoints) — the h264 glue-win patterns (frame clones, unused-feature work) don't exist here |
| 07-01 | LF-alloc | scope rec_subset.scratch_copy + cdef_work.clone + lrf_work Plane::new×3 (800 calls) | setup+alloc = **2.9-4.0 ms = 0.1%** of the 383 ms loop-filter RDO | n/a | hash ✔ | **NOT A BRICK** — allocs are a red herring; the 8.3% is inherent CDEF/LRF search |
| 07-01 | LF-rate-hoist | cache the LRF `count_lrf_switchable` rate (constant across cdef_index, doesn't affect argmin) instead of re-summing 1<<cdef_bits× per SB | loop-filter stage FLAT: A 339/341/387 vs B 352/345/330 (overlapping) | n/a | hash ✔ · 547/547 ✔ | **REVERTED** — real redundancy (removed 7/8 of the calls) but count_lrf_switchable is below the ~5% noise floor. Loop-filter RDO = inherent search; no byte-identical brick. Real lever is algorithmic (fewer trials via speed preset = BD-rate, out of scope) |

## Definitive whole-encode result (2026-07-01)

The one measurement that gives a single defensible number: the real `rav1e` CLI binary
at **opt-entropy HEAD (885a2887)** vs **stock rav1e (564ae3b0** = merge-base with
xiph/master = local `master`), built with identical flags, encoding the same clip,
interleaved to cancel thermal drift.

- Clip: 640×480 × 60f `testsrc2` (deterministic). Args: `--speed 6 --quantizer 100
  --tiles 1 --threads 1` (single-thread single-tile isolates the serial per-block path
  the bricks optimize; lowest noise).
- 7 interleaved rounds (stock, opt), median: **stock 7.588 s → opt 6.911 s = 1.098× =
  ~9.8% faster** (opt takes 8.9% less wall-clock). Tight distribution 1.072–1.117×.
- **Output BYTE-IDENTICAL**: both SHA256 `e6c294bb…6ce2d`, 262 180 B. Pure speed win —
  same bitstream, size, quality — and an independent re-validation that B7a / B7a-SIMD /
  Q2 stay byte-identical in the *default CLI config*, not just the profiling harness.

**Honest reconciliation:** the earlier "~16%" was a *sum of per-stage / per-brick
deltas on the synthetic harness clip*. The true **whole-encode** figure on a real CLI
encode is **~10%** — the sum dilutes into one number because whole-encode also runs ME,
transforms, prediction and I/O that the bricks never touch, and `testsrc2` has a
different coefficient density than the harness clip. Content-dependent: denser /
higher-bitrate content puts more of the encode in the entropy path → larger speedup.
Repro: worktree `rusty_av1e_stock` @564ae3b0 + `scratchpad/ab.sh`.

## The racecar switch (2026-07-02)

`--racecar <on|off>` (default **on**) on the CLI: one binary, both worlds.
`on` = the kept bricks (B7a area kernel + AVX2 twin, Q2 branchless); `off` =
the original stock rav1e code paths, resurrected verbatim from history (nz-map
per-scan-position stencil from `434d9202~1`, branchy quantize loop from
`560e8e52~1`) behind `racecar::on()` (`src/racecar.rs` — OnceLock latch:
CLI flag > `RAV1E_RACECAR` env (`0`/`off` disables) > default on). **Not a
brick** — single-binary A/B / demo infrastructure; the racecar path is
bit-for-bit the pre-switch code, and the switch itself costs one latched
atomic load per `get_nz_map_contexts`/`quantize` call (sub-noise).

Validation (2026-07-02):

- 547/547 lib tests.
- `stage_breakdown` (10f gate config) FNV `688d5eeaee94d95e` in **both**
  modes = the recorded campaign baseline. Stage medians with racecar off:
  `get_nz_map_contexts` 102→713 ms, quantize main loop 446→670 ms; untouched
  stages flat — the toggle bites exactly where the bricks live.
- Single-binary CLI A/B (same clip + args as the definitive A/B above; clip
  regenerated with `ffmpeg testsrc2` and bit-identical; 7 interleaved
  rounds): median **on 7.099 s vs off 7.661 s = 1.079×** (per-round
  1.055–1.115) — reproduces the two-binary stock-vs-opt result (1.098×,
  1.072–1.117) within the box's thermal noise. Outputs byte-identical to
  each other **and** SHA256-equal to the recorded definitive artifact
  (`e6c294bb…6ce2d`, 262 180 B) — normal mode reproduces the stock
  bitstream exactly.
