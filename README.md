# rusty-av1-toolkit

A pair of AV1 codecs in Rust — a performance-tuned **encoder** and a
safe-Rust **decoder** — kept together in one repository. Each lives in its own
subdirectory as an independent Cargo crate with its own build, tests, and
license; this repo just bundles them (it is **not** a single Cargo workspace,
because each upstream is already a workspace of its own).

| Dir | Crate | Upstream | What it is |
|-----|-------|----------|------------|
| [`rusty_av1e/`](rusty_av1e/) | `rav1e` | [xiph/rav1e](https://github.com/xiph/rav1e) | AV1 encoder, performance-tuned fork |
| [`rusty_av1d/`](rusty_av1d/) | `rav1d` | [memorysafety/rav1d](https://github.com/memorysafety/rav1d) | AV1 decoder, safe-Rust fork (dav1d port) |

## Encoder (`rusty_av1e/`)

A fork of rav1e focused on encode speed. Its **default output is
bit-identical to stock rav1e** — same bitstream, byte for byte — while running
noticeably faster thanks to reworked hot kernels. A `--racecar` switch selects
the tradeoff:

| `--racecar` | Speed vs stock | Bitstream | Notes |
|-------------|----------------|-----------|-------|
| `off` (default) | ~1.10× | **byte-identical** | faster kernels only, no output change |
| `on` | ~1.69× | changed | transform-domain rate estimation; pair with `--tune Psnr` |
| `stock` | 1.0× | byte-identical | original rav1e code path (baseline) |

```bash
cd rusty_av1e
cargo build --release
./target/release/rav1e input.y4m -o out.ivf                 # byte-identical, ~10% faster
./target/release/rav1e input.y4m -o out.ivf --racecar on --tune Psnr   # ~1.7×
```

## Decoder (`rusty_av1d/`)

A fork of rav1d, the memory-safe Rust port of dav1d, used here as a conformance
oracle and profiling target. The decode path has been instrumented and audited
per-primitive; it decodes standard AV1 bitstreams.

```bash
cd rusty_av1d
cargo build --release
./target/release/dav1d -i in.ivf -o out.y4m
```

## Building

There is intentionally no top-level `cargo build`. Build each crate from its
own directory (`cd rusty_av1e && cargo build` / `cd rusty_av1d && cargo build`)
so each keeps its own profiles, lockfile, and toolchain pin.

## License

Both projects are BSD-2-Clause. The encoder additionally carries the Alliance
for Open Media Patent License (see [`rusty_av1e/PATENTS`](rusty_av1e/PATENTS)).
Each subdirectory retains its upstream license files verbatim; see
[`NOTICE`](NOTICE) for full provenance and [`LICENSE`](LICENSE) for the umbrella
terms.

This is a fork of existing open-source projects — please preserve upstream
attribution and license files in any redistribution.
