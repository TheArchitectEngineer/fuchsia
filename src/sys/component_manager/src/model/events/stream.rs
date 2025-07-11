// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::model::events::dispatcher::{EventDispatcher, EventDispatcherScope};
use crate::model::events::event::Event;
use crate::model::events::registry::ComponentEventRoute;
use futures::channel::mpsc;
use futures::stream::Peekable;
use futures::task::Context;
use futures::{Stream, StreamExt};
use moniker::ExtendedMoniker;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::Poll;

#[cfg(all(test, not(feature = "src_model_tests")))]
use {
    hooks::{EventType, HasEventType},
    moniker::Moniker,
};

pub struct EventStream {
    /// The receiving end of a channel of Events.
    rx: Peekable<mpsc::UnboundedReceiver<(Event, Option<Vec<ComponentEventRoute>>)>>,

    /// The sending end of a channel of Events.
    tx: mpsc::UnboundedSender<(Event, Option<Vec<ComponentEventRoute>>)>,

    /// A vector of EventDispatchers to this EventStream.
    /// EventStream assumes ownership of the dispatchers. They are
    /// destroyed when this EventStream is destroyed.
    dispatchers: Vec<Arc<EventDispatcher>>,

    /// The route taken for this event stream.
    /// This is used for access control and namespacing during
    /// serving of the event stream.
    pub route: Vec<ComponentEventRoute>,
    /// Routing tasks associated with this event stream.
    /// Tasks associated with the stream will be terminated
    /// when the EventStream is destroyed.
    pub tasks: Vec<fuchsia_async::Task<()>>,
}

impl EventStream {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded();
        Self { rx: rx.peekable(), tx, dispatchers: vec![], route: vec![], tasks: vec![] }
    }

    pub fn create_dispatcher(
        &mut self,
        subscriber: ExtendedMoniker,
        scopes: Vec<EventDispatcherScope>,
        route: Vec<ComponentEventRoute>,
    ) -> Weak<EventDispatcher> {
        self.route = route.clone();
        let dispatcher =
            Arc::new(EventDispatcher::new_with_route(subscriber, scopes, self.tx.clone(), route));
        self.dispatchers.push(dispatcher.clone());
        Arc::downgrade(&dispatcher)
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<(Event, Option<Vec<ComponentEventRoute>>)> {
        self.tx.clone()
    }

    /// Waits for an event with a particular EventType against a component with a
    /// particular moniker. Ignores all other events.
    #[cfg(all(test, not(feature = "src_model_tests")))]
    pub async fn wait_until(
        &mut self,
        expected_event_type: EventType,
        expected_moniker: Moniker,
    ) -> Option<Event> {
        let expected_moniker = ExtendedMoniker::ComponentInstance(expected_moniker);
        while let Some((event, _)) = self.next().await {
            let actual_event_type = event.event.event_type();
            if expected_moniker == event.event.target_moniker
                && expected_event_type == actual_event_type
            {
                return Some(event);
            }
        }
        None
    }
}

impl Stream for EventStream {
    type Item = (Event, Option<Vec<ComponentEventRoute>>);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}
