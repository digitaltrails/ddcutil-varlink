// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/main.rs
mod ddcutil;

use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{unbounded, Sender};
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::ffi::c_void;
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, panic, ptr, thread};
use varlink::Result;
use varlink::*;

static SUBSCRIBER_ID: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIBERS: OnceLock<Mutex<Vec<(usize, Sender<Event>)>>> = OnceLock::new();

static NEED_POLL: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
enum CallbackEventKind {
    Connected,
    Disconnected,
    DpmsAwake,
    DpmsAsleep,
    DdcWorking,
    DdcNotWorking, // optional, depending on what the library provides
    Unknown(i32),  // fallback for future event types
}

impl CallbackEventKind {
    fn as_str(&self) -> &'static str {
        match self {
            CallbackEventKind::Connected => "DisplayConnected",
            CallbackEventKind::Disconnected => "DisplayDisconnected",
            CallbackEventKind::DpmsAwake => "DpmsAwake",
            CallbackEventKind::DpmsAsleep => "DpmsAsleep",
            CallbackEventKind::DdcWorking => "DdcWorking",
            CallbackEventKind::DdcNotWorking => "DdcNotWorking",
            CallbackEventKind::Unknown(_) => "Unknown",
        }
    }
}

struct CallbackEvent {
    kind: CallbackEventKind,
    connector: String,
    // optionally: io_path, flags, etc.
}

static CALLBACK_EVENT_SENDER: OnceLock<Sender<CallbackEvent>> = OnceLock::new();

fn get_subscribers() -> &'static Mutex<Vec<(usize, Sender<Event>)>> {
    SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
}

const POLL_WAKE_STEP_MS: u64 = 250; // Check NEED_POLL every 250ms

/// Sleep for the given duration, but wake up early if NEED_POLL is set.
/// Returns `true` if the sleep was interrupted by NEED_POLL.
fn sleep_interruptible(duration: Duration) -> bool {
    let step = Duration::from_millis(POLL_WAKE_STEP_MS);
    let mut remaining = duration;

    while remaining > Duration::ZERO {
        let wait = std::cmp::min(remaining, step);
        std::thread::sleep(wait);
        remaining -= wait;

        // Check if a callback wants us to poll immediately
        if NEED_POLL.swap(false, Ordering::SeqCst) {
            debug!("NEED_POLL triggered during sleep, waking up early");
            return true;
        }
    }
    false
}

/// Builds a `VcpChanged` event for broadcasting.
fn build_vcp_changed_event(
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    vcp_code: i64,
    new_value: i64,
) -> Event {
    let data = serde_json::json!({
        "display_number": display_number,
        "edid_base64": edid_base64,
        "vcp_code": vcp_code,
        "new_value": new_value,
    }).to_string();

    Event {
        kind: Event_kind::vcp_changed,
        data,
    }
}


fn convert_capabilities_data(data: ddcutil::CapabilitiesData) -> (String, i64, i64, Vec<KeyValueIntString>, Vec<KeyValueIntCapabilitiesFeature>) {
    // model_name is not in CapabilitiesData – you might need to pass it separately
    // or have ddcutil provide it.
    let model_name = "Unknown".to_string();

    let commands = data.commands.into_iter().map(|cmd| KeyValueIntString {
        key: cmd.code as i64,
        value: cmd.description,
    }).collect();

    let capabilities = data.features.into_iter().map(|feature| {
        let values = feature.values.into_iter().map(|val| CapabilitiesValueEntry {
            value_code: val.code as i64,
            value_name: val.name,
        }).collect();

        KeyValueIntCapabilitiesFeature {
            key: feature.code as i64,
            value: CapabilitiesFeature {
                feature_name: feature.name,
                feature_description: feature.description,
                values,
            },
        }
    }).collect();

    (model_name, data.mccs_major as i64, data.mccs_minor as i64, commands, capabilities)
}


// ============================================================================
// Custom error handling for ddcutil operations
// ============================================================================

#[derive(Debug)]
enum DdcError {
    DisplayNotFound {
        display_number: i64,
        edid_base64: String,
        status: i64,
        message: String,
    },
    Ddcutil(ddcutil::Error),
    InvalidIdentifier(String),
}

