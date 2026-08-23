// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/main.rs

mod ddcutil;

use crossbeam_channel::{unbounded, Sender, Receiver};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::{env};
use std::thread;
use std::time::Duration;
use varlink::Result;
use varlink::*;

// ============================================================================
// Macros
// ============================================================================

/// Logs the Varlink call method name and parameters for debugging.
macro_rules! debug_varlink_call {
    ($call:expr) => {{
        let req = $call.get_request().expect("Varlink call missing request");
        log::debug!("VARLINK CALL: {:?}: {:?}", req.method, req.parameters);
    }};
}

// ============================================================================
// Imports from generated interface
// ============================================================================

mod com_ddcutil_service;
use com_ddcutil_service::*;
use crate::ddcutil::{DisplayRef, DisplayInfo, DdcutilEvent, DdcutilEventKind};

// ============================================================================
// Global subscribers
// ============================================================================

static SUBSCRIBER_ID: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIBERS: OnceLock<Mutex<Vec<(usize, Sender<Event>)>>> = OnceLock::new();

fn get_subscribers() -> &'static Mutex<Vec<(usize, Sender<Event>)>> {
    SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn broadcast_event(event: Event) {
    let mut subscribers = get_subscribers().lock().unwrap();
    debug!("broadcast event: subscribers={} event={:?}", subscribers.len(), event);
    subscribers.retain(|(_, event_listener)| event_listener.send(event.clone()).is_ok());
}


impl From<ddcutil::Error> for varlink::Error {
    fn from(e: ddcutil::Error) -> Self {
        //let msg = format!("{}", e.to_string());
        let msg = match &e {
            ddcutil::Error::Status(code) => ddcutil::get_status_message(*code),
            _ => e.to_string(),
        };
        varlink::ErrorKind::InvalidParameter(msg).into()
    }
}

// ============================================================================
// ServiceState – everything protected by the single lock
// ============================================================================

/// All state that must be protected by the single mutex.
/// This includes configuration, polling thread handles, and any other shared data.
struct DdcutilSharedState {
    // Configuration
    poll_interval_secs: u32,
    poll_cascade_secs: f64,
    events_enabled: bool,

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

impl DdcutilSharedState {
    /// Set the poll interval (in seconds).
    fn set_poll_interval(&mut self, seconds: u32) {
        self.poll_interval_secs = seconds;
    }

    /// Set the cascade interval (in seconds).
    fn set_cascade_interval(&mut self, seconds: f64) {
        self.poll_cascade_secs = seconds;
    }

