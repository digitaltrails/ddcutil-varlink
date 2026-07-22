//SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
//SPDX-License-Identifier: GPL-2.0-or-later
// src/ddcutil.rs
use std::ffi::{CStr, CString};
use std::ptr;
use std::os::raw::{c_char, c_int};

// Include the generated bindings
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("DDC/CI error: {0}")]
    Status(c_int),
    #[error("UTF-8 conversion error")]
    Utf8,
}

pub type Result<T> = std::result::Result<T, Error>;

// RAII handle for display
pub struct DisplayHandle {
    handle: DDCA_Display_Handle,
    dref: *mut std::ffi::c_void, // we keep dref for metadata
}

impl Drop for DisplayHandle {
    fn drop(&mut self) {
        unsafe { ddca_close_display(self.handle); }
    }
}

pub struct DisplayInfo {
    pub dispno: i32,
    pub mfg_id: String,
    pub model_name: String,
    pub sn: String,
    pub edid_bytes: [u8; 128],
    pub dref: *mut std::ffi::c_void,
}

impl Clone for DisplayInfo {
    fn clone(&self) -> Self {
        Self {
            dispno: self.dispno,
            mfg_id: self.mfg_id.clone(),
            model_name: self.model_name.clone(),
            sn: self.sn.clone(),
            edid_bytes: self.edid_bytes,
            dref: self.dref, // just copy the pointer – safe as long as we don't free it
        }
    }
}


use base64;

pub struct DisplayList {
    ptr: *mut DDCA_Display_Info_List,
}

impl DisplayList {
    pub fn new(include_invalid: bool) -> Result<Self> {
        let mut list_ptr = ptr::null_mut();
        let status = unsafe {
            ddca_get_display_info_list2(include_invalid, &mut list_ptr)
        };
        if status != 0 {
            return Err(Error::Status(status));
        }
        if list_ptr.is_null() {
            return Err(Error::Status(-1));
        }
        Ok(DisplayList { ptr: list_ptr })
    }

    /// Find a display by display_number or EDID (with optional prefix).
    /// Returns (dispno, edid_base64, dref) if found.
    pub fn find_by_number_or_edid(
        &self,
        display_number: i64,
        edid_base64: &str,
        flags: i64,
    ) -> Option<(i32, String, *mut std::ffi::c_void)> {
        log::info!("find_by_number_or_edid: entered, list ptr = {:?}", self.ptr);
        if self.ptr.is_null() {
            log::error!("find_by_number_or_edid: null pointer");
            return None;
        }
        let list = unsafe { &*self.ptr };
        log::info!("find_by_number_or_edid: list.ct = {}", list.ct);

        for i in 0..list.ct {
            log::info!("find_by_number_or_edid: checking i={}", i);
            let raw = unsafe { &*list.info.as_ptr().add(i as usize) };
            // Number precedence
            if display_number != -1 && display_number == raw.dispno as i64 {
                let edid = base64::encode(&raw.edid_bytes);
                return Some((raw.dispno, edid, raw.dref));
            }
            // EDID matching
            if !edid_base64.is_empty() {
                let edid = base64::encode(&raw.edid_bytes);
                let matches = if (flags & 1) != 0 {
                    edid.starts_with(edid_base64)
                } else {
                    edid == edid_base64
                };
                if matches {
                    return Some((raw.dispno, edid, raw.dref));
                }
            }
        }
        log::info!("find_by_number_or_edid: returning None");
        None
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
            unsafe { ddca_free_display_info_list(self.ptr); }
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
    // 1. Get the base status name (e.g., "DDCRC_OK", "DDCRC_RETRIES")
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

    // 2. Try to obtain extra error detail
    let detail_ptr = unsafe { ddca_get_error_detail() };
    let message = if !detail_ptr.is_null() {
        let detail = unsafe { &*detail_ptr };
        if !detail.detail.is_null() {
            let detail_str = unsafe { CStr::from_ptr(detail.detail) }
                .to_string_lossy();
            format!("{}: {}", name, detail_str)
        } else {
            name
        }
    } else {
        name
    };

    // 3. Free the detail struct (if allocated)
    if !detail_ptr.is_null() {
        unsafe { ddca_free_error_detail(detail_ptr) };
    }

    message
}

pub fn init() -> Result<()> {
    unsafe {
        let status = ddca_init(
            std::ptr::null(), // no options string
            9, // LOG_NOTICE
            0,
        );
        if status != 0 { return Err(Error::Status(status)); }
    }
    Ok(())
}

pub fn redetect() -> Result<()> {
    unsafe {
        let status = ddca_redetect_displays();
        if status != 0 { return Err(Error::Status(status)); }
    }
    Ok(())
}

pub fn get_display_info_list(include_invalid: bool) -> Result<Vec<DisplayInfo>> {
    let mut list_ptr = ptr::null_mut();
    let status = unsafe {
        ddca_get_display_info_list2(
            if include_invalid { true } else { false },
            &mut list_ptr
        )
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
            dispno: raw.dispno,
            mfg_id: cstr_from_fixed_array(&raw.mfg_id),
            model_name: cstr_from_fixed_array(&raw.model_name),
            sn: cstr_from_fixed_array(&raw.sn),                    // raw.sn is *const c_char
            edid_bytes: raw.edid_bytes,
            dref: raw.dref,
        });
    }

    unsafe { ddca_free_display_info_list(list_ptr); }
    Ok(infos)
}
pub fn open_display(dref: *mut std::ffi::c_void) -> Result<DisplayHandle> {
    let mut handle: DDCA_Display_Handle = ptr::null_mut();
    let status = unsafe { ddca_open_display2(dref, true, &mut handle) };
    if status != 0 { return Err(Error::Status(status)); }
    Ok(DisplayHandle { handle, dref })
}