impl From<ddcutil::Error> for DdcError {
    fn from(e: ddcutil::Error) -> Self {
        DdcError::Ddcutil(e)
    }
}

impl From<ddcutil::Error> for varlink::Error {
    fn from(e: ddcutil::Error) -> Self {
        let msg = match &e {
            ddcutil::Error::Status(code) => ddcutil::get_status_message(*code),
            _ => e.to_string(),
        };
        varlink::ErrorKind::InvalidParameter(msg).into()
    }
}

// Include the generated interface module.
// The generator creates a module named after the interface file.
// For example, "com.ddcutil.service" becomes "com_ddcutil_service".
mod com_ddcutil_service;
use com_ddcutil_service::*;
use crate::ddcutil::get_status_message;
// ============================================================================
// Service State
// ============================================================================

pub struct ServiceState {
    pub poll_interval_secs: u32,
    pub poll_cascade_secs: f64,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30,    // Poll seconds, quite long – detect can be slow.
            poll_cascade_secs: 0.5,    // Poll sooner after an event, in case it's a cluster.
        }
    }
}

// ============================================================================
// Interface Implementation
// ============================================================================
pub struct DdcutilService {
    state: Arc<Mutex<ServiceState>>,       // only for fields that need mutability
    locked: Arc<AtomicBool>,               // separate atomic flag
    poll_thread: Option<thread::JoinHandle<()>>,
}

impl DdcutilService {
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(ServiceState::default()));
        let state_clone = state.clone();
        // Start polling thread
        let handle = thread::spawn(move || {
            polling_task(state_clone);
        });
        Self {
            state: Arc::new(Mutex::new(ServiceState::default())),
            locked: Arc::new(AtomicBool::new(false)),
            poll_thread: Some(handle),
        }
    }

    fn list_displays(include_offline: bool) -> Result<Vec<DetectEntry>> {
        let display_info = ddcutil::list_displays(include_offline)?;
        let mut result = Vec::with_capacity(display_info.len());

        for data in display_info {
            result.push(DetectEntry {
                display_number: data.dispno as i64,
                usb_bus: data.usb_bus as i64,
                usb_device: data.usb_device as i64,
                mfg_id: data.mfg_id,
                model_name: data.model_name,
                serial: data.sn,
                product_code: data.product_code as i64,
                edid_base64: base64::encode(&data.edid_bytes),
                binary_serial: 0, // your D-Bus version sets this to 0
            });
        }

        Ok(result)
    }

}

fn is_edid_prefix_allowed(options: &Option<CallOptions>) -> bool {
    if let Some(opts) = options {
        opts.allow_edid_prefix.unwrap_or(false)
    } else {
        false
    }
}

fn is_setvcp_verifying(options: &Option<CallOptions>) -> bool {
    if let Some(opts) = options {
        !opts.no_verify.unwrap_or(false)
    } else {
        true
    }//options.as_ref().map_or(true, |o| !o.no_verify.unwrap_or(false))
}

impl VarlinkInterface for DdcutilService {

    fn detect(&self, call: &mut dyn Call_Detect, include_offline: bool) -> Result<()> {
        if let Err(e) = ddcutil::redetect() {
            let err_msg = format!("Detect failed: {}", e);
            call.reply_detect_error(e.status_code(), err_msg.clone())?;  // some unknown problem
            return Ok(())
        }
        let displays = Self::list_displays(include_offline)?;
        call.reply(displays.len() as i64, displays, 0, "OK".to_owned())
    }

    fn get_capabilities_metadata(
        &self,
        call: &mut dyn Call_GetCapabilitiesMetadata,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>, // TODO: handle options later
     ) -> Result<()> {

        let ddc_operation_fn = || -> std::result::Result<_, DdcError> {
            let edid_ref = edid_base64.as_deref();
            let (_list, dref) = ddcutil::find_display(display_number, edid_ref, is_edid_prefix_allowed(&options))?;
            let handle = open_display_from_dref(dref)?;
            let caps = ddcutil::parse_capabilities(handle);
            let (model_name, mccs_major, mccs_minor, commands, capabilities) =
                convert_capabilities_data(caps.unwrap());
            Ok((model_name, mccs_major, mccs_minor, commands, capabilities))
        };

        match ddc_operation_fn() {
            Ok((model_name, mccs_major, mccs_minor, commands, capabilities)) => {
                call.reply(
                    model_name,
                    mccs_major as i64,
                    mccs_minor as i64,
                    commands,
                    capabilities,
                    0,
                    "OK".to_string(),
                )
            }
            Err(e) => send_ddc_error(call, display_number, edid_base64, &e),
        }
    }

