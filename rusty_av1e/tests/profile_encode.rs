// rusty_av1e analyzer — deterministic in-process encode benchmark + stage breakdown.
//
// The two spine instruments from the `analyzer` skill, for the rav1e encoder:
//
//   (2) deterministic benchmark  -> `bench_encode`     — honest Mpx/s, best-of-N.
//   (1) stage profiler dump      -> `stage_breakdown`  — where the encode time goes.
//   (3) cache-boundedness sweep  -> `cache_probe`      — is it memory-bound?
//
// All three are `#[ignore]`d (they are benchmarks, not unit tests) — run explicitly.
//
//   # honest throughput (profiler OFF — no rdtsc overhead in the number):
//   cargo test --release --test profile_encode -- --ignored --nocapture bench_encode
//
//   # stage breakdown (profiler ON, single-threaded for meaningful %):
//   cargo test --release --features profile --test profile_encode \
//       -- --ignored --nocapture --test-threads=1 stage_breakdown
//
//   # cache sweep (profiler OFF):
//   cargo test --release --test profile_encode -- --ignored --nocapture cache_probe
//
// Knobs via env vars (with defaults): W, H, FRAMES, SPEED, THREADS, RUNS, QP.

use rav1e::color::ChromaSampling;
use rav1e::{Config, Context, EncoderConfig, EncoderStatus};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Deterministic synthetic clip: a smooth luma/chroma ramp (predictable → intra
// & inter prediction do real work) + mild high-frequency texture (non-zero
// residual → transform/quant/entropy do real work) + a per-frame pan (real
// motion → motion estimation/compensation do real work). No RNG, no I/O — the
// same bytes every run, so best-of-N differences are the encoder, not the input.
// ---------------------------------------------------------------------------
fn synth_plane(pw: usize, ph: usize, frame_idx: usize, plane_id: usize) -> Vec<u8> {
  let mut v = vec![0u8; pw * ph];
  let dx = frame_idx.wrapping_mul(3); // horizontal pan (px/frame)
  let dy = frame_idx; // vertical pan
  for y in 0..ph {
    for x in 0..pw {
      let sx = x.wrapping_add(dx);
      let sy = y.wrapping_add(dy);
      // Dominant smooth ramp (compressible; drives prediction).
      let smooth = (sx.wrapping_mul(2).wrapping_add(sy) & 0xff) as u32;
      // Mild deterministic high-frequency detail (drives residual coding).
      let tex = (((sx.wrapping_mul(37)) ^ (sy.wrapping_mul(101))) >> 3 & 0x1f) as u32;
      let val = smooth.wrapping_add(tex).wrapping_add((plane_id as u32) * 16);
      v[y * pw + x] = (val & 0xff) as u8;
    }
  }
  v
}

struct Clip {
  frames: usize,
  // raw[frame][plane] = 8-bit samples, pre-computed ONCE so the synth-hash cost
  // never lands inside the timed region (only the plane memcpy does).
  raw: Vec<Vec<Vec<u8>>>,
  plane_dims: Vec<(usize, usize)>, // (pw, ph) per plane, for the chosen chroma
}

impl Clip {
  fn new(width: usize, height: usize, frames: usize) -> Self {
    // 4:2:0 => luma full, two chroma at half res each dimension.
    let plane_dims =
      vec![(width, height), (width.div_ceil(2), height.div_ceil(2)), (width.div_ceil(2), height.div_ceil(2))];
    let raw = (0..frames)
      .map(|fidx| {
        plane_dims
          .iter()
          .enumerate()
          .map(|(pli, &(pw, ph))| synth_plane(pw, ph, fidx, pli))
          .collect()
      })
      .collect();
    Clip { frames, raw, plane_dims }
  }
}

/// `tiles`: <=1 forces a single tile (clean single-threaded breakdown); >1 asks
/// the encoder to split into ~that many tiles so threads actually parallelize.
fn make_config(
  cfg_w: usize, cfg_h: usize, speed: u8, threads: usize, qp: usize, tiles: usize,
) -> Config {
  let mut enc = EncoderConfig::with_speed_preset(speed);
  enc.width = cfg_w;
  enc.height = cfg_h;
  enc.bit_depth = 8;
  enc.chroma_sampling = ChromaSampling::Cs420;
  // Constant-quality (fixed quantizer, no bitrate target) => the rate controller
  // never triggers a trial re-encode, so `encode_frame` runs exactly once/frame
  // and the stage buckets aren't polluted by a throwaway pass.
  enc.quantizer = qp;
  if tiles <= 1 {
    // Single tile keeps the breakdown clean (one `encode_tile` call/frame).
    enc.tile_cols = 1;
    enc.tile_rows = 1;
  } else {
    // Multi-tile so tile-parallelism can use the threadpool.
    enc.tiles = tiles;
  }
  Config::new().with_encoder_config(enc).with_threads(threads)
}

fn cpu_cores() -> usize {
  std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// FNV-1a fold — the byte-identical gate. Every optimization brick must leave
/// this hash unchanged (same clip/config ⇒ same bitstream bytes, in order).
struct Fnv(u64);
impl Fnv {
  fn new() -> Self {
    Fnv(0xcbf2_9ce4_8422_2325)
  }
  fn eat(&mut self, bytes: &[u8]) {
    for &b in bytes {
      self.0 ^= u64::from(b);
      self.0 = self.0.wrapping_mul(0x1000_0000_01b3);
    }
  }
}

/// Encode the whole clip once. Returns (encoded bytes, wall time of the
/// send/flush/receive loop, FNV-1a hash of the bitstream). Frame fill is a
/// memcpy from pre-computed `raw`.
fn encode_once(clip: &Clip, cfg: &Config) -> (usize, Duration, u64) {
  let mut ctx: Context<u8> = cfg.new_context().expect("valid config");
  let mut bytes = 0usize;
  let mut hash = Fnv::new();

  let t0 = Instant::now();
  let mut sent = 0usize;
  while sent < clip.frames {
    let mut f = ctx.new_frame();
    for (pli, plane) in f.planes.iter_mut().enumerate() {
      let (pw, _ph) = clip.plane_dims[pli];
      plane.copy_from_raw_u8(&clip.raw[sent][pli], pw, 1);
    }
    match ctx.send_frame(f) {
      Ok(()) => sent += 1,
      Err(EncoderStatus::EnoughData) => drain(&mut ctx, &mut bytes, &mut hash),
      Err(e) => panic!("send_frame: {e:?}"),
    }
    drain(&mut ctx, &mut bytes, &mut hash);
  }
  ctx.flush();
  loop {
    match ctx.receive_packet() {
      Ok(pkt) => {
        bytes += pkt.data.len();
        hash.eat(&pkt.data);
      }
      Err(EncoderStatus::LimitReached) => break,
      Err(EncoderStatus::Encoded) | Err(EncoderStatus::NeedMoreData) => {}
      Err(e) => panic!("receive_packet: {e:?}"),
    }
  }
  (bytes, t0.elapsed(), hash.0)
}

fn drain(ctx: &mut Context<u8>, bytes: &mut usize, hash: &mut Fnv) {
  loop {
    match ctx.receive_packet() {
      Ok(pkt) => {
        *bytes += pkt.data.len();
        hash.eat(&pkt.data);
      }
      Err(_) => break, // Encoded / NeedMoreData / LimitReached -> stop draining
    }
  }
}

fn env<T: std::str::FromStr>(key: &str, default: T) -> T {
  std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn mpx_per_s(width: usize, height: usize, frames: usize, dur: Duration) -> f64 {
  (width * height * frames) as f64 / dur.as_secs_f64() / 1.0e6
}

// ---------------------------------------------------------------------------
// (2) Deterministic benchmark — the honest number. Best-of-RUNS, report the
//     distribution (min = best, median, max). Run with the `profile` feature OFF.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn bench_encode() {
  let w = env("W", 640usize);
  let h = env("H", 480usize);
  let frames = env("FRAMES", 20usize);
  let speed = env("SPEED", 6u8);
  let threads = env("THREADS", 0usize); // 0 = all cores
  let runs = env("RUNS", 3usize);
  let qp = env("QP", 100usize);
  // Default to ~one tile per worker so threading actually parallelizes.
  let tiles = env("TILES", if threads == 0 { cpu_cores() } else { threads });

  let clip = Clip::new(w, h, frames);
  let cfg = make_config(w, h, speed, threads, qp, tiles);

  // Warm-up run (page-ins, code caches) not counted.
  let (bytes, _, hash) = encode_once(&clip, &cfg);

  let mut durs: Vec<Duration> = Vec::with_capacity(runs);
  for _ in 0..runs {
    let (_, d, h) = encode_once(&clip, &cfg);
    assert_eq!(h, hash, "bitstream hash differs between runs — nondeterminism!");
    durs.push(d);
  }
  durs.sort();
  let best = durs[0];
  let median = durs[durs.len() / 2];
  let worst = durs[durs.len() - 1];

  if cfg!(feature = "profile") {
    eprintln!(
      "\n[!] built WITH --features profile — this Mpx/s includes rdtsc overhead; \
       rebuild WITHOUT it for the honest throughput number.\n"
    );
  }
  eprintln!("\n=== bench_encode ({w}x{h}, {frames}f, speed {speed}, {} threads, QP {qp}) ===",
    if threads == 0 { "all".to_string() } else { threads.to_string() });
  eprintln!("  bitstream FNV : {hash:016x}   <- byte-identical gate");
  eprintln!("  encoded size : {} KiB ({:.3} bpp avg)", bytes / 1024,
    (bytes * 8) as f64 / (w * h * frames) as f64);
  eprintln!("  best    : {:>8.2} Mpx/s  ({:>7.2} ms/frame)", mpx_per_s(w, h, frames, best),
    best.as_secs_f64() * 1000.0 / frames as f64);
  eprintln!("  median  : {:>8.2} Mpx/s  ({:>7.2} ms/frame)", mpx_per_s(w, h, frames, median),
    median.as_secs_f64() * 1000.0 / frames as f64);
  eprintln!("  worst   : {:>8.2} Mpx/s  ({:>7.2} ms/frame)", mpx_per_s(w, h, frames, worst),
    worst.as_secs_f64() * 1000.0 / frames as f64);
  eprintln!("  spread  : {:.1}%\n",
    100.0 * (worst.as_secs_f64() - best.as_secs_f64()) / best.as_secs_f64());
}

// ---------------------------------------------------------------------------
// (1) Stage breakdown — WHERE the time goes. Single-threaded so per-stage cycle
//     sums don't exceed wall time. Needs `--features profile` (else a no-op).
// ---------------------------------------------------------------------------
#[test]
#[ignore = "benchmark; run with --features profile --ignored --nocapture --test-threads=1"]
fn stage_breakdown() {
  if !cfg!(feature = "profile") {
    eprintln!(
      "\n[stage_breakdown] built WITHOUT the `profile` feature — nothing to report.\n\
       Rerun: cargo test --release --features profile --test profile_encode \\\n\
       \t-- --ignored --nocapture --test-threads=1 stage_breakdown\n"
    );
    return;
  }

  let w = env("W", 640usize);
  let h = env("H", 480usize);
  let frames = env("FRAMES", 20usize);
  let speed = env("SPEED", 6u8);
  let qp = env("QP", 100usize);

  let clip = Clip::new(w, h, frames);
  let cfg = make_config(w, h, speed, 1 /* single-threaded */, qp, 1 /* single tile */);

  // Warm up, then reset the buckets so only the measured pass counts.
  let _ = encode_once(&clip, &cfg);
  rav1e::prof::reset();
  let (bytes, dur, hash) = encode_once(&clip, &cfg);

  eprintln!(
    "\n=== stage_breakdown ({w}x{h}, {frames}f, speed {speed}, 1 thread, QP {qp}) ===",
  );
  eprintln!("  bitstream FNV : {hash:016x}   <- byte-identical gate");
  eprintln!(
    "  wall {:.1} ms ({:.2} ms/frame), {} KiB, {:.2} Mpx/s (profiled build — slower than honest)",
    dur.as_secs_f64() * 1000.0,
    dur.as_secs_f64() * 1000.0 / frames as f64,
    bytes / 1024,
    mpx_per_s(w, h, frames, dur),
  );
  rav1e::prof::dump("encode");
}

// ---------------------------------------------------------------------------
// (3) Cache-boundedness sweep — encode the SAME content density at sizes that
//     span L2->L3. If per-pixel Mpx/s DROPS as the frame grows, the encode is
//     memory-bound (cache-tiles may help); if it's flat/rises, it is NOT.
//     Run with the `profile` feature OFF.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture cache_probe"]
fn cache_probe() {
  let speed = env("SPEED", 6u8);
  let frames = env("FRAMES", 8usize);
  let qp = env("QP", 100usize);
  // Square sizes crossing typical L2 (256-512 KiB) and L3 (MiBs) working sets.
  let sizes = [256usize, 384, 512, 768, 1024, 1536];

  // Single-threaded + single-tile so the ONLY thing changing across sizes is the
  // working set — otherwise thread-starvation at small sizes masks the cache signal.
  eprintln!("\n=== cache_probe (speed {speed}, {frames}f/size, 1 thread/1 tile, QP {qp}) ===");
  eprintln!("  {:>6}  {:>10}  {:>12}", "size", "Mpx/s", "ms/frame");
  for &s in &sizes {
    let clip = Clip::new(s, s, frames);
    let cfg = make_config(s, s, speed, 1, qp, 1);
    let _ = encode_once(&clip, &cfg); // warm
    let mut best = Duration::MAX;
    for _ in 0..3 {
      let (_, d, _) = encode_once(&clip, &cfg);
      best = best.min(d);
    }
    eprintln!(
      "  {:>5}²  {:>10.2}  {:>12.2}",
      s,
      mpx_per_s(s, s, frames, best),
      best.as_secs_f64() * 1000.0 / frames as f64
    );
  }
  eprintln!("  (per-pixel Mpx/s dropping as size grows => memory-bound)\n");
}
