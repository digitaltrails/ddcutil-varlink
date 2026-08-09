//SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
//SPDX-License-Identifier: GPL-2.0-or-later
// src/ddcutil.rs

use std::collections::{HashMap, HashSet};
use base64::{engine::general_purpose, Engine as _};
use log::{debug, error, info};
use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

static CALLBACK_EVENT_SENDER: OnceLock<Sender<DdcutilEvent>> = OnceLock::new();

// import the Varlink event type
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};

use crate::com_ddcutil_service::DetectEntry;
use crate::ddcutil;

// Include the generated bindings
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("DDC/CI error: {0}")]
    Status(c_int),
    #[error("UTF-8 conversion error")]
    Utf8,
    #[error("No Display_Number or EDID supplied")]
    MissingIdentifier,
    #[error("Display not found")]
    DisplayNotFound {
        display_ref: i64,
        display_number: i64,
        edid_base64: String,
        status: i64,
        message: String,
    },
    #[error("Missing Capabilities")]
    MissingCapabilities,
    #[error("DDC/CI DPMS query failed")]
    DpmsQueryFailed {
        display_ref: i64,
        message: String,
    },
}

impl Error {
    pub fn status_code(&self) -> i64 {
        match self {
            Error::Status(code) => *code as i64,
            _ => -1,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// RAII handle for display
pub struct DisplayHandle {
    pub handle: DDCA_Display_Handle,
    dref: *mut std::ffi::c_void, // we keep dref for metadata
}

impl Drop for DisplayHandle {
    fn drop(&mut self) {
        unsafe {
            ddca_close_display(self.handle);
        }
    }
}

#[derive(Clone)]
pub struct DisplayInfo {
    pub display_ref: usize,
    pub display_number: i32,
    pub manufacturer_id: String,
    pub model_name: String,
    pub serial_number: String,
    pub edid_bytes: [u8; 128],
    pub product_code: u16,
    pub usb_bus: i32,
    pub usb_device: i32,
}

#[derive(Debug, Clone)]
pub struct CapabilitiesData {
    pub mccs_major: u8,
    pub mccs_minor: u8,
    pub commands: Vec<CommandData>,
    pub features: Vec<FeatureData>,
}

#[derive(Debug, Clone)]
pub struct CommandData {
    pub code: u8,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct FeatureData {
    pub code: u8,
    pub name: String,
    pub description: String,
    pub values: Vec<ValueData>,
}

#[derive(Debug, Clone)]
pub struct ValueData {
    pub code: u8,
    pub name: String,
}

impl From<&DDCA_Display_Info> for DisplayInfo {
    fn from(raw: &DDCA_Display_Info) -> Self {
        Self {
            display_ref: raw.dref as usize,
            display_number: raw.dispno,
            manufacturer_id: cstr_from_fixed_array(&raw.mfg_id),
            model_name: cstr_from_fixed_array(&raw.model_name),
            product_code: raw.product_code,
            usb_bus: raw.usb_bus,
            usb_device: raw.usb_device,
            serial_number: cstr_from_fixed_array(&raw.sn),
            edid_bytes: raw.edid_bytes,
        }
    }
}

impl From<&DisplayInfo> for DetectEntry {
    fn from(info: &DisplayInfo) -> Self {
        Self {
            display_ref: info.display_ref as i64,
            display_number: info.display_number as i64,
            usb_bus: info.usb_bus as i64,
            usb_device: info.usb_device as i64,
            mfg_id: info.manufacturer_id.clone(),
            model_name: info.model_name.clone(),
            serial: info.serial_number.clone(),
            product_code: info.product_code as i64,
            edid_base64: base64::encode(&info.edid_bytes),
            binary_serial: 0, // or compute from EDID/serial_number
        }
    }
}

#[derive(Debug, Clone)]
pub enum DdcutilEventKind {
    Connected,
    Disconnected,
    ConnectedDisplaysChanged,
    DpmsAwake,
    DpmsAsleep,
    DdcWorking,
    DdcNotWorking, // optional, depending on what the library provides
    Unknown(i32),  // fallback for future event types
}

impl DdcutilEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DdcutilEventKind::Connected => "DisplayConnected",
            DdcutilEventKind::Disconnected => "DisplayDisconnected",
            DdcutilEventKind::ConnectedDisplaysChanged => "ConnectedDisplaysChanged",
            DdcutilEventKind::DpmsAwake => "DpmsAwake",
            DdcutilEventKind::DpmsAsleep => "DpmsAsleep",
            DdcutilEventKind::DdcWorking => "DdcWorking",
            DdcutilEventKind::DdcNotWorking => "DdcNotWorking",
            DdcutilEventKind::Unknown(_) => "Unknown",
        }
    }
}

pub struct DdcutilEvent {
    pub kind: DdcutilEventKind,
    pub data: String,
    // optionally: io_path, flags, etc.
}

pub fn list_displays(include_invalid: bool) -> Result<Vec<DisplayInfo>> {
    let list = DisplayList::new(include_invalid)?;
    let mut result = Vec::with_capacity(list.len());

    for raw in list.iter() {
        result.push(DisplayInfo::from(raw));
    }

    Ok(result)
}

pub struct DisplayList {
    ptr: *mut DDCA_Display_Info_List,
}

impl DisplayList {
    pub fn new(include_invalid: bool) -> Result<Self> {
        let mut list_ptr = ptr::null_mut();
        let status = unsafe { ddca_get_display_info_list2(include_invalid, &mut list_ptr) };
        if status != 0 {
            return Err(Error::Status(status));
        }
        if list_ptr.is_null() {
            return Err(Error::Status(-1));
        }
        let list = unsafe { &*list_ptr };
        log::debug!("find_by_number_or_edid: list.ct = {}", list.ct);

        for i in 0..list.ct {
            let raw = unsafe { &*list.info.as_ptr().add(i as usize) };
            log::debug!("list dref={} dispno={}", raw.dref as usize, raw.dispno as i64);
        }
        Ok(DisplayList { ptr: list_ptr })
    }

