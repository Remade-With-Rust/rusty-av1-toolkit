//! The [`CaseSet`] API below is a safe and simplified version of the `case_set*` macros in `ctx.h`.
//!
//! The `case_set*` macros themselves replaced `memset`s in order to further optimize them
//! (in e3b5d4d044506f9e0e95e79b3de42fd94386cc61,
//! which performed the following optimizations:
//! 1. larger, inlinable writes for small power of 2 lengths
//! 2. aligned writes for the above
//! 3. 8-byte aligned fields [`BlockContext`]
//!   * allows 8-byte aligned writes
//!   * better cache boundary alignment
//!
//! (1) is easy to preserve in Rust, but (2) is difficult to do so without overhead,
//! as unaligned writes are UB, and so we'd need to check at runtime if they're aligned
//! (a runtime-determined `off`set is used, so we can't reasonably ensure this at compile-time).
//!
//! To more thoroughly check this, I ran the same benchmarks done in
//! e3b5d4d044506f9e0e95e79b3de42fd94386cc61, which introduced the `case_set*` macros:
//!
//! ```sh
//! cargo build --release && hyperfine './target/release/dav1d -i ./tests/large/chimera_8b_1080p.ivf -l 1000 -o /dev/null'
//! ```
//!
//! for 3 implementations:
//! 1. the original `case_set*` macros translated directly to `unsafe` Rust `fn`s
//! 2. the safe [`CaseSet`] implementation below, whose stores use its small powers of 2 optimization
//! 3. a safe [`CaseSet`] implementation using [`slice::fill`]/`memset` only
//!
//! The small powers of 2 version was ~1.27% faster than the `case_set*` one,
//! and ~3.26% faster than the `memset` one.
//! The `case_set*` macros were also faster than `memset` in C by a similar margin,
//! meaning the `memset` option is the slowest in both C and Rust,
//! and since it was replaced with `case_set*` in C, we shouldn't use it in Rust.
//! Thus, the small powers of 2 implementation seems optimal, as it:
//! * is the fastest of the Rust implementations
//! * is completely safe
//! * employs the same small powers of 2 optimization the `case_set*` implementation did
//! * is far simpler than the `case_set*` implementation, consisting of a `match` and array writes
//!
//! [`BlockContext`]: crate::env::BlockContext
use std::iter::zip;

use crate::disjoint_mut::{AsMutPtr, DisjointMut};

#[inline(always)]
fn as_array<T, const N: usize>(buf: &mut [T]) -> &mut [T; N] {
    buf.try_into().unwrap()
}

/// The store width a [`CaseSetter`] dispatches on.
///
/// [`CaseSet::one`] classifies `len` once, so each [`CaseSetter::set`] is a jump on this
/// dense tag followed by an array write of a compile-time known length, rather than
/// re-deriving the width from `len` for every store (a power of 2 test, a `tzcnt`,
/// and a range check, repeated for each of the many context arrays a block sets).
/// Consecutive dispatches on the same tag also let LLVM merge them into one jump.
#[derive(Clone, Copy)]
enum Width {
    W1,
    W2,
    W4,
    W8,
    W16,
    W32,
    /// Not a power of 2 `<= UP_TO`: [`slice::fill`] if `WITH_DEFAULT`, else skipped.
    Other,
}

impl Width {
    const fn classify(len: usize, up_to: usize) -> Self {
        match len {
            01 if up_to >= 01 => Self::W1,
            02 if up_to >= 02 => Self::W2,
            04 if up_to >= 04 => Self::W4,
            08 if up_to >= 08 => Self::W8,
            16 if up_to >= 16 => Self::W16,
            32 if up_to >= 32 => Self::W32,
            _ => Self::Other,
        }
    }

    /// A compare and a table load, instead of the power of 2 test and `tzcnt`
    /// a `match` on `len` compiles to.
    #[inline(always)]
    fn of<const UP_TO: usize>(len: usize) -> Self {
        let table = const {
            let mut table = [Self::Other; 33];
            let mut len = 0;
            while len < 33 {
                table[len] = Self::classify(len, UP_TO);
                len += 1;
            }
            table
        };
        match table.get(len) {
            Some(&width) => width,
            None => Self::Other,
        }
    }
}

/// Sets `len` elements at `offset`, optimized for lengths that are small powers of 2.
///
/// For power of 2 lengths `<= UP_TO`,
/// the `memset` is done as an array write of that exactly (compile-time known) length.
/// If the length is not a power of 2 or `> UP_TO`,
/// then the `memset` is done by [`slice::fill`] (a `memset` call) if `WITH_DEFAULT` is `true`,
/// or else skipped if `WITH_DEFAULT` is `false`.
///
/// This optimizes for the common cases where the length is a small power of 2,
/// where the array write is optimized as few and large stores as possible.
pub struct CaseSetter<const UP_TO: usize, const WITH_DEFAULT: bool> {
    offset: usize,
    len: usize,
    width: Width,
}

