//! Per-thread scratch buffers for the pure-Rust DSP fallbacks.
//!
//! dav1d's C fallbacks declare large uninitialized stack arrays (tens to hundreds of
//! KiB) and only ever read cells that were written earlier in the same call. The Rust
//! port zero-filled the equivalent arrays on every call, which on the no-`asm` and
//! `wasm32` paths cost tens of thousands of instructions per loop-restoration unit.
//!
//! An [`Arena`] grows once per thread and is reused without clearing. That stays in
//! safe Rust and stays deterministic: every cell a kernel reads is written earlier in
//! the same call, so data left over from a previous call is never observed. If that
//! invariant were ever broken, output would depend on the previous call. Debug builds
//! enforce it by poisoning every buffer handed out, so a read-before-write changes the
//! decoded output and fails the conformance md5s.
//!
//! The typed arrays are handed straight to the caller's closure (rather than carved
//! inside it) so their lifetimes are implied by the closure's argument types, which
//! keeps generic element types like `BD::Coef` free of a `'static` bound.

use std::cell::{RefCell, RefMut};
use std::mem;
use std::thread::LocalKey;

use zerocopy::{AsBytes, FromBytes};

/// A per-thread arena. Declare it with
/// `thread_local! { static X: Arena = const { Arena::new() }; }`.
pub struct Arena(RefCell<Vec<u64>>);

impl Arena {
    pub const fn new() -> Self {
        Self(RefCell::new(Vec::new()))
    }

    #[inline]
    fn borrow(&self, len: usize) -> RefMut<'_, [u64]> {
        let words = len.div_ceil(8);
        let mut v = self.0.borrow_mut();
        if v.len() < words {
            v.resize(words, 0);
        }
        let mut buf = RefMut::map(v, |v| &mut v[..words]);
        if cfg!(debug_assertions) {
            buf.fill(0x5a5a_5a5a_5a5a_5a5a);
        }
        buf
    }
}

/// Bytes an `[T; n]` occupies in an arena (every carve is 8-byte aligned).
const fn bytes<T>(n: usize) -> usize {
    (n * mem::size_of::<T>()).next_multiple_of(8)
}

#[inline]
fn carve<'a, T: FromBytes + AsBytes, const N: usize>(rest: &mut &'a mut [u8]) -> &'a mut [T; N] {
    let (head, tail) = mem::take(rest).split_at_mut(bytes::<T>(N));
    *rest = tail;
    let (arr, _) = T::mut_slice_from_prefix(head, N).unwrap();
    arr.try_into().unwrap()
}

/// Runs `f` with one array from this thread's arena `key`. The contents are whatever
/// the previous user left there; `f` must write every cell before reading it.
#[inline]
pub fn with1<A, const NA: usize, R>(
    key: &'static LocalKey<Arena>,
    f: impl FnOnce(&mut [A; NA]) -> R,
) -> R
where
    A: FromBytes + AsBytes,
{
    key.with(|arena| {
        let mut buf = arena.borrow(bytes::<A>(NA));
        let mut rest = buf.as_bytes_mut();
        f(carve::<A, NA>(&mut rest))
    })
}

/// As [`with1`], with two disjoint arrays.
#[inline]
pub fn with2<A, const NA: usize, B, const NB: usize, R>(
    key: &'static LocalKey<Arena>,
    f: impl FnOnce(&mut [A; NA], &mut [B; NB]) -> R,
) -> R
where
    A: FromBytes + AsBytes,
    B: FromBytes + AsBytes,
{
    key.with(|arena| {
        let mut buf = arena.borrow(bytes::<A>(NA) + bytes::<B>(NB));
        let mut rest = buf.as_bytes_mut();
        let a = carve::<A, NA>(&mut rest);
        let b = carve::<B, NB>(&mut rest);
        f(a, b)
    })
}

/// As [`with1`], with three disjoint arrays.
#[inline]
pub fn with3<A, const NA: usize, B, const NB: usize, C, const NC: usize, R>(
    key: &'static LocalKey<Arena>,
    f: impl FnOnce(&mut [A; NA], &mut [B; NB], &mut [C; NC]) -> R,
) -> R
where
    A: FromBytes + AsBytes,
    B: FromBytes + AsBytes,
    C: FromBytes + AsBytes,
{
    key.with(|arena| {
        let mut buf = arena.borrow(bytes::<A>(NA) + bytes::<B>(NB) + bytes::<C>(NC));
        let mut rest = buf.as_bytes_mut();
        let a = carve::<A, NA>(&mut rest);
        let b = carve::<B, NB>(&mut rest);
        let c = carve::<C, NC>(&mut rest);
        f(a, b, c)
    })
}