    /// Find a display by display_number or EDID (with optional prefix match).
    /// Returns (dispno, edid_base64, dref) if found.
    pub fn find_by_id(
        &self,
        display_ref: Option<i64>,
        display_number: Option<i64>,
        edid_base64: Option<&str>,
        allow_edid_prefix: bool,
    ) -> Option<(i32, String, *mut std::ffi::c_void)> {
        let target_display_ref = display_ref.unwrap_or(0) ;
        let target_display_number: i64 = display_number.unwrap_or(-1);
        let target_edid_base64: &str = edid_base64.unwrap_or("");

        log::debug!("find_by_number_or_edid: entered, list ptr = {:?}", self.ptr);
        if self.ptr.is_null() {
            log::error!("find_by_number_or_edid: null pointer");
            return None;
        }
        let list = unsafe { &*self.ptr };
        log::debug!("find_by_number_or_edid: list.ct = {}", list.ct);

        for i in 0..list.ct {
            log::info!("find_by_number_or_edid: checking i={}", i);
            let raw = unsafe { &*list.info.as_ptr().add(i as usize) };
            // display_ref precedence
            log::info!("refs target='{}' raw dref='{}' dispno={}", target_display_ref, raw.dref as i64, raw.dispno);
            if !display_ref.is_none() && target_display_ref == raw.dref as i64 {
                let edid = general_purpose::STANDARD.encode(&raw.edid_bytes);
                return Some((raw.dispno, edid, raw.dref));
            }
            // display_number precedence
            if !display_number.is_none() && target_display_number == raw.dispno as i64 {
                let edid = general_purpose::STANDARD.encode(&raw.edid_bytes);
                return Some((raw.dispno, edid, raw.dref));
            }
            // EDID matching
            if !edid_base64.is_none() {
                let edid = general_purpose::STANDARD.encode(&raw.edid_bytes);
                let matches = if allow_edid_prefix {
                    edid.starts_with(target_edid_base64)
                } else {
                    edid == target_edid_base64
                };
                if matches {
                    return Some((raw.dispno, edid, raw.dref));
                }
            }
        }
        log::info!("find_by_number_or_edid: not found");
        None
    }

