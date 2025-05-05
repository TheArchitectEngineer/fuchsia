// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::util::cobalt_logger::log_cobalt_batch;
use fidl_fuchsia_metrics::{MetricEvent, MetricEventPayload};
use fuchsia_inspect::Node as InspectNode;
use fuchsia_inspect_contrib::inspect_log;
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use {fuchsia_async as fasync, wlan_legacy_metrics_registry as metrics, zx};

pub const INSPECT_TOGGLE_EVENTS_LIMIT: usize = 20;
const TIME_QUICK_TOGGLE_WIFI: zx::BootDuration = zx::BootDuration::from_seconds(5);

#[derive(Debug, PartialEq)]
pub enum ClientConnectionsToggleEvent {
    Enabled,
    Disabled,
}

pub struct ToggleLogger {
    toggle_inspect_node: BoundedListNode,
    cobalt_proxy: fidl_fuchsia_metrics::MetricEventLoggerProxy,
    /// This is None until telemetry is notified of an off or on event, since these metrics don't
    /// currently need to know the starting state.
    current_state: Option<ClientConnectionsToggleEvent>,
    /// The last time wlan was toggled on
    time_started: Option<fasync::BootInstant>,
    /// The last time wlan was toggled off, or None if it hasn't been. Used to determine if WLAN
    /// was turned on right after being turned off.
    time_stopped: Option<fasync::BootInstant>,
}

impl ToggleLogger {
    pub fn new(
        cobalt_proxy: fidl_fuchsia_metrics::MetricEventLoggerProxy,
        inspect_node: &InspectNode,
    ) -> Self {
        // Initialize inspect children
        let toggle_events = inspect_node.create_child("client_connections_toggle_events");
        let toggle_inspect_node = BoundedListNode::new(toggle_events, INSPECT_TOGGLE_EVENTS_LIMIT);
        let current_state = None;
        let time_started = None;
        let time_stopped = None;

        Self { toggle_inspect_node, cobalt_proxy, current_state, time_started, time_stopped }
    }