    fn get_capabilities_string(
        &self,
        call: &mut dyn Call_GetCapabilitiesString,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>,
    ) -> Result<()> {
        // Group all fallible operations (including FFI) into a closure.
        let ddc_operation_fn = || -> std::result::Result<_, DdcError> {
            let edid_ref = edid_base64.as_deref();
            let (_list, dref) = ddcutil::find_display(display_number, edid_ref, is_edid_prefix_allowed(&options))?;
            let handle = open_display_from_dref(dref)?;
            debug!("get_capabilities_string - found display");
            let caps_str = ddcutil::get_capabilities_string(&handle);
            Ok(caps_str)
        };

        match ddc_operation_fn() {
            Ok(caps) => call.reply(caps.unwrap(), 0, "OK".to_string()),
            Err(e) => send_ddc_error(call, display_number, edid_base64, &e),
        }
    }

    fn get_ddcutil_dynamic_sleep(&self, call: &mut dyn Call_GetDdcutilDynamicSleep) -> Result<()> {
        call.reply(ddcutil::is_dynamic_sleep_enabled())
    }

    fn get_ddcutil_output_level(&self, call: &mut dyn Call_GetDdcutilOutputLevel) -> Result<()> {
        call.reply(ddcutil::get_output_level() as i64)
    }

    fn get_ddcutil_version(&self, call: &mut dyn Call_GetDdcutilVersion) -> Result<()> {
        call.reply(ddcutil::get_ddcutil_version())
    }

    fn get_display_state(
        &self,
        call: &mut dyn Call_GetDisplayState,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>
    ) -> Result<()> {

        let ddc_operation_fn = || -> std::result::Result<_, DdcError> {
            let (status, message) = ddcutil::get_display_state(
                display_number,
                edid_base64.as_deref(),
                is_edid_prefix_allowed(&options),
            )?;
            Ok((status, message))
        };

        match ddc_operation_fn() {
            Ok((status, message)) => call.reply(status as i64, message),
            Err(e) => send_ddc_error(call, display_number, edid_base64, &e),
        }
    }

    fn get_service_flag_options(&self, call: &mut dyn Call_GetServiceFlagOptions) -> Result<()> {
        call.reply(vec![])
    }

    fn get_service_info_logging(&self, call: &mut dyn Call_GetServiceInfoLogging) -> Result<()> {
        call.reply(false)
    }

    fn get_service_interface_version(&self, call: &mut dyn Call_GetServiceInterfaceVersion) -> Result<()> {
        call.reply("1.0.0".to_owned())
    }

    fn get_service_parameters_locked(&self, call: &mut dyn Call_GetServiceParametersLocked) -> Result<()> {

        call.reply(self.locked.load(Ordering::SeqCst))
    }

    fn get_service_poll_cascade_interval(&self, call: &mut dyn Call_GetServicePollCascadeInterval) -> Result<()> {
        call.reply(0.5)
    }

    // ---------- Properties
    fn get_service_poll_interval(
        &self,
        call: &mut dyn Call_GetServicePollInterval,
    ) -> Result<()> {
        let secs = self.state.lock().unwrap().poll_interval_secs;
        call.reply(secs as i64)
    }

    fn get_sleep_multiplier(&self,
                            call: &mut dyn Call_GetSleepMultiplier,
                            display_number: Option<i64>, edid_base64: Option<String>,
                            options: Option<CallOptions>) -> Result<()> {
        call.reply(1.0, 0, "Stub: get_sleep_multiplier not implemented".to_owned())
    }

    fn get_status_values(&self, call: &mut dyn Call_GetStatusValues) -> Result<()> {
        call.reply(vec![]) // empty dictionary-replacement array
    }

