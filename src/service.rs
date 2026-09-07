// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/service.rs

use crate::ddcutil::{InternalEvent, InternalEventKind, InternalEventType};
use crate::{ddcutil, polling, subscribers};
use crossbeam_channel::{unbounded, Receiver, Sender};
use log::{debug, error, info};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;

// ============================================================================
// ServiceState – everything protected by the single lock
// ============================================================================

/// All state that must be protected by the single mutex.
/// This includes configuration, polling thread handles, and any other shared data.
///
/// The poll_do_redetect is probably only ever needed if linked against libddcutil
/// version <= 2.1. From 2.2 onward libddcutil events for hotplugging of monitors
/// seems to be reliable for all drivers.  This option is provided in incase there
/// is someone out there that still has issues or wants to use an old libddutil.
pub struct ServiceSharedState {
    // Configuration
    pub poll_interval_secs: u32,
    pub poll_cascade_secs: f64,
    pub poll_do_redetect: bool,  // This is probably only ever needed if linked against libddcutil version <= 2.1
    pub events_enabled: bool,
    // Polling thread management
    poll_thread: Option<thread::JoinHandle<()>>,
    shutdown_dispatcher: Option<Sender<()>>,
}

impl Default for ServiceSharedState {
    fn default() -> Self {
        let poll_do_detect = std::env::var("DDCUTIL_POLL_DO_REDETECT")
            .map(|val| val.to_lowercase() == "true" || val == "1")
            .unwrap_or(false); // Fallback default if env var is not set
        info!("Environment variable DDCUTIL_POLL_DO_REDETECT={} (not needed for libddcutil >= 2.2)",
            poll_do_detect);
        Self {
            poll_interval_secs: 30,
            poll_cascade_secs: 0.5,
            poll_do_redetect: poll_do_detect,
            events_enabled: false,
            poll_thread: None,
            shutdown_dispatcher: None,
        }
    }
}

// ============================================================================
// DdcutilService – main service implementation
// ============================================================================

pub struct DdcutilService {
    /// Single mutex protecting all shared state and libddcutil access.
    pub state: Arc<Mutex<ServiceSharedState>>,
    /// Channel for sending events from the polling thread and native callback.
    internal_event_sender: Sender<ddcutil::InternalEvent>,
    /// If true, configuration‑changing methods are rejected.
    pub configuration_locked: Arc<AtomicBool>,
}

impl DdcutilService {
    /// Create a new service instance. Initializes libddcutil and starts the native callback.
    /// Returns a receiver for internal events, other modules should use the receiver
    /// to forward events for dispatch to external varlink subscribers.
    pub fn new() -> (Self, Receiver<ddcutil::InternalEvent>) {

        // Initialize libddcutil
        ddcutil::init().expect("ddcutil init failed");

        if log::log_enabled!(log::Level::Debug) {
            ddcutil::redetect().expect("initial redetect failed");
            let display_info = ddcutil::list_displays(false);
            for display_info in display_info.unwrap() {
                display_info.log_diagnostics();
            }
        }

        // Create event channel
        let (internal_event_sender, internal_event_receiver) = unbounded();

        // Store the sender globally for the native C callback
        ddcutil::set_internal_event_sender(internal_event_sender.clone()).unwrap();

        // Register the native callback (C callback)
        if let Err(status) = ddcutil::register_callback(Some(ddcutil::native_ddc_event_callback)) {
            error!("Failed to register ddcutil event callback: {:?}", status)
        };

        let service = Self {
            state: Arc::new(Mutex::new(ServiceSharedState::default())),
            internal_event_sender,
            configuration_locked: Arc::new(AtomicBool::new(false)),
        };

        (service, internal_event_receiver)
    }

    // ----- Subscriptions control -----

    pub fn subscribe_to_internal_events(event_sender: Sender<InternalEvent>) -> usize {
        subscribers::subscribe_to_intneral_events(event_sender)
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
        let internal_event = build_vcp_changed_event(
            display_number,
            edid_base64,
            vcp_code,
            new_value,
            client_context.unwrap_or_default(),
        );
        subscribers::broadcast_to_subscribers(internal_event);
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
        let internal_event_sender = self.internal_event_sender.clone();

        let handle = thread::spawn(move || {
            polling::polling_loop(state_arc, internal_event_sender, shutdown_listener);
        });

        state.poll_thread = Some(handle);
        state.shutdown_dispatcher = Some(shutdown_dispatcher);
        info!("Polling thread started");
    }

    /// Stop the polling thread if it's running.
    pub fn stop_polling(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(shutdown_dispatcher) = state.shutdown_dispatcher.take() {
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
                if state.events_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            });
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
// Event helpers
// ============================================================================

/// Builds a VCP Changed event.
fn build_vcp_changed_event(
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    vcp_code: i64,
    new_value: i64,
    client_context: String,
) -> InternalEvent {
    let data = serde_json::json!({
        "event_type": InternalEventType::VcpChange.as_str(),
        "origin": "ddcutil-varlink",  // for now this is the only origin for set vcp
        "display_number": display_number,
        "edid_base64": edid_base64,
        "vcp_code": vcp_code,
        "new_value": new_value,
        "client_context": client_context,
    })
    .to_string();

    InternalEvent {
        kind: InternalEventKind::VcpChange,
        data,
    }
}

/// Builds an envent for a hotplug connect or disconnect.
/// The edit_base64 may be empty for a disconnect (no longer available).
pub fn build_hotplug_event(edid: &String, event_type: InternalEventType) -> InternalEvent {
    let data = serde_json::json!({
        "edid_base64": edid,
        "event_type": event_type.as_str(),
        "origin": "polling",
        "flags": 0,
    }).to_string();
    InternalEvent {
        kind: InternalEventKind::ConnectedDisplaysChanged,
        data,
    }
}

/// Builds an event for DPMS awake or asleep.
pub fn build_dpms_event(edid: &String, event_type: InternalEventType) -> InternalEvent {
    let data = serde_json::json!({
                                        "event_type": event_type.as_str(),
                                        "origin": "polling",
                                        "edid_base64": edid,
                                        "awake": InternalEventType::DpmsAwake == event_type,
                                        "flags": 0,
                                    })
        .to_string();
    InternalEvent { kind: InternalEventKind::ConnectedDisplaysChanged, data }
}