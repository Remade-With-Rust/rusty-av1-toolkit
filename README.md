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
| Decoder | rav1d (safe-Rust dav1d port) | **same, profiled at its performance floor** |
| Memory safety | encoder mixed · decoder safe-forward | **same** — `unsafe`/asm isolated |
| License (copyright) | BSD-2 | **BSD-2** — embed freely, no copyleft |

<sub>Encoder figures are whole-encode wall-clock vs stock rav1e via real CLI A/B on
this machine; `--racecar off` is verified byte-identical to stock output. Your
mileage varies with clip, speed level, and CPU.</sub>

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
predictable performance, and the freedom of a permissive license. Each project is a reimplementation, not a fork: same wire protocols and file formats,
new code you can actually depend on.

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