    fn get_vcp(
        &self,
        call: &mut dyn Call_GetVcp,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_code: i64,
        options: Option<CallOptions>
    ) -> Result<()> {

        let ddc_operation_fn = || -> std::result::Result<_, DdcError> {
            let (_list, dref) = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            let mut handle = open_display_from_dref(dref)?;
            let (current, max, formatted) = ddcutil::get_vcp(&mut handle, vcp_code as u8)?;
            Ok((current as u32, max as u32, formatted))
        };

        // 2. Clear, expressive execution phase
        match ddc_operation_fn() {
            Ok((current, max, formatted)) =>
                call.reply(current as i64, max as i64, formatted, 0, "OK".to_owned()),
            Err(e) => send_ddc_error(call, display_number, edid_base64, &e),
        }
    }

    fn get_multiple_vcp(
        &self,
        call: &mut dyn Call_GetMultipleVcp,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_codes: Vec<i64>,
        options: Option<CallOptions>
    ) -> Result<()> {
        // if self.locked.load(Ordering::SeqCst) {  // not needed for read operations?
        //     return call.reply_configuration_locked(); // or a custom error
        // }

        let mut handle = match (|| {
            let (_list, dref) = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            open_display_from_dref(dref)
        })() {
            Ok(h) => h,
            Err(e) => return send_ddc_error(call, display_number, edid_base64, &e),
        };

        // Now we have the handle; perform the per‑code operations.
        let mut values = Vec::new();
        let mut overall_status = 0;
        let mut error_messages = Vec::new();

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
                    let detail = match &e {
                        ddcutil::Error::Status(status_code) => ddcutil::get_status_message(status_code.clone()),
                        _ => e.to_string(),
                    };
                    let formatted_err = format!("VCP 0x{:02x}: {}", code, detail);
                    log::warn!("GetMultipleVcp: {}", formatted_err);
                    error_messages.push(formatted_err);
                    overall_status = -1;
                }
            }
        }

        let message = if error_messages.is_empty() {
            "OK".to_owned()
        } else {
            format!("Partial failure: {}", error_messages.join("; "))
        };
        call.reply(values, overall_status, message)
    }

    fn get_vcp_metadata(&self, call: &mut dyn Call_GetVcpMetadata,
                        display_number: Option<i64>,
                        edid_base64: Option<String>,
                        vcp_code: i64,
                        options: Option<CallOptions>) -> Result<()> {
        call.reply(
            "stub_feature".to_owned(),
            "".to_owned(),
            false, false, false, false, false,
            0,
            "Stub: get_vcp_metadata not implemented".to_owned(),
        )
    }

    fn list_detected(&self, call: &mut dyn Call_ListDetected, include_offline: bool) -> Result<()> {
        let displays = Self::list_displays(include_offline)?;
        call.reply(displays.len() as i64, displays, 0, "OK".to_owned())
    }

    fn set_ddcutil_dynamic_sleep(&self, call: &mut dyn Call_SetDdcutilDynamicSleep, enabled: bool) -> Result<()> {
        call.reply()
    }

    fn set_ddcutil_output_level(&self, call: &mut dyn Call_SetDdcutilOutputLevel, level: i64) -> Result<()> {
        call.reply()
    }

    fn set_service_info_logging(&self, call: &mut dyn Call_SetServiceInfoLogging, enabled: bool) -> Result<()> {
        call.reply()
    }

    fn set_service_poll_cascade_interval(&self, call: &mut dyn Call_SetServicePollCascadeInterval, seconds: f64) -> Result<()> {
        if self.locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        // validation...
        call.reply()
    }

    fn set_service_poll_interval(
        &self,
        call: &mut dyn Call_SetServicePollInterval,
        seconds: i64,
    ) -> Result<()> {
        if self.locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        if seconds < 0 || (seconds > 0 && seconds < 10) {
            return Err(varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_owned()).into());
        }

        call.reply()
    }

    fn set_sleep_multiplier(&self, call: &mut dyn Call_SetSleepMultiplier,
                            display_number: Option<i64>,
                            edid_base64: Option<String>,
                            new_multiplier: f64,
                            options: Option<CallOptions>) -> Result<()> {
        call.reply(0, "Stub: set_sleep_multiplier not implemented".to_owned())
    }

    fn set_vcp(
        &self,
        call: &mut dyn Call_SetVcp,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_code: i64,
        new_value: i64,
        options: Option<CallOptions>,
    ) -> Result<()> {

        if self.locked.load(Ordering::SeqCst) {
            return call.reply_configuration_locked();
        }
        let verify = is_setvcp_verifying(&options);
        if !verify {
            debug!("Non-verified set.")
        }

        let ddc_operation_fn = || -> std::result::Result<_, DdcError> {

            let (_list, dref) = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            let mut handle = open_display_from_dref(dref)?;

            unsafe { let _ = ddcutil::ddca_enable_verify(verify); };
            ddcutil::set_vcp(&mut handle, vcp_code as u8, new_value as u16)?;

            let event = build_vcp_changed_event(
                display_number,
                edid_base64.as_deref(),
                vcp_code,
                new_value,
            );
            broadcast_event(event);

            Ok(())
        };

        match ddc_operation_fn() {
            Ok(()) => call.reply(0, "OK".to_owned()),
            Err(e) => return send_ddc_error(call, display_number, edid_base64, &e),
        }
    }

    fn set_vcp_with_context(&self, call: &mut dyn Call_SetVcpWithContext,
                            display_number: Option<i64>,
                            edid_base64: Option<String>,
                            vcp_code: i64,
                            new_value: i64,
                            client_context: String,
                            options: Option<CallOptions>) -> Result<()> {
        call.reply(0, "Stub: set_vcp_with_context not implemented".to_owned())
    }

    fn subscribe(&self, call: &mut dyn Call_Subscribe) -> Result<()> {
        // 1. Create a channel for this subscriber
        let (tx, rx) = unbounded::<Event>();
        let id = SUBSCRIBER_ID.fetch_add(1, Ordering::SeqCst);

        // Tell the client we're going to stream multiple events
        call.set_continues(true);   // <-- Must be before the first reply

        // TODO: If a client crashes without closing the socket, the sender will persist
        // until a display change occurs. This is a known limitation that can be addressed
        // later with a heartbeat or timeout mechanism.

        // 2. Send the initial event
        let initial_event = Event {
            kind: Event_kind::service_initialized,
            data: "{}".to_owned(),
        };
        if let Err(e) = call.reply(initial_event) {
            eprintln!("Subscribe: initial reply failed: {}", e);
            return Ok(());
        }
        call.set_continues(true);

        // 3. Store the sender in the global list
        {
            let mut subscribers = get_subscribers().lock().unwrap();
            subscribers.push((id, tx.clone()));
        }

        // 4. Main loop: wait for events and send them
        loop {
            match rx.recv() {
                Ok(event) => {
                    if let Err(_) = call.reply(event) {
                        // Client disconnected – break out
                        break;
                    }
                    call.set_continues(true);
                }
                Err(_) => {
                    // All senders dropped (polling thread died) – stop
                    break;
                }
            }
        }

        // 5. Cleanup: remove our sender from the global list
        {
            let mut subscribers = get_subscribers().lock().unwrap();
            subscribers.retain(|(stored_id, _)| *stored_id != id);
        }

        // 6. Close the stream gracefully
        call.set_continues(false);
        let _ = call.reply(Event {
            kind: Event_kind::stream_closed,
            data: "{}".to_owned(),
        });

        Ok(())
    }

}
/// Find a display by number or EDID, returning the raw dref and the DisplayList
/// that keeps it alive. The caller must hold onto the DisplayList for the
/// lifetime of the dref.


