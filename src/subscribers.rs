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

pub static SUBSCRIBER_ID: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIBERS: OnceLock<Mutex<Vec<(usize, Sender<Event>)>>> = OnceLock::new();

fn get_subscribers() -> &'static Mutex<Vec<(usize, Sender<Event>)>> {
    SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn subscribe_to_events(event_listener: Sender<Event>) -> usize {
    let id = SUBSCRIBER_ID.fetch_add(1, Ordering::SeqCst);
    {
        let mut subscribers = crate::subscribers::get_subscribers().lock().unwrap();
        subscribers.push((id, event_listener.clone()));
    }
    id
}

pub fn unsubscribe_from_events(id: usize) {
    let mut subscribers = crate::subscribers::get_subscribers().lock().unwrap();
    subscribers.retain(|(stored_id, _)| *stored_id != id);
}

pub fn broadcast_event(event: Event) {
    let mut subscribers = get_subscribers().lock().unwrap();
    debug!(
        "broadcast event: subscribers={} event={:?}",
        subscribers.len(),
        event
    );
    subscribers.retain(|(_, event_listener)| event_listener.send(event.clone()).is_ok());
}

pub fn forward_events(event_listener: Receiver<DdcutilEvent>) {
    for ddc_event in event_listener {
        if let Some(varlink_event) = service::convert_ddc_event(ddc_event) {
            broadcast_event(varlink_event);
        }
    }
}