    /// Enable or disable event watching. Calls libddcutil to start/stop watching.
    /// # Safety
    /// This calls unsafe FFI functions. The caller must hold the lock.
    fn set_events_enabled(&mut self, enabled: bool) -> Result<()> {
        self.events_enabled = enabled;
        if enabled {
            ddcutil::start_watch_displays()?;
        } else {
            ddcutil::stop_watch_displays()?;
        }
        Ok(())
    }
}

// ============================================================================
// DdcutilService – main service implementation
// ============================================================================

pub struct DdcutilService {
    /// Single mutex protecting all shared state and libddcutil access.
    state: Arc<Mutex<DdcutilSharedState>>,
    /// Channel for sending events from the polling thread and native callback.
    event_dispatcher: Sender<DdcutilEvent>,
    /// If true, configuration‑changing methods are rejected.
    configuration_locked: Arc<AtomicBool>,
}

impl DdcutilService {
    /// Create a new service instance. Initializes libddcutil and starts the native callback.
    pub fn new() -> (Self, Receiver<DdcutilEvent>) {
        // Initialize libddcutil
        ddcutil::init().expect("ddcutil init failed");
        ddcutil::redetect().expect("initial redetect failed");

        // Create event channel
        let (event_dispatcher, event_listener) = unbounded();

        // Store the sender globally for the native C callback
        ddcutil::set_callback_sender(event_dispatcher.clone()).unwrap();

        // Register the native callback (C callback)
        match ddcutil::register_callback(Some(ddcutil::native_ddc_event_callback)) {
            Err(status) => { error!("Failed to register ddcutil event callback: {:?}", status) }
            Ok(..) => {}
        };

        let service = DdcutilService {
            state: Arc::new(Mutex::new(DdcutilSharedState::default())),
            event_dispatcher,
            configuration_locked: Arc::new(AtomicBool::new(false)),
        };

        (service, event_listener)
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
            polling_loop(state_arc, event_dispatcher, shutdown_listener);
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

    // ----- Configuration getters (used by Varlink handlers) -----

    pub fn get_poll_interval(&self) -> u32 {
        self.state.lock().unwrap().poll_interval_secs
    }

    pub fn get_cascade_interval(&self) -> f64 {
        self.state.lock().unwrap().poll_cascade_secs
    }

    pub fn get_events_enabled(&self) -> bool {
        self.state.lock().unwrap().events_enabled
    }

    /// Enable or disable events. This also starts/stops the native watch.
    pub fn set_events_enabled(&self, enabled: bool) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.set_events_enabled(enabled)
    }
}

// ============================================================================
// Polling loop (runs in a background thread)
// ============================================================================

/// State of a single display for the polling loop.
#[derive(Debug, Clone, Copy)]
struct DisplayState {
    display_number: i32,
    display_ref: DisplayRef,
    awake: bool,
}

/// The main polling loop. Runs in its own thread.
fn polling_loop(
    state: Arc<Mutex<DdcutilSharedState>>,
    event_dispatcher: Sender<DdcutilEvent>,
    shutdown_listener: Receiver<()>,
) {
    use std::collections::{HashMap, HashSet};
    use base64::{engine::general_purpose, Engine as _};
    use ddcutil::{sleep_interruptible, get_display_info_list, redetect, is_dpms_awake};

    let mut previous_states: HashMap<String, DisplayState> = HashMap::new();
    let mut initializing = true;

    loop {
        // Check for shutdown signal
        if shutdown_listener.try_recv().is_ok() {
            info!("Polling thread received shutdown signal, exiting.");
            break;
        }

        // ---- Acquire the lock and read config ----
        let guard = state.lock().unwrap();
        let (interval, cascade, events_enabled) = {
            let cfg = &*guard;
            (cfg.poll_interval_secs, cfg.poll_cascade_secs, cfg.events_enabled)
        };

        if !events_enabled {
            drop(guard);
            sleep_interruptible(Duration::from_secs(5));
            continue;
        }

        // ---- Call libddcutil (safe because we hold the lock) ----
        if let Err(e) = redetect() {
            error!("redetect failed: {}", e);
            drop(guard);
            sleep_interruptible(Duration::from_secs(interval as u64));
            continue;
        }

        let current_displays = match get_display_info_list(false) {
            Ok(list) => list,
            Err(e) => {
                error!("get_display_info_list failed: {}", e);
                drop(guard);
                sleep_interruptible(Duration::from_secs(interval as u64));
                continue;
            }
        };

        // Build current state (also needs libddcutil for DPMS check)
        let mut current_states = HashMap::with_capacity(current_displays.len());
        for display in &current_displays {
            let edid = general_purpose::STANDARD.encode(&display.edid_bytes);
            let awake = match is_dpms_awake(display.display_ref) {
                Ok(a) => a,
                Err(e) => {
                    warn!("DPMS query failed for display {}: {}", display.display_number, e);
                    false
                }
            };
            current_states.insert(edid, DisplayState {
                display_number: display.display_number,
                display_ref: display.display_ref,
                awake,
            });
        }

        // ---- Release the lock before comparing states and sending events ----
        drop(guard);

        // Compare states (no lock needed)
        let current_edids: HashSet<_> = current_states.keys().collect();
        let previous_edids: HashSet<_> = previous_states.keys().collect();

        let newly_detected: Vec<_> = current_edids.difference(&previous_edids).collect();
        let lost: Vec<_> = previous_edids.difference(&current_edids).collect();

        let connection_change = !newly_detected.is_empty() || !lost.is_empty();
        if connection_change && !initializing {
            let event_type = if !newly_detected.is_empty() {
                DdcutilEventKind::Connected.as_str()
            } else {
                DdcutilEventKind::Disconnected.as_str()
            };
            let data = serde_json::json!({
                "event_type": event_type,
                "flags": 0,
            }).to_string();
            let event = DdcutilEvent {
                kind: DdcutilEventKind::ConnectedDisplaysChanged,
                data,
            };
            debug!("poll: sending connection change event");
            let _ = event_dispatcher.send(event);
        }

        // Detect DPMS changes
        for (edid, state) in &current_states {
            if let Some(prev_state) = previous_states.get(edid) {
                if prev_state.awake != state.awake && !initializing {
                    let kind = if state.awake {
                        DdcutilEventKind::DpmsAwake
                    } else {
                        DdcutilEventKind::DpmsAsleep
                    };
                    let data = serde_json::json!({
                        "display_number": state.display_number,
                        "edid_base64": edid,
                        "awake": state.awake,
                        "flags": 0,
                    }).to_string();
                    let event = DdcutilEvent { kind, data };
                    debug!("poll: sending DPMS change event");
                    let _ = event_dispatcher.send(event);
                }
            }
        }

        previous_states = current_states;
        initializing = false;

        // Sleep without holding the lock
        let sleep_duration = if connection_change {
            Duration::from_millis((cascade * 1000.0) as u64)
        } else {
            Duration::from_secs(interval as u64)
        };
        sleep_interruptible(sleep_duration);
    }
}

// ============================================================================
// Event conversion helpers
// ============================================================================

fn convert_ddc_event(ddc_event: DdcutilEvent) -> Option<Event> {
    match ddc_event.kind {
        DdcutilEventKind::Connected |
        DdcutilEventKind::Disconnected |
        DdcutilEventKind::ConnectedDisplaysChanged |
        DdcutilEventKind::DpmsAwake |
        DdcutilEventKind::DpmsAsleep => {
            Some(Event {
                kind: Event_kind::connected_displays_changed,
                data: ddc_event.data,
            })
        }
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
    }).to_string();