    pub async fn log_toggle_event(&mut self, event_type: ClientConnectionsToggleEvent) {
        // This inspect macro logs the time as well
        inspect_log!(self.toggle_inspect_node, {
            event_type: std::format!("{:?}", event_type)
        });

        let mut metric_events = vec![];
        let now = fasync::BootInstant::now();
        match &event_type {
            ClientConnectionsToggleEvent::Enabled => {
                // Log an occurrence if the client connection was not already enabled
                if self.current_state != Some(ClientConnectionsToggleEvent::Enabled) {
                    self.time_started = Some(now);

                    metric_events.push(MetricEvent {
                        metric_id: metrics::CLIENT_CONNECTION_ENABLED_OCCURRENCE_METRIC_ID,
                        event_codes: vec![],
                        payload: MetricEventPayload::Count(1),
                    });
                }

                // If connections were just disabled before this, log a metric for the quick wifi
                // restart.
                if self.current_state == Some(ClientConnectionsToggleEvent::Disabled) {
                    if let Some(time_stopped) = self.time_stopped {
                        if now - time_stopped < TIME_QUICK_TOGGLE_WIFI {
                            metric_events.push(MetricEvent {
                                metric_id: metrics::CLIENT_CONNECTIONS_STOP_AND_START_METRIC_ID,
                                event_codes: vec![],
                                payload: MetricEventPayload::Count(1),
                            });
                        }
                    }
                }
            }
            ClientConnectionsToggleEvent::Disabled => {
                // Only change the time and log duration if connections were not already disabled.
                if self.current_state == Some(ClientConnectionsToggleEvent::Enabled) {
                    self.time_stopped = Some(now);

                    if let Some(time_started) = self.time_started {
                        let duration = now - time_started;
                        metric_events.push(MetricEvent {
                            metric_id: metrics::CLIENT_CONNECTION_ENABLED_DURATION_METRIC_ID,
                            event_codes: vec![],
                            payload: MetricEventPayload::IntegerValue(duration.into_millis()),
                        });
                    }
                }
            }
        }
        self.current_state = Some(event_type);

        if !metric_events.is_empty() {
            log_cobalt_batch!(self.cobalt_proxy, &metric_events, "log_toggle_events");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{setup_test, TestHelper};
    use diagnostics_assertions::{assert_data_tree, AnyNumericProperty};
    use futures::task::Poll;
    use std::pin::pin;
    use wlan_common::assert_variant;

    #[fuchsia::test]
    fn test_toggle_is_recorded_to_inspect() {
        let mut test_helper = setup_test();
        let node = test_helper.create_inspect_node("wlan_mock_node");
        let mut toggle_logger = ToggleLogger::new(test_helper.cobalt_proxy.clone(), &node);

        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        let event = ClientConnectionsToggleEvent::Disabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        assert_data_tree!(test_helper.inspector, root: contains {
            wlan_mock_node: {
                client_connections_toggle_events: {
                    "0": {
                        "event_type": "Enabled",
                        "@time": AnyNumericProperty
                    },
                    "1": {
                        "event_type": "Disabled",
                        "@time": AnyNumericProperty
                    },
                    "2": {
                        "event_type": "Enabled",
                        "@time": AnyNumericProperty
                    },
                }
            }
        });
    }

    // Uses the test helper to run toggle_logger.log_toggle_event so that any cobalt metrics sent
    // will be acked and not block anything. It expects no response from the log_toggle_event.
    fn run_log_toggle_event(
        test_helper: &mut TestHelper,
        toggle_logger: &mut ToggleLogger,
        event: ClientConnectionsToggleEvent,
    ) {
        let mut test_fut = pin!(toggle_logger.log_toggle_event(event));
        assert_eq!(
            test_helper.run_until_stalled_drain_cobalt_events(&mut test_fut),
            Poll::Ready(())
        );
    }

    #[fuchsia::test]
    fn test_quick_toggle_metric_is_recorded() {
        let mut test_helper = setup_test();
        let inspect_node = test_helper.create_inspect_node("test_stats");
        let mut toggle_logger = ToggleLogger::new(test_helper.cobalt_proxy.clone(), &inspect_node);

        // Start with client connections enabled.
        let mut test_time = fasync::MonotonicInstant::from_nanos(123);
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        // Stop client connections and quickly start them again.
        test_time += fasync::MonotonicDuration::from_minutes(40);
        test_helper.exec.set_fake_time(test_time);
        let event = ClientConnectionsToggleEvent::Disabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        test_time += fasync::MonotonicDuration::from_seconds(1);
        test_helper.exec.set_fake_time(test_time);
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        // Check that a metric is logged for the quick stop and start.
        let logged_metrics =
            test_helper.get_logged_metrics(metrics::CLIENT_CONNECTIONS_STOP_AND_START_METRIC_ID);
        assert_variant!(&logged_metrics[..], [metric] => {
            let expected_metric = fidl_fuchsia_metrics::MetricEvent {
                metric_id: metrics::CLIENT_CONNECTIONS_STOP_AND_START_METRIC_ID,
                event_codes: vec![],
                payload: fidl_fuchsia_metrics::MetricEventPayload::Count(1),
            };
            assert_eq!(metric, &expected_metric);
        });
    }

    #[fuchsia::test]
    fn test_quick_toggle_no_metric_is_recorded_if_not_quick() {
        let mut test_helper = setup_test();
        let inspect_node = test_helper.create_inspect_node("test_stats");
        let mut toggle_logger = ToggleLogger::new(test_helper.cobalt_proxy.clone(), &inspect_node);

        // Start with client connections enabled.
        let mut test_time = fasync::MonotonicInstant::from_nanos(123);
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        // Stop client connections and a while later start them again.
        test_time += fasync::MonotonicDuration::from_minutes(20);
        test_helper.exec.set_fake_time(test_time);
        let event = ClientConnectionsToggleEvent::Disabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        test_time += fasync::MonotonicDuration::from_minutes(30);
        test_helper.exec.set_fake_time(test_time);
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        // Check that no metric is logged for quick toggles since there was a while between the
        // stop and start.
        let logged_metrics =
            test_helper.get_logged_metrics(metrics::CLIENT_CONNECTIONS_STOP_AND_START_METRIC_ID);
        assert!(logged_metrics.is_empty());
    }

    #[fuchsia::test]
    fn test_quick_toggle_metric_second_disable_doesnt_update_time() {
        // Verify that if two consecutive disables happen, only the first is used to determine
        // quick toggles since the following ones don't change the state.
        let mut test_helper = setup_test();
        let inspect_node = test_helper.create_inspect_node("test_stats");
        let mut toggle_logger = ToggleLogger::new(test_helper.cobalt_proxy.clone(), &inspect_node);

        // Start with client connections enabled.
        let mut test_time = fasync::MonotonicInstant::from_nanos(123);
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        // Stop client connections and a while later stop them again.
        test_time += fasync::MonotonicDuration::from_minutes(40);
        test_helper.exec.set_fake_time(test_time);
        let event = ClientConnectionsToggleEvent::Disabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        test_time += fasync::MonotonicDuration::from_minutes(30);
        test_helper.exec.set_fake_time(test_time);
        let event = ClientConnectionsToggleEvent::Disabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        // Start client connections right after the last disable message.
        test_time += fasync::MonotonicDuration::from_seconds(1);
        test_helper.exec.set_fake_time(test_time);
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        // Check that no metric is logged since the enable message came a while after the first
        // disable message.
        let logged_metrics =
            test_helper.get_logged_metrics(metrics::CLIENT_CONNECTIONS_STOP_AND_START_METRIC_ID);
        assert!(logged_metrics.is_empty());
    }

    #[fuchsia::test]
    fn test_log_client_connection_enabled() {
        let mut test_helper = setup_test();
        let inspect_node = test_helper.create_inspect_node("test_stats");
        let mut toggle_logger = ToggleLogger::new(test_helper.cobalt_proxy.clone(), &inspect_node);

        // Start with client connections enabled.
        test_helper.exec.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000));
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        let metrics =
            test_helper.get_logged_metrics(metrics::CLIENT_CONNECTION_ENABLED_OCCURRENCE_METRIC_ID);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].payload, MetricEventPayload::Count(1));

        // Send enabled event again. This should not log any metric because the device was not
        // in a disabled state
        test_helper.clear_cobalt_events();
        test_helper.exec.set_fake_time(fasync::MonotonicInstant::from_nanos(50_000_000));
        let event = ClientConnectionsToggleEvent::Enabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        let metrics =
            test_helper.get_logged_metrics(metrics::CLIENT_CONNECTION_ENABLED_OCCURRENCE_METRIC_ID);
        assert!(metrics.is_empty());

        // Send disabled event. This should log the duration between now and when the
        // first enabled event was sent.
        test_helper.clear_cobalt_events();
        test_helper.exec.set_fake_time(fasync::MonotonicInstant::from_nanos(100_000_000));
        let event = ClientConnectionsToggleEvent::Disabled;
        run_log_toggle_event(&mut test_helper, &mut toggle_logger, event);

        let metrics =
            test_helper.get_logged_metrics(metrics::CLIENT_CONNECTION_ENABLED_DURATION_METRIC_ID);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].payload, MetricEventPayload::IntegerValue(90));
    }
}
