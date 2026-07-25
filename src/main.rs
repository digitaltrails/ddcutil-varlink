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

/// Convert a raw `DDCA_Capabilities` from libddcutil into Varlink-friendly structures.
///
/// # Safety
/// The `caps_ptr` must be a valid pointer to a parsed capabilities struct.
unsafe fn parse_capabilities_to_varlink(
    caps_ptr: *mut ddcutil::DDCA_Capabilities,
    handle: &ddcutil::DisplayHandle,
) -> std::result::Result<(u8, u8, Vec<KeyValueIntString>, Vec<KeyValueIntCapabilitiesFeature>), DdcError> {
    if caps_ptr.is_null() {
        return Err(DdcError::InvalidIdentifier("Capabilities pointer is null".to_string()));
    }

    let caps = &*caps_ptr; // Dereference the raw pointer (safe because we checked for null)

    // 1. Extract MCCS version
    let major = caps.version_spec.major;
    let minor = caps.version_spec.minor;

    // 2. Build the `commands` vector (KeyValueIntString)
    let mut commands = Vec::with_capacity(caps.cmd_ct as usize);
    for i in 0..caps.cmd_ct as usize {
        let cmd_code = *caps.cmd_codes.add(i);
        // Use the built-in description from libddcutil if available.
        let desc = ddcutil::get_feature_name(cmd_code)?; // You may need to add this helper
        commands.push(KeyValueIntString {
            key: cmd_code as i64,
            value: desc,
        });
    }

    // 3. Build the `capabilities` vector (KeyValueIntCapabilitiesFeature)
    let mut capabilities = Vec::with_capacity(caps.vcp_code_ct as usize);
    for i in 0..caps.vcp_code_ct as usize {
        let vcp = &*caps.vcp_codes.add(i); // Reference to DDCA_Cap_Vcp

        // Get metadata for this feature
        let mut meta_ptr: *mut ddcutil::DDCA_Feature_Metadata = std::ptr::null_mut();
        let status = ddcutil::ddca_get_feature_metadata_by_dh(
            vcp.feature_code,
            handle.handle, // raw handle
            true, // create_default_if_not_found = true
            &mut meta_ptr,
        );

        if status != ddcutil::DDCRC_OK {
            // Log but continue – use fallback values
            log::warn!("Failed to get metadata for feature 0x{:02x}: {}", vcp.feature_code, status);
        }

        // Build the feature entry
        let feature_name = if meta_ptr.is_null() {
            format!("VCP 0x{:02x}", vcp.feature_code)
        } else {
            let meta = &*meta_ptr;
            std::ffi::CStr::from_ptr(meta.feature_name)
                .to_string_lossy()
                .into_owned()
        };

        let feature_desc = if meta_ptr.is_null() {
            String::new()
        } else {
            let meta = &*meta_ptr;
            if meta.feature_desc.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(meta.feature_desc)
                    .to_string_lossy()
                    .into_owned()
            }
        };

        // Build the `values` vector (CapabilitiesValueEntry)
        let mut values = Vec::with_capacity(vcp.value_ct as usize);

        // First, get the value names from metadata (if available)
        let mut value_map: std::collections::HashMap<u8, String> = std::collections::HashMap::new();
        if !meta_ptr.is_null() {
            let meta = &*meta_ptr;
            let mut fve_ptr = meta.sl_values;
            while !fve_ptr.is_null() && !(*fve_ptr).value_name.is_null() {
                let entry = &*fve_ptr;
                value_map.insert(entry.value_code,
                                 std::ffi::CStr::from_ptr(entry.value_name)
                                     .to_string_lossy()
                                     .into_owned()
                );
                fve_ptr = fve_ptr.add(1);
            }
        }

        // Now iterate over the actual values
        for j in 0..vcp.value_ct as usize {
            let value_code = *vcp.values.add(j);
            let value_name = value_map.get(&value_code)
                .cloned()
                .unwrap_or_else(|| format!("Value 0x{:02x}", value_code));

            values.push(CapabilitiesValueEntry {
                value_code: value_code as i64,
                value_name,
            });
        }

        // Free metadata if we allocated it
        if !meta_ptr.is_null() {
            ddcutil::ddca_free_feature_metadata(meta_ptr);
        }

        let feature = CapabilitiesFeature {
            feature_name,
            feature_description: feature_desc,
            values,
        };

        capabilities.push(KeyValueIntCapabilitiesFeature {
            key: vcp.feature_code as i64,
            value: feature,
        });
    }

    Ok((major, minor, commands, capabilities))
}
/// Frees a C string allocated by libddcutil.
/// # Safety
/// The pointer must have been allocated by `malloc` and not previously freed.
unsafe fn free_c_string(ptr: *mut libc::c_char) {
    if !ptr.is_null() {
        libc::free(ptr as *mut libc::c_void);
    }
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
        let list = ddcutil::DisplayList::new(include_offline)?;
        let mut displays = Vec::new();
        for raw in list.iter() {
            let edid_enc = general_purpose::STANDARD.encode(&raw.edid_bytes);
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
        let status = unsafe { ddcutil::ddca_redetect_displays() };
        if status != 0 {
            let err_msg = format!("ddca_redetect_displays failed with status code: {}", status);
            call.reply_detect_error(status as i64, err_msg.clone())?;  // some unknown problem
            return Ok(())
        }
        let displays = Self::list_displays(include_offline)?;
        call.reply(displays.len() as i64, displays, 0, "OK".to_owned())
    }

    // ---------- Properties (as methods) ----------
    fn get_attributes_returned_by_detect(&self, call: &mut dyn Call_GetAttributesReturnedByDetect) -> Result<()> {
        call.reply(vec!["display_number".to_owned(), "edid_base64".to_owned()])
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
            let (_list, dref) = find_display(display_number, edid_ref, &options)?;
            let handle = open_display_from_dref(dref)?;

            // 1. Get the raw capabilities string
            let mut caps_text: *mut libc::c_char = std::ptr::null_mut();
            let status1 = unsafe {
                ddcutil::ddca_get_capabilities_string(handle.handle, &mut caps_text)
            };
            if status1 != ddcutil::DDCRC_OK {
                return Err(DdcError::Ddcutil(ddcutil::Error::Status(status1)));
            }

            // 2. Parse the capabilities string
            let mut parsed_caps_ptr: *mut ddcutil::DDCA_Capabilities = std::ptr::null_mut();
            let status2 = unsafe {
                ddcutil::ddca_parse_capabilities_string(caps_text, &mut parsed_caps_ptr)
            };
            // Free the raw string immediately – we no longer need it.
            unsafe { free_c_string(caps_text); }

            if status2 != ddcutil::DDCRC_OK {
                return Err(DdcError::Ddcutil(ddcutil::Error::Status(status2)));
            }

            // 3. Convert the parsed capabilities to Varlink types
            //    The `parse_capabilities_to_varlink` helper does this.
            let (mccs_major, mccs_minor, commands, capabilities) = unsafe {
                parse_capabilities_to_varlink(parsed_caps_ptr, &handle)
            }?;

            // 4. Free the parsed capabilities struct
            unsafe { ddcutil::ddca_free_parsed_capabilities(parsed_caps_ptr); }

            // 5. Extract model name from the display info
            let model_name = unsafe {
                // We have the `dref` from earlier – get the display info
                let mut dinfo_ptr: *mut ddcutil::DDCA_Display_Info = std::ptr::null_mut();
                let status3 = ddcutil::ddca_get_display_info(dref, &mut dinfo_ptr);
                if status3 != ddcutil::DDCRC_OK {
                    return Err(DdcError::Ddcutil(ddcutil::Error::Status(status3)));
                }
                let dinfo = &*dinfo_ptr;
                let name = std::ffi::CStr::from_ptr(dinfo.model_name.as_ptr())
                    .to_string_lossy()
                    .into_owned();
                ddcutil::ddca_free_display_info(dinfo_ptr);
                name
            };

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
            let (_list, dref) = find_display(display_number, edid_ref, &options)?;
            let handle = open_display_from_dref(dref)?;
            debug!("get_capabilities_string - found display");
            let mut caps_ptr: *mut libc::c_char = std::ptr::null_mut();
            let raw_handle = handle.handle;
            let status = unsafe {
                ddcutil::ddca_get_capabilities_string(raw_handle, &mut caps_ptr)
            };
            debug!("get_capabilities_string - status: {}", status);
            if status != 0 {
                return Err(DdcError::Ddcutil(ddcutil::Error::Status(status)));
            }

            // Convert the C string to a Rust String. The pointer should be non‑null on success.
            let caps_str = unsafe {
                if caps_ptr.is_null() {
                    String::new()
                } else {
                    debug!("get_capabilities_string - converting:");
                    let cstr = std::ffi::CStr::from_ptr(caps_ptr);
                    let result = cstr.to_string_lossy().into_owned();
                    debug!("get_capabilities_string - converted {}", result);
                    //  Free the C string immediately after conversion.
                    free_c_string(caps_ptr);
                    result
                }
            };
            Ok(caps_str)
        };

        match ddc_operation_fn() {
            Ok(caps) => call.reply(caps, 0, "OK".to_string()),
            Err(e) => send_ddc_error(call, display_number, edid_base64, &e),
        }
    }

    fn get_ddcutil_dynamic_sleep(&self, call: &mut dyn Call_GetDdcutilDynamicSleep) -> Result<()> {
        let enabled = unsafe { ddcutil::ddca_is_dynamic_sleep_enabled() };
        call.reply(enabled)
    }

    fn get_ddcutil_output_level(&self, call: &mut dyn Call_GetDdcutilOutputLevel) -> Result<()> {
        let output_level = unsafe { ddcutil::ddca_get_output_level() };
        call.reply(output_level as i64)
    }

    fn get_ddcutil_version(&self, call: &mut dyn Call_GetDdcutilVersion) -> Result<()> {
        let version = unsafe { CStr::from_ptr(ddcutil::ddca_ddcutil_extended_version_string()) }.to_string_lossy().into_owned();
        call.reply(version)
    }

    fn get_display_state(
        &self,
        call: &mut dyn Call_GetDisplayState,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        options: Option<CallOptions>
    ) -> Result<()> {

        let ddc_operation_fn = || -> std::result::Result<_, DdcError> {
            let (_list, dref) = find_display(display_number, edid_base64.as_deref(), &options)?;
            let status = unsafe { ddcutil::ddca_validate_display_ref(dref, true) };
            let message = ddcutil::get_status_message(status);
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
            let (_list, dref) = find_display(display_number, edid_base64.as_deref(), &options)?;
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
            let (_list, dref) = find_display(display_number, edid_base64.as_deref(), &options)?;
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
                    log::warn!("GetMultipleVcp: failed for VCP 0x{:02x}: {}", code, e);
                    error_messages.push(format!("VCP 0x{:02x}: {}", code, e));
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

            let (_list, dref) = find_display(display_number, edid_base64.as_deref(), &options)?;
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
fn find_display(
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    options: &Option<CallOptions>,
) -> std::result::Result<(ddcutil::DisplayList, *mut c_void), DdcError> {
    let allow_edid_prefix = is_edid_prefix_allowed(options);
    let list = ddcutil::DisplayList::new(allow_edid_prefix)?;

    if display_number.is_none() && edid_base64.is_none() {
        if display_number.is_none() && edid_base64.is_none() {
            return Err(DdcError::InvalidIdentifier(
                "Must provide either display_number or edid_base64".to_owned()
            ));
        }
    }
    let target_display_number: i64 = display_number.unwrap_or(-1);
    let target_edid_base64: &str = edid_base64.unwrap_or("");


    match list.find_by_number_or_edid(target_display_number, target_edid_base64, allow_edid_prefix) {
        Some((_, _, dref)) => Ok((list, dref)),
        None => Err(DdcError::DisplayNotFound {
            display_number: target_display_number,
            edid_base64: target_edid_base64.to_string(),
            status: -1,
            message: format!("Display {} not found", target_display_number),
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

        let status = unsafe { ddcutil::ddca_redetect_displays() };
        if status != 0 {
            error!("ddca_redetect_displays failed with status: {}", status);
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
