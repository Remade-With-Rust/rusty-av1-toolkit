# Changelog

## 1.3.0 (2026-11-09)

A performance release. Decoded output is unchanged and bit-exact with dav1d.

- 45 output-identical speedups from an instruction-count campaign. Against 1.2.1, single-thread
  wall time is 1.07x to 1.24x faster for the pure-Rust build, and 1.03x to 1.04x faster for the
  asm build on two of three test streams, with the third within noise. Instruction counts fell
  30% to 43% for the pure-Rust build and 3% to 9% for the asm build. See "Performance in 1.3.0"
  in the README.
- The README now describes this fork ahead of the upstream rav1d build notes.
- The `dav1d` CLI in `tools/`, which is not published, defaults to rusty_alloc 2.4.

## 1.2.1 (2026-10-16)

- Ship `src/arm/asm-offsets.h` so the ARM assembly build works from the published crate.

## 1.2.0 (2026-08-02)

- The library no longer depends on libc and builds for `wasm32-unknown-unknown`.
