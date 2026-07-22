// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/main.rs
mod ddcutil;

use std::sync::{Arc, Mutex};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, panic, ptr, thread};
use std::ffi::CStr;
use std::fmt::format;
use std::time::Duration;
use varlink::*;
use log::{debug, info, warn, error};
use varlink::Result;
use std::ffi::c_void;

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
}

impl From<ddcutil::Error> for DdcError {
    fn from(e: ddcutil::Error) -> Self {
        DdcError::Ddcutil(e)
    }
}

/// Find a display by number or EDID, returning the raw dref and the DisplayList
/// that keeps it alive. The caller must hold onto the DisplayList for the
/// lifetime of the dref.
fn find_display(
    display_number: i64,
    edid_base64: &str,
    flags: i64,
) -> std::result::Result<(ddcutil::DisplayList, *mut c_void), DdcError> {
    let list = ddcutil::DisplayList::new((flags & 8) != 0)?;
    match list.find_by_number_or_edid(display_number, edid_base64, flags) {
        Some((_, _, dref)) => Ok((list, dref)),
        None => Err(DdcError::DisplayNotFound {
            display_number,
            edid_base64: edid_base64.to_string(),
            status: -1,
            message: format!("Display {} not found", display_number),
        }),
    }
}

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
            (status, err.to_string())
        }
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

// ============================================================================
// Service State
// ============================================================================
#[derive(Default)]
pub struct ServiceState {
    pub emit_connectivity: bool,
    pub poll_interval_secs: u32,
    pub poll_cascade_secs: f64,
    // Broadcast channel for events – we use a simple multi-producer multi-consumer channel.
    // Since we're in sync land, we use std::sync::mpsc, but we need a broadcast to many.
    // We'll use a Vec of Senders, each subscription gets its own channel.
    // But for simplicity, we can use a crossbeam_channel or a broadcast from tokio.
    // However, tokio is not used in the main loop. We'll use crossbeam_channel.
    // Add dependency: crossbeam-channel = "0.5"
    // For simplicity, we'll use a Vec<Mutex<Option<Box<dyn Fn(...) + Send>>>>? That's overkill.
    // Alternative: each Subscribe method has its own receiver; we store them in a list and send to all.
    // We'll use a Vec<crossbeam_channel::Sender<Event>>.
    // But we must handle the case where a subscriber disconnects.
    // To keep this concise, I'll show a simplified version using a broadcast channel from the 'broadcast' crate.
    // Actually, we can use std::sync::mpsc and a shared list of senders; we need to clean up dead ones.
    // For a production version, use crossbeam_channel::broadcast.
    // I'll add a dependency: crossbeam-channel = "0.5"
    // And use crossbeam_channel::bounded(64) for each subscription.
    // But for brevity, I'll just show the concept; the full code would need a proper event bus.
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

    fn list_displays(flags: i64) -> Result<Vec<DetectEntry>> {
        let list = ddcutil::DisplayList::new((flags & 8) != 0)?;
        let mut displays = Vec::new();
        for raw in list.iter() {
            let edid_enc = base64::encode(&raw.edid_bytes);
            displays.push(DetectEntry {
                display_number: raw.dispno as i64,
                usb_bus: 0,
                usb_device: 0,
                mfg_id: ddcutil::cstr_from_fixed_array(&raw.mfg_id),
                model_name: ddcutil::cstr_from_fixed_array(&raw.model_name),
                serial: ddcutil::cstr_from_fixed_array(&raw.sn),
                product_code: 0,
                edid_base64: edid_enc,
                binary_serial: 0,
            });
        }
        Ok(displays)
    }
}

impl VarlinkInterface for DdcutilService {

    fn detect(&self, call: &mut dyn Call_Detect, flags: i64) -> Result<()> {
        let status = unsafe { ddcutil::ddca_redetect_displays() };
        if status != 0 {
            let err_msg = format!("ddca_redetect_displays failed with status code: {}", status);
            call.reply_detect_error(status as i64, err_msg.clone())?;  // some unknown problem
            return Ok(())
        }
        let displays = Self::list_displays(flags)?;
        call.reply(displays.len() as i64, displays, 0, "OK".to_string())
    }