    Event {
        kind: Event_kind::vcp_changed,
        data,
    }
}

// ============================================================================
// Helper functions for Varlink handlers
// ============================================================================

const DDCUTIL_VARLINK_VERSION: &str = "1.0.0";

fn is_edid_prefix_allowed(options: &Option<CallOptions>) -> bool {
    options.as_ref().map_or(false, |o| o.allow_edid_prefix.unwrap_or(false))
}

fn is_setvcp_verifying(options: &Option<CallOptions>) -> bool {
    options.as_ref().map_or(true, |o| !o.no_verify.unwrap_or(false))
}

/// Open a display handle from a raw dref.
fn open_display_from_dref(dref: DisplayRef) -> crate::ddcutil::Result<ddcutil::DisplayHandle> {
    ddcutil::open_display(dref)
}

/// Convert ddcutil capabilities data to Varlink format.
fn convert_capabilities_data(data: ddcutil::CapabilitiesData) -> (
    String,
    i64,
    i64,
    StringHashMap<String>,
    StringHashMap<CapabilitiesFeature>,
) {
    let commands = data.commands.into_iter().map(|cmd| {
        (format!("{:02X}", cmd.code), cmd.description)
    }).collect();

    let capabilities = data.features.into_iter().map(|feature| {
        let values = feature.values.into_iter().map(|val| {
            (format!("{:02X}", val.code), val.name)
        }).collect();
        (
            format!("{:02X}", feature.code),
            CapabilitiesFeature {
                feature_name: feature.name,
                feature_description: feature.description,
                values,
            }
        )
    }).collect();

    (data.model_name, data.mccs_major as i64, data.mccs_minor as i64, commands, capabilities)
}

/// Send a DDC error reply.
fn send_ddc_error(
    call: &mut dyn VarlinkCallError,
    display_ref: Option<i64>,
    display_number: Option<i64>,
    edid_base64: Option<String>,
    vcp_code: Option<i64>,
    error: &ddcutil::Error,
) -> varlink::Result<()> {
    let status = error.status_code();
    let message = if let ddcutil::Error::Status(code) = error {
        ddcutil::get_status_message(*code)
    } else {
        error.to_string()
    };
    let edid = edid_base64.unwrap_or_else(String::new);
    call.reply_ddc_error(
        display_ref.unwrap_or(-1),
        display_number.unwrap_or(-1),
        edid,
        vcp_code.unwrap_or(-1),
        status,
        message,
    )
}

// ============================================================================
// Varlink Interface Implementation
// ============================================================================