/// Open a handle from a raw dref.
fn open_display_from_dref(dref: *mut c_void) -> std::result::Result<ddcutil::DisplayHandle, DdcError> {
    ddcutil::open_display(dref).map_err(DdcError::Ddcutil)
}

/// Extract status code and message from a DdcError for the Varlink error reply.
fn extract_error_details(e: &DdcError) -> (i64, String) {
    match e {
        DdcError::DisplayNotFound { status, message, .. } => (*status, message.clone()),
        DdcError::Ddcutil(err) => {
            // You can map specific error kinds to custom status codes if desired.
            let status = match err {
                ddcutil::Error::Status(code) => *code as i64,
                _ => -1, // generic failure
            };
            (status, get_status_message(status as i32))
        }
        DdcError::InvalidIdentifier(msg) => (-1, msg.clone()),
    }
}

fn send_ddc_error(
    call: &mut dyn VarlinkCallError,
    display_number: Option<i64>,
    edid_base64: Option<String>,
    error: &DdcError,
) -> varlink::Result<()> {
    let (status, message) = extract_error_details(error);
    let edid = edid_base64.unwrap_or_else(String::new);
    call.reply_ddc_error(display_number.unwrap_or(-1), edid, status, message)
}

/// Polling Task (runs in a background thread)
fn polling_task(state: Arc<Mutex<ServiceState>>) {
    let mut previous_edids = HashSet::new();
    loop {
        // Refresh configuration
        let (interval, cascade_interval) = {
            let guard = state.lock().unwrap();
            (guard.poll_interval_secs, guard.poll_cascade_secs)
        };

        // If no subscribers, just idle sleep
        if get_subscribers().lock().unwrap().is_empty() {
            // Clear the flag so it doesn't linger
            if NEED_POLL.swap(false, Ordering::SeqCst) {
                debug!("NEED_POLL cleared while idle (no subscribers)");
            }
            // debug!("No subscribers - idle sleep (5s)");
            sleep_interruptible(Duration::from_secs(5));
            continue;
        }

        // Only reaches here if subscribers exist
        debug!("polling");

        if let Err(e) = ddcutil::redetect() {
            error!("redetect displays failed: {}", e);
            // While polling, we will ignore this and carry on.
        }

        let current = match ddcutil::get_display_info_list(false) {
            Ok(list) => list,
            Err(_) => {
                // On error, sleep with interruptible check, then continue
                sleep_interruptible(Duration::from_secs(interval as u64));
                continue;
            }
        };

        let current_edids: HashSet<String> =
            current.iter().map(|d| general_purpose::STANDARD.encode(&d.edid_bytes)).collect();

        let newly_detected_edids: Vec<_> = current_edids.difference(&previous_edids).collect();
        let lost_edids: Vec<_> = previous_edids.difference(&current_edids).collect();
        let event_occurred = !newly_detected_edids.is_empty() || !lost_edids.is_empty();

        if event_occurred {

            let edid = newly_detected_edids
                .iter()
                .next()
                .or_else(|| lost_edids.iter().next())
                .map(|s| s.to_string())
                .unwrap_or_else(String::new);

            let event_type = if !newly_detected_edids.is_empty() { 1 } else { 2 };

            let data = serde_json::json!({
                "edid_base64": edid,
                "event_type": event_type,
                "flags": 0,
            }).to_string();

            let event = Event {
                kind: Event_kind::connected_displays_changed,
                data,
            };
            broadcast_event(event);
        }

        previous_edids = current_edids;

        let sleep_duration = if event_occurred {
            Duration::from_millis((cascade_interval * 1000.0) as u64)
        } else {
            Duration::from_secs(interval as u64)
        };

        debug!("poll: sleeping for {:?} (interruptible)", sleep_duration);
        sleep_interruptible(sleep_duration);

    }
}