    pub fn len(&self) -> usize {
        let list = unsafe { &*self.ptr };
        list.ct as usize
    }

    /// Iterate over all displays (useful for Detect)
    pub fn iter(&self) -> DisplayListIter<'_> {
        DisplayListIter {
            list: unsafe { &*self.ptr },
            index: 0,
        }
    }
}

impl Drop for DisplayList {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            log::info!("Dropping DisplayList, freeing ptr={:p}", self.ptr);
            unsafe {
                ddca_free_display_info_list(self.ptr);
            }
        } else {
            log::warn!("DisplayList drop: ptr is null, skipping free");
        }
    }
}

/// Iterator over DisplayInfo entries
pub struct DisplayListIter<'a> {
    list: &'a DDCA_Display_Info_List,
    index: usize,
}

impl<'a> Iterator for DisplayListIter<'a> {
    type Item = &'a DDCA_Display_Info;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.list.ct as usize {
            let item = unsafe { &*self.list.info.as_ptr().add(self.index) };
            self.index += 1;
            Some(item)
        } else {
            None
        }
    }
}

/// Get a human‑readable message for a DDCA_Status code,
/// including any additional error detail from libddcutil.
pub fn get_status_message(status: i32) -> String {
    // Get the base status name (e.g., "DDCRC_OK", "DDCRC_RETRIES")
    let name_ptr = unsafe { ddca_rc_name(status) };
    let name = if name_ptr.is_null() {
        format!("Unknown error code {}", status)
    } else {
        unsafe { CStr::from_ptr(name_ptr) }
            .to_string_lossy()
            .into_owned()
    };

    // If status is OK, return just the name
    if status == 0 {
        return name;
    }

    // Description
    let desc_ptr = unsafe { ddca_rc_desc(status) };
    let desc: String = if desc_ptr.is_null() {
        "".to_owned()
    } else {
        unsafe { CStr::from_ptr(desc_ptr) }
            .to_string_lossy()
            .into_owned()
    };

    // Detail
    let detail_ptr = unsafe { ddca_get_error_detail() };
    let detail_str = if detail_ptr.is_null() {
        "no details".to_owned()
    } else {
        let detail = unsafe { &*detail_ptr };
        unsafe { CStr::from_ptr(detail.detail) }
            .to_string_lossy()
            .into_owned()
    };

    let message = format!("{}: {}: {}", name, desc, detail_str);

    // 3. Free the detail struct (if allocated)
    if !detail_ptr.is_null() {
        unsafe { ddca_free_error_detail(detail_ptr) };
    }
    //debug!("Message {}", message);
    message
}

pub fn init() -> Result<()> {
    unsafe {
        log::info!("initializing ddcutil");
        let status = ddca_init(
            std::ptr::null(), // no options string
            9,                // LOG_NOTICE
            0,
        );
        if status != 0 {
            log::error!("ddca_init failed {} {}", status, get_status_message(status));
            return Err(Error::Status(status));
        }
    }
    Ok(())
}

pub fn redetect() -> Result<()> {
    unsafe {
        let status = ddca_redetect_displays();
        if status != 0 {
            return Err(Error::Status(status));
        }
    }
    Ok(())
}

pub fn get_display_info_list(include_invalid: bool) -> Result<Vec<DisplayInfo>> {
    let mut list_ptr = ptr::null_mut();
    let status = unsafe {
        ddca_get_display_info_list2(if include_invalid { true } else { false }, &mut list_ptr)
    };
    if status != 0 {
        return Err(Error::Status(status));
    }

    let list = unsafe { &*list_ptr };
    let mut infos = Vec::with_capacity(list.ct as usize);

    for i in 0..list.ct {
        // Access the i-th element using pointer arithmetic
        let raw = unsafe { &*list.info.as_ptr().add(i as usize) };
        infos.push(raw.into());
    }

    unsafe {
        ddca_free_display_info_list(list_ptr);
    }
    Ok(infos)
}

