// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::writer::{
    ArithmeticArrayProperty, ArrayProperty, HistogramProperty, InspectType, IntArrayProperty, Node,
};
use diagnostics_hierarchy::{ArrayFormat, ExponentialHistogramParams};
use inspect_format::constants;
use log::error;
use std::borrow::Cow;

#[derive(Debug, Default)]
/// An exponential histogram property for int values.
pub struct IntExponentialHistogramProperty {
    array: IntArrayProperty,
    floor: i64,
    initial_step: i64,
    step_multiplier: i64,
    slots: usize,
}

impl InspectType for IntExponentialHistogramProperty {}

impl IntExponentialHistogramProperty {
    pub(crate) fn new(
        name: Cow<'_, str>,
        params: ExponentialHistogramParams<i64>,
        parent: &Node,
    ) -> Self {
        let slots = params.buckets + constants::EXPONENTIAL_HISTOGRAM_EXTRA_SLOTS;
        let array =
            parent.create_int_array_internal(name, slots, ArrayFormat::ExponentialHistogram);
        array.set(0, params.floor);
        array.set(1, params.initial_step);
        array.set(2, params.step_multiplier);
        Self {
            floor: params.floor,
            initial_step: params.initial_step,
            step_multiplier: params.step_multiplier,
            slots,
            array,
        }
    }

    fn get_index(&self, value: i64) -> usize {
        let mut current_floor = self.floor;
        let mut offset = self.initial_step;
        // Start in the underflow index.
        let mut index = constants::EXPONENTIAL_HISTOGRAM_EXTRA_SLOTS - 2;
        while value >= current_floor && index < self.slots - 1 {
            current_floor = self.floor + offset;
            offset *= self.step_multiplier;
            index += 1;
        }
        index
    }
}

impl HistogramProperty for IntExponentialHistogramProperty {
    type Type = i64;

    fn insert(&self, value: i64) {
        self.insert_multiple(value, 1);
    }

    fn insert_multiple(&self, value: i64, count: usize) {
        self.array.add(self.get_index(value), count as i64);
    }

    fn clear(&self) {
        if let Some(ref inner_ref) = self.array.inner.inner_ref() {
            // Ensure we don't delete the array slots that contain histogram metadata.
            inner_ref
                .state
                .try_lock()
                .and_then(|mut state| {
                    // -2 = the overflow and underflow slots which still need to be cleared.
                    state.clear_array(
                        inner_ref.block_index,
                        constants::EXPONENTIAL_HISTOGRAM_EXTRA_SLOTS - 2,
                    )
                })
                .unwrap_or_else(|err| {
                    error!(err:?; "Failed to clear property");
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::testing_utils::GetBlockExt;
    use crate::writer::Inspector;
    use inspect_format::{Array, Int};

    #[fuchsia::test]
    fn test_int_exp_histogram() {
        let inspector = Inspector::default();
        let root = inspector.root();
        let node = root.create_child("node");
        {
            let int_histogram = node.create_int_exponential_histogram(
                "int-histogram",
                ExponentialHistogramParams {
                    floor: 1,
                    initial_step: 1,
                    step_multiplier: 2,
                    buckets: 4,
                },
            );
            int_histogram.insert_multiple(-1, 2); // underflow
            int_histogram.insert(8);
            int_histogram.insert(500); // overflow
            int_histogram.array.get_block::<_, Array<Int>>(|block| {
                for (i, value) in [1, 1, 2, 2, 0, 0, 0, 1, 1].iter().enumerate() {
                    assert_eq!(block.get(i).unwrap(), *value);
                }
            });

            node.get_block::<_, inspect_format::Node>(|node_block| {
                assert_eq!(node_block.child_count(), 1);
            });
        }
        node.get_block::<_, inspect_format::Node>(|node_block| {
            assert_eq!(node_block.child_count(), 0);
        });
    }

    #[fuchsia::test]
    fn exp_histogram_insert() {
        let inspector = Inspector::default();
        let root = inspector.root();
        let hist = root.create_int_exponential_histogram(
            "test",
            ExponentialHistogramParams {
                floor: 0,
                initial_step: 2,
                step_multiplier: 4,
                buckets: 4,
            },
        );
        for i in -200..200 {
            hist.insert(i);
        }
        hist.array.get_block::<_, Array<Int>>(|block| {
            assert_eq!(block.get(0).unwrap(), 0);
            assert_eq!(block.get(1).unwrap(), 2);
            assert_eq!(block.get(2).unwrap(), 4);

            // Buckets
            let i = 3;
            assert_eq!(block.get(i).unwrap(), 200);
            assert_eq!(block.get(i + 1).unwrap(), 2);
            assert_eq!(block.get(i + 2).unwrap(), 6);
            assert_eq!(block.get(i + 3).unwrap(), 24);
            assert_eq!(block.get(i + 4).unwrap(), 96);
            assert_eq!(block.get(i + 5).unwrap(), 72);
        });
    }
}
