// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/subscribers.rs

use crate::ddcutil::InternalEvent;
use crossbeam_channel::{Receiver, Sender};
use log::{debug, info};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
// ============================================================================
// Event subscribers to the varlink Subscribe call which is set_continues(true).
// Each subscriber receives a stream of results/events.
// ============================================================================

#[derive(Debug)]
struct Subscriber {
    pub id: usize,
    pub sender: Sender<InternalEvent>,
}

type SubscriberMutexList = Mutex<Vec<Subscriber>>;
type SubscriberList = OnceLock<SubscriberMutexList>;

// For allocating new subscriber ID numbers
pub static SUBSCRIBER_NEXT_ID: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIBERS: SubscriberList = SubscriberList::new();

fn get_subscribers() -> &'static SubscriberMutexList {
    SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Add a subscriber to the list of subscribers, assign a unique id
pub fn subscribe_to_intneral_events(event_sender: Sender<InternalEvent>) -> usize {
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

/// Unsubscribe a subscriber with the given id
pub fn unsubscribe_from_events(id: usize) {
    let mut subscribers = get_subscribers().lock().unwrap();
    subscribers.retain(|subscriber| subscriber.id != id);
}

/// Take a single internal_event and dispatch a clone to all the subscribers.
pub fn broadcast_to_subscribers(internal_event: InternalEvent) {
    info!("subscriber sending DDC event {:?}", internal_event);
    let mut subscribers = get_subscribers().lock().unwrap();
    debug!(
        "broadcast event: subscribers={} event={:?}",
        subscribers.len(),
        internal_event
    );
    // For each subscriber in subscribers send the event
    subscribers.retain(|subscriber| subscriber.sender.send(internal_event.clone()).is_ok());
}

/// Continuously listen for internal events from the receiver. When one arrives,
/// dispatch it to all subscribers.
pub fn forward_to_all_subscribers(internal_event_receiver: Receiver<InternalEvent>) {
    for internal_event in internal_event_receiver {
        broadcast_to_subscribers(internal_event);
    }
}