/// Event cCallback for passing to libddcutil
extern "C" fn my_display_callback(event: ddcutil::DDCA_Display_Status_Event) {
    // Map the C event type to our Rust enum
    let kind = match event.event_type {
        DDCA_Display_Event_Type_DDCA_EVENT_DISPLAY_CONNECTED => CallbackEventKind::Connected,
        DDCA_Display_Event_Type_DDCA_EVENT_DISPLAY_DISCONNECTED => CallbackEventKind::Disconnected,
        DDCA_Display_Event_Type_DDCA_EVENT_DPMS_AWAKE => CallbackEventKind::DpmsAwake,
        DDCA_Display_Event_Type_DDCA_EVENT_DPMS_ASLEEP => CallbackEventKind::DpmsAsleep,
        DDCA_Display_Event_Type_DDCA_EVENT_DDC_WORKING => CallbackEventKind::DdcWorking,
        // DDCA_EVENT_UNUSED2 exists, but we can ignore or treat as Unknown
        _ => CallbackEventKind::Unknown(event.event_type as i32),
    };

    match kind {
        CallbackEventKind::Connected | CallbackEventKind::Disconnected => {
            NEED_POLL.store(true, Ordering::SeqCst);
        }
        _ => {}
    }

    // Read the connector name (it's a fixed-size C char array)
    let connector = unsafe {
        // event.connector_name is [c_char; 32], we treat it as a C string
        CStr::from_ptr(event.connector_name.as_ptr())
            .to_string_lossy()
            .into_owned()
    };

    // Send to the channel (if initialized)
    if let Some(sender) = CALLBACK_EVENT_SENDER.get() {
        // If the receiver is gone, just drop the event – no harm.
        let _ = sender.send(CallbackEvent { kind, connector });
    }
}

