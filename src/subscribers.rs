// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/subscribers.rs

use crate::com_ddcutil_service::Event;
use crate::ddcutil::InternalEvent;
use crate::service;
use crossbeam_channel::{Receiver, Sender};
use log::{debug, info};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
// ============================================================================
// Event subscribers to the varlink Subscribe call which is set_continues(true).
// Each subscriber receives a stream of results/events.
// ============================================================================

type EventSender = Sender<Event>;

#[derive(Debug)]
struct Subscriber {
    pub id: usize,
    pub sender: Sender<Event>,
}

type SubscriberMutexList = Mutex<Vec<Subscriber>>;
type SubscriberList = OnceLock<SubscriberMutexList>;

// For allocating new subscriber ID numbers
pub static SUBSCRIBER_NEXT_ID: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIBERS: SubscriberList = SubscriberList::new();

fn get_subscribers() -> &'static SubscriberMutexList {
    SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Add an external subscriber to the list of subscribers, assign a unique id
pub fn subscribe_to_events(event_sender: EventSender) -> usize {
    let id = SUBSCRIBER_NEXT_ID.fetch_add(1, Ordering::SeqCst);
    {
        let mut subscribers = get_subscribers().lock().unwrap();
        subscribers.push(Subscriber {
            id,
            sender: event_sender,
        });
    }
    id
}

/// Unsubscribe an external subscriber with the given id
pub fn unsubscribe_from_events(id: usize) {
    let mut subscribers = get_subscribers().lock().unwrap();
    subscribers.retain(|subscriber| subscriber.id != id);
}

/// Take a single internal_event and dispatch equivalent external/varlink events
/// to all the external subscribers.
pub fn broadcast_to_external_subscribers(internal_event: InternalEvent) {
    // Convert from internal event to external event and send.
    if let Some(varlink_event) = service::convert_internal_event(internal_event) {
        info!("subscriber sending DDC event {:?}", varlink_event);
        let mut subscribers = get_subscribers().lock().unwrap();
        debug!(
            "broadcast event: subscribers={} event={:?}",
            subscribers.len(),
            varlink_event
        );
        // For each subscriber in subscribers send the event
        subscribers.retain(|subscriber| subscriber.sender.send(varlink_event.clone()).is_ok());
    }
}

/// Continuously listen for internal events from the receiver. When one arrives,
/// dispatch equivalent external/varlink events to all the external subscribers.
pub fn forward_to_all_external_subscribers(internal_event_receiver: Receiver<InternalEvent>) {
    for internal_event in internal_event_receiver {
        broadcast_to_external_subscribers(internal_event);
    }
}
