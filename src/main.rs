// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/main.rs
mod ddcutil;

use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{unbounded, Sender};
use log::{debug, error, info, warn};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::{env, panic, ptr, thread};
use varlink::Result;
use varlink::*;

static SUBSCRIBER_ID: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIBERS: OnceLock<Mutex<Vec<(usize, Sender<Event>)>>> = OnceLock::new();


fn get_subscribers() -> &'static Mutex<Vec<(usize, Sender<Event>)>> {
    SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
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
enum DdcServiceError {
    Ddcutil(ddcutil::Error),
    InvalidIdentifier(String),
}

impl From<ddcutil::Error> for DdcServiceError {
    fn from(e: ddcutil::Error) -> Self {
        DdcServiceError::Ddcutil(e)
    }
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

// Include the generated interface module.
// The generator creates a module named after the interface file.
// For example, "com.ddcutil.service" becomes "com_ddcutil_service".
mod com_ddcutil_service;
use com_ddcutil_service::*;
use crate::ddcutil::{get_status_message, DdcutilEventKind, DisplayInfo};


// ============================================================================
// Interface Implementation
// ============================================================================
pub struct DdcutilService {
    ddcutil_mutex: Mutex<ddcutil::Ddcutil>,     // Thread safe access to service (easier to apply it access the board).
    configuration_locked: Arc<AtomicBool>,      // Service will reject requests to change configuration parameters.
}

// Varlink DdcutilService
impl DdcutilService {
    pub fn new(ddcutil_instance: ddcutil::Ddcutil) -> Self {

        Self {
            ddcutil_mutex: Mutex::new(ddcutil_instance),
            configuration_locked: Arc::new(AtomicBool::new(false)),
        }
    }

    fn list_displays(include_offline: bool) -> Result<Vec<DetectEntry>> {
        let display_info = ddcutil::list_displays(include_offline)?;
        let mut result = Vec::with_capacity(display_info.len());
        for raw in display_info { // assuming you have a slice of raw pointers
            let info = DisplayInfo::from(raw);
            result.push(DetectEntry::from(&info));
        }
        Ok(result)
    }

    fn enable_polling(&self, use_polling: bool) {
        let mut ddcutil = self.ddcutil_mutex.lock().unwrap();
        if use_polling {
            ddcutil.start_polling();
        }
        else {
            ddcutil.stop_polling()
        }
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
    }
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

        let ddc_operation_fn = || -> std::result::Result<_, DdcServiceError> {
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
        let ddc_operation_fn = || -> std::result::Result<_, DdcServiceError> {
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

        let ddc_operation_fn = || -> std::result::Result<_, DdcServiceError> {
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

    fn get_multiple_vcp(
        &self,
        call: &mut dyn Call_GetMultipleVcp,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        vcp_codes: Vec<i64>,
        options: Option<CallOptions>
    ) -> Result<()> {

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
        call.reply(self.configuration_locked.load(Ordering::SeqCst))
    }

    fn get_service_poll_cascade_interval(&self, call: &mut dyn Call_GetServicePollCascadeInterval) -> Result<()> {
        call.reply(self.ddcutil_mutex.lock().unwrap().get_cascade_interval())
    }

    fn get_service_poll_interval(&self, call: &mut dyn Call_GetServicePollInterval) -> Result<()> {
        call.reply(self.ddcutil_mutex.lock().unwrap().get_poll_interval() as i64)
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

        let ddc_operation_fn = || -> std::result::Result<_, DdcServiceError> {
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
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        call.reply()
    }

    fn set_ddcutil_output_level(&self, call: &mut dyn Call_SetDdcutilOutputLevel, level: i64) -> Result<()> {
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        call.reply()
    }

    fn set_service_info_logging(&self, call: &mut dyn Call_SetServiceInfoLogging, enabled: bool) -> Result<()> {
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        call.reply()
    }

    fn set_service_poll_cascade_interval(&self, call: &mut dyn Call_SetServicePollCascadeInterval, seconds: f64,) -> Result<()> {
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        // Validation moved to setter? Or keep here.
        if seconds < 0.0 || (seconds > 0.0 && seconds < 1.0) {
            return Err(varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_owned()).into());
        }
        self.ddcutil_mutex.lock().unwrap().set_cascade_interval(seconds)
            .map_err(|e| varlink::ErrorKind::InvalidParameter(e.to_string()))?;
        call.reply()
    }

    fn set_service_poll_interval(&self, call: &mut dyn Call_SetServicePollInterval, seconds: i64,) -> Result<()> {
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        if seconds < 0 || (seconds > 0 && seconds < 10) {
            return Err(varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_owned()).into());
        }
        self.ddcutil_mutex.lock().unwrap().set_poll_interval(seconds as u32)
            .map_err(|e| varlink::ErrorKind::InvalidParameter(e.to_string()))?;
        call.reply()
    }

    fn set_sleep_multiplier(&self, call: &mut dyn Call_SetSleepMultiplier,
                            display_number: Option<i64>,
                            edid_base64: Option<String>,
                            new_multiplier: f64,
                            options: Option<CallOptions>) -> Result<()> {
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into());
        }
        call.reply(0, "Stub: set_sleep_multiplier not implemented".to_owned())
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

        let ddc_operation_fn = || -> std::result::Result<_, DdcServiceError> {
            let (_list, dref) = ddcutil::find_display(display_number, edid_base64.as_deref(), is_edid_prefix_allowed(&options))?;
            let mut handle = open_display_from_dref(dref)?;
            let client_context_string: String = client_context.unwrap_or_default();
            let verify = is_setvcp_verifying(&options);

            ddcutil::set_vcp(&mut handle, vcp_code as u8, new_value as u16, verify)?;

            let event = build_vcp_changed_event(
                display_number,
                edid_base64.as_deref(),
                vcp_code,
                new_value,
                client_context_string,
            );
            broadcast_event(event);

            Ok(())
        };

        match ddc_operation_fn() {
            Ok(()) => call.reply(0, "OK".to_owned()),
            Err(e) => return send_ddc_error(call, display_number, edid_base64, &e),
        }
    }

