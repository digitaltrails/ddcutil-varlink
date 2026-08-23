//SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
//SPDX-License-Identifier: GPL-2.0-or-later
// src/ddcutil.rs

use base64::{engine::general_purpose, Engine as _};
use log::{debug, error, info, warn};
use std::ffi::{CStr};
use std::os::raw::{c_char, c_int};
use std::ptr;
use scopeguard::guard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock};
use std::time::Duration;
use crossbeam_channel::{Sender};
use serde_derive::Serialize;
use regex::Regex;
use crate::com_ddcutil_service::DetectEntry;
use crate::ddcutil;

// Include the generated bindings
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// ============================================================================
// Macros
// ============================================================================
macro_rules! ddca_call {
    ($call:expr) => {{
        let status = unsafe { $call };
        if status == 0 {
            Ok(())
        } else {
            debug!("ddca_call failed: {} -> {} ({})", stringify!($call), status,  ddcutil::get_status_message(status));
            Err(Error::Status(status))
        }
    }};
}

static CALLBACK_EVENT_SENDER: OnceLock<Sender<DdcutilEvent>> = OnceLock::new();

pub type DisplayRef = usize;

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
    #[error("Failed to register callback")]
    FailedRegisterCallback {
        message: String,
    },
    #[error("Already set callback sender")]
    AlreadySetCallbackSender,
}

impl Error {
    pub fn status_code(&self) -> i64 {
        match self {
            Error::Status(code) => *code as i64,
            _ => -1,
        }
    }
}


