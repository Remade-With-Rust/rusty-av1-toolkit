### In The Wild with 6,208 Active Installs

FREE RAG Converter Online -- <a href="https://RAGconverter.com">RAGconverter.com</a>

# rusty-av1-toolkit — fast Rust AV1 encoder + decoder

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: BSD-2-Clause](https://img.shields.io/badge/license-BSD--2--Clause-blue)](LICENSE)
![Platforms: Windows · macOS · Linux](https://img.shields.io/badge/platforms-Windows%20%C2%B7%20macOS%20%C2%B7%20Linux-informational)

> **rusty-av1-toolkit** pairs a **performance-tuned AV1 encoder** and a
> **safe-Rust AV1 decoder** in one repository — a fork of
> [rav1e](https://github.com/xiph/rav1e) (encoder) and
> [rav1d](https://github.com/memorysafety/rav1d) (decoder), permissively licensed
> (BSD-2), with an encoder fast-path that is **byte-identical to stock rav1e**.

<!-- encoder = rusty_av1e (rav1e fork); decoder = rusty_av1d (rav1d fork). Two independent crates, not one workspace. -->

---

## Mandatory Requirements (the ruleset)

1. **Fork & optimize, don't reinvent.** The codecs start from the established Rust
   AV1 projects — [rav1e](https://github.com/xiph/rav1e) (encoder) and
   [rav1d](https://github.com/memorysafety/rav1d) (decoder) — and are made faster
   without a clean-room rewrite. Upstream structure is preserved so fixes stay
   mergeable. See [NOTICE](NOTICE) for attribution.
2. **Byte-identical is the default.** The encoder's default (`--racecar off`)
   emits a bitstream **bit-for-bit identical to stock rav1e** — same output, just
   faster kernels. Speed-ups that change the bitstream are **opt-in only**.
3. **Every change is measured against stock.** Each optimization is A/B-tested
   against the pristine upstream baseline (byte-identical gate + wall-clock), and
   reverted if it doesn't pay — no speculative changes.
4. **asm and `unsafe` are allowed where they pay for speed.** This is **not** a
   `forbid(unsafe)` codebase — it inherits the hand-written SIMD and `unsafe`
   buffer/threading machinery a competitive codec needs. `unsafe`/asm are
   **isolated and documented**; memory-safety is the direction, not an absolute ban.
5. **Permissive & patent-honest.** BSD-2 source (embed freely, no copyleft); the
   AV1 **patent** grant is the [Alliance for Open Media Patent License](https://aomedia.org/license/),
   separate from the source license (see [License](#license)).

---

## ⚡ The headline

> Drop-in faster AV1. The encoder's default output is **the exact stock rav1e
> bitstream**, byte for byte — so nothing downstream changes — while encoding
> runs measurably faster. Flip one switch to trade identity for more speed.

| | Stock rav1e / rav1d | **rusty-av1-toolkit** |
|---|---|---|
| Language | Rust | **Rust** (fork + tuned) |
| Encoder, default (`--racecar off`) | baseline | **~1.10× faster, byte-identical bitstream** |
| Encoder, opt-in (`--racecar on`) | — | **~1.69× faster** (bitstream changes; pair `--tune Psnr`) |
| Encoder, opt-in (`RAV1E_FAST=fast`) | — | **content-adaptive quality tier** — per-clip dispatches, each gated so declined content stays byte-identical |
| Decoder | rav1d (safe-Rust dav1d port) | **same, profiled at its performance floor** |
| Memory safety | encoder mixed · decoder safe-forward | **same** — `unsafe`/asm isolated |
| License (copyright) | BSD-2 | **BSD-2** — embed freely, no copyleft |

<sub>Encoder speed figures are whole-encode wall-clock vs stock rav1e via real CLI A/B on
this machine; `--racecar off` is verified byte-identical to stock output. Your
mileage varies with clip, speed level, and CPU.</sub>

### The content-adaptive tier (`RAV1E_FAST=fast`, opt-in)

A quality tier built brick by brick, where each brick that helps some content and
hurts other content is **dispatched on a measured content signal** rather than
shipped as a fixed compromise. Every dispatch is gated the same way: on content it
declines, the bitstream is **byte-identical** to the tier without it, so a
misclassification cannot regress quality — it can only miss a win.

The most recent addition (partition dispatch, `prom_av1e065`) routes by **spatial
detail** — mean |horizontal luma gradient| measured on the source before encoding:

| | measured |
|---|---|
| Corpus | 8 Derf CIF clips, 32 frames, 7-point quality ladders |
| Engaged on | 3 clips (mobile −1.98%, tempete −1.70%, bus −0.44% BD-rate) |
| Declined on | 5 clips — **byte-identical**, 0.00% |
| Mean / worst clip | **−0.52% / 0.00%** (monotone non-regression) |

Two things worth stating plainly, because they are easy to get wrong:

- **The tier's bricks are sub-additive.** The same partition dispatch measured
  −2.01% on bus in isolation and **−0.44%** inside the tier, because the tier's
  other tools already capture part of the same gain. Tier figures are measured in
  the tier; brick figures do not add up.
- **The tier is a quality/speed *trade*, not a free win.** Against the
  byte-identical default it is content-dependent (measured +5.3% on bus, +3.3% on
  akiyo, −8.7% on mobile at speed 6) — which is precisely why it is opt-in and why
  its bricks are dispatched rather than defaulted globally.

<sub>BD-rate is Bjøntegaard-Delta over PCHIP-interpolated ladders on the measured
PSNR overlap only, computed frame-by-index on decoded output. Encodes are
deterministic, so BD figures are exact; timing verdicts use a paired win-rate
(both arms per round, alternating order) because wall-clock on this machine drifts
far more than the effects being measured.</sub>

---

## What is this?

Two AV1 codecs in Rust, bundled in one repo:

| Dir | Crate | Upstream | What it is |
|-----|-------|----------|------------|
| [`rusty_av1e/`](rusty_av1e/) | `rav1e` | [xiph/rav1e](https://github.com/xiph/rav1e) | AV1 **encoder**, performance-tuned fork |
| [`rusty_av1d/`](rusty_av1d/) | `rav1d` | [memorysafety/rav1d](https://github.com/memorysafety/rav1d) | AV1 **decoder**, safe-Rust fork (dav1d port) |

The encoder is a fork of **rav1e** focused on encode speed: its hot kernels have
been reworked so the **default path produces the identical stock bitstream, just
faster**, and a `--racecar` switch exposes a faster bitstream-changing mode for
when you control both ends. The decoder is a fork of **rav1d** (VideoLAN's dav1d,
ported to safe Rust), instrumented and audited per-primitive; it decodes standard
AV1 and doubles as the encoder's conformance oracle.

## Remade With Rust

<!-- ORG BOILERPLATE — keep identical across repos -->

**Remade With Rust** is an initiative by [Mata Network](https://www.mata.network)
to rebuild essential C and C++ tools in Rust — for the memory safety, the
predictable performance, and the freedom of a permissive license. Most projects
are clean reimplementations; this one forks and hardens the best
permissively-licensed Rust AV1 codecs already out there — either way, new code
you can actually depend on, no copyleft.

We build the core to production grade and open-source it so the community can
extend it. No copyleft. No surprises. Just the tools we rely on, made faster and
safer.

→ More projects: **[github.com/remade-with-rust](https://github.com/remade-with-rust)**

<!-- /ORG BOILERPLATE -->

## Architecture

One repository, two **independent** Cargo crates — deliberately **not** a single
workspace, because each upstream is already a workspace of its own (so each keeps
its own profiles, lockfile, and toolchain pin):

```
rusty-av1-toolkit/
  rusty_av1e/   the encoder — fork of rav1e (lib + CLI + asm), tuned hot kernels + --racecar
  rusty_av1d/   the decoder — fork of rav1d (safe-Rust dav1d port, lib + CLI + asm)
  LICENSE       BSD-2 umbrella
  NOTICE        per-subtree provenance (rav1e / rav1d) + AOM patent note
```

## Building

There is intentionally **no top-level `cargo build`** — build each crate from its
own directory so its profiles and toolchain apply:

```sh
# Encoder (asm release — needs nasm on PATH):
cd rusty_av1e && cargo build --release
./target/release/rav1e input.y4m -o out.ivf                          # byte-identical, ~10% faster
./target/release/rav1e input.y4m -o out.ivf --racecar on --tune Psnr # ~1.7×, bitstream changes

# Decoder:
cd rusty_av1d && cargo build --release
./target/release/dav1d -i in.ivf -o out.y4m
```

**Requirements:** Rust (each crate pins its own toolchain where needed) and `nasm`
on `PATH` for the SIMD kernels.

### Optional: the pure-Rust allocator (`--features rusty_alloc`)

Both CLIs can route their allocations through
[rusty_alloc](https://github.com/Remade-With-Rust/rusty_alloc), the sibling
pure-Rust mimalloc remake, instead of the platform allocator:

```sh
cd rusty_av1e && cargo build --release --features rusty_alloc
cd rusty_av1d && cargo build --release -p rav1d-cli --features rusty_alloc
```

The `#[global_allocator]` is declared **in the binaries only**. Neither library
crate — nor the encoder's `capi` cdylib — picks an allocator on behalf of its
consumers, which is what lets an embedder keep its own. Output is unaffected on
either allocator, so the conformance gates hold on both arms. Requires a
toolchain new enough for edition 2024 (rustc ≥ 1.85); the default build keeps the
crates' declared MSRVs.

**Measured** — foreman_cif 300f, `--tiles 8 --threads 16`, ABBA-interleaved,
High priority, N=31 pairs, against a null arm (the same binary vs itself):

| arm | median wall ratio | paired | median CPU ratio | cores-busy |
|---|---:|---:|---:|---:|
| null (stock vs itself) | 1.0013 | z=0.54 | 1.0152 | 3.95 → 3.91 |
| **stock vs rusty_alloc** | **1.0619** | **31/31, z=5.57** | 1.0296 (z=2.69) | 3.97 → **4.07** |

**~6% faster wall on a threaded encode**, winning every pair, against a 0.13%
null floor. The mechanism is allocator lock contention, not allocation speed:
cores-busy *rises* (3.97 → 4.07), so the threads are blocking less. It follows
that the win needs real parallelism — on the default single-tile config, which
runs at cores-busy 0.85, the same comparison reads 1.0070 (z=0.22), i.e. nothing.

The **decoder is at parity** and does not benefit: CPU 1.0000 (z=−0.90), wall
0.9971 over N=31 — a ~0.3% wall cost sitting on the instrument's own floor. That
is expected rather than disappointing, because `MemPool` already recycles the
frame buffers, so steady-state decoding barely calls the allocator at all. It is
wired up for symmetry and for the `seek_stress` soak, not for speed.

Both features are **opt-in**, deliberately: turning them on raises the MSRV to
1.85, and the encoder win is real only in threaded configurations.

## Roadmap

- [x] Fork rav1e → `rusty_av1e`; rework hot kernels; **byte-identical ~1.10× fast path**.
- [x] `--racecar` modes (`off` byte-identical · `on` tx-domain-rate ~1.69× · `stock` baseline).
- [x] Fork rav1d → `rusty_av1d`; per-primitive profiling/audit; decodes AV1.
- [ ] Track upstream rav1e / rav1d fixes (remotes wired: `rav1e-upstream`, `rav1d-upstream`).
- [ ] Publish prebuilt binaries.

## License

BSD-2-Clause — see [LICENSE](LICENSE). Both codecs are BSD-2; each subdirectory
retains its upstream license files verbatim, and [NOTICE](NOTICE) records the full
provenance ([`rusty_av1e/LICENSE`](rusty_av1e/LICENSE) +
[`rusty_av1e/PATENTS`](rusty_av1e/PATENTS), [`rusty_av1d/COPYING`](rusty_av1d/COPYING)).
This is a fork of existing open-source projects — please preserve upstream
attribution and license files in any redistribution.

> **Patents.** The *source* is permissively licensed, but AV1 (like every modern
> codec) carries **patent** considerations covered by the
> [Alliance for Open Media Patent License](https://aomedia.org/license/) — **separate
> from and not granted by** the BSD source license. The encoder ships that grant as
> [`rusty_av1e/PATENTS`](rusty_av1e/PATENTS); retain it in any redistribution. "No
> copyleft" is a copyright statement, not a patent grant.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->
