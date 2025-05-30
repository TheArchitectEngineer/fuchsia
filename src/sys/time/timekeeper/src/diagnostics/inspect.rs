// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::diagnostics::{Diagnostics, Event};
use crate::enums::{
    ClockCorrectionStrategy, ClockUpdateReason, FrequencyDiscardReason, InitializeRtcOutcome, Role,
    SampleValidationError, TimeSourceError, Track, UserAdjustUtcOutcome, WriteRtcOutcome,
};
use crate::{MonitorTrack, PrimaryTrack, TimeSource};
use fidl_fuchsia_time_external::Status;
use fuchsia_inspect::{
    Inspector, IntProperty, Node, NumericProperty, Property, StringProperty, UintProperty,
};
use fuchsia_runtime::{UtcClock, UtcClockDetails, UtcDuration, UtcInstant};
use fuchsia_sync::Mutex;

use futures::FutureExt;
use inspect_writable::{InspectWritable, InspectWritableNode};
use lazy_static::lazy_static;
use log::warn;
use std::collections::HashMap;
use std::sync::Arc;

const ONE_MILLION: i32 = 1_000_000;
/// The value stored in place of any time that could not be generated.
const FAILED_TIME: i64 = -1;
/// The number of Kalman filter state updates that are retained.
const FILTER_STATE_COUNT: usize = 5;
/// The number of frequency estimates that are retained.
const FREQUENCY_COUNT: usize = 3;
/// The number of clock corrections that are retained.
const CLOCK_CORRECTION_COUNT: usize = 3;

lazy_static! {
    pub static ref INSPECTOR: Inspector = Inspector::default();
}

fn reference_time() -> i64 {
    zx::BootInstant::get().into_nanos()
}

/// A vector of inspect nodes used to store some struct implementing `InspectWritable`, where the
/// contents of the oldest node are replaced on each write.
///
/// An 'counter' field is added to each node labeling which write led to the current node contents.
/// The first write will have a counter of 1 and the node with the highest counter value is always
/// the most recently written.
///
/// Potentially this is worth moving into a library at some point.
pub struct CircularBuffer<T: InspectWritable + Default> {
    count: usize,
    nodes: Vec<T::NodeType>,
    counters: Vec<UintProperty>,
}

impl<T: InspectWritable + Default> CircularBuffer<T> {
    /// Construct a new `CircularBuffer` of the supplied size within the supplied parent node.
    /// Each node is named with the supplied prefix and an integer suffix, all nodes are initialized
    /// to default values.
    fn new(size: usize, prefix: &str, node: &Node) -> Self {
        let mut nodes: Vec<T::NodeType> = Vec::new();
        let mut counters: Vec<UintProperty> = Vec::new();
        for i in 0..size {
            let child = node.create_child(format!("{}{}", prefix, i));
            counters.push(child.create_uint("counter", 0));
            nodes.push(T::default().create(child));
        }
        CircularBuffer { count: 0, nodes, counters }
    }

    /// Write the supplied data into the oldest node in the circular buffer.
    fn update(&mut self, data: &T) {
        let index = self.count % self.nodes.len();
        self.count += 1;
        self.nodes[index].update(data);
        self.counters[index].set(self.count as u64);
    }
}

/// A representation of a point in time as measured by all pertinent clocks.
#[derive(InspectWritable)]
pub struct TimeSet {
    /// The reference time, in ns.
    reference: i64,
    /// The UTC UtcClock time, in ns.
    clock_utc: i64,
}

impl TimeSet {
    /// Creates a new `TimeSet` set to current time.
    pub fn now(clock: &UtcClock) -> Self {
        TimeSet {
            reference: reference_time(),
            clock_utc: clock.read().map(UtcInstant::into_nanos).unwrap_or(FAILED_TIME),
        }
    }
}

/// A representation of a single update to a UTC UtcClock.
#[derive(InspectWritable)]
pub struct ClockDetails {
    /// The reference time at which the details were retrieved. Note this is the time the Rust
    /// object was created, which may not exactly match the time its contents were supplied by
    /// the kernel.
    retrieval_reference: i64,
    /// The generation counter as documented in the UtcClock.
    generation_counter: u32,
    /// The reference time from the monotonic-UTC correspondence pair, in ns.
    reference_offset: i64,
    /// The UTC time from the reference-UTC correspondence pair, in ns.
    utc_offset: i64,
    /// The ratio between UTC tick rate and reference tick rate in parts per million, expressed as
    /// a PPM deviation from nominal. A positive number means UTC is running faster than reference.
    rate_ppm: i32,
    /// The error bounds as documented in the UtcClock.
    error_bounds: u64,
    /// The reason this clock update occurred, if known.
    reason: Option<ClockUpdateReason>,
}

