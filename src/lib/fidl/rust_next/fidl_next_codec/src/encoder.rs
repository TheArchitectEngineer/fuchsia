// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The core [`Encoder`] trait.

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::slice::from_raw_parts;

use crate::{Chunk, Encode, EncodeError, WireU64, ZeroPadding, CHUNK_SIZE};

/// An encoder for FIDL handles (internal).
pub trait InternalHandleEncoder {
    /// Returns the number of handles written to the encoder.
    ///
    /// This method exposes details about Fuchsia resources that plain old FIDL shouldn't need to
    /// know about. Do not use this method outside of this crate.
    #[doc(hidden)]
    fn __internal_handle_count(&self) -> usize;
}

/// An encoder for FIDL messages.
pub trait Encoder: InternalHandleEncoder {
    /// Returns the number of bytes written to the encoder.
    fn bytes_written(&self) -> usize;

    /// Writes zeroed bytes to the end of the encoder.
    ///
    /// Additional bytes are written to pad the written data to a multiple of [`CHUNK_SIZE`].
    fn write_zeroes(&mut self, len: usize);

    /// Copies bytes to the end of the encoder.
    ///
    /// Additional bytes are written to pad the written data to a multiple of [`CHUNK_SIZE`].
    fn write(&mut self, bytes: &[u8]);

    /// Rewrites bytes at a position in the encoder.
    fn rewrite(&mut self, pos: usize, bytes: &[u8]);
}

impl InternalHandleEncoder for Vec<Chunk> {
    #[inline]
    fn __internal_handle_count(&self) -> usize {
        0
    }
}

impl Encoder for Vec<Chunk> {
    #[inline]
    fn bytes_written(&self) -> usize {
        self.len() * CHUNK_SIZE
    }

    #[inline]
    fn write_zeroes(&mut self, len: usize) {
        let count = len.div_ceil(CHUNK_SIZE);
        self.reserve(count);
        let ptr = unsafe { self.as_mut_ptr().add(self.len()) };
        unsafe {
            ptr.write_bytes(0, count);
        }
        unsafe {
            self.set_len(self.len() + count);
        }
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let count = bytes.len().div_ceil(CHUNK_SIZE);
        self.reserve(count);

        // Zero out the last chunk
        unsafe {
            self.as_mut_ptr().add(self.len() + count - 1).write(WireU64(0));
        }
        let ptr = unsafe { self.as_mut_ptr().add(self.len()).cast::<u8>() };

        // Copy all the bytes
        unsafe {
            ptr.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
        }

        // Set the new length
        unsafe {
            self.set_len(self.len() + count);
        }
    }

    #[inline]
    fn rewrite(&mut self, pos: usize, bytes: &[u8]) {
        assert!(pos + bytes.len() <= self.bytes_written());

        let ptr = unsafe { self.as_mut_ptr().cast::<u8>().add(pos) };
        unsafe {
            ptr.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
        }
    }
}

/// Extension methods for [`Encoder`].
pub trait EncoderExt {
    /// Pre-allocates space for a slice of elements.
    fn preallocate<T>(&mut self, len: usize) -> Preallocated<'_, Self, T>;

    /// Encodes an iterator of elements.
    ///
    /// Returns `Err` if encoding failed.
    fn encode_next_iter<T: Encode<Self>>(
        &mut self,
        values: impl ExactSizeIterator<Item = T>,
    ) -> Result<(), EncodeError>;

    /// Encodes a value.
    ///
    /// Returns `Err` if encoding failed.
    fn encode_next<T: Encode<Self>>(&mut self, value: T) -> Result<(), EncodeError>;
}

impl<E: Encoder + ?Sized> EncoderExt for E {
    fn preallocate<T>(&mut self, len: usize) -> Preallocated<'_, Self, T> {
        let pos = self.bytes_written();

        // Zero out the next `count` bytes
        self.write_zeroes(len * size_of::<T>());

        Preallocated {
            encoder: self,
            pos,
            #[cfg(debug_assertions)]
            remaining: len,
            _phantom: PhantomData,
        }
    }

    fn encode_next_iter<T: Encode<Self>>(
        &mut self,
        values: impl ExactSizeIterator<Item = T>,
    ) -> Result<(), EncodeError> {
        let mut outputs = self.preallocate::<T::Encoded>(values.len());

        let mut out = MaybeUninit::<T::Encoded>::uninit();
        <T::Encoded as ZeroPadding>::zero_padding(&mut out);
        for value in values {
            value.encode(outputs.encoder, &mut out)?;
            unsafe {
                outputs.write_next(out.assume_init_ref());
            }
        }

        Ok(())
    }

    fn encode_next<T: Encode<Self>>(&mut self, value: T) -> Result<(), EncodeError> {
        self.encode_next_iter(core::iter::once(value))
    }
}

/// A pre-allocated slice of elements
pub struct Preallocated<'a, E: ?Sized, T> {
    /// The encoder.
    pub encoder: &'a mut E,
    pos: usize,
    #[cfg(debug_assertions)]
    remaining: usize,
    _phantom: PhantomData<T>,
}

impl<E: Encoder + ?Sized, T> Preallocated<'_, E, T> {
    /// Writes into the next pre-allocated slot in the encoder.
    ///
    /// # Safety
    ///
    /// All of the bytes of `value` must be initialized, including padding.
    pub unsafe fn write_next(&mut self, value: &T) {
        #[cfg(debug_assertions)]
        {
            assert!(self.remaining > 0, "attemped to write more slots than preallocated");
            self.remaining -= 1;
        }

        let bytes_ptr = (value as *const T).cast::<u8>();
        let bytes = unsafe { from_raw_parts(bytes_ptr, size_of::<T>()) };
        self.encoder.rewrite(self.pos, bytes);
        self.pos += size_of::<T>();
    }
}