pub fn find_display(
    display_ref: Option<i64>,
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    allow_edid_prefix: bool,
) -> Result<(DisplayList, *mut c_void)> {
    let list = DisplayList::new(allow_edid_prefix)?;

    if display_ref.is_none() && display_number.is_none() && edid_base64.is_none() {
        return Err(Error::MissingIdentifier);
    }


    match list.find_by_id(display_ref, display_number, edid_base64, allow_edid_prefix)
    {
        Some((_, _, dref)) => Ok((list, dref)),
        None => {
            let edid_display = edid_base64.unwrap_or("");
            // let edid_display = (!edid_base64.is_none())
            //     .then_some(edid_base64.unwrap())
            //     .unwrap_or("None");
            Err(Error::DisplayNotFound {
                display_ref: display_ref.unwrap_or(0),
                display_number: display_number.unwrap_or(-1),
                edid_base64: edid_display.to_owned(),
                status: -1,  // TODO what should this be
                message: format!(
                    "DisplayRef={:?} DisplayNumber={:?} EDID={:?} - display not found",
                    display_ref,  display_number, edid_display
                ),
            })
        }
    }
}

pub fn open_display(dref: *mut std::ffi::c_void) -> Result<DisplayHandle> {
    let mut handle: DDCA_Display_Handle = ptr::null_mut();
    let status = unsafe { ddca_open_display2(dref, true, &mut handle) };
    if status != 0 {
        return Err(Error::Status(status));
    }
    Ok(DisplayHandle { handle, dref })
}

pub fn get_display_state(
    display_ref: Option<i64>,
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    allow_edid_prefix: bool,
) -> Result<(DDCA_Status, String)> {
    let (_list, dref) = find_display(display_ref, display_number, edid_base64, allow_edid_prefix)?;
    let status = unsafe { ddca_validate_display_ref(dref, true) };
    let message = get_status_message(status);
    Ok((status, message))
}

pub fn is_dynamic_sleep_enabled() -> bool {
    unsafe { ddca_is_dynamic_sleep_enabled() }
}

pub fn get_output_level() -> DDCA_Output_Level {
    unsafe { ddca_get_output_level() }
}

pub fn get_ddcutil_version() -> String {
    unsafe { CStr::from_ptr(ddca_ddcutil_extended_version_string()) }
        .to_string_lossy()
        .into_owned()
}


/// Frees a C string allocated by libddcutil.
/// # Safety
/// The pointer must have been allocated by `malloc` and not previously freed.
unsafe fn free_c_string(ptr: *mut libc::c_char) {
    if !ptr.is_null() {
        libc::free(ptr as *mut libc::c_void);
    }
}

pub fn get_vcp(handle: &DisplayHandle, vcp_code: u8) -> Result<(u16, u16, String)> {
    let mut valrec = DDCA_Non_Table_Vcp_Value {
        mh: 0,
        ml: 0,
        sh: 0,
        sl: 0,
    };
    let status = unsafe { ddca_get_non_table_vcp_value(handle.handle, vcp_code, &mut valrec) };
    if status != 0 {
        return Err(Error::Status(status));
    }

    // For simplicity, we just return raw 16-bit and formatted empty
    let current = (valrec.sh as u16) << 8 | valrec.sl as u16;
    let max = (valrec.mh as u16) << 8 | valrec.ml as u16;
    let mut formatted = ptr::null_mut();
    let status = unsafe {
        ddca_format_non_table_vcp_value_by_dref(
            vcp_code,
            handle.dref,
            &mut valrec as *mut _,
            &mut formatted,
        )
    };
    let formatted_str = if status == 0 && !formatted.is_null() {
        let str_val = unsafe { CStr::from_ptr(formatted) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            libc::free(formatted as *mut libc::c_void);
        }
        str_val
    } else {
        String::new()
    };
    Ok((current, max, formatted_str))
}