pub fn get_vcp(handle: &DisplayHandle, vcp_code: u8) -> Result<(u16, u16, String)> {
    let mut valrec = DDCA_Non_Table_Vcp_Value{mh: 0, ml: 0, sh: 0, sl: 0};
    let status = unsafe { ddca_get_non_table_vcp_value(handle.handle, vcp_code, &mut valrec) };
    if status != 0 { return Err(Error::Status(status)); }

    // For simplicity, we just return raw 16-bit and formatted empty
    let current = (valrec.sh as u16) << 8 | valrec.sl as u16;
    let max = (valrec.mh as u16) << 8 | valrec.ml as u16;
    let mut formatted = ptr::null_mut();
    let status = unsafe {
        ddca_format_non_table_vcp_value_by_dref(vcp_code, handle.dref, &mut valrec as *mut _, &mut formatted)
    };
    let formatted_str = if status == 0 && !formatted.is_null() {
        let s = unsafe { CStr::from_ptr(formatted) }.to_string_lossy().into_owned();
        unsafe { libc::free(formatted as *mut libc::c_void); }
        s
    } else {
        String::new()
    };
    Ok((current, max, formatted_str))
}

pub fn set_vcp(handle: &DisplayHandle, vcp_code: u8, value: u16) -> Result<()> {
    let high = (value >> 8) as u8;
    let low = value as u8;
    let status = unsafe { ddca_set_non_table_vcp_value(handle.handle, vcp_code, high, low) };
    if status != 0 { return Err(Error::Status(status)); }
    Ok(())
}

pub fn cstr_from_fixed_array<const N: usize>(arr: &[c_char; N]) -> String {
    // Find the first null byte (0)
    let len = arr.iter().position(|&c| c == 0).unwrap_or(N);
    // Convert the bytes up to that length (as u8)
    let bytes = &arr[..len] as &[c_char];
    // Safety: c_char is i8 or u8; we reinterpret as u8.
    let bytes_u8 = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u8, len) };
    String::from_utf8_lossy(bytes_u8).replace('\x00', "?").to_string()
}

/// Convert a null‑terminated C string pointer to a Rust String.
pub fn cstr_from_ptr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_string_lossy().into_owned()
}