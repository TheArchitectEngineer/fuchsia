// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::{needs_drop, MaybeUninit};
use core::ops::Deref;
use core::ptr::{copy_nonoverlapping, NonNull};
use core::{fmt, slice};

use munge::munge;

use super::raw::RawWireVector;
use crate::{
    Decode, DecodeError, Decoder, DecoderExt as _, Encodable, Encode, EncodeError, EncodeRef,
    Encoder, EncoderExt as _, Slot, TakeFrom, WirePointer, ZeroPadding,
};

/// A FIDL vector
#[repr(transparent)]
pub struct WireVector<T> {
    raw: RawWireVector<T>,
}

unsafe impl<T> ZeroPadding for WireVector<T> {
    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { raw } = out);
        RawWireVector::<T>::zero_padding(raw);
    }
}

impl<T> Drop for WireVector<T> {
    fn drop(&mut self) {
        if needs_drop::<T>() {
            unsafe {
                self.raw.as_slice_ptr().drop_in_place();
            }
        }
    }
}

impl<T> WireVector<T> {
    /// Encodes that a vector is present in a slot.
    pub fn encode_present(out: &mut MaybeUninit<Self>, len: u64) {
        munge!(let Self { raw } = out);
        RawWireVector::encode_present(raw, len);
    }

    /// Returns the length of the vector in elements.
    pub fn len(&self) -> usize {
        self.raw.len() as usize
    }

    /// Returns whether the vector is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a pointer to the elements of the vector.
    fn as_slice_ptr(&self) -> NonNull<[T]> {
        unsafe { NonNull::new_unchecked(self.raw.as_slice_ptr()) }
    }

    /// Returns a slice of the elements of the vector.
    pub fn as_slice(&self) -> &[T] {
        unsafe { self.as_slice_ptr().as_ref() }
    }

    /// Decodes a wire vector which contains raw data.
    ///
    /// # Safety
    ///
    /// The elements of the wire vecot rmust not need to be individually decoded, and must always be
    /// valid.
    pub unsafe fn decode_raw<D>(
        mut slot: Slot<'_, Self>,
        mut decoder: &mut D,
    ) -> Result<(), DecodeError>
    where
        D: Decoder + ?Sized,
        T: Decode<D>,
    {
        munge!(let Self { raw: RawWireVector { len, mut ptr } } = slot.as_mut());

        if !WirePointer::is_encoded_present(ptr.as_mut())? {
            return Err(DecodeError::RequiredValueAbsent);
        }

        let mut slice = decoder.take_slice_slot::<T>(**len as usize)?;
        WirePointer::set_decoded(ptr, slice.as_mut_ptr().cast());

        Ok(())
    }
}

impl<T> Deref for WireVector<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T: fmt::Debug> fmt::Debug for WireVector<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_slice().fmt(f)
    }
}

unsafe impl<D: Decoder + ?Sized, T: Decode<D>> Decode<D> for WireVector<T> {
    fn decode(mut slot: Slot<'_, Self>, mut decoder: &mut D) -> Result<(), DecodeError> {
        munge!(let Self { raw: RawWireVector { len, mut ptr } } = slot.as_mut());

        if !WirePointer::is_encoded_present(ptr.as_mut())? {
            return Err(DecodeError::RequiredValueAbsent);
        }

        let mut slice = decoder.take_slice_slot::<T>(**len as usize)?;
        for i in 0..**len as usize {
            T::decode(slice.index(i), decoder)?;
        }
        WirePointer::set_decoded(ptr, slice.as_mut_ptr().cast());

        Ok(())
    }
}

#[inline]
fn encode_to_vector<V, E, T>(
    value: V,
    encoder: &mut E,
    out: &mut MaybeUninit<WireVector<T::Encoded>>,
) -> Result<(), EncodeError>
where
    V: AsRef<[T]> + IntoIterator,
    V::IntoIter: ExactSizeIterator,
    V::Item: Encode<E, Encoded = T::Encoded>,
    E: Encoder + ?Sized,
    T: Encode<E>,
{
    let len = value.as_ref().len();
    if T::COPY_OPTIMIZATION.is_enabled() {
        let slice = value.as_ref();
        // SAFETY: `T` has copy optimization enabled, which guarantees that it has no uninit bytes
        // and can be copied directly to the output instead of calling `encode`. This means that we
        // may cast `&[T]` to `&[u8]` and write those bytes.
        let bytes = unsafe { slice::from_raw_parts(slice.as_ptr().cast(), size_of_val(slice)) };
        encoder.write(bytes);
    } else {
        encoder.encode_next_iter(value.into_iter())?;
    }
    WireVector::encode_present(out, len as u64);
    Ok(())
}

impl<T: Encodable> Encodable for Vec<T> {
    type Encoded = WireVector<T::Encoded>;
}

unsafe impl<E: Encoder + ?Sized, T: Encode<E>> Encode<E> for Vec<T> {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        encode_to_vector(self, encoder, out)
    }
}

unsafe impl<E: Encoder + ?Sized, T: EncodeRef<E>> EncodeRef<E> for Vec<T> {
    fn encode_ref(
        &self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        encode_to_vector(self, encoder, out)
    }
}

impl<T: Encodable> Encodable for &[T] {
    type Encoded = WireVector<T::Encoded>;
}

unsafe impl<E: Encoder + ?Sized, T: EncodeRef<E>> Encode<E> for &[T] {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        encode_to_vector(self, encoder, out)
    }
}

impl<T: TakeFrom<WT>, WT> TakeFrom<WireVector<WT>> for Vec<T> {
    fn take_from(from: &WireVector<WT>) -> Self {
        let mut result = Vec::<T>::with_capacity(from.len());
        if T::COPY_OPTIMIZATION.is_enabled() {
            unsafe {
                copy_nonoverlapping(from.as_ptr().cast(), result.as_mut_ptr(), from.len());
            }
            unsafe {
                result.set_len(from.len());
            }
        } else {
            for item in from.as_slice() {
                result.push(T::take_from(item));
            }
        }
        result
    }
}