    fn subscribe(&self, call: &mut dyn Call_Subscribe, use_polling: bool) -> Result<()> {

        debug!("subscribe use_polling={}", use_polling);
        self.enable_polling(use_polling);

        // 1. Create a channel for this subscriber
        let (tx, rx) = unbounded::<Event>();
        let id = SUBSCRIBER_ID.fetch_add(1, Ordering::SeqCst);

        let _ = self.ddcutil_mutex.lock().unwrap().set_events_enable(true);

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
fn open_display_from_dref(dref: *mut c_void) -> std::result::Result<ddcutil::DisplayHandle, DdcServiceError> {
    ddcutil::open_display(dref).map_err(DdcServiceError::Ddcutil)
}

/// Extract status code and message from a DdcError for the Varlink error reply.
fn extract_error_details(e: &DdcServiceError) -> (i64, String) {
    match e {
        DdcServiceError::Ddcutil(err) => {
            // You can map specific error kinds to custom status codes if desired.
            let status = match err {
                ddcutil::Error::Status(code) => *code as i64,
                _ => -1, // generic failure
            };
            (status, format!("{} - {}", err, get_status_message(status as i32)))
        }
        DdcServiceError::InvalidIdentifier(msg) => (-1, msg.clone()),
    }
}

fn send_ddc_error(
    call: &mut dyn VarlinkCallError,
    display_number: Option<i64>,
    edid_base64: Option<String>,
    error: &DdcServiceError,
) -> varlink::Result<()> {
    let (status, message) = extract_error_details(error);
    let edid = edid_base64.unwrap_or_else(String::new);
    call.reply_ddc_error(display_number.unwrap_or(-1), edid, status, message)
}

fn convert_ddc_event(ddc_event: ddcutil::DdcutilEvent) -> Option<Event> {
    match ddc_event.kind {
        DdcutilEventKind::Connected |
        DdcutilEventKind::Disconnected |
        DdcutilEventKind::ConnectedDisplaysChanged |
        DdcutilEventKind::DpmsAwake |
        DdcutilEventKind::DpmsAsleep => {
            let data = serde_json::json!({
                "data": format!("{} {}", ddc_event.kind.as_str(), ddc_event.data),
            }).to_string();
            Some(Event {
                kind: Event_kind::connected_displays_changed,
                data,
            })
        }
        _ => return None,
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

    let (ddcutil_instance, event_rx) = ddcutil::Ddcutil::create();

    // Spawn a thread to forward events to Varlink subscribers
    std::thread::spawn(move || {
        for ddcutil_event in event_rx {
            if let Some(varlink_event) = convert_ddc_event(ddcutil_event) {
                broadcast_event(varlink_event);
            }
        }
    });

    let service_impl = DdcutilService::new(ddcutil_instance);

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