    fn list_detected(&self, call: &mut dyn Call_ListDetected, flags: i64) -> Result<()> {
        let displays = Self::list_displays(flags)?;
        call.reply(displays.len() as i64, displays, 0, "OK".to_string())
    }

    fn get_vcp(
        &self,
        call: &mut dyn Call_GetVcp,
        display_number: i64,
        edid_base64: String,
        vcp_code: i64,
        flags: i64,
    ) -> Result<()> {
        let result = (|| {
            let (_list, dref) = find_display(display_number, &edid_base64, flags)?;
            let mut handle = open_display_from_dref(dref)?;
            let (current, max, formatted) = ddcutil::get_vcp(&mut handle, vcp_code as u8)?;
            Ok((current, max, formatted))
        })();

        match result {
            Ok((current, max, formatted)) =>
                call.reply(current as i64, max as i64, formatted, 0, "OK".to_string()),
            Err(e) => {
                let (status, message) = extract_error_details(&e);
                call.reply_ddc_error(display_number, edid_base64, status, message)
            }
        }
    }

    fn set_vcp(
        &self,
        call: &mut dyn Call_SetVcp,
        display_number: i64,
        edid_base64: String,
        vcp_code: i64,
        new_value: i64,
        flags: i64,
    ) -> Result<()> {
        if self.locked.load(Ordering::SeqCst) {
            return call.reply_configuration_locked();
        }

        let result = (|| {
            let (_list, dref) = find_display(display_number, &edid_base64, flags)?;
            let mut handle = open_display_from_dref(dref)?;
            ddcutil::set_vcp(&mut handle, vcp_code as u8, new_value as u16)?;
            Ok(())
        })();

        match result {
            Ok(()) => call.reply(0, "OK".to_string()),
            Err(e) => {
                let (status, message) = extract_error_details(&e);
                call.reply_ddc_error(display_number, edid_base64, status, message)
            }
        }
    }