impl ClockDetails {
    /// Attaches a reason for the clock update.
    pub fn with_reason(mut self, reason: ClockUpdateReason) -> Self {
        self.reason = Some(reason);
        self
    }
}

impl From<UtcClockDetails> for ClockDetails {
    fn from(details: UtcClockDetails) -> ClockDetails {
        // Handle the potential for a divide by zero in an unset rate.
        let rate_ppm = match (
            details.reference_to_synthetic.rate.synthetic_ticks,
            details.reference_to_synthetic.rate.reference_ticks,
        ) {
            (0, _) => -ONE_MILLION,
            (_, 0) => std::i32::MAX,
            (syn, refr) => ((syn as i64 * ONE_MILLION as i64) / refr as i64) as i32 - ONE_MILLION,
        };
        ClockDetails {
            retrieval_reference: reference_time(),
            generation_counter: details.generation_counter,
            reference_offset: details.reference_to_synthetic.reference_offset.into_nanos(),
            utc_offset: details.reference_to_synthetic.synthetic_offset.into_nanos(),
            rate_ppm,
            error_bounds: details.error_bounds,
            reason: None,
        }
    }
}

/// An inspect `Node` and properties used to describe interactions with a real time clock.
struct RealTimeClockNode {
    /// The number of successful writes to the RTC.
    write_success_counter: UintProperty,
    /// The number of failed writes to the RTC.
    write_failure_counter: UintProperty,
    /// The inspect Node these fields are exported to.
    _node: Node,
}

impl RealTimeClockNode {
    /// Constructs a new `RealTimeClockNode`, recording the initial state.
    pub fn new(
        node: Node,
        outcome: InitializeRtcOutcome,
        initial_time: Option<UtcInstant>,
    ) -> Self {
        node.record_string("initialization", format!("{:?}", outcome));
        if let Some(time) = initial_time {
            node.record_int("initial_time", time.into_nanos());
        }
        RealTimeClockNode {
            write_success_counter: node.create_uint("write_successes", 0u64),
            write_failure_counter: node.create_uint("write_failures", 0u64),
            _node: node,
        }
    }

    /// Records an attempt to write to the clock.
    pub fn write(&mut self, outcome: WriteRtcOutcome) {
        match outcome {
            WriteRtcOutcome::Succeeded => self.write_success_counter.add(1),
            WriteRtcOutcome::Failed => self.write_failure_counter.add(1),
        };
    }
}

#[derive(Debug)]
enum UserAdjustResult {
    Unspecified,
    Success,
    Failure,
}

/// An inspect `Node` and properties used to describe the history of user UTC
/// adjustments.
struct UserAdjustUtcNode {
    /// The count of successful user adjustment UTC attempts.
    success_count: UintProperty,
    /// The count of failed user adjustment UTC attempts.
    failure_count: UintProperty,
    /// The value of the offset resulting from the last successful proposal.
    last_allowed_offset_nanos: IntProperty,
    /// Was last update was a failure?
    last_update_result: StringProperty,
    /// The boot timestamp at which the last UTC adjustment was applied.
    last_update_timestamp_nanos: IntProperty,
    /// The inspect node these fields are exported to.
    _node: Node,
}

impl UserAdjustUtcNode {
    pub fn new(node: Node) -> Self {
        Self {
            success_count: node.create_uint("success_count", 0),
            failure_count: node.create_uint("failure_count", 0),
            last_allowed_offset_nanos: node.create_int("last_proposed_offset_nanos", 0),
            last_update_result: node.create_string(
                "last_update_result",
                &format!("{:?}", UserAdjustResult::Unspecified),
            ),
            last_update_timestamp_nanos: node.create_int("last_update_timestamp_nanos", 0),
            _node: node,
        }
    }
}

impl UserAdjustUtcNode {
    fn update_user_adjust_utc(&mut self, outcome: UserAdjustUtcOutcome, offset: UtcDuration) {
        self.last_update_timestamp_nanos.set(reference_time());
        match outcome {
            UserAdjustUtcOutcome::Succeeded => {
                self.success_count.add(1);
                self.last_update_result.set(&format!("{:?}", UserAdjustResult::Success));
                self.last_allowed_offset_nanos.set(offset.into_nanos());
            }
            UserAdjustUtcOutcome::Failed => {
                self.failure_count.add(1);
                self.last_update_result.set(&format!("{:?}", UserAdjustResult::Failure));
            }
        }
    }
}

/// An inspect `Node` and properties used to describe the health of a time source.
struct TimeSourceNode {
    /// The most recent status of the time source.
    status: StringProperty,
    /// The reference time at which the time source last changed.
    status_change: IntProperty,
    /// The number of time source failures for each failure mode.
    failure_counters: HashMap<TimeSourceError, UintProperty>,
    /// The number of sample validation failutes for each rejection mode.
    rejection_counters: HashMap<SampleValidationError, UintProperty>,
    /// The inspect Node these fields are exported to.
    node: Node,
}

