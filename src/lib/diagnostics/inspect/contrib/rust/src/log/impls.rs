// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::WriteInspect;
use fuchsia_inspect::Node;
use std::borrow::Cow;

// --- Utility macros to help with implementing WriteInspect ---

macro_rules! impl_write_inspect {
    ($inspect_value_type:ident, $_self:ident => $self_expr:expr, $($ty:ty),+) => {
        $(
            impl WriteInspect for $ty {
                fn write_inspect<'a>(&$_self, writer: &Node, key: impl Into<Cow<'a, str>>) {
                    write_inspect_value!($inspect_value_type, writer, key, $self_expr);
                }
            }
        )+
    }
}

macro_rules! write_inspect_value {
    (Str, $node_writer:expr, $key:expr, $expr:expr) => {
        $node_writer.record_string($key, $expr);
    };
    (StrRef, $node_writer:expr, $key:expr, $expr:expr) => {
        $node_writer.record_string($key, $expr);
    };
    (Uint, $node_writer:expr, $key:expr, $expr:expr) => {
        $node_writer.record_uint($key, $expr);
    };
    (Int, $node_writer:expr, $key:expr, $expr:expr) => {
        $node_writer.record_int($key, $expr);
    };
    (Double, $node_writer:expr, $key:expr, $expr:expr) => {
        $node_writer.record_double($key, $expr);
    };
    (Bool, $node_writer:expr, $key:expr, $expr:expr) => {
        $node_writer.record_bool($key, $expr);
    };
}

// --- Implementations for basic types ---

impl_write_inspect!(Str, self => self, String);
impl_write_inspect!(StrRef, self => *self, &str);
impl_write_inspect!(Uint, self => (*self).into(), u8, u16, u32);
impl_write_inspect!(Uint, self => *self, u64);
impl_write_inspect!(Int, self => (*self).into(), i8, i16, i32);
impl_write_inspect!(Int, self => *self, i64);
impl_write_inspect!(Double, self => (*self).into(), f32);
impl_write_inspect!(Double, self => *self, f64);
impl_write_inspect!(Bool, self => *self, bool);

impl<V: WriteInspect + ?Sized> WriteInspect for &V {
    fn write_inspect<'a>(&self, writer: &Node, key: impl Into<Cow<'a, str>>) {
        (*self).write_inspect(writer, key)
    }
}
