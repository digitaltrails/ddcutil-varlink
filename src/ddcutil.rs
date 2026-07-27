use std::collections::HashSet;
//SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
//SPDX-License-Identifier: GPL-2.0-or-later
// src/ddcutil.rs
use crate::com_ddcutil_service::{
    CallOptions, Call_GetDdcutilDynamicSleep, Call_GetDdcutilOutputLevel, Call_GetDdcutilVersion,
    Call_GetDisplayState, CapabilitiesFeature, CapabilitiesValueEntry, Event, Event_kind,
    KeyValueIntCapabilitiesFeature, KeyValueIntString,
};
use crate::{broadcast_event, ddcutil, get_subscribers, CallbackEvent, CallbackEventKind};
use base64::{engine::general_purpose, Engine as _};
use log::{debug, error};
use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

static CALLBACK_EVENT_SENDER: OnceLock<Sender<CallbackEvent>> = OnceLock::new();

// import the Varlink event type
use crossbeam_channel::{unbounded, Receiver, Sender};

// A global channel for events (internal to ddcutil)
static EVENT_TX: OnceLock<Sender<Event>> = OnceLock::new();

// Include the generated bindings
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

pub const DDCRC_OK: i32 = 0;

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

pub struct DisplayInfo {
    pub display_number: i32,
    pub manufacturer_id: String,
    pub model_name: String,
    pub serial_number: String,
    pub edid_bytes: [u8; 128],
    pub product_code: u16,
    pub usb_bus: i32,
    pub usb_device: i32,
}
// TODO derive clone?
impl Clone for DisplayInfo {
    fn clone(&self) -> Self {
        Self {
            display_number: self.display_number,
            manufacturer_id: self.manufacturer_id.clone(),
            model_name: self.model_name.clone(),
            product_code: self.product_code,
            usb_bus: self.usb_bus,
            usb_device: self.usb_device,
            serial_number: self.serial_number.clone(),
            edid_bytes: self.edid_bytes.clone(),
        }
    }
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
// ddcutil.rs
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
        Ok(DisplayList { ptr: list_ptr })
    }

    /// Find a display by display_number or EDID (with optional prefix match).
    /// Returns (dispno, edid_base64, dref) if found.
    pub fn find_by_number_or_edid(
        &self,
        display_number: i64,
        edid_base64: &str,
        allow_edid_prefix: bool,
    ) -> Option<(i32, String, *mut std::ffi::c_void)> {
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
            // Number precedence
            if display_number != -1 && display_number == raw.dispno as i64 {
                let edid = general_purpose::STANDARD.encode(&raw.edid_bytes);
                return Some((raw.dispno, edid, raw.dref));
            }
            // EDID matching
            if !edid_base64.is_empty() {
                let edid = general_purpose::STANDARD.encode(&raw.edid_bytes);
                let matches = if allow_edid_prefix {
                    edid.starts_with(edid_base64)
                } else {
                    edid == edid_base64
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
        unsafe {
            unsafe { CStr::from_ptr(detail.detail) }
                .to_string_lossy()
                .into_owned()
        }
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
        let status = ddca_init(
            std::ptr::null(), // no options string
            9,                // LOG_NOTICE
            0,
        );
        if status != 0 {
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
        let edid_bytes = raw.edid_bytes;
        infos.push(DisplayInfo {
            display_number: raw.dispno,
            manufacturer_id: cstr_from_fixed_array(&raw.mfg_id),
            model_name: cstr_from_fixed_array(&raw.model_name),
            product_code: raw.product_code,
            usb_bus: raw.usb_bus,
            usb_device: raw.usb_device,
            serial_number: cstr_from_fixed_array(&raw.sn), // raw.sn is *const c_char
            edid_bytes: raw.edid_bytes,
        });
    }

    unsafe {
        ddca_free_display_info_list(list_ptr);
    }
    Ok(infos)
}

pub fn find_display(
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    allow_edid_prefix: bool,
) -> Result<(DisplayList, *mut c_void)> {
    let list = DisplayList::new(allow_edid_prefix)?;

    if display_number.is_none() && edid_base64.is_none() {
        if display_number.is_none() && edid_base64.is_none() {
            return Err(Error::MissingIdentifier);
        }
    }
    let target_display_number: i64 = display_number.unwrap_or(-1);
    let target_edid_base64: &str = edid_base64.unwrap_or("");

    match list.find_by_number_or_edid(target_display_number, target_edid_base64, allow_edid_prefix)
    {
        Some((_, _, dref)) => Ok((list, dref)),
        None => {
            let edid_display = (!target_edid_base64.is_empty())
                .then_some(target_edid_base64)
                .unwrap_or("None");
            Err(Error::DisplayNotFound {
                display_number: target_display_number,
                edid_base64: target_edid_base64.to_string(),
                status: -1,
                message: format!(
                    "DisplayNumber={} EDID={} - display not found",
                    target_display_number, edid_display
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
    display_number: Option<i64>,
    edid_base64: Option<&str>,
    allow_edid_prefix: bool,
) -> Result<(DDCA_Status, String)> {
    let (_list, dref) = find_display(display_number, edid_base64, allow_edid_prefix)?;
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

/// Convert a raw `DDCA_Capabilities` from libddcutil into Varlink-friendly structures.
///
/// # Safety
/// The `caps_ptr` must be a valid pointer to a parsed capabilities struct.
unsafe fn parse_capabilities_to_varlink(
    caps_ptr: *mut DDCA_Capabilities,
    handle: &DisplayHandle,
) -> Result<(
    u8,
    u8,
    Vec<KeyValueIntString>,
    Vec<KeyValueIntCapabilitiesFeature>,
)> {
    if caps_ptr.is_null() {
        return Err(Error::MissingCapabilities);
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
        let desc = get_feature_name(cmd_code)?; // You may need to add this helper
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
        let mut meta_ptr: *mut DDCA_Feature_Metadata = std::ptr::null_mut();
        let status = ddca_get_feature_metadata_by_dh(
            vcp.feature_code,
            handle.handle, // raw handle
            true,          // create_default_if_not_found = true
            &mut meta_ptr,
        );

        if status != DDCRC_OK {
            // Log but continue – use fallback values
            log::warn!(
                "Failed to get metadata for feature 0x{:02x}: {}",
                vcp.feature_code,
                status
            );
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
                value_map.insert(
                    entry.value_code,
                    std::ffi::CStr::from_ptr(entry.value_name)
                        .to_string_lossy()
                        .into_owned(),
                );
                fve_ptr = fve_ptr.add(1);
            }
        }

        // Now iterate over the actual values
        for j in 0..vcp.value_ct as usize {
            let value_code = *vcp.values.add(j);
            let value_name = value_map
                .get(&value_code)
                .cloned()
                .unwrap_or_else(|| format!("Value 0x{:02x}", value_code));

            values.push(CapabilitiesValueEntry {
                value_code: value_code as i64,
                value_name,
            });
        }

        // Free metadata if we allocated it
        if !meta_ptr.is_null() {
            ddca_free_feature_metadata(meta_ptr);
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
        let s = unsafe { CStr::from_ptr(formatted) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            libc::free(formatted as *mut libc::c_void);
        }
        s
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

/// Convert a null‑terminated C string pointer to a Rust String.
pub fn cstr_from_ptr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_string_lossy().into_owned()
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

// ddcutil.rs

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

/// Polling Task (runs in a background thread)
fn polling_task(config: Arc<Mutex<DdcutilConfig>>, poll_tx: Sender<Event>) {
    let mut previous_edids = HashSet::new();
    loop {
        // Refresh configuration
        let (interval, cascade_interval) = {
            let config_locked = config.lock().unwrap();
            (
                config_locked.poll_interval_secs,
                config_locked.poll_cascade_secs,
            )
        }; // lock is dropped here

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
        debug!("polled");
        let current = match ddcutil::get_display_info_list(false) {
            Ok(list) => list,
            Err(_) => {
                // On error, sleep with interruptible check, then continue
                sleep_interruptible(Duration::from_secs(interval as u64));
                continue;
            }
        };
        debug!("comparing current len={}", current.len());
        let current_edids: HashSet<String> = current
            .iter()
            .map(|d| general_purpose::STANDARD.encode(&d.edid_bytes))
            .collect();

        let newly_detected_edids: Vec<_> = current_edids.difference(&previous_edids).collect();
        let lost_edids: Vec<_> = previous_edids.difference(&current_edids).collect();
        let event_occurred = !newly_detected_edids.is_empty() || !lost_edids.is_empty();
        debug!(
            "compared {} {} {}",
            newly_detected_edids.len(),
            lost_edids.len(),
            event_occurred
        );
        if event_occurred {
            let edid = newly_detected_edids
                .iter()
                .next()
                .or_else(|| lost_edids.iter().next())
                .map(|s| s.to_string())
                .unwrap_or_else(String::new);

            let event_type = if !newly_detected_edids.is_empty() {
                1
            } else {
                2
            };

            let data = serde_json::json!({
                "edid_base64": edid,
                "event_type": event_type,
                "flags": 0,
            })
            .to_string();

            let event = Event {
                kind: Event_kind::connected_displays_changed,
                data,
            };
            debug!("sending");
            let _ = poll_tx.send(event);
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

// ddcutil.rs

pub struct DdcutilConfig {
    pub poll_interval_secs: u32,
    pub poll_cascade_secs: f64,
}

impl Default for DdcutilConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30, // Poll seconds, quite long – detect can be slow.
            poll_cascade_secs: 0.5, // Poll sooner after an event, in case it's a cluster.
        }
    }
}

pub struct Ddcutil {
    config: Arc<Mutex<DdcutilConfig>>,
    event_tx: Sender<Event>, // Sender for events (polling + callback)
    _poll_thread: Option<thread::JoinHandle<()>>,
}

impl Ddcutil {
    /// Creates a new `Ddcutil` instance and starts the polling thread.
    /// Returns the instance and a receiver for events.
    pub fn create() -> (Self, Receiver<Event>) {
        let polling_config = Arc::new(Mutex::new(DdcutilConfig::default()));
        let (tx, rx) = unbounded::<Event>();

        // Start the polling thread
        let poll_config = polling_config.clone();
        let poll_tx = tx.clone();
        let poll_handle = thread::spawn(move || {
            polling_task(poll_config, poll_tx);
        });

        // Register the libddcutil callback (as before, but now inside ddcutil)
        debug!("registering callback");
        let status =
            unsafe { ddcutil::ddca_register_display_status_callback(Some(my_display_callback)) };
        if status != 0 {
            eprintln!(
                "Warning: failed to register display status callback: {}",
                status
            );
            // Polling will still work, so continue
        }

        let ddc = Ddcutil {
            config: polling_config,
            event_tx: tx,
            _poll_thread: Some(poll_handle),
        };

        (ddc, rx)
    }

    pub fn get_poll_interval(&self) -> u32 {
        self.config.lock().unwrap().poll_interval_secs
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