pub fn get_capabilities_string(handle: &DisplayHandle) -> Result<String> {
    debug!("get_capabilities_string - found display");
    let mut caps_ptr: *mut libc::c_char = std::ptr::null_mut();
    let raw_handle = handle.handle;
    let status = unsafe { ddca_get_capabilities_string(raw_handle, &mut caps_ptr) };
    debug!("get_capabilities_string - status: {}", status);
    if status != 0 {
        return Err(Error::Status(status));
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
}

pub fn set_vcp(handle: &DisplayHandle, vcp_code: u8, value: u16, verify: bool) -> Result<()> {
    if !verify {
        debug!("set_vcp: non-verified set.")
    }

    unsafe {
        let _ = ddca_enable_verify(verify);
    };

    let high = (value >> 8) as u8;
    let low = value as u8;
    let status = unsafe { ddca_set_non_table_vcp_value(handle.handle, vcp_code, high, low) };
    if status != 0 {
        return Err(Error::Status(status));
    }
    Ok(())
}

pub fn cstr_from_fixed_array<const N: usize>(arr: &[c_char; N]) -> String {
    // Find the first null byte (0)
    let len = arr.iter().position(|&c| c == 0).unwrap_or(N);
    // Convert the bytes up to that length (as u8)
    let bytes = &arr[..len] as &[c_char];
    // Safety: c_char is i8 or u8; we reinterpret as u8.
    let bytes_u8 = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u8, len) };
    String::from_utf8_lossy(bytes_u8)
        .replace('\x00', "?")
        .to_string()
}