/// Broadcasts events to all subscribers
fn broadcast_event(event: Event) {
    let mut subscribers = get_subscribers().lock().unwrap();
    subscribers.retain(|(_, sender)| sender.send(event.clone()).is_ok());
}


/// Init libddcutil, establish callbacks, polling, and start varlink service
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {

    std::panic::set_hook(Box::new(|panic_info| {
        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown"
        };
        let location = panic_info.location().unwrap_or_else(|| {
            // fallback location if not available
            panic::Location::caller()
        });
        log::error!(
        "PANIC at {}:{}: {}",
        location.file(),
        location.line(),
        payload
    );
    }));

    info!("Running with user privileges (UID: {})", rustix::process::getuid().as_raw());
    env_logger::init();
    ddcutil::init()?;

    // Create the channel
    let (tx, rx) = unbounded::<CallbackEvent>();
    CALLBACK_EVENT_SENDER.set(tx).unwrap();

    // Register the callback with libddcutil
    debug!("registering callback");
    let status = unsafe { ddcutil::ddca_register_display_status_callback(Some(my_display_callback)) };
    if status != 0 {
        eprintln!("Warning: failed to register display status callback: {}", status);
        // Polling will still work, so continue
    }

    // Spawn a thread to handle callback events and broadcast them to subscribers
    std::thread::spawn(move || {
        for ev in rx {
            // Build the Varlink Event struct
            let data = serde_json::json!({
            "connector": ev.connector,
            // Add more fields if desired, e.g., "io_path": ...
        }).to_string();

            let varlink_event = Event {
                kind: Event_kind::connected_displays_changed,
                data,
            };

            // Broadcast to all active subscribers
            broadcast_event(varlink_event);
        }
    });



    let service_impl = DdcutilService::new();

    // Create the Varlink service
    let interface = com_ddcutil_service::new(Box::new(service_impl));
    let service = VarlinkService::new(
        "com.ddcutil",
        "ddcutil-varlink",
        "1.0.0",
        "https://github.com/digitaltrails/ddcutil-varlink",
        vec![Box::new(interface)],
    );

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let socket_address = format!("unix:{}/ddcutil-varlink.socket", runtime_dir);

    // Check for systemd Socket Activation (LISTEN_FDS environment variable)
    // Will be on unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket
    if let Ok(fds) = env::var("LISTEN_FDS") {
        // Systemd handles binding the file descriptor for us.
        // We pass an empty/dummy address string because varlink crate
        // automatically prioritises the systemd FD when LISTEN_FDS exists.
        info!("LISTEN_FDS is set {}. Activated via systemd.", fds);
        info!("Listening on socket: {}", socket_address);  // Assuming all is good
        varlink::listen(service, "systemd:",
                        &varlink::ListenConfig {
                            idle_timeout: 0, // Stay alive permanently when run manually
                            ..Default::default()
                        })?;
    } else {
        // Fallback for manual local debugging/development
        // Dynamically build the path using XDG_RUNTIME_DIR safely
        let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());

        let fallback_address = format!("unix:{}/ddcutil-varlink.socket", runtime_dir);

        warn!("LISTEN_FDS is not set.  Running in manual mode.");
        info!("Listening on socket: {}", socket_address);

        varlink::listen(
            service,
            &socket_address,
            &varlink::ListenConfig {
                idle_timeout: 0, // Stay alive permanently when run manually
                ..Default::default()
            }
        )?;
    }

    Ok(())
}
