# Changelog

## 0.8.1 (2026-11-09)

A performance release. The default encode stays byte-identical to stock rav1e.

- 30 output-identical speedups from an instruction-count campaign. Single-thread wall time is
  1.08x to 1.18x faster than 0.8.0 (paired medians 1.12x to 1.16x), and the instruction count
  is 20% to 22% lower. See "Performance in 0.8.1" in the README.
- The CLI's default allocator moves to rusty_alloc 2.4 (`rusty_alloc-api` 2.2 to 2.4), measured
  neutral.
- Debug builds pass the lib test suite again (555/555). A debug assertion in
  `write_tx_size_inter` read a block's own mode cell, which counting trials stopped filling in
  an earlier speedup; it now applies only on writers that keep the block context.
- Items behind the `bench` feature, which re-exports internal modules for the crate's own
  benchmarks, changed signature: `encode_block_post_cdef` and `rdo_tx_size_type` take a
  prediction-reuse argument. The default-feature API is unchanged.

## Version 0.6.0

- See https://github.com/xiph/rav1e/projects/20