impl TimeSourceNode {
    /// Constructs a new `TimeSourceNode`, recording the initial state.
    pub fn new(node: Node, time_source: &TimeSource) -> Self {
        node.record_string("component", format!("{:?}", time_source));
        TimeSourceNode {
            status: node.create_string("status", "Launched"),
            status_change: node.create_int("status_change_reference", reference_time()),
            failure_counters: HashMap::new(),
            rejection_counters: HashMap::new(),
            node,
        }
    }

    /// Records a change in status of the time source.
    pub fn status(&mut self, status: Status) {
        self.status.set(&format!("{:?}", &status));
        self.status_change.set(reference_time());
    }

    /// Records a failure of the time source.
    pub fn failure(&mut self, error: TimeSourceError) {
        self.status.set(&format!("Failed({:?})", error));
        self.status_change.set(reference_time());
        match self.failure_counters.get_mut(&error) {
            Some(field) => {
                let _ = field.add(1);
            }
            None => {
                let property = self.node.create_uint(&format!("failure_count_{:?}", &error), 1);
                self.failure_counters.insert(error, property);
            }
        }
    }

    /// Records a rejection of a sample produced by the time source.
    pub fn sample_rejection(&mut self, error: SampleValidationError) {
        match self.rejection_counters.get_mut(&error) {
            Some(field) => {
                let _ = field.add(1);
            }
            None => {
                let property = self.node.create_uint(&format!("rejection_count_{:?}", &error), 1);
                self.rejection_counters.insert(error, property);
            }
        }
    }
}

/// A representation of the state of a Kalman filter at a point in time.
#[derive(InspectWritable, Default)]
pub struct KalmanFilterState {
    /// The reference time at which the state applies, in nanoseconds.
    reference: i64,
    /// The estimated UTC corresponding to reference, in nanoseconds.
    utc: i64,
    /// The square root of element [0,0] of the covariance matrix, in nanoseconds.
    sqrt_covariance: u64,
}

/// A representation of a frequency estimate at a point in time.
#[derive(InspectWritable, Default)]
pub struct FrequencyState {
    /// The reference time at which the state applies, in nanoseconds.
    reference: i64,
    /// The estimated frequency as a PPM deviation from nominal. A positive number means UTC is
    /// running faster than reference, i.e. the oscillator is slow.
    rate_adjust_ppm: i32,
    /// The number of frequency windows that contributed to this estimate.
    window_count: u32,
}

/// A representation of a single planned clock correction.
#[derive(InspectWritable, Default)]
pub struct ClockCorrection {
    /// The reference time at which the clock correction was received.
    reference: i64,
    /// The change to be applied to the current clock value, in nanoseconds.
    correction: i64,
    /// The strategy that will be used to apply this correction.
    strategy: ClockCorrectionStrategy,
}

/// An inspect `Node` and properties used to describe the state and history of a time track.
struct TrackNode {
    /// A circular buffer of recent updates to the Kalman filter state.
    filter_states: CircularBuffer<KalmanFilterState>,
    /// A circular buffer of recent updates to the frequency.
    frequencies: CircularBuffer<FrequencyState>,
    /// A circular buffer of recently planned clock corrections.
    corrections: CircularBuffer<ClockCorrection>,
    /// The details of the most recent update to the clock object.
    last_update: Option<<ClockDetails as InspectWritable>::NodeType>,
    /// The number of frequency window discards for each failure mode.
    frequency_discard_counters: HashMap<FrequencyDiscardReason, UintProperty>,
    /// The clock used to determine the result of a clock update operation.
    clock: Arc<UtcClock>,
    /// The inspect `Node` these fields are exported to.
    node: Node,
}

impl TrackNode {
    /// Constructs a new `TrackNode`.
    pub fn new(node: Node, clock: Arc<UtcClock>) -> Self {
        TrackNode {
            filter_states: CircularBuffer::new(FILTER_STATE_COUNT, "filter_state_", &node),
            frequencies: CircularBuffer::new(FREQUENCY_COUNT, "frequency_", &node),
            corrections: CircularBuffer::new(CLOCK_CORRECTION_COUNT, "clock_correction_", &node),
            frequency_discard_counters: HashMap::new(),
            last_update: None,
            clock,
            node,
        }
    }

    /// Records the discard of a frequency window.
    pub fn discard_frequency(&mut self, reason: FrequencyDiscardReason) {
        match self.frequency_discard_counters.get_mut(&reason) {
            Some(field) => {
                let _ = field.add(1);
            }
            None => {
                let prop = self.node.create_uint(&format!("frequency_discard_{:?}", &reason), 1);
                self.frequency_discard_counters.insert(reason, prop);
            }
        }
    }

