// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use component_events::events::*;
use component_events::matcher::*;
use fuchsia_async as fasync;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, Ref, Route};

// TODO(https://fxbug.dev/42172627): Deduplicate this function. It is used in other CM integration tests
async fn start_nested_cm_and_wait_for_clean_stop(root_url: &str, moniker_to_wait_on: &str) {
    let builder = RealmBuilder::new().await.unwrap();
    let root = builder.add_child("root", root_url, ChildOptions::new().eager()).await.unwrap();
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .capability(Capability::protocol_by_name("fuchsia.process.Launcher"))
                .capability(Capability::event_stream("started").with_scope(&root))
                .capability(Capability::event_stream("stopped").with_scope(&root))
                .capability(Capability::event_stream("destroyed").with_scope(&root))
                .capability(Capability::event_stream("capability_requested").with_scope(&root))
                .from(Ref::parent())
                .to(&root),
        )
        .await
        .unwrap();
    let instance =
        builder.build_in_nested_component_manager("#meta/component_manager.cm").await.unwrap();
    let proxy = instance.root.connect_to_protocol_at_exposed_dir().unwrap();

    let mut event_stream = EventStream::new(proxy);

    instance.start_component_tree().await.unwrap();

    // Expect the component to stop
    EventMatcher::ok()
        .stop(Some(ExitStatusMatcher::Clean))
        .moniker(moniker_to_wait_on)
        .wait::<Stopped>(&mut event_stream)
        .await
        .unwrap();
}

#[fasync::run_singlethreaded(test)]
async fn from_framework_should_not_work() {
    let root_url = "#meta/async_reporter.cm";
    let moniker_to_wait_on = "./root";
    let builder = RealmBuilder::new().await.unwrap();
    let root = builder.add_child("root", root_url, ChildOptions::new().eager()).await.unwrap();
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .capability(Capability::event_stream("started").with_scope(&root))
                .capability(Capability::event_stream("stopped").with_scope(&root))
                .capability(Capability::event_stream("destroyed").with_scope(&root))
                .capability(Capability::event_stream("capability_requested").with_scope(&root))
                .from(Ref::framework())
                .to(&root),
        )
        .await
        .unwrap();
    let instance =
        builder.build_in_nested_component_manager("#meta/component_manager.cm").await.unwrap();
    let proxy = instance.root.connect_to_protocol_at_exposed_dir().unwrap();

    let mut event_stream = EventStream::new(proxy);

    instance.start_component_tree().await.unwrap();

    // Expect the component to stop
    EventMatcher::ok()
        .stop(Some(ExitStatusMatcher::AnyCrash))
        .moniker(moniker_to_wait_on)
        .wait::<Stopped>(&mut event_stream)
        .await
        .unwrap();
}

#[fasync::run_singlethreaded(test)]
async fn async_event_source_test() {
    start_nested_cm_and_wait_for_clean_stop("#meta/async_reporter.cm", "./root").await;
}

#[fasync::run_singlethreaded(test)]
async fn scoped_events_test() {
    start_nested_cm_and_wait_for_clean_stop("#meta/echo_realm.cm", "./root/echo_reporter").await;
}

#[fasync::run_singlethreaded(test)]
async fn realm_offered_event_source_test() {
    start_nested_cm_and_wait_for_clean_stop(
        "#meta/realm_offered_root.cm",
        "./root/nested_realm/reporter",
    )
    .await;
}

#[fasync::run_singlethreaded(test)]
async fn nested_event_source_test() {
    start_nested_cm_and_wait_for_clean_stop("#meta/nested_reporter.cm", "./root").await;
}

#[fasync::run_singlethreaded(test)]
async fn event_capability_requested() {
    start_nested_cm_and_wait_for_clean_stop("#meta/capability_requested_root.cm", "./root").await;
}