impl<const UP_TO: usize, const WITH_DEFAULT: bool> CaseSetter<UP_TO, WITH_DEFAULT> {
    /// Dispatches on the precomputed [`Width`]. `always`: each arm carries its own
    /// bounds check, so a plain `#[inline]` is declined and every store becomes a call.
    #[inline(always)]
    pub fn set<T: Clone + Copy>(&self, buf: &mut [T], val: T) {
        let buf = &mut buf[self.offset..];
        match self.width {
            Width::W1 if UP_TO >= 01 => *as_array(&mut buf[..01]) = [val; 01],
            Width::W2 if UP_TO >= 02 => *as_array(&mut buf[..02]) = [val; 02],
            Width::W4 if UP_TO >= 04 => *as_array(&mut buf[..04]) = [val; 04],
            Width::W8 if UP_TO >= 08 => *as_array(&mut buf[..08]) = [val; 08],
            Width::W16 if UP_TO >= 16 => *as_array(&mut buf[..16]) = [val; 16],
            Width::W32 if UP_TO >= 32 => *as_array(&mut buf[..32]) = [val; 32],
            _ => {
                let buf = &mut buf[..self.len];
                if WITH_DEFAULT {
                    buf.fill(val)
                }
            }
        }
    }

    /// # Safety
    ///
    /// Caller must ensure that no elements of the written range are concurrently
    /// borrowed (immutably or mutably) at all during the call to `set_disjoint`.
    ///
    /// Dispatches on the precomputed [`Width`], like [`Self::set`].
    #[inline(always)]
    pub fn set_disjoint<T, V>(&self, buf: &DisjointMut<T>, val: V)
    where
        T: AsMutPtr<Target = V>,
        V: Clone + Copy,
    {
        let o = self.offset;
        match self.width {
            Width::W1 if UP_TO >= 01 => *as_array(&mut buf.index_mut(o..o + 01)) = [val; 01],
            Width::W2 if UP_TO >= 02 => *as_array(&mut buf.index_mut(o..o + 02)) = [val; 02],
            Width::W4 if UP_TO >= 04 => *as_array(&mut buf.index_mut(o..o + 04)) = [val; 04],
            Width::W8 if UP_TO >= 08 => *as_array(&mut buf.index_mut(o..o + 08)) = [val; 08],
            Width::W16 if UP_TO >= 16 => *as_array(&mut buf.index_mut(o..o + 16)) = [val; 16],
            Width::W32 if UP_TO >= 32 => *as_array(&mut buf.index_mut(o..o + 32)) = [val; 32],
            _ => {
                let mut buf = buf.index_mut(o..o + self.len);
                if WITH_DEFAULT {
                    buf.fill(val)
                }
            }
        }
    }
}

/// The entrypoint to the [`CaseSet`] API.
///
/// `UP_TO` and `WITH_DEFAULT` are made const generic parameters rather than have multiple `case_set*` `fn`s,
/// and these are put in a separate `struct` so that these 2 generic parameters
/// can be manually specified while the ones on the methods are inferred.
pub struct CaseSet<const UP_TO: usize, const WITH_DEFAULT: bool>;

impl<const UP_TO: usize, const WITH_DEFAULT: bool> CaseSet<UP_TO, WITH_DEFAULT> {
    /// Perform one case set.
    ///
    /// This API is generic over the element type (`T`) rather than hardcoding `u8`,
    /// as sometimes other types are used, though only `i8` is used currently.
    ///
    /// The `len` and `offset` are supplied here and
    /// applied to each `buf` passed to [`CaseSetter::set`] in `set_ctx`.
    #[inline]
    pub fn one<T, F>(ctx: T, len: usize, offset: usize, mut set_ctx: F)
    where
        F: FnMut(&CaseSetter<UP_TO, WITH_DEFAULT>, T),
    {
        let width = Width::of::<UP_TO>(len);
        set_ctx(&CaseSetter { offset, len, width }, ctx);
    }

    /// Perform many case sets in one call.
    ///
    /// This allows specifying the `set_ctx` closure inline easily,
    /// and also allows you to group the same args together.
    ///
    /// The `lens`, `offsets`, and `dirs` are zipped and passed to [`CaseSet::one`],
    /// where `dirs` can be an array of any type and whose elements are passed back to the `set_ctx` closure.
    #[inline]
    pub fn many<T, F, const N: usize>(
        dirs: [T; N],
        lens: [usize; N],
        offsets: [usize; N],
        mut set_ctx: F,
    ) where
        F: FnMut(&CaseSetter<UP_TO, WITH_DEFAULT>, T),
    {
        for (dir, (len, offset)) in zip(dirs, zip(lens, offsets)) {
            Self::one(dir, len, offset, &mut set_ctx);
        }
    }
}