impl VarlinkInterface for DdcutilService {
    fn detect(&self, call: &mut dyn Call_Detect, include_offline: bool) -> Result<()> {
        debug_varlink_call!(call);
        // Acquire the lock once for the entire operation
        let _guard = self.state.lock().unwrap();

        if let Err(e) = ddcutil::redetect() {
            let err_msg = format!("Detect failed: {}", e);
            call.reply_detect_error(e.status_code(), err_msg)?;
            return Ok(());
        }
        let displays = ddcutil::list_displays(include_offline)?;
        let detect_entries: Vec<DetectEntry> = displays.iter().map(Into::into).collect();
        call.reply(detect_entries.len() as i64, detect_entries)
    }

    fn get_capabilities_metadata(
        &self,
        call: &mut dyn Call_GetCapabilitiesMetadata,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let edid_ref = edid_base64.as_deref();
            let dref = ddcutil::find_display(display_number, edid_ref, is_edid_prefix_allowed(&options))?;
            let handle = open_display_from_dref(dref)?;
            let caps = ddcutil::get_capabilities_data(handle)?;
            Ok(convert_capabilities_data(caps))
        };

        match ddc_operation() {
            Ok((model_name, mccs_major, mccs_minor, commands, capabilities)) => {
                call.reply(model_name, mccs_major, mccs_minor, commands, capabilities)
            }
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn get_capabilities_string(
        &self,
        call: &mut dyn Call_GetCapabilitiesString,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let edid_ref = edid_base64.as_deref();
            let dref = ddcutil::find_display(display_number, edid_ref, is_edid_prefix_allowed(&options))?;
            let handle = open_display_from_dref(dref)?;
            ddcutil::get_capabilities_string(&handle)
        };

        match ddc_operation() {
            Ok(caps_str) => call.reply(caps_str),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn get_ddcutil_dynamic_sleep(&self, call: &mut dyn Call_GetDdcutilDynamicSleep) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();
        call.reply(ddcutil::is_dynamic_sleep_enabled())
    }

    fn get_ddcutil_output_level(&self, call: &mut dyn Call_GetDdcutilOutputLevel) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();
        call.reply(ddcutil::get_output_level() as i64)
    }

