<!--
  rusty_av1e — PROJECT BLUEPRINT (AV1 encoder, forked from rav1e for byte-identical speed).
  Method / running log: docs/entropy-bricks.md (the brick ledger — every attempt, kept or reverted).
  Profiler spine: src/prof.rs.  Harness + gate: tests/profile_encode.rs.
-->

# rusty_av1e — Project Blueprint

**Goal:** make [rav1e](https://github.com/xiph/rav1e) **measurably faster without
changing a single output byte** — profile the encoder, find where the cycles actually
go, and optimize the hot primitives one **brick** at a time, each gated **byte-identical**
against the stock bitstream. The same disciplined, measured primitives seed the future
AV2 encoder (`rav2e`, rav1e-derived). A separate **experimental** track
(bitstream-changing, BD-rate-gated) explores speed/size trade-offs behind a flag.

**Status (2026-07-01):** on branch `opt-entropy`, **~10% faster whole-encode than stock
rav1e — byte-identical** (real CLI, 7-round interleaved A/B; both binaries emit the exact
same file, SHA256 `e6c294bb…6ce2d`). Three kept bricks; the coefficient-entropy path
(48% of encode) profiled to its irreducible serial floor.

## The ruleset (hard rules)

1. **Measure first — the profiler decides, never the code-size intuition.** Every brick
   starts from a `src/prof.rs` measurement (rdtsc cycle buckets). Agent code-size surveys
   repeatedly mis-ranked ROI here (a stage guessed at "20-30%" measured 1.4%; another
   "10-15%" measured 2.6%) — the profiler refuted them every time. No optimization is
   attempted on an unmeasured target.
2. **The byte-identical gate is absolute.** A brick may not change one output byte. The
   gate is an **FNV-1a hash of the bitstream** (`688d…95e`, pinned in
   `tests/profile_encode.rs`) plus the 547-test lib suite; kept bricks are re-validated
   against the stock CLI's SHA256. If the hash moves, it is not a brick — it is a bug or
   an experiment (see rule 5).
3. **One brick per commit; revert if not faster.** Each change is isolated, measured
   against its own oracle, and **reverted the moment it fails to beat the baseline** — the
   reverts are ledgered *do-not-retry* with the reason, so no session re-runs a dead end.
4. **Stage-median is the verdict when whole-encode is noise-bound.** This box has ~±5%
   thermal noise. A win worth <5% of encode is judged on its **non-overlapping per-stage
   median** (baseline max < optimized min), not the noisy whole-encode number — with the
   stage isolation documented so the claim is honest.
5. **Bitstream changes are experiments, not bricks — gated on corpus BD-rate.** Anything
   that changes the output (e.g. the `tx_domain_rate` fast-rate path) lives behind a flag
   and is judged by BD-rate on a real-video corpus + a decoder round-trip, never on a
   single synthetic clip. Prove the ceiling offline before paying integration cost.

## Workspace

```
src/prof.rs               feature-gated stage profiler (rdtsc buckets, is_rdo_child / is_info tiers).
                          OFF by default = shipped byte-identical (ZST no-op scope guard, no Drop).
tests/profile_encode.rs   the harness: bench_encode (honest Mpx/s), stage_breakdown (per-stage %),
                          cache_probe (working-set sweep), and the FNV byte-identical gate.
docs/entropy-bricks.md    THE BRICK LEDGER — every brick B1-B9 / F1-F2 / Q1-Q3 / glue / loop-filter,
                          kept or reverted, with the measured deltas and the reason.
src/context/…             the hot edit surface: write_coeffs_lv_map (coeff entropy/rate, 48%),
src/quantize/…            the nz-map context stencil, the quantize main loop.
```

Run:
```sh
# honest throughput (profile OFF, all threads):
cargo test --release --no-default-features --features asm,threading \
  --test profile_encode -- --ignored --nocapture bench_encode
# per-stage breakdown (profile ON, single-thread):
cargo test --release --no-default-features --features asm,threading,profile \
  --test profile_encode -- --ignored --nocapture --test-threads=1 stage_breakdown
```

## The kept bricks (byte-identical wins on `opt-entropy`)

| brick | what it does | measured | verdict |
|---|---|---|---|
| **B7a** | full-area, column-contiguous nz-map **context stencil** (the `TX_PAD` layout was built for this SIMD pattern); pure restructure — LLVM had never vectorized the scattered original | stencil 878 → 320 ms; **−8.3% whole encode** (interleaved 1T A/B) | **KEPT** |
| **B7a-SIMD** | hand-AVX2 twin of that stencil — one `ymm` per ≤32-row column, `vpavgb ≡ (mag+1)>>1`, per-tx offset vectors; cached CPU-feature dispatch + hard bounds guard | stencil 320 → 96 ms (**−89% cumulative**); **~−4% whole encode** | **KEPT** |
| **Q2** | **branchless** quantize main loop — kills the two data-dependent mispredicting branches in the serial (loop-carried) loop; the division was *not* the bottleneck | main loop 676 → 444 ms (**−35% stage**) | **KEPT** |

**Definitive whole-encode A/B** (real `rav1e` CLI, `opt-entropy` HEAD vs stock rav1e
`564ae3b0`, 640×480×60f, `--speed 6 --quantizer 100 --tiles 1 --threads 1`, 7 interleaved
rounds): median **7.588 s → 6.911 s = 1.098× = ~9.8% faster**, tight 1.072–1.117×, **output
byte-identical** (262 180 B, same SHA256). Content-dependent — denser content puts more of
the encode in the entropy path, so the speedup grows.

**Single-binary A/B:** `--racecar <on|off>` (default on) switches at runtime between the
kept bricks and the resurrected stock code paths — byte-identical either way, ~8-10%
apart in speed. `RAV1E_RACECAR=0` does the same for library/test builds. See "The racecar
switch" in [docs/entropy-bricks.md](docs/entropy-bricks.md).

Reverted, ledgered *do-not-retry*: **B4** (levels scatter — already autovec'd), **F1**
(update_cdf split-loop — LLVM already emits cmov), **F2** (fc_log dedup — a random side-table
load costs more than the streaming copy it saved, +6-7%), **Q2-twopass** (division-hoist,
+15% — proved the branches, not the divide, were the cost), **LF-rate-hoist** (real
redundancy but below the noise floor).

## Campaign phases (the map so far)

- **P0 — Analyzer spine.** `src/prof.rs` + `tests/profile_encode.rs` + the FNV gate. The
  measurement that turned "optimize the encoder" into a ranked target list.
- **P1 — Coeff entropy/rate (48%).** The #1 target. `write_coeffs_lv_map` decomposed to the
  last piece: **~77% is irreducibly serial arithmetic + sign coding** (CDF adaptation is
  loop-carried — no SIMD, no threading); the one structural piece (the nz-map context
  stencil) is exactly **B7a + B7a-SIMD**. **Path proven at its floor.**
- **P2 — Quantize (12%).** Serial, loop-carried. **Q2** made the hot branches branchless.
- **P3 — RDO glue (~27%).** Decomposed: loop-filter RDO search 8.3% (inherent trial search),
  distortion 2.6%, rd-cost 0.1% — rav1e's glue is *tight* (stack ArrayVecs, cheap
  checkpoints); the h264 glue-win patterns don't exist here. No byte-identical brick found.
- **P4 — Experimental (parallel track).** `tx_domain_rate` measured at ~2× speed / +49% size
  on the synthetic clip — a real lever, but it changes the bitstream, so it is parked behind
  a flag pending a real-video BD-rate corpus.

## The core technique — the brick loop

For each measured target, in ROI order:

1. **LOCATE** the cost with `prof.rs` (a stage scope; nest `is_rdo_child` scopes to
   sub-divide). The biggest *measured* pure-compute cost, not the biggest-looking code.
2. **ORACLE** — keep the slow, obviously-correct version as a test oracle (e.g. the scalar
   nz-map kernel vs its AVX2 twin, checked at every raster position × tx size × class).
3. **OPTIMIZE** — one technique: eliminate-redundancy → vectorize → hand-SIMD/asm. **Inspect
   the emitted asm first** (`--emit asm`) — "the compiler already vectorized it" was *false*
   for B7a-SIMD; the scalar kernel had zero SIMD.
4. **GATE** — FNV bitstream hash unchanged **and** 547/547 lib tests **and** oracle-equal.
5. **VERDICT** — interleaved whole-encode A/B if the win is >noise; otherwise the
   non-overlapping stage median. **Revert if it doesn't beat baseline.**
6. **RECORD** — one row in `docs/entropy-bricks.md` (kept or *do-not-retry*), so the ledger,
   not memory, carries the campaign across sessions.

The reverts are the point: this codebase is entropy-coding-bound and serial, so most
"obvious" optimizations are already done by LLVM or below the noise floor. The wins came
only from removing *structure* that blocked vectorization (B7a) or from killing branch
mispredicts in a serial loop (Q2) — found by measuring, proven by the gate.

Full brick-by-brick history and numbers: **[docs/entropy-bricks.md](docs/entropy-bricks.md)**.