    /// Records a new Kalman filter update for the track.
    pub fn update_filter_state(
        &mut self,
        reference: zx::BootInstant,
        utc: UtcInstant,
        sqrt_covariance: zx::BootDuration,
    ) {
        let filter_state = KalmanFilterState {
            reference: reference.into_nanos(),
            utc: utc.into_nanos(),
            sqrt_covariance: sqrt_covariance.into_nanos() as u64,
        };
        self.filter_states.update(&filter_state);
    }

    /// Records a new frequency update for the track.
    pub fn update_frequency(
        &mut self,
        reference: zx::BootInstant,
        rate_adjust_ppm: i32,
        window_count: u32,
    ) {
        let frequency_state =
            FrequencyState { reference: reference.into_nanos(), rate_adjust_ppm, window_count };
        self.frequencies.update(&frequency_state);
    }

    /// Records a new planned correction for the clock.
    pub fn clock_correction(&mut self, correction: UtcDuration, strategy: ClockCorrectionStrategy) {
        let clock_correction = ClockCorrection {
            reference: reference_time(),
            correction: correction.into_nanos(),
            strategy,
        };
        self.corrections.update(&clock_correction);
    }

    /// Records an update to the clock object.
    pub fn update_clock(&mut self, reason: Option<ClockUpdateReason>) {
        match self.clock.get_details() {
            Ok(details) => {
                let mut details_struct: ClockDetails = details.into();
                if let Some(reason) = reason {
                    details_struct = details_struct.with_reason(reason);
                }
                if let Some(last_update_node) = &self.last_update {
                    last_update_node.update(&details_struct);
                } else {
                    self.last_update
                        .replace(details_struct.create(self.node.create_child("last_update")));
                }
            }
            Err(err) => {
                warn!("Failed to export clock update to inspect: {}", err);
            }
        };
    }
}

/// The complete set of Timekeeper information exported through Inspect.
pub struct InspectDiagnostics {
    /// Details of the health of time sources.
    time_sources: Mutex<HashMap<Role, TimeSourceNode>>,
    /// Details of the current state and history of time tracks.
    tracks: Mutex<HashMap<Track, TrackNode>>,
    /// Details of interactions with the real time clock.
    rtc: Mutex<Option<RealTimeClockNode>>,
    /// Details of user utc adjustments.
    user_utc_adjustments: Mutex<Option<UserAdjustUtcNode>>,
    /// The inspect node used to export the contents of this `InspectDiagnostics`.
    node: Node,
}

impl InspectDiagnostics {
    /// Construct a new `InspectDiagnostics` exporting at the supplied `Node` using data from
    /// the supplied clock.
    pub(crate) fn new(
        node: &Node,
        primary: &PrimaryTrack,
        optional_monitor: &Option<MonitorTrack>,
        allow_user_utc_adjustments: bool,
    ) -> Self {
        // Record fixed data directly into the node without retaining any references.
        node.record_child("initialization", |child| TimeSet::now(&primary.clock).record(child));
        let backstop = primary.clock.get_details().expect("failed to get clock details").backstop;
        node.record_int("backstop", backstop.into_nanos());

        let mut time_sources_hashmap = HashMap::new();
        let mut tracks_hashmap = HashMap::new();
        time_sources_hashmap.insert(
            Role::Primary,
            TimeSourceNode::new(node.create_child("primary_time_source"), &primary.time_source),
        );
        tracks_hashmap.insert(
            Track::Primary,
            TrackNode::new(node.create_child("primary_track"), Arc::clone(&primary.clock)),
        );

        if let Some(monitor) = optional_monitor {
            time_sources_hashmap.insert(
                Role::Monitor,
                TimeSourceNode::new(node.create_child("monitor_time_source"), &monitor.time_source),
            );
            tracks_hashmap.insert(
                Track::Monitor,
                TrackNode::new(node.create_child("monitor_track"), Arc::clone(&monitor.clock)),
            );
        }

        let user_utc_adjustments = Mutex::new(if allow_user_utc_adjustments {
            Some(UserAdjustUtcNode::new(node.create_child("user_utc_adjustments")))
        } else {
            None
        });

        let diagnostics = InspectDiagnostics {
            time_sources: Mutex::new(time_sources_hashmap),
            tracks: Mutex::new(tracks_hashmap),
            rtc: Mutex::new(None),
            node: node.clone_weak(),
            user_utc_adjustments,
        };
        let clock = Arc::clone(&primary.clock);
        node.record_lazy_child("current", move || {
            let clock_clone = Arc::clone(&clock);
            async move {
                let inspector = Inspector::default();
                TimeSet::now(&clock_clone).record(inspector.root());
                Ok(inspector)
            }
            .boxed()
        });
        diagnostics
    }

    fn update_source<F>(&self, role: Role, function: F)
    where
        F: FnOnce(&mut TimeSourceNode),
    {
        self.time_sources.lock().get_mut(&role).map(function);
    }

