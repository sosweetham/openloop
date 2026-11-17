//! Plain data that crosses the process boundary, and the atomic-word cell it lives in there
//! (docs/ARCHITECTURE.md § Runtime ↔ tetherd).

use std::cell::UnsafeCell;
use std::mem::{MaybeUninit, size_of};
use std::sync::atomic::{AtomicU64, Ordering};

/// Data that crosses the process boundary as bytes.
///
/// # Safety
/// The type has no padding bytes, every bit pattern is a valid value (so no `bool`, `char`,
/// enums or references) and its alignment is at most 8.
pub unsafe trait Pod: Copy + Send + Sync + 'static {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: a Pod type has no padding, so all of its bytes are initialized.
        unsafe { std::slice::from_raw_parts((self as *const Self).cast(), size_of::<Self>()) }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        // SAFETY: the length matches and any bit pattern is a valid Pod value.
        (bytes.len() == size_of::<Self>())
            .then(|| unsafe { bytes.as_ptr().cast::<Self>().read_unaligned() })
    }

    fn zeroed() -> Self {
        // SAFETY: any bit pattern, including all zeros, is a valid Pod value.
        unsafe { std::mem::zeroed() }
    }
}

macro_rules! pod_primitives {
    ($($t:ty),*) => { $(
        // SAFETY: primitive integers and floats have no padding and no invalid bit patterns.
        unsafe impl Pod for $t {}
    )* };
}
pod_primitives!(u8, u16, u32, u64, i32, i64, f32, f64);

// SAFETY: an array of Pod elements has no padding between them.
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

/// Declares a `#[repr(C)]` record and proves at compile time that it is [`Pod`]: its size must
/// equal the sum of its fields' sizes, so it has no padding.
macro_rules! pod {
    ($(#[$meta:meta])* pub struct $name:ident {
        $($(#[$field_meta:meta])* pub $field:ident: $ty:ty,)*
    }) => {
        $(#[$meta])*
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name {
            $($(#[$field_meta])* pub $field: $ty,)*
        }

        // SAFETY: every field is Pod (checked below) and the size check proves no padding.
        unsafe impl $crate::Pod for $name {}

        const _: () = {
            fn fields_are_pod() {
                fn pod<T: $crate::Pod>() {}
                $(pod::<$ty>();)*
            }
            let _ = fields_are_pod;
            assert!(
                ::std::mem::size_of::<$name>() == 0 $(+ ::std::mem::size_of::<$ty>())*,
                concat!(stringify!($name), " has padding")
            );
            assert!(::std::mem::align_of::<$name>() <= 8);
        };

        impl Default for $name {
            fn default() -> Self {
                <Self as $crate::Pod>::zeroed()
            }
        }
    };
}
pub(crate) use pod;

/// A `T` that another process may be writing at the same moment. Every access is a relaxed
/// atomic word, so a racing writer can leave the copy stale or torn but never undefined; the
/// structure that owns the cell supplies the ordering and detects tearing.
#[repr(C, align(8))]
pub(crate) struct WordCell<T: Pod>(UnsafeCell<MaybeUninit<T>>);

// SAFETY: the cell is only ever accessed through atomic words.
unsafe impl<T: Pod> Sync for WordCell<T> {}

impl<T: Pod> WordCell<T> {
    const WORDS: usize = {
        assert!(
            size_of::<T>().is_multiple_of(8),
            "a WordCell's type must be whole words"
        );
        size_of::<T>() / 8
    };

    fn word(&self, index: usize) -> &AtomicU64 {
        debug_assert!(index < Self::WORDS);
        // SAFETY: the cell is 8-aligned and `index` is within it. Nothing accesses the cell other
        // than through these 64-bit atomics (the region it lives in starts zeroed by the kernel).
        unsafe { AtomicU64::from_ptr(self.0.get().cast::<u64>().add(index)) }
    }

    pub(crate) fn store(&self, value: &T) {
        let words = (value as *const T).cast::<u64>();
        for index in 0..Self::WORDS {
            // SAFETY: `value` spans WORDS words, all initialized because Pod has no padding.
            let word = unsafe { words.add(index).read_unaligned() };
            self.word(index).store(word, Ordering::Relaxed);
        }
    }

    pub(crate) fn load(&self) -> T {
        let mut value = MaybeUninit::<T>::uninit();
        let words = value.as_mut_ptr().cast::<u64>();
        for index in 0..Self::WORDS {
            // SAFETY: `value` spans WORDS words.
            unsafe {
                words
                    .add(index)
                    .write_unaligned(self.word(index).load(Ordering::Relaxed))
            };
        }
        // SAFETY: every byte was written, and any bit pattern is a valid Pod value.
        unsafe { value.assume_init() }
    }
}

/// Keeps a value on its own cache line (128 bytes on Apple silicon), so a writer on one side
/// never invalidates the line a reader on the other side is polling.
#[repr(C, align(128))]
pub(crate) struct Line<T>(pub(crate) T);

#[cfg(test)]
mod tests {
    use super::*;

    pod! {
        pub struct Sample {
            pub a: u64,
            pub b: [f32; 3],
            pub c: u32,
        }
    }

    #[test]
    fn bytes_round_trip() {
        let sample = Sample {
            a: 7,
            b: [1.0, 2.5, -3.0],
            c: 9,
        };
        assert_eq!(Sample::from_bytes(sample.as_bytes()), Some(sample));
        assert_eq!(Sample::from_bytes(&sample.as_bytes()[1..]), None);
    }

    #[test]
    fn word_cell_round_trip() {
        let cell = WordCell::<Sample>(UnsafeCell::new(MaybeUninit::zeroed()));
        assert_eq!(cell.load(), Sample::default());
        let sample = Sample {
            a: u64::MAX,
            b: [f32::NAN.copysign(1.0), 0.0, 1.0],
            c: 3,
        };
        cell.store(&sample);
        assert_eq!(cell.load().as_bytes(), sample.as_bytes());
    }
}