// In your ddcutil module:
pub fn get_feature_name(code: u8) -> Result<String> {
    unsafe {
        let ptr = ddca_get_feature_name(code);
        if ptr.is_null() {
            Ok(format!("0x{:02x}", code))
        } else {
            Ok(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

pub fn parse_capabilities(handle: DisplayHandle) -> Result<CapabilitiesData> {
    // 1. Get the raw capabilities string
    let mut caps_text: *mut libc::c_char = std::ptr::null_mut();
    let status1 = unsafe { ddca_get_capabilities_string(handle.handle, &mut caps_text) };
    if status1 != 0 {
        return Err(Error::Status(status1));
    }

    // 2. Parse it
    let mut parsed_caps_ptr: *mut DDCA_Capabilities = std::ptr::null_mut();
    let status2 = unsafe { ddca_parse_capabilities_string(caps_text, &mut parsed_caps_ptr) };
    unsafe { libc::free(caps_text as *mut libc::c_void) }; // free immediately

    if status2 != 0 {
        return Err(Error::Status(status2));
    }

    // 3. Convert to safe Rust structs
    let caps = unsafe { &*parsed_caps_ptr };
    let mccs_major = caps.version_spec.major;
    let mccs_minor = caps.version_spec.minor;

    // Commands
    let mut commands = Vec::with_capacity(caps.cmd_ct as usize);
    for i in 0..caps.cmd_ct as usize {
        let code = unsafe { *caps.cmd_codes.add(i) };
        let desc = get_feature_name(code)?; // safe helper
        commands.push(CommandData {
            code,
            description: desc,
        });
    }

    // Features
    let mut features = Vec::with_capacity(caps.vcp_code_ct as usize);
    for i in 0..caps.vcp_code_ct as usize {
        let vcp = unsafe { &*caps.vcp_codes.add(i) };

        // Get metadata
        let mut meta_ptr: *mut DDCA_Feature_Metadata = std::ptr::null_mut();
        let status3 = unsafe {
            ddca_get_feature_metadata_by_dh(vcp.feature_code, handle.handle, true, &mut meta_ptr)
        };
        if status3 != 0 {
            // Log and continue with fallback values
            eprintln!(
                "Warning: failed to get metadata for feature 0x{:02x}",
                vcp.feature_code
            );
        }

        let (name, desc) = if meta_ptr.is_null() {
            (format!("VCP 0x{:02x}", vcp.feature_code), String::new())
        } else {
            let meta = unsafe { &*meta_ptr };
            let name = unsafe { CStr::from_ptr(meta.feature_name) }
                .to_string_lossy()
                .into_owned();
            let desc = if meta.feature_desc.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(meta.feature_desc) }
                    .to_string_lossy()
                    .into_owned()
            };
            unsafe { ddca_free_feature_metadata(meta_ptr) };
            (name, desc)
        };

        // Values
        let mut values = Vec::with_capacity(vcp.value_ct as usize);
        for j in 0..vcp.value_ct as usize {
            let value_code = unsafe { *vcp.values.add(j) };
            // Could look up the value name from metadata if available
            let value_name = format!("0x{:02x}", value_code);
            values.push(ValueData {
                code: value_code,
                name: value_name,
            });
        }

        features.push(FeatureData {
            code: vcp.feature_code,
            name,
            description: desc,
            values,
        });
    }

    // Free the C structure
    unsafe { ddca_free_parsed_capabilities(parsed_caps_ptr) };

    Ok(CapabilitiesData {
        mccs_major,
        mccs_minor,
        commands,
        features,
    })
}

static NEED_POLL: AtomicBool = AtomicBool::new(false);

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

fn is_dpms_awake(dref: usize) -> Result<bool> {
    let dmps_vp_code = 0xd6u8;
    let mut handle = open_display(dref as *mut c_void)?;
    let (current, max, formatted) = ddcutil::get_vcp(&mut handle, dmps_vp_code)?;
    Ok(current != 0)
}


/// State of a display relevant to the polling task.
#[derive(Debug, Clone, Copy)]
struct DisplayState {
    display_number: i32,
    display_ref: usize,
    awake: bool,
    // Add more fields later if needed (e.g., ddc_working)
}

/// Polling Task (runs in a background thread)
fn polling_task(
    config: Arc<Mutex<DdcutilConfig>>,
    poll_tx: Sender<DdcutilEvent>,
    shutdown_rx: Receiver<()>,
) {
    // Previous state: EDID base64 - DisplayState
    let mut previous_states: HashMap<String, DisplayState> = HashMap::new();

    loop {
        // Check for shutdown signal
        if shutdown_rx.try_recv().is_ok() {
            info!("Polling thread received shutdown signal, exiting.");
            break;
        }

        // Refresh configuration
        let (interval, cascade_interval, subscriptions_active) = {
            let config_locked = config.lock().unwrap();
            (
                config_locked.poll_interval_secs,
                config_locked.poll_cascade_secs,
                config_locked.events_enabled,
            )
        };

        let _ = NEED_POLL.swap(false, Ordering::SeqCst);

        // If no subscriptions, sleep idly
        if !subscriptions_active {
            if NEED_POLL.swap(false, Ordering::SeqCst) {
                debug!("NEED_POLL cleared while idle (no subscribers)");
            }
            sleep_interruptible(Duration::from_secs(5));
            continue;
        }

        // Redetect displays
        if let Err(e) = redetect() {
            error!("redetect displays failed: {}", e);
            sleep_interruptible(Duration::from_secs(interval as u64));
            continue;
        }

        let current_displays = match get_display_info_list(false) {
            Ok(list) => list,
            Err(e) => {
                error!("get_display_info_list failed: {}", e);
                sleep_interruptible(Duration::from_secs(interval as u64));
                continue;
            }
        };

        // Build current state map: EDID - DisplayState
        let mut current_states = HashMap::with_capacity(current_displays.len());
        for display in &current_displays {
            let edid = general_purpose::STANDARD.encode(&display.edid_bytes);
            let display_number = display.display_number;
            let display_ref = display.display_ref;
            let awake = match is_dpms_awake(display.display_ref) {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("Failed to get DPMS state for display {}: {} - assuming it is asleep.", display.display_number, e);
                    // If we had a previous state, keep it; otherwise assume asleep
                    //previous_states.get(&edid).map(|s| s.awake).unwrap_or(false)
                    // Lets assume failure means asleep.
                    false
                }
            };
            log::debug!("display {} awake={}", display.display_number, awake);
            current_states.insert(edid, DisplayState { display_number, display_ref, awake });
        }

        // Detect connection changes (new / lost displays)
        let current_edids: HashSet<&String> = current_states.keys().collect();
        let previous_edids: HashSet<&String> = previous_states.keys().collect();

        let newly_detected: Vec<_> = current_edids.difference(&previous_edids).collect();
        let lost: Vec<_> = previous_edids.difference(&current_edids).collect();

        let connection_change = !newly_detected.is_empty() || !lost.is_empty();
        if connection_change {
            let edid = newly_detected
                .iter()
                .next()
                .or_else(|| lost.iter().next())
                .map(|s| s.to_string())
                .unwrap_or_default();

            let event_type = if !newly_detected.is_empty() {
                DdcutilEventKind::Connected.as_str()
            }
            else {
                DdcutilEventKind::Disconnected.as_str()
            };

            let data = serde_json::json!({
                "edid_base64": edid,
                "event_type": event_type,
                "flags": 0,
            })
                .to_string();

            let event = DdcutilEvent {
                kind: DdcutilEventKind::ConnectedDisplaysChanged,
                data,
            };
            let _ = poll_tx.send(event);
        }

        // Detect DPMS state changes for displays that are still present
        for (edid, state) in &current_states {
            if let Some(prev_state) = previous_states.get(edid) {
                if prev_state.awake != state.awake {
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
                    let _ = poll_tx.send(event);
                }
            }
        }

        // 9. Remember current state for next iteration
        previous_states = current_states;

        // 10. Sleep (interruptible) until next poll
        let sleep_duration = if connection_change {
            Duration::from_millis((cascade_interval * 1000.0) as u64)
        } else {
            Duration::from_secs(interval as u64)
        };

        debug!("poll: sleeping for {:?} (interruptible)", sleep_duration);
        sleep_interruptible(sleep_duration);
    }
}

/// Event cCallback for passing to libddcutil
extern "C" fn native_ddc_event_callback(event: DDCA_Display_Status_Event) {
    debug!("my_display_callback event {}", event.event_type);
    // Map the C event type to our Rust enum
    let kind = match event.event_type {
        DDCA_Display_Event_Type_DDCA_EVENT_DISPLAY_CONNECTED => DdcutilEventKind::Connected,
        DDCA_Display_Event_Type_DDCA_EVENT_DISPLAY_DISCONNECTED => DdcutilEventKind::Disconnected,
        DDCA_Display_Event_Type_DDCA_EVENT_DPMS_AWAKE => DdcutilEventKind::DpmsAwake,
        DDCA_Display_Event_Type_DDCA_EVENT_DPMS_ASLEEP => DdcutilEventKind::DpmsAsleep,
        DDCA_Display_Event_Type_DDCA_EVENT_DDC_WORKING => DdcutilEventKind::DdcWorking,
        // DDCA_EVENT_UNUSED2 exists, but we can ignore or treat as Unknown
        _ => DdcutilEventKind::Unknown(event.event_type as i32),
    };

    match kind {
        DdcutilEventKind::Connected | DdcutilEventKind::Disconnected |
        DdcutilEventKind::DpmsAwake | DdcutilEventKind::DpmsAsleep => {
            NEED_POLL.store(true, Ordering::SeqCst);
        }
        _ => {}
    }

    // Read the connector name (it's a fixed-size C char array)
    let data = unsafe {
        // event.connector_name is [c_char; 32], we treat it as a C string
        CStr::from_ptr(event.connector_name.as_ptr())
            .to_string_lossy()
            .into_owned()
    };

    debug!("sending {} {}", kind.as_str(), data);
    // Send to the channel (if initialized)
    if let Some(sender) = CALLBACK_EVENT_SENDER.get() {
        // If the receiver is gone, just drop the event – no harm.
        let _ = sender.send(DdcutilEvent { kind, data });
    }
}

// ddcutil.rs

pub struct DdcutilConfig {
    pub poll_interval_secs: u32,
    pub poll_cascade_secs: f64,
    pub events_enabled: bool,
}

impl Default for DdcutilConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30, // Poll seconds, quite long – detect can be slow.
            poll_cascade_secs: 0.5, // Poll sooner after an event, in case it's a cluster.
            events_enabled: false,
        }
    }
}