    fn update_track<F>(&self, track: Track, function: F)
    where
        F: FnOnce(&mut TrackNode),
    {
        self.tracks.lock().get_mut(&track).map(function);
    }
}

impl Diagnostics for InspectDiagnostics {
    fn record(&self, event: Event) {
        match event {
            Event::Initialized { .. } => {}
            Event::InitializeRtc { outcome, time } => {
                self.rtc.lock().get_or_insert_with(|| {
                    RealTimeClockNode::new(self.node.create_child("real_time_clock"), outcome, time)
                });
            }
            Event::TimeSourceFailed { role, error } => {
                self.update_source(role, |tsn| tsn.failure(error));
            }
            Event::TimeSourceStatus { role, status } => {
                self.update_source(role, |tsn| tsn.status(status));
            }
            Event::SampleRejected { role, error } => {
                self.update_source(role, |tsn| tsn.sample_rejection(error));
            }
            Event::FrequencyWindowDiscarded { track, reason } => {
                self.update_track(track, |tn| tn.discard_frequency(reason));
            }
            Event::KalmanFilterUpdated { track, reference, utc, sqrt_covariance } => {
                self.update_track(track, |tn| {
                    tn.update_filter_state(reference, utc, sqrt_covariance)
                });
            }
            Event::FrequencyUpdated { track, reference, rate_adjust_ppm, window_count } => {
                self.update_track(track, |tn| {
                    tn.update_frequency(reference, rate_adjust_ppm, window_count)
                });
            }
            Event::ClockCorrection { track, correction, strategy } => {
                self.update_track(track, |tn| tn.clock_correction(correction, strategy));
            }
            Event::WriteRtc { outcome } => {
                if let Some(ref mut rtc_node) = *self.rtc.lock() {
                    rtc_node.write(outcome);
                }
            }
            Event::StartClock { track, .. } => {
                self.update_track(track, |tn| tn.update_clock(None));
            }
            Event::UpdateClock { track, reason } => {
                self.update_track(track, |tn| tn.update_clock(Some(reason)));
            }
            Event::UserAdjustUtc { outcome, offset } => {
                if let Some(ref mut utc_node) = *self.user_utc_adjustments.lock() {
                    utc_node.update_user_adjust_utc(outcome, offset);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{
        FrequencyDiscardReason as FDR, SampleValidationError as SVE, StartClockSource,
        TimeSourceError as TSE,
    };
    use crate::time_source::FakePushTimeSource;
    use diagnostics_assertions::{assert_data_tree, AnyProperty};
    use fuchsia_runtime::UtcClockUpdate;
    use lazy_static::lazy_static;

    const BACKSTOP_TIME: i64 = 111111111;
    const RTC_INITIAL_TIME: i64 = 111111234;
    const RATE_ADJUST: i32 = 222;
    const ERROR_BOUNDS: u64 = 4444444444;
    const GENERATION_COUNTER: u32 = 7777;
    const OFFSET: zx::BootDuration = zx::BootDuration::from_seconds(311);
    const CORRECTION: UtcDuration = UtcDuration::from_millis(88);
    const SQRT_COVARIANCE: i64 = 5454545454;

    lazy_static! {
        static ref VALID_DETAILS: UtcClockDetails = zx::ClockDetails {
            backstop: zx::Instant::from_nanos(BACKSTOP_TIME),
            ticks_to_synthetic: zx::ClockTransformation {
                reference_offset: zx::Instant::from_nanos(777777777777),
                synthetic_offset: zx::Instant::from_nanos(787878787878),
                rate: zx::sys::zx_clock_rate_t { reference_ticks: 1_000, synthetic_ticks: 1_000 },
            },
            reference_to_synthetic: zx::ClockTransformation {
                reference_offset: zx::Instant::from_nanos(888888888888),
                synthetic_offset: zx::Instant::from_nanos(898989898989),
                rate: zx::sys::zx_clock_rate_t {
                    reference_ticks: ONE_MILLION as u32,
                    synthetic_ticks: (RATE_ADJUST + ONE_MILLION) as u32,
                },
            },
            error_bounds: ERROR_BOUNDS,
            query_ticks: 12345789,
            last_value_update_ticks: 36363636,
            last_rate_adjust_update_ticks: 37373737,
            last_error_bounds_update_ticks: 38383838,
            generation_counter: GENERATION_COUNTER,
        };
    }

    /// Creates a new wrapped clock set to backstop time.
    fn create_clock() -> Arc<UtcClock> {
        Arc::new(
            UtcClock::create(zx::ClockOpts::empty(), Some(UtcInstant::from_nanos(BACKSTOP_TIME)))
                .unwrap(),
        )
    }

    /// Creates a new `InspectDiagnostics` object recording to the root of the supplied inspector,
    /// returning a tuple of the object and the primary clock it is using.
    fn create_test_object(
        inspector: &Inspector,
        include_monitor: bool,
    ) -> (InspectDiagnostics, Arc<UtcClock>) {
        let primary = PrimaryTrack {
            time_source: FakePushTimeSource::failing().into(),
            clock: create_clock(),
        };
        let monitor = match include_monitor {
            true => Some(MonitorTrack {
                time_source: FakePushTimeSource::failing().into(),
                clock: create_clock(),
            }),
            false => None,
        };

        (InspectDiagnostics::new(inspector.root(), &primary, &monitor, false), primary.clock)
    }

    #[fuchsia::test]
    fn valid_clock_details_conversion() {
        let details = ClockDetails::from(VALID_DETAILS.clone());
        assert_eq!(details.generation_counter, GENERATION_COUNTER);
        assert_eq!(
            details.utc_offset,
            VALID_DETAILS.reference_to_synthetic.synthetic_offset.into_nanos()
        );
        assert_eq!(
            details.reference_offset,
            VALID_DETAILS.reference_to_synthetic.reference_offset.into_nanos()
        );
        assert_eq!(details.rate_ppm, RATE_ADJUST);
        assert_eq!(details.error_bounds, ERROR_BOUNDS);
    }

    #[fuchsia::test]
    fn invalid_clock_details_conversion() {
        let mut zx_details = VALID_DETAILS.clone();
        zx_details.reference_to_synthetic.rate.synthetic_ticks = 1000;
        zx_details.reference_to_synthetic.rate.reference_ticks = 0;
        let details = ClockDetails::from(zx_details);
        assert_eq!(details.generation_counter, GENERATION_COUNTER);
        assert_eq!(
            details.utc_offset,
            VALID_DETAILS.reference_to_synthetic.synthetic_offset.into_nanos()
        );
        assert_eq!(
            details.reference_offset,
            VALID_DETAILS.reference_to_synthetic.reference_offset.into_nanos()
        );
        assert_eq!(details.rate_ppm, std::i32::MAX);
        assert_eq!(details.error_bounds, ERROR_BOUNDS);
    }

    #[fuchsia::test]
    fn after_initialization() {
        let inspector = &Inspector::default();
        let (_inspect_diagnostics, _) = create_test_object(&inspector, false);
        assert_data_tree!(
            inspector,
            root: contains {
                initialization: contains {
                    reference: AnyProperty,
                    clock_utc: AnyProperty,
                },
                backstop: BACKSTOP_TIME,
                current: contains {
                    reference: AnyProperty,
                    clock_utc: AnyProperty,
                },
                primary_time_source: contains {
                    component: "Push(FakePushTimeSource)",
                    status: "Launched",
                    status_change_reference: AnyProperty,
                },
                primary_track: contains {
                    filter_state_0: contains {
                        counter: 0u64,
                        reference: 0i64,
                        utc: 0i64,
                        sqrt_covariance: 0u64,
                    }
                    // For brevity we omit the other empty estimates we expect in the circular
                    // buffer.
                },
            }
        );
    }

    #[fuchsia::test]
    fn after_update() {
        let inspector = &Inspector::default();
        let (inspect_diagnostics, clock) = create_test_object(&inspector, false);

        // Perform two updates to the clock. The inspect data should reflect the most recent.
        let reference_time = zx::BootInstant::get();
        clock
            .update(
                UtcClockUpdate::builder()
                    .absolute_value(reference_time, UtcInstant::from_nanos(BACKSTOP_TIME + 1234))
                    .rate_adjust(0)
                    .error_bounds(0),
            )
            .expect("Failed to update test clock");
        inspect_diagnostics
            .record(Event::StartClock { track: Track::Primary, source: StartClockSource::Rtc });

        let reference_time = zx::BootInstant::get();
        clock
            .update(
                UtcClockUpdate::builder()
                    .absolute_value(reference_time, UtcInstant::from_nanos(BACKSTOP_TIME + 2345))
                    .rate_adjust(RATE_ADJUST)
                    .error_bounds(ERROR_BOUNDS),
            )
            .expect("Failed to update test clock");
        inspect_diagnostics.record(Event::UpdateClock {
            track: Track::Primary,
            reason: ClockUpdateReason::TimeStep,
        });
        assert_data_tree!(
            inspector,
            root: contains {
                initialization: contains {
                    reference: AnyProperty,
                    clock_utc: AnyProperty,
                },
                backstop: BACKSTOP_TIME,
                current: contains {
                    reference: AnyProperty,
                    clock_utc: AnyProperty,
                },
                primary_track: contains {
                    last_update: contains {
                        retrieval_reference: AnyProperty,
                        reference_offset: reference_time.into_nanos() as i64,
                        utc_offset: (BACKSTOP_TIME + 2345) as i64,
                        rate_ppm: RATE_ADJUST as i64,
                        error_bounds: ERROR_BOUNDS,
                        reason: "Some(TimeStep)",
                    },
                }
            }
        );
    }

    #[fuchsia::test]
    fn real_time_clock() {
        let inspector = &Inspector::default();
        let (inspect_diagnostics, _) = create_test_object(&inspector, false);
        inspect_diagnostics.record(Event::InitializeRtc {
            outcome: InitializeRtcOutcome::Succeeded,
            time: Some(UtcInstant::from_nanos(RTC_INITIAL_TIME)),
        });
        assert_data_tree!(
            inspector,
            root: contains {
                real_time_clock: contains {
                    initialization: "Succeeded",
                    initial_time: RTC_INITIAL_TIME,
                    write_successes: 0u64,
                    write_failures: 0u64,
                }
            }
        );

        inspect_diagnostics.record(Event::WriteRtc { outcome: WriteRtcOutcome::Succeeded });
        inspect_diagnostics.record(Event::WriteRtc { outcome: WriteRtcOutcome::Succeeded });
        inspect_diagnostics.record(Event::WriteRtc { outcome: WriteRtcOutcome::Failed });
        assert_data_tree!(
            inspector,
            root: contains {
                real_time_clock: contains {
                    initialization: "Succeeded",
                    initial_time: RTC_INITIAL_TIME,
                    write_successes: 2u64,
                    write_failures: 1u64,
                }
            }
        );
    }

    #[fuchsia::test]
    fn time_sources() {
        let inspector = &Inspector::default();
        let (test, _) = create_test_object(&inspector, true);
        assert_data_tree!(
            inspector,
            root: contains {
                primary_time_source: contains {
                    component: "Push(FakePushTimeSource)",
                    status: "Launched",
                    status_change_reference: AnyProperty,
                },
                monitor_time_source: contains {
                    component: "Push(FakePushTimeSource)",
                    status: "Launched",
                    status_change_reference: AnyProperty,
                }
            }
        );

        test.record(Event::TimeSourceFailed { role: Role::Primary, error: TSE::LaunchFailed });
        test.record(Event::TimeSourceFailed { role: Role::Primary, error: TSE::CallFailed });
        test.record(Event::TimeSourceStatus { role: Role::Primary, status: Status::Ok });
        test.record(Event::SampleRejected { role: Role::Primary, error: SVE::BeforeBackstop });
        test.record(Event::TimeSourceFailed { role: Role::Primary, error: TSE::CallFailed });
        test.record(Event::TimeSourceStatus { role: Role::Monitor, status: Status::Network });

        assert_data_tree!(
            inspector,
            root: contains {
                primary_time_source: contains {
                    component: "Push(FakePushTimeSource)",
                    status: "Failed(CallFailed)",
                    status_change_reference: AnyProperty,
                    failure_count_LaunchFailed: 1u64,
                    failure_count_CallFailed: 2u64,
                    rejection_count_BeforeBackstop: 1u64,
                },
                monitor_time_source: contains {
                    component: "Push(FakePushTimeSource)",
                    status: "Network",
                    status_change_reference: AnyProperty,
                }
            }
        );
    }

    #[fuchsia::test]
    fn tracks() {
        let inspector = &Inspector::default();
        let (test, _) = create_test_object(&inspector, true);
        assert_data_tree!(
            inspector,
            root: contains {
                // For brevity we only verify the uninitialized contents of one entry per buffer.
                primary_track: contains {
                    filter_state_0: contains {
                        counter: 0u64,
                        reference: 0i64,
                        utc: 0i64,
                        sqrt_covariance: 0u64,
                    },
                    filter_state_1: contains {},
                    filter_state_2: contains {},
                    filter_state_3: contains {},
                    filter_state_4: contains {},
                    frequency_0: contains {
                        counter: 0u64,
                        reference: 0i64,
                        rate_adjust_ppm: 0i64,
                        window_count: 0u64,
                    },
                    frequency_1: contains {},
                    frequency_2: contains {},
                    clock_correction_0: contains {
                        counter: 0u64,
                        reference: 0i64,
                        correction: 0i64,
                        strategy: "Step",
                    },
                    clock_correction_1: contains {},
                    clock_correction_2: contains {},
                },
                monitor_track: contains {
                    filter_state_0: contains {},
                    filter_state_1: contains {},
                    filter_state_2: contains {},
                    filter_state_3: contains {},
                    filter_state_4: contains {},
                    frequency_0: contains {},
                    frequency_1: contains {},
                    frequency_2: contains {},
                    clock_correction_0: contains {},
                    clock_correction_1: contains {},
                    clock_correction_2: contains {},
                },
            }
        );

        // Write enough to wrap all of the circular buffers
        for i in 1..8 {
            test.record(Event::KalmanFilterUpdated {
                track: Track::Primary,
                reference: zx::BootInstant::ZERO + OFFSET * i,
                utc: UtcInstant::from_nanos(BACKSTOP_TIME + OFFSET.into_nanos() * i),
                sqrt_covariance: zx::BootDuration::from_nanos(SQRT_COVARIANCE) * i,
            });
            test.record(Event::FrequencyUpdated {
                track: Track::Primary,
                reference: zx::BootInstant::ZERO + OFFSET * i,
                rate_adjust_ppm: -i as i32,
                window_count: i as u32,
            });
            test.record(Event::ClockCorrection {
                track: Track::Primary,
                correction: CORRECTION * i,
                strategy: ClockCorrectionStrategy::MaxDurationSlew,
            });
            test.record(Event::UpdateClock {
                track: Track::Primary,
                reason: ClockUpdateReason::BeginSlew,
            });
        }

        // And record a few frequency window discard events.
        let make_event = |reason| Event::FrequencyWindowDiscarded { track: Track::Primary, reason };
        test.record(make_event(FDR::InsufficientSamples));
        test.record(make_event(FDR::UtcBeforeWindow));
        test.record(make_event(FDR::InsufficientSamples));

        assert_data_tree!(
            inspector,
            root: contains {
                primary_track: contains {
                    filter_state_0: contains {
                        counter: 6u64,
                        reference: 6 * OFFSET.into_nanos(),
                        utc: BACKSTOP_TIME + 6 * OFFSET.into_nanos(),
                        sqrt_covariance: 6 * SQRT_COVARIANCE as u64,
                    },
                    filter_state_1: contains {
                        counter: 7u64,
                        reference: 7 * OFFSET.into_nanos(),
                        utc: BACKSTOP_TIME + 7 * OFFSET.into_nanos(),
                        sqrt_covariance: 7 * SQRT_COVARIANCE as u64,
                    },
                    filter_state_2: contains {
                        counter: 3u64,
                        reference: 3 * OFFSET.into_nanos(),
                        utc: BACKSTOP_TIME + 3 * OFFSET.into_nanos(),
                        sqrt_covariance: 3 * SQRT_COVARIANCE as u64,
                    },
                    filter_state_3: contains {
                        counter: 4u64,
                        reference: 4 * OFFSET.into_nanos(),
                        utc: BACKSTOP_TIME + 4 * OFFSET.into_nanos(),
                        sqrt_covariance: 4 * SQRT_COVARIANCE as u64,
                    },
                    filter_state_4: contains {
                        counter: 5u64,
                        reference: 5 * OFFSET.into_nanos(),
                        utc: BACKSTOP_TIME + 5 * OFFSET.into_nanos(),
                        sqrt_covariance: 5 * SQRT_COVARIANCE as u64,
                    },
                    frequency_0: contains {
                        counter: 7u64,
                        reference: 7 * OFFSET.into_nanos(),
                        rate_adjust_ppm: -7i64,
                        window_count: 7u64,
                    },
                    frequency_1: contains {
                        counter: 5u64,
                        reference: 5 * OFFSET.into_nanos(),
                        rate_adjust_ppm: -5i64,
                        window_count: 5u64,
                    },
                    frequency_2: contains {
                        counter: 6u64,
                        reference: 6 * OFFSET.into_nanos(),
                        rate_adjust_ppm: -6i64,
                        window_count: 6u64,
                    },
                    clock_correction_0: contains {
                        counter: 7u64,
                        reference: AnyProperty,
                        correction: 7 * CORRECTION.into_nanos(),
                        strategy: "MaxDurationSlew",
                    },
                    clock_correction_1: contains {
                        counter: 5u64,
                        reference: AnyProperty,
                        correction: 5 * CORRECTION.into_nanos(),
                        strategy: "MaxDurationSlew",
                    },
                    clock_correction_2: contains {
                        counter: 6u64,
                        reference: AnyProperty,
                        correction: 6 * CORRECTION.into_nanos(),
                        strategy: "MaxDurationSlew",
                    },
                    last_update: contains {
                        retrieval_reference: AnyProperty,
                        reference_offset: AnyProperty,
                        utc_offset: AnyProperty,
                        rate_ppm: AnyProperty,
                        error_bounds: AnyProperty,
                        reason: "Some(BeginSlew)",
                    },
                    frequency_discard_InsufficientSamples: 2u64,
                    frequency_discard_UtcBeforeWindow: 1u64,
                },
                monitor_track: contains {
                    filter_state_0: contains {},
                    filter_state_1: contains {},
                    filter_state_2: contains {},
                    filter_state_3: contains {},
                    filter_state_4: contains {},
                    frequency_0: contains {},
                    frequency_1: contains {},
                    frequency_2: contains {},
                    clock_correction_0: contains {},
                    clock_correction_1: contains {},
                    clock_correction_2: contains {},
                },
            }
        );
    }
}