    fn get_multiple_vcp(
        &self,
        call: &mut dyn Call_GetMultipleVcp,
        display_number: i64,
        edid_base64: String,
        vcp_codes: Vec<i64>,
        flags: i64,
    ) -> Result<()> {
        if self.locked.load(Ordering::SeqCst) {
            return call.reply_configuration_locked(); // or a custom error
        }

        let result = (|| {
            let (_list, dref) = find_display(display_number, &edid_base64, flags)?;
            let mut handle = open_display_from_dref(dref)?;
            Ok(handle) // return the handle to the outer scope
        })();

        let mut handle = match result {
            Ok(h) => h,
            Err(e) => {
                let (status, message) = extract_error_details(&e);
                return call.reply_ddc_error(display_number, edid_base64, status, message);
            }
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
                    log::warn!("GetMultipleVcp: failed for VCP 0x{:02x}: {}", code, e);
                    error_messages.push(format!("VCP 0x{:02x}: {}", code, e));
                    overall_status = -1;
                }
            }
        }

        let message = if error_messages.is_empty() {
            "OK".to_string()
        } else {
            format!("Partial failure: {}", error_messages.join("; "))
        };
        call.reply(values, overall_status, message)
    }

    fn set_vcp_with_context(&self, call: &mut dyn Call_SetVcpWithContext, display_number: i64, edid_base64: String, vcp_code: i64, new_value: i64, client_context: String, flags: i64) -> Result<()> {
        call.reply(0, "Stub: set_vcp_with_context not implemented".to_string())
    }

    fn get_vcp_metadata(&self, call: &mut dyn Call_GetVcpMetadata, display_number: i64, edid_base64: String, vcp_code: i64, flags: i64) -> Result<()> {
        call.reply(
            "stub_feature".to_string(),
            "".to_string(),
            false, false, false, false, false,
            0,
            "Stub: get_vcp_metadata not implemented".to_string(),
        )
    }

    fn get_capabilities_string(&self, call: &mut dyn Call_GetCapabilitiesString, display_number: i64, edid_base64: String, flags: i64) -> Result<()> {
        call.reply("".to_string(), 0, "Stub: get_capabilities_string not implemented".to_string())
    }

    fn get_capabilities_metadata(&self, call: &mut dyn Call_GetCapabilitiesMetadata, display_number: i64, edid_base64: String, flags: i64) -> Result<()> {
        call.reply(
            "stub_model".to_string(),
            0, 0,
            vec![], // commands: []KeyValueIntString
            vec![], // capabilities: []KeyValueIntCapabilitiesFeature
            0,
            "Stub: get_capabilities_metadata not implemented".to_string(),
        )
    }

    fn get_display_state(
        &self,
        call: &mut dyn Call_GetDisplayState,
        display_number: i64,
        edid_base64: String,
        flags: i64,
    ) -> Result<()> {
        let result = (|| {
            let (_list, dref) = find_display(display_number, &edid_base64, flags)?;
            let status = unsafe { ddcutil::ddca_validate_display_ref(dref, true) };
            let message = ddcutil::get_status_message(status);
            Ok((status, message))
        })();

        match result {
            Ok((status, message)) => call.reply(status as i64, message),
            Err(e) => {
                let (status, message) = extract_error_details(&e);
                let edid = match &e {
                    DdcError::DisplayNotFound { edid_base64, .. } => edid_base64.clone(),
                    _ => edid_base64,
                };
                call.reply_ddc_error(display_number, edid, status, message)
            }
        }
    }

    fn get_sleep_multiplier(&self, call: &mut dyn Call_GetSleepMultiplier, display_number: i64, edid_base64: String, flags: i64) -> Result<()> {
        call.reply(1.0, 0, "Stub: get_sleep_multiplier not implemented".to_string())
    }

    fn set_sleep_multiplier(&self, call: &mut dyn Call_SetSleepMultiplier, display_number: i64, edid_base64: String, new_multiplier: f64, flags: i64) -> Result<()> {
        call.reply(0, "Stub: set_sleep_multiplier not implemented".to_string())
    }

    // ---------- Properties (as methods) ----------
    fn get_attributes_returned_by_detect(&self, call: &mut dyn Call_GetAttributesReturnedByDetect) -> Result<()> {
        call.reply(vec!["display_number".to_string(), "edid_base64".to_string()])
    }

    fn get_status_values(&self, call: &mut dyn Call_GetStatusValues) -> Result<()> {
        call.reply(vec![]) // empty dictionary-replacement array
    }

    fn get_ddcutil_version(&self, call: &mut dyn Call_GetDdcutilVersion) -> Result<()> {
        let version = unsafe { CStr::from_ptr(ddcutil::ddca_ddcutil_extended_version_string()) }.to_string_lossy().into_owned();
        call.reply(version)
    }

    fn get_ddcutil_dynamic_sleep(&self, call: &mut dyn Call_GetDdcutilDynamicSleep) -> Result<()> {
        call.reply(false)
    }

    fn set_ddcutil_dynamic_sleep(&self, call: &mut dyn Call_SetDdcutilDynamicSleep, enabled: bool) -> Result<()> {
        call.reply()
    }

    fn get_ddcutil_output_level(&self, call: &mut dyn Call_GetDdcutilOutputLevel) -> Result<()> {
        call.reply(0)
    }

    fn set_ddcutil_output_level(&self, call: &mut dyn Call_SetDdcutilOutputLevel, level: i64) -> Result<()> {
        call.reply()
    }

    fn get_display_event_types(&self, call: &mut dyn Call_GetDisplayEventTypes) -> Result<()> {
        call.reply(vec![])
    }

    fn get_service_interface_version(&self, call: &mut dyn Call_GetServiceInterfaceVersion) -> Result<()> {
        call.reply("1.0.0".to_string())
    }

    fn get_service_info_logging(&self, call: &mut dyn Call_GetServiceInfoLogging) -> Result<()> {
        call.reply(false)
    }

    fn set_service_info_logging(&self, call: &mut dyn Call_SetServiceInfoLogging, enabled: bool) -> Result<()> {
        call.reply()
    }

    fn get_service_emit_connectivity_signals(&self, call: &mut dyn Call_GetServiceEmitConnectivitySignals) -> Result<()> {
        call.reply(false)
    }

    fn set_service_emit_connectivity_signals(&self, call: &mut dyn Call_SetServiceEmitConnectivitySignals, enabled: bool) -> Result<()> {
        call.reply()
    }

    fn get_service_flag_options(&self, call: &mut dyn Call_GetServiceFlagOptions) -> Result<()> {
        call.reply(vec![])
    }

    fn get_service_parameters_locked(&self, call: &mut dyn Call_GetServiceParametersLocked) -> Result<()> {

        call.reply(self.locked.load(Ordering::SeqCst))
    }

    fn get_service_poll_cascade_interval(&self, call: &mut dyn Call_GetServicePollCascadeInterval) -> Result<()> {
        call.reply(0.5)
    }

    fn set_service_poll_cascade_interval(&self, call: &mut dyn Call_SetServicePollCascadeInterval, seconds: f64) -> Result<()> {
        if self.locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_string()).into());
        }
        // validation...
        call.reply()
    }

    // ---------- Subscribe ----------
    fn subscribe(&self, call: &mut dyn Call_Subscribe) -> Result<()> {
        // Send an initial event
        let event = Event {
            kind: "ServiceInitialized".to_string(),
            data: r#"{"flags":0}"#.to_string(),
        };
        call.reply(event)?;

        // Later, send a display change event
        let event = Event {
            kind: "ConnectedDisplaysChanged".to_string(),
            data: r#"{"edid_base64":"dummy","event_type":1,"flags":0}"#.to_string(),
        };
        call.reply(event)?;

        // Keep the call open...
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    // ---------- Properties
    fn get_service_poll_interval(
        &self,
        call: &mut dyn Call_GetServicePollInterval,
    ) -> Result<()> {
        let secs = self.state.lock().unwrap().poll_interval_secs;
        call.reply(secs as i64)
    }

    fn set_service_poll_interval(
        &self,
        call: &mut dyn Call_SetServicePollInterval,
        seconds: i64,
    ) -> Result<()> {
        if self.locked.load(Ordering::SeqCst) {
            return Err(varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_string()).into());
        }
        if seconds < 0 || (seconds > 0 && seconds < 10) {
            return Err(varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_string()).into());
        }

        call.reply()
    }

}

