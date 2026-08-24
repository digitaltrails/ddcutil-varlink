// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/service.rs

use crate::com_ddcutil_service::{Event, Event_kind};
use crate::ddcutil::{DdcutilEvent, DdcutilEventKind, DisplayRef};
use crate::{ddcutil, polling, subscribers};
use crossbeam_channel::{unbounded, Receiver, Sender};
use log::{debug, error, info};
use std::sync::atomic::{AtomicBool};
use std::sync::{Arc, Mutex};
use std::thread;

// ============================================================================
// ServiceState – everything protected by the single lock
// ============================================================================

/// All state that must be protected by the single mutex.
/// This includes configuration, polling thread handles, and any other shared data.
pub struct DdcutilSharedState {
    // Configuration
    pub poll_interval_secs: u32,
    pub poll_cascade_secs: f64,
    pub events_enabled: bool,

    // Polling thread management
    poll_thread: Option<thread::JoinHandle<()>>,
    shutdown_displatcher: Option<Sender<()>>,
}

impl Default for DdcutilSharedState {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30,
            poll_cascade_secs: 0.5,
            events_enabled: false,
            poll_thread: None,
            shutdown_displatcher: None,
        }
    }
}

// ============================================================================
// DdcutilService – main service implementation
// ============================================================================

pub struct DdcutilService {
    /// Single mutex protecting all shared state and libddcutil access.
    pub state: Arc<Mutex<DdcutilSharedState>>,
    /// Channel for sending events from the polling thread and native callback.
    event_dispatcher: Sender<ddcutil::DdcutilEvent>,
    /// If true, configuration‑changing methods are rejected.
    pub configuration_locked: Arc<AtomicBool>,
}

impl DdcutilService {
    /// Create a new service instance. Initializes libddcutil and starts the native callback.
    pub fn new() -> (Self, Receiver<ddcutil::DdcutilEvent>) {
        // Initialize libddcutil
        ddcutil::init().expect("ddcutil init failed");
        ddcutil::redetect().expect("initial redetect failed");

        // Create event channel
        let (event_dispatcher, event_listener) = unbounded();

        // Store the sender globally for the native C callback
        ddcutil::set_callback_sender(event_dispatcher.clone()).unwrap();

        // Register the native callback (C callback)
        match ddcutil::register_callback(Some(ddcutil::native_ddc_event_callback)) {
            Err(status) => {
                error!("Failed to register ddcutil event callback: {:?}", status)
            }
            Ok(..) => {}
        };

        let service = DdcutilService {
            state: Arc::new(Mutex::new(DdcutilSharedState::default())),
            event_dispatcher,
            configuration_locked: Arc::new(AtomicBool::new(false)),
        };

        (service, event_listener)
    }

    // ----- Subscriptions control -----

    pub fn subscribe_to_events(event_listener: Sender<Event>) -> usize {
        subscribers::subscribe_to_events(event_listener)
    }

    pub fn unsubscribe_from_events(id: usize) {
        subscribers::unsubscribe_from_events(id)
    }

    pub fn broadcast_set_vcp(
        display_number: Option<i64>,
        edid_base64: Option<&str>,
        vcp_code: i64,
        new_value: i64,
        client_context: Option<String>,
    ) {
        let event = build_vcp_changed_event(
            display_number,
            edid_base64.as_deref(),
            vcp_code,
            new_value,
            client_context.unwrap_or_default(),
        );
        crate::subscribers::broadcast_event(event);
    }

    // ----- Polling control -----

    /// Start the polling thread if it's not already running.
    pub fn start_polling(&self) {
        let mut state = self.state.lock().unwrap();
        if state.poll_thread.is_some() {
            debug!("Polling thread already running");
            return;
        }

        // Create an unbounded message channel to receive shutdown messages
        let (shutdown_dispatcher, shutdown_listener) = unbounded();

        let state_arc = self.state.clone();
        let event_dispatcher = self.event_dispatcher.clone();

        let handle = thread::spawn(move || {
            polling::polling_loop(state_arc, event_dispatcher, shutdown_listener);
        });

        state.poll_thread = Some(handle);
        state.shutdown_displatcher = Some(shutdown_dispatcher);
        info!("Polling thread started");
    }

    /// Stop the polling thread if it's running.
    pub fn stop_polling(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(shutdown_dispatcher) = state.shutdown_displatcher.take() {
            let _ = shutdown_dispatcher.send(());
        }
        if let Some(handle) = state.poll_thread.take() {
            let _ = handle.join();
        }
        info!("Polling thread stopped");
    }

    /// Enable or disable event watching. Calls libddcutil to start/stop watching.
    /// # Safety
    /// This calls unsafe FFI functions. The caller must hold the lock.
    pub fn set_events_enabled(&self, enable: bool) -> varlink::Result<()> {
        let mut state = self.state.lock().unwrap();
        if enable == state.events_enabled && enable {
            debug!("Events for libddcutil already {}.", {
                if state.events_enabled { "enabled" } else { "disabled"}});
        } else {
            state.events_enabled = enable;
            if enable {
                ddcutil::start_watch_displays()?;
                debug!("Enabled libddcutil events.");
            } else {
                ddcutil::stop_watch_displays()?;
                debug!("Disabled libddcutil events.");
            }
        }
        Ok(())
    }
}

// ============================================================================
// Event conversion helpers
// ============================================================================
pub fn convert_ddc_event(ddc_event: DdcutilEvent) -> Option<Event> {
    match ddc_event.kind {
        DdcutilEventKind::Connected
        | DdcutilEventKind::Disconnected
        | DdcutilEventKind::ConnectedDisplaysChanged
        | DdcutilEventKind::DpmsAwake
        | DdcutilEventKind::DpmsAsleep => Some(Event {
            kind: Event_kind::connected_displays_changed,
            data: ddc_event.data,
        }),
        _ => None,
    }
}

/// Builds a `VcpChanged` event for broadcasting.
fn build_vcp_changed_event(
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    vcp_code: i64,
    new_value: i64,
    client_context: String,
) -> Event {
    let data = serde_json::json!({
        "display_number": display_number,
        "edid_base64": edid_base64,
        "vcp_code": vcp_code,
        "new_value": new_value,
        "client_context": client_context,
    })
    .to_string();

    Event {
        kind: Event_kind::vcp_changed,
        data,
    }
}