struct PollState {
    poll_thread: Option<thread::JoinHandle<()>>,
    shutdown_tx: Option<Sender<()>>,  // To tell the polling thread to stop
}
pub struct Ddcutil {
    config: Arc<Mutex<DdcutilConfig>>,
    event_tx: Sender<DdcutilEvent>,      // Sender for events (shared with callback)
    poll_state_mutex: Mutex<PollState>,
}

impl Ddcutil {
    pub fn create() -> (Self, Receiver<DdcutilEvent>) {
        let (tx, rx) = unbounded::<DdcutilEvent>();
        let config = Arc::new(Mutex::new(DdcutilConfig::default()));

        init().expect("Initialization failed");
        redetect().expect("Initialization rededect failed");

        // Store the sender globally for the callback
        CALLBACK_EVENT_SENDER.set(tx.clone()).unwrap();

        // Register the callback (uses the same event_tx)
        let status =
            unsafe { ddca_register_display_status_callback(Some(native_ddc_event_callback)) };
        if status != 0 {
            eprintln!(
                "Warning: failed to register display status callback: {}",
                status
            );
            // Polling will still work, so continue
        }

        let ddc = Ddcutil {
            config,
            event_tx: tx,
            poll_state_mutex: Mutex::new(PollState { poll_thread: None, shutdown_tx: None }),
        };
        (ddc, rx)
    }