    fn get_ddcutil_version(&self, call: &mut dyn Call_GetDdcutilVersion) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();
        call.reply(ddcutil::get_ddcutil_version())
    }

    fn get_display_state(
        &self,
        call: &mut dyn Call_GetDisplayState,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let (status, message) = ddcutil::get_display_state(
                display_number,
                edid_base64.as_deref(),
                is_edid_prefix_allowed(&options),
            )?;
            Ok((status, message))
        };

        match ddc_operation() {
            Ok((status, message)) => call.reply(status as i64, message),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn get_multiple_vcp(
        &self,
        call: &mut dyn Call_GetMultipleVcp,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_codes: Vec<i64>,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let dref = match ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options)) {
            Ok(d) => d,
            Err(e) => return send_ddc_error(call, None, display_number, edid_base64, None, &e),
        };
        let mut handle = match open_display_from_dref(dref) {
            Ok(h) => h,
            Err(e) => return send_ddc_error(call, None, display_number, edid_base64, None, &e),
        };

        let mut values = Vec::new();
        for &code in &vcp_codes {
            match ddcutil::get_vcp(&mut handle, code as u8) {
                Ok((current, max, formatted)) => {
                    values.push(com_ddcutil_service::VcpValue {
                        vcp_code: code,
                        current: current as i64,
                        maximum: max as i64,
                        formatted,
                    });
                }
                Err(e) => {
                    return send_ddc_error(call, None, display_number, edid_base64, Some(code), &e);
                }
            }
        }
        call.reply(values)
    }

    fn get_service_interface_version(&self, call: &mut dyn Call_GetServiceInterfaceVersion) -> Result<()> {
        debug_varlink_call!(call);
        call.reply(DDCUTIL_VARLINK_VERSION.to_owned())
    }

    fn get_service_poll_cascade_interval(&self, call: &mut dyn Call_GetServicePollCascadeInterval) -> Result<()> {
        debug_varlink_call!(call);
        // No lock needed for a simple read – but we acquire it anyway for consistency
        let guard = self.state.lock().unwrap();
        call.reply(guard.poll_cascade_secs)
    }

    fn get_service_poll_interval(&self, call: &mut dyn Call_GetServicePollInterval) -> Result<()> {
        debug_varlink_call!(call);
        let guard = self.state.lock().unwrap();
        call.reply(guard.poll_interval_secs as i64)
    }

    fn get_sleep_multiplier(
        &self,
        call: &mut dyn Call_GetSleepMultiplier,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            ddcutil::get_sleep_multiplier(dref)
        };

        match ddc_operation() {
            Ok(multiplier) => call.reply(multiplier),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn get_vcp(
        &self,
        call: &mut dyn Call_GetVcp,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_code: i64,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            let mut handle = open_display_from_dref(dref)?;
            let (current, max, formatted) = ddcutil::get_vcp(&mut handle, vcp_code as u8)?;
            Ok((current as u32, max as u32, formatted))
        };

        match ddc_operation() {
            Ok((current, max, formatted)) => {
                call.reply(current as i64, max as i64, formatted)
            }
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, Some(vcp_code), &e),
        }
    }

    fn get_vcp_metadata(
        &self,
        call: &mut dyn Call_GetVcpMetadata,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_code: i64,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            let handle = open_display_from_dref(dref)?;
            ddcutil::get_vcp_metadata(&handle, vcp_code)
        };

        match ddc_operation() {
            Ok(metadata) => {
                call.reply(
                    metadata.feature_name,
                    metadata.description,
                    metadata.is_read_only,
                    metadata.is_write_only,
                    metadata.is_rw,
                    metadata.is_complex,
                    metadata.is_continuous,
                )
            }
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn list_detected(&self, call: &mut dyn Call_ListDetected, include_offline: bool) -> Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();
        let displays = ddcutil::list_displays(include_offline)?;
        let detect_entries: Vec<DetectEntry> = displays.iter().map(Into::into).collect();
        call.reply(detect_entries.len() as i64, detect_entries)
    }

    fn set_ddcutil_dynamic_sleep(&self, call: &mut dyn Call_SetDdcutilDynamicSleep, enabled: bool) -> Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        let _guard = self.state.lock().unwrap();
        unsafe { ddcutil::ddca_enable_dynamic_sleep(enabled) };
        call.reply()
    }

    fn set_ddcutil_output_level(&self, call: &mut dyn Call_SetDdcutilOutputLevel, level: i64) -> Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        let _guard = self.state.lock().unwrap();
        unsafe { ddcutil::ddca_output_level_name(level as ddcutil::DDCA_Output_Level) };
        call.reply()
    }

    fn set_service_poll_cascade_interval(
        &self,
        call: &mut dyn Call_SetServicePollCascadeInterval,
        seconds: f64,
    ) -> Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        if seconds < 0.0 || (seconds > 0.0 && seconds < 1.0) {
            return Err(varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_owned()).into());
        }
        let mut guard = self.state.lock().unwrap();
        guard.set_cascade_interval(seconds);
        call.reply()
    }

    fn set_service_poll_interval(
        &self,
        call: &mut dyn Call_SetServicePollInterval,
        seconds: i64,
    ) -> Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        if seconds < 0 || (seconds > 0 && seconds < 10) {
            return Err(varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_owned()).into());
        }
        let mut guard = self.state.lock().unwrap();
        guard.set_poll_interval(seconds as u32);
        call.reply()
    }

    fn set_sleep_multiplier(
        &self,
        call: &mut dyn Call_SetSleepMultiplier,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        new_multiplier: f64,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }

        let _guard = self.state.lock().unwrap();
        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            ddcutil::set_sleep_multiplier(dref, new_multiplier)?;
            Ok(())
        };

        match ddc_operation() {
            Ok(()) => call.reply(),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn set_vcp(
        &self,
        call: &mut dyn Call_SetVcp,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_code: i64,
        new_value: i64,
        client_context: Option<String>,
        options: Option<CallOptions>,
    ) -> Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }

        let _guard = self.state.lock().unwrap();
        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            let mut handle = open_display_from_dref(dref)?;
            let verify = is_setvcp_verifying(&options);

            ddcutil::set_vcp(&mut handle, vcp_code as u8, new_value as u16, verify)?;

            let event = build_vcp_changed_event(
                display_number,
                edid_base64.as_deref(),
                vcp_code,
                new_value,
                client_context.unwrap_or_default(),
            );
            broadcast_event(event);

            Ok(())
        };

        match ddc_operation() {
            Ok(()) => call.reply(),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, Some(vcp_code), &e),
        }
    }

    fn subscribe(&self, call: &mut dyn Call_Subscribe, use_polling: bool) -> Result<()> {
        debug_varlink_call!(call);

        info!("subscribe use_polling={}", use_polling);

        // Start or stop polling
        if use_polling {
            self.start_polling();
        } else {
            self.stop_polling();
        }

        // Enable events (this also starts/stop native watch)
        if let Err(e) = self.set_events_enabled(true) {
            error!("Failed to enable events: {}", e);
        }

        // Create a channel for this subscriber
        let (event_dispatcher, event_listener) = unbounded::<Event>();
        let id = SUBSCRIBER_ID.fetch_add(1, Ordering::SeqCst);

        // Tell the client we're going to stream multiple events
        call.set_continues(true);

        // Send initial event
        let initial_event = Event {
            kind: Event_kind::service_initialized,
            data: "{}".to_owned(),
        };
        if let Err(e) = call.reply(initial_event) {
            error!("Subscribe: initial reply failed: {}", e);
            return Ok(());
        }
        call.set_continues(true);

        // Store the sender
        {
            let mut subscribers = get_subscribers().lock().unwrap();
            subscribers.push((id, event_dispatcher.clone()));
        }

        // Main loop: forward events from the channel
        loop {
            match event_listener.recv() {
                Ok(event) => {
                    if let Err(_) = call.reply(event) {
                        // Client disconnected
                        break;
                    }
                    call.set_continues(true);
                }
                Err(_) => {
                    // All senders dropped
                    break;
                }
            }
        }

        // Cleanup
        {
            let mut subscribers = get_subscribers().lock().unwrap();
            subscribers.retain(|(stored_id, _)| *stored_id != id);
        }

        // Close the stream
        call.set_continues(false);
        let _ = call.reply(Event {
            kind: Event_kind::stream_closed,
            data: "{}".to_owned(),
        });

        Ok(())
    }
}