/// Converts a nullable C string pointer to a Rust `String`.
/// If the pointer is null, returns the provided default (which can be a `&str` or `String`).
/// # Safety
/// The caller must ensure that the pointer is either null or points to a valid,
/// null‑terminated C string.
fn c_ptr_to_string(ptr: *const c_char, default: impl Into<String>) -> String {
    if ptr.is_null() {
        default.into()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// RAII handle for display
pub struct DisplayHandle {
    pub ddca_handle: DDCA_Display_Handle,
    dref: usize, // we keep dref for metadata
}

impl Drop for DisplayHandle {
    fn drop(&mut self) {
        match ddca_call!(ddca_close_display(self.ddca_handle)) {
            Ok(_) => {}
            Err(e) => warn!("Failed to close display: {}", e),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DisplayInfo {
    pub display_ref: DisplayRef,
    pub display_number: i32,
    pub manufacturer_id: String,
    pub model_name: String,
    pub serial_number: String,
    pub edid_bytes: [u8; 128],
    pub product_code: u16,
    pub usb_bus: i32,
    pub usb_device: i32,
    pub edid_serial_number: String,
}

#[derive(Debug, Clone)]
pub struct CapabilitiesData {
    pub model_name: String,
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

fn edid_serial_number(edid: &[u8; 128]) -> u32 {
    u32::from_le_bytes([edid[0x0c], edid[0x0d], edid[0x0e], edid[0x0f]])
}

impl From<&DDCA_Display_Info> for DisplayInfo {
    fn from(raw: &DDCA_Display_Info) -> Self {
        Self {
            display_ref: raw.dref as DisplayRef,
            display_number: raw.dispno,
            manufacturer_id: cstr_from_fixed_array(&raw.mfg_id),
            model_name: cstr_from_fixed_array(&raw.model_name),
            product_code: raw.product_code,
            usb_bus: raw.usb_bus,
            usb_device: raw.usb_device,
            serial_number: cstr_from_fixed_array(&raw.sn),
            edid_bytes: raw.edid_bytes,
            edid_serial_number: edid_serial_number(&raw.edid_bytes).to_string(),
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
            serial_number: info.serial_number.clone(),
            product_code: info.product_code as i64,
            edid_base64: general_purpose::STANDARD.encode(&info.edid_bytes),
            edid_serial_number: info.edid_serial_number.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Serialize)]
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

        ddca_call!(ddca_get_display_info_list2(include_invalid, &mut list_ptr))?;

        if list_ptr.is_null() {
            return Err(Error::Status(-1));
        }
        let list = unsafe { &*list_ptr };

        debug!("Created DisplayList: len={} ptr={:p}", list.ct, list_ptr);
        for i in 0..list.ct {
            let raw = unsafe { &*list.info.as_ptr().add(i as usize) };
            debug!("   DisplayList display dref={} dispno={} edid={:.20}... ptr={:p}",
                raw.dref as DisplayRef, raw.dispno as i64, general_purpose::STANDARD.encode(raw.edid_bytes), list_ptr);
        }

        Ok(DisplayList { ptr: list_ptr })
    }

    /// Find a display by display_number or EDID (with optional prefix match).
    /// Although historically a pointer and declared as such, dref is now an int u64.
    /// Returns dref if found
    pub fn find_by_id(
        &self,
        display_number: Option<i64>,
        edid_base64: Option<&str>,
        allow_edid_prefix: bool,
    ) -> Option<DisplayRef> {
        
        let target_display_number: i64 = display_number.unwrap_or(-1);
        let target_edid_base64: &str = edid_base64.unwrap_or("");

        debug!("find_by_id: display_number={} edid_base64={:.20}... allow_edid_prefix={}... ptr = {:?}",
            target_display_number, target_edid_base64, allow_edid_prefix, self.ptr);
        if self.ptr.is_null() {
            log::error!("find_by_number_or_edid: null pointer");
            return None;
        }
        // C array
        let display_info_list = unsafe { &*self.ptr };
        //debug!("find_by_number_or_edid: list.ct = {}", display_info_list.ct);

        // Walk C array
        for i in 0..display_info_list.ct {
            //debug!("find_by_number_or_edid: checking i={}", i);
            let ddca_display_info = unsafe { &*display_info_list.info.as_ptr().add(i as usize) };
            // display_number precedence
            if !display_number.is_none() && target_display_number == ddca_display_info.dispno as i64 {
                return Some(ddca_display_info.dref as DisplayRef);
            }
            // EDID matching
            if !edid_base64.is_none() {
                let edid = general_purpose::STANDARD.encode(&ddca_display_info.edid_bytes);
                let matches = if allow_edid_prefix {
                    edid.starts_with(target_edid_base64)
                } else {
                    edid == target_edid_base64
                };
                if matches {
                    return Some(ddca_display_info.dref as DisplayRef);
                }
            }
        }
        info!("find_by_id: NOT FOUND:  display_number={} edid_base64={:.20}... allow_edid_prefix={}... list ptr = {:?}",
            target_display_number, target_edid_base64, allow_edid_prefix, self.ptr);
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
            debug!("Freeing DisplayList: ptr={:p}", self.ptr);
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
    let name = c_ptr_to_string(name_ptr, format!("Unknown error code {}", status));

    // If status is OK, return just the name
    if status == 0 {
        return name;
    }

    let desc_ptr = unsafe { ddca_rc_desc(status) };
    let desc = c_ptr_to_string(desc_ptr, "");

    let detail_ptr = unsafe { ddca_get_error_detail() };

    let _guard_details_ptr = guard(detail_ptr,|ptr|{
            if !ptr.is_null() {
                unsafe { ddca_free_error_detail(ptr) };
            }
        });

    let detail_str = if detail_ptr.is_null() {
        "no details".to_owned()
    } else {
        let error_detail = unsafe { &*detail_ptr };
        c_ptr_to_string(error_detail.detail, "")
    };

    let message = format!("{}: {}: {}", name, desc, detail_str);

    //debug!("Message {}", message);
    message
}

pub fn init() -> Result<()> {
    info!("Initializing ddcutil");
    ddca_call!(ddca_init(
            std::ptr::null(), // no options string
            9,                // LOG_NOTICE
            0))
}

pub fn redetect() -> Result<()> {
    debug!("Redetect displays");
    ddca_call!(ddca_redetect_displays())
}

pub fn get_display_info_list(include_invalid: bool) -> Result<Vec<DisplayInfo>> {
    let mut list_ptr = ptr::null_mut();

    ddca_call!(ddca_get_display_info_list2(if include_invalid { true } else { false }, &mut list_ptr))?;

    let _guard_list_ptr = guard(list_ptr, |ptr| {  // will not be null
        unsafe { ddca_free_display_info_list(ptr); } });

    let list = unsafe { &*list_ptr };
    let mut infos = Vec::with_capacity(list.ct as usize);

    for i in 0..list.ct {
        // Access the i-th element using pointer arithmetic
        let raw = unsafe { &*list.info.as_ptr().add(i as usize) };
        infos.push(raw.into());
    }

    Ok(infos)
}

/// Find a display by number or EDID, returning the raw dref and the DisplayList
/// that keeps it alive. The caller must hold onto the DisplayList for the
/// lifetime of the dref.
pub fn find_display(
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    allow_edid_prefix: bool,
) -> Result<DisplayRef> {
    let display_list = DisplayList::new(allow_edid_prefix)?;

    if display_number.is_none() && edid_base64.is_none() {
        return Err(Error::MissingIdentifier);
    }

    match display_list.find_by_id(display_number, edid_base64, allow_edid_prefix)
    {
        Some(dref) => Ok(dref),
        None => {
            let edid_display = edid_base64.unwrap_or("");
            Err(Error::DisplayNotFound {
                display_number: display_number.unwrap_or(-1),
                edid_base64: edid_display.to_owned(),
                status: -1,  // TODO what should this be
                message: format!(
                    "DisplayNumber={:?} EDID={:?} - display not found",
                    display_number, edid_display
                ),
            })
        }
    }
}

pub fn open_display(dref: DisplayRef) -> Result<DisplayHandle> {
    let mut handle: DDCA_Display_Handle = ptr::null_mut();

    ddca_call!(ddca_open_display2(dref as DDCA_Display_Ref, true, &mut handle))?;

    Ok(DisplayHandle { ddca_handle: handle, dref })
}

pub fn get_display_state(
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    allow_edid_prefix: bool,
) -> Result<(DDCA_Status, String)> {
    let dref= find_display(display_number, edid_base64, allow_edid_prefix)?;
    let status = unsafe { ddca_validate_display_ref(dref as DDCA_Display_Ref, true) };
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
    let version_ptr = unsafe { ddca_ddcutil_extended_version_string() };
    c_ptr_to_string(version_ptr, "unknown")
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

    ddca_call!(ddca_get_non_table_vcp_value(handle.ddca_handle, vcp_code, &mut valrec))?;

    // For simplicity, we just return raw 16-bit and formatted empty
    let current = (valrec.sh as u16) << 8 | valrec.sl as u16;
    let max = (valrec.mh as u16) << 8 | valrec.ml as u16;
    let mut formatted = ptr::null_mut();

    // Format the value
    let formatter_status = unsafe {
        ddca_format_non_table_vcp_value_by_dref(
            vcp_code,
            handle.dref as DDCA_Display_Ref,
            &mut valrec as *mut _,
            &mut formatted,
        )
    };

    let formatted_str = if formatter_status == 0 {
        let formatted_value_str = c_ptr_to_string(formatted, "");
        unsafe { libc::free(formatted as *mut libc::c_void); }
        formatted_value_str
    } else {
        String::new()
    };

    Ok((current, max, formatted_str))
}

pub fn get_capabilities_string(handle: &DisplayHandle) -> Result<String> {
    debug!("get_capabilities_string - found display");
    let mut caps_ptr: *mut libc::c_char = std::ptr::null_mut();
    let raw_handle = handle.ddca_handle;

    ddca_call!(ddca_get_capabilities_string(raw_handle, &mut caps_ptr))?;

    let caps_str = c_ptr_to_string(caps_ptr, "");
    unsafe { free_c_string(caps_ptr); }
    Ok(caps_str)
}


#[derive(Debug)]
pub struct VcpFeatureMetadata {
    pub feature_name: String,
    pub description: String,
    pub is_read_only: bool,
    pub is_write_only: bool,
    pub is_rw: bool,
    pub is_complex: bool,
    pub is_continuous: bool,
}

pub fn get_vcp_metadata(handle: &DisplayHandle, feature_code:i64) -> Result<VcpFeatureMetadata> {
    debug!("get_capabilities_string - found display");
    let mut md_ptr: *mut DDCA_Feature_Metadata = std::ptr::null_mut();
    let raw_handle = handle.ddca_handle;

    ddca_call!(ddca_get_feature_metadata_by_dh(feature_code as DDCA_Vcp_Feature_Code, raw_handle, true, &mut md_ptr))?;

    let _guard_md_ptr = guard(md_ptr, |ptr| {
        unsafe { ddca_free_feature_metadata(ptr);} });

    let result = unsafe {
        let feature_flags = (*md_ptr).feature_flags as u32;
        debug!("get_capabilities_string - feature_flags: {}", feature_flags);

        let name = c_ptr_to_string((*md_ptr).feature_name, "unknown");
        let desc = c_ptr_to_string((*md_ptr).feature_desc, "");

        VcpFeatureMetadata {
            feature_name: name,
            description: desc,
            is_read_only: (feature_flags & DDCA_RO) != 0,
            is_write_only: (feature_flags & DDCA_WO) != 0,
            is_rw: (feature_flags & DDCA_RW) != 0,
            is_complex: (feature_flags & (DDCA_COMPLEX_CONT | DDCA_COMPLEX_NC)) != 0,
            is_continuous: (feature_flags & DDCA_CONT) != 0,
        }
    };

    debug!("get_capabilities_string - result: {:?}", result);
    Ok(result)
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

    ddca_call!(ddca_set_non_table_vcp_value(handle.ddca_handle, vcp_code, high, low))
}

pub fn get_sleep_multiplier(dref: DisplayRef) -> Result<f64> {
    let mut multiplier = 0.0;

    ddca_call!(ddca_get_current_display_sleep_multiplier(dref as DDCA_Display_Ref, &mut multiplier))?;

    Ok(multiplier)
}

pub fn set_sleep_multiplier(dref: DisplayRef, multiplier: f64) -> Result<()> {
    ddca_call!(ddca_set_display_sleep_multiplier(dref as DDCA_Display_Ref, multiplier))
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

pub fn get_feature_name(code: u8) -> Result<String> {
    unsafe {
        let ptr = ddca_get_feature_name(code);
        Ok(c_ptr_to_string(ptr, format!("0x{:02x}", code)))
    }
}

fn extract_model(ptr: *const c_char) -> Option<String> {
    // SAFETY: caller ensures pointer is valid and null‑terminated.
    let c_str = unsafe { CStr::from_ptr(ptr) };
    let input = c_str.to_str().ok()?;

    let re = Regex::new(r"model\(([^)]*)\)").ok()?;
    re.captures(input)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
}

pub fn get_capabilities_data(handle: DisplayHandle) -> Result<CapabilitiesData> {
    // Get the raw capabilities string
    let mut caps_text_ptr: *mut libc::c_char = std::ptr::null_mut();

    ddca_call!(ddca_get_capabilities_string(handle.ddca_handle, &mut caps_text_ptr))?;

    let _guard = guard(caps_text_ptr, |ptr| {  // Will free on going out of scope
        if !ptr.is_null() {
            unsafe { libc::free(ptr as *mut libc::c_void) };
        }
    });

    // debug!("ddca_get_capabilities_string - status: {} {:?}", status1, c_ptr_to_cow_str(caps_text, ""));
    let model_name = extract_model(caps_text_ptr).unwrap_or_else(|| "unknown model".to_owned());

    let mut parsed_caps_ptr: *mut DDCA_Capabilities = std::ptr::null_mut();

    ddca_call!(ddca_parse_capabilities_string(caps_text_ptr, &mut parsed_caps_ptr))?;

    let _parsed_caps_guard = guard(parsed_caps_ptr, |ptr| {
        unsafe { ddca_free_parsed_capabilities(ptr) } });  // Will not be null

    // Convert to safe Rust structs
    let caps = unsafe { &*parsed_caps_ptr };
    let mccs_major = caps.version_spec.major;
    let mccs_minor = caps.version_spec.minor;

    let mut commands = Vec::with_capacity(caps.cmd_ct as usize);
    for i in 0..caps.cmd_ct as usize {
        let code = unsafe { *caps.cmd_codes.add(i) };
        let desc = get_feature_name(code)?; // safe helper
        commands.push(CommandData {
            code,
            description: desc,
        });
    }

    // Loop over feature defs
    let mut supported_features_vec = Vec::with_capacity(caps.vcp_code_ct as usize);
    for i in 0..caps.vcp_code_ct as usize {
        let supported_feature = unsafe { &*caps.vcp_codes.add(i) };

        let mut allowed_values = Vec::with_capacity(supported_feature.value_ct as usize);

        // Get metadata - which may generically define a superset of values,
        // of which allowed_values is a subset.
        let mut metadata_ptr: *mut DDCA_Feature_Metadata = std::ptr::null_mut();

        let feature_status = unsafe {
            ddca_get_feature_metadata_by_dh(supported_feature.feature_code, handle.ddca_handle, true, &mut metadata_ptr)
        };

        // Frees on each iteration of loop - always create a guard, errors are unlikely, overhead is small
        let _metadata_guard = guard(metadata_ptr, |ptr| {
                if !ptr.is_null() {
                    unsafe { ddca_free_feature_metadata(ptr) };
                }
            });

        if feature_status != 0 {
            // Log and continue with fallback values
            error!("Warning: failed to get metadata for feature 0x{:02x}", supported_feature.feature_code);
        }

        let (name, desc) = if metadata_ptr.is_null() {
            // Make something up
            (format!("VCP 0x{:02x}", supported_feature.feature_code), String::new())
        } else {
            let metadata = unsafe { &*metadata_ptr };
            let name = c_ptr_to_string(metadata.feature_name, "");
            let desc = c_ptr_to_string(metadata.feature_desc, "");

            // Loop over this feature def's values to get each values name and description (if any)
            for i in 0..supported_feature.value_ct as usize {
                let feature_value_ptr = unsafe { &*supported_feature.values.add(i) };
                let value_code = *feature_value_ptr;

                let mut metadata_value_def_ptr = metadata.sl_values;
                while !metadata_value_def_ptr.is_null() {
                    let metadata_value_def = unsafe { &*metadata_value_def_ptr };

                    if metadata_value_def.value_name.is_null() {
                        break;
                    }
                    if metadata_value_def.value_code == value_code {
                        //  Found the definition for value_code.
                        allowed_values.push(ValueData {
                            code: metadata_value_def.value_code,
                            name: c_ptr_to_string(metadata_value_def.value_name, ""),
                        });

                        // Found, so we're finished for this value_code.
                        break;
                    }
                    // Still not found, move on to the next metadata value def
                    metadata_value_def_ptr = unsafe { metadata_value_def_ptr.add(1) };
                }
            }
            (name, desc)
        };

        supported_features_vec.push(FeatureData {
            code: supported_feature.feature_code,
            name,
            description: desc,
            values: allowed_values,
        });
    }

    Ok(CapabilitiesData {
        model_name,
        mccs_major,
        mccs_minor,
        commands,
        features: supported_features_vec,
    })
}

static NEED_POLL: AtomicBool = AtomicBool::new(false);

const POLL_WAKE_STEP_MS: u64 = 250; // Check NEED_POLL every 250ms

/// Sleep for the given duration, but wake up early if NEED_POLL is set.
/// Returns `true` if the sleep was interrupted by NEED_POLL.
pub fn sleep_interruptible(duration: Duration) -> bool {
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

pub fn is_dpms_awake(dref: DisplayRef) -> Result<bool> {
    let dmps_vp_code = 0xd6u8;
    let mut handle = open_display(dref)?;
    let (current, _, _) = ddcutil::get_vcp(&mut handle, dmps_vp_code)?;
    Ok(current != 0)
}

pub fn stop_watch_displays() -> Result<()> {
    ddca_call!(ddca_stop_watch_displays(false))?;
    Ok(())
}

pub fn start_watch_displays() -> Result<()> {
    // Use the ddca_call! macro from ddcutil
    ddca_call!(ddca_start_watch_displays(
                DDCA_Display_Event_Class_DDCA_EVENT_CLASS_DPMS |
                DDCA_Display_Event_Class_DDCA_EVENT_CLASS_DISPLAY_CONNECTION
            ))?;
    Ok(())
}

pub fn set_callback_sender(sender_channel: Sender<DdcutilEvent>) -> Result<()> {
    match CALLBACK_EVENT_SENDER.set(sender_channel).map_err(|_| ()) {
        Ok(_) => Ok(()),
        Err(_) => Err(Error::AlreadySetCallbackSender),
    }
}

/// Register the native callback with libddcutil.
/// This must be called once before any events can be received.
pub fn register_callback(callback: Option<unsafe extern "C" fn(DDCA_Display_Status_Event)>) -> Result<()> {
    let status = unsafe { ddca_register_display_status_callback(callback) };
    if status == 0 {
        Ok(())
    } else {
        Err(Error::FailedRegisterCallback{message: format!("Failed to register callback: {}", get_status_message(status)) })
    }
}


/// Event c Callback for passing to libddcutil
pub extern "C" fn native_ddc_event_callback(event: DDCA_Display_Status_Event) {
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

    let data = serde_json::json!({
                "event_type": event.event_type,
                "flags": 0, }).to_string();

    // Send to the channel (if initialized)
    if let Some(sender) = CALLBACK_EVENT_SENDER.get() {
        // If the receiver is gone, just drop the event – no harm.
        let event = DdcutilEvent { kind, data };
        debug!("native-event: SENDING {:?}", event);
        let _ = sender.send(event);
    }
}