    /// Start the polling thread if it's not already running.
    pub fn start_polling(&mut self) {  // DPMS polling - which ddcutil can't do.
        let mut poll_state = self.poll_state_mutex.lock().unwrap();
        if poll_state.poll_thread.is_some() {
            debug!("Polling thread already running");
            return;
        }

        let (shutdown_tx, shutdown_rx) = bounded(0);
        let tx = self.event_tx.clone();
        let config = self.config.clone();

        let handle = std::thread::spawn(move || {
            polling_task(config, tx, shutdown_rx);
        });

        poll_state.poll_thread = Some(handle);
        poll_state.shutdown_tx = Some(shutdown_tx);
        debug!("Ddcutil::start_polling: Polling thread started");
    }

    /// Stop the polling thread (if running).
    pub fn stop_polling(&mut self) {
        let mut poll_state = self.poll_state_mutex.lock().unwrap();
        if poll_state.poll_thread.is_some() {
            if let Some(tx) = poll_state.shutdown_tx.take() {
                let _ = tx.send(()); // Signal the thread to exit
            }
            if let Some(handle) = poll_state.poll_thread.take() {
                // Wait for the thread to finish (optional – you can detach if you prefer)
                let _ = handle.join();
            }
            debug!("Ddcutil::stop_polling: Polling thread stopped");
        } else {
            debug!("Ddcutil::stop_polling: Polling thread not running.");
        }

    }

    pub fn get_poll_interval(&self) -> u32 {
        self.config.lock().unwrap().poll_interval_secs
    }

    pub fn set_events_enable(&self, enable: bool) -> Result<()> {
        let mut cfg = self.config.lock().unwrap();
        cfg.events_enabled = enable;
        if enable {
            unsafe { ddca_start_watch_displays(
                DDCA_Display_Event_Class_DDCA_EVENT_CLASS_DPMS |
                DDCA_Display_Event_Class_DDCA_EVENT_CLASS_DISPLAY_CONNECTION); }
        }
        else {
            unsafe { ddca_stop_watch_displays(false); }
        }
        Ok(())
    }

    pub fn set_poll_interval(&self, seconds: u32) -> Result<()> {
        // Optional: validate here (e.g., >= 10)
        // if seconds < 10 && seconds != 0 { return Err(...); }
        let mut cfg = self.config.lock().unwrap();
        cfg.poll_interval_secs = seconds;
        Ok(())
    }

    pub fn get_cascade_interval(&self) -> f64 {
        self.config.lock().unwrap().poll_cascade_secs
    }

    pub fn set_cascade_interval(&self, seconds: f64) -> Result<()> {
        let mut cfg = self.config.lock().unwrap();
        cfg.poll_cascade_secs = seconds;
        Ok(())
    }

    // Access to config (for getters/setters)
    pub fn config(&self) -> Arc<Mutex<DdcutilConfig>> {
        self.config.clone()
    }
}
