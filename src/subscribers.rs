// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/subscribers.rs

use crate::com_ddcutil_service::Event;
use crate::ddcutil::DdcutilEvent;
use crate::service;
use crossbeam_channel::{Receiver, Sender};
use log::debug;
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

pub fn subscribe_to_events(event_listener: EventSender) -> usize {
    let id = SUBSCRIBER_NEXT_ID.fetch_add(1, Ordering::SeqCst);
    {
        let mut subscribers = get_subscribers().lock().unwrap();
        subscribers.push(Subscriber{id, sender:event_listener});
    }
    id
}

pub fn unsubscribe_from_events(id: usize) {
    let mut subscribers = get_subscribers().lock().unwrap();
    subscribers.retain(|subscriber| subscriber.id != id);
}

pub fn broadcast_event(event: Event) {
    let mut subscribers = get_subscribers().lock().unwrap();
    debug!(
        "broadcast event: subscribers={} event={:?}",
        subscribers.len(),
        event
    );
    subscribers.retain(|subscriber| subscriber.sender.send(event.clone()).is_ok());
}

pub fn forward_events(event_listener: Receiver<DdcutilEvent>) {
    for ddc_event in event_listener {
        if let Some(varlink_event) = service::convert_ddc_event(ddc_event) {
            broadcast_event(varlink_event);
        }
    }
}