// ============================================================================
// Main entry point
// ============================================================================

fn main() ->  std::result::Result<(), Box<dyn std::error::Error>>  {
    // Set up panic hook for better logging
    std::panic::set_hook(Box::new(|panic_info| {
        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown"
        };
        let location = panic_info.location().unwrap_or_else(|| {
            std::panic::Location::caller()
        });
        error!(
            "PANIC at {}:{}: {}",
            location.file(),
            location.line(),
            payload
        );
    }));

    info!("Running with user privileges (UID: {})", rustix::process::getuid().as_raw());
    env_logger::init();

    // Create the service
    let (service, event_listener) = DdcutilService::new();

    // Spawn thread to forward ddcutil events to Varlink subscribers
    std::thread::spawn(move || {
        for ddc_event in event_listener {
            if let Some(varlink_event) = convert_ddc_event(ddc_event) {
                broadcast_event(varlink_event);
            }
        }
    });

    // Build the Varlink interface
    let interface = com_ddcutil_service::new(Box::new(service));
    let varlink_service = VarlinkService::new(
        "com.ddcutil",
        "ddcutil-varlink",
        "1.0.0",
        "https://github.com/digitaltrails/ddcutil-varlink",
        vec![Box::new(interface)],
    );

    // Determine socket address
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let socket_address = format!("unix:{}/ddcutil-varlink.socket", runtime_dir);

    // Check for systemd Socket Activation (LISTEN_FDS environment variable)
    // Will be on unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket
    if let Ok(fds) = env::var("LISTEN_FDS") {
        // Systemd handles binding the file descriptor for us.
        // We pass an empty/dummy address string because varlink crate
        // automatically prioritizes the systemd FD when LISTEN_FDS exists.
        info!("LISTEN_FDS is set {}. Activated via systemd.", fds);
        info!("Listening on systemd assigned socket - which might be: {}", socket_address);
        varlink::listen(
            varlink_service,
            "systemd:",
            &varlink::ListenConfig {
                idle_timeout: 600,
                ..Default::default()
            },
        )?;
    } else {
        // Fallback for manual local debugging/development
        // Dynamically build the path using XDG_RUNTIME_DIR safely
        warn!("LISTEN_FDS is not set. Running in manual mode.");
        info!("Listening on socket: {}", socket_address);
        varlink::listen(
            varlink_service,
            &socket_address,
            &varlink::ListenConfig {
                idle_timeout: 0,
                ..Default::default()
            },
        )?;
    }

    Ok(())
}