// ============================================================================
// Polling Task (runs in a background thread)
// ============================================================================
fn polling_task(state: Arc<Mutex<ServiceState>>) {
    let mut previous_edids = HashSet::new();
    loop {
        // debug!("poll locking");
        // Read config quickly, release the lock immediately
        let (emit, interval) = {
            let guard = state.lock().unwrap();
            (guard.emit_connectivity, guard.poll_interval_secs)
        }; // guard dropped here – lock released
        // debug!("poll unlocked");

        // If disabled, sleep without holding the lock
        if !emit {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        // Now do all blocking I/O – no lock held
        let current = match ddcutil::get_display_info_list(false) {
            Ok(list) => list,
            Err(_) => {
                std::thread::sleep(Duration::from_secs(interval as u64));
                continue;
            }
        };

        let current_edids: HashSet<String> =
            current.iter().map(|d| base64::encode(&d.edid_bytes)).collect();

        // Detect changes
        let added: Vec<_> = current_edids.difference(&previous_edids).collect();
        let removed: Vec<_> = previous_edids.difference(&current_edids).collect();

        // In a real implementation, you'd send events to all active subscriptions.
        // For now, just log.
        for edid in added {
            info!("Display connected: {}", edid);
        }
        for edid in removed {
            info!("Display disconnected: {}", edid);
        }
        previous_edids = current_edids;

        // Sleep until next poll
        thread::sleep(Duration::from_secs(interval as u64));
    }
}


// ============================================================================
// Main
// ============================================================================
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

    // Tell cargo to link against libddcutil (dynamic library)
    info!("Running with user privileges (UID: {})", rustix::process::getuid().as_raw());
    env_logger::init();
    ddcutil::init()?;

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

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
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
        let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());

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
