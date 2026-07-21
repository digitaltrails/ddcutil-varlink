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
    // other fields...
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

fn cstr_from_fixed_array<const N: usize>(arr: &[c_char; N]) -> String {
    // Find the first null byte (0)
    let len = arr.iter().position(|&c| c == 0).unwrap_or(N);
    // Convert the bytes up to that length (as u8)
    let bytes = &arr[..len] as &[c_char];
    // Safety: c_char is i8 or u8; we reinterpret as u8.
    let bytes_u8 = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u8, len) };
    String::from_utf8_lossy(bytes_u8).replace('\x00', "?").to_string()
}

/// Convert a null‑terminated C string pointer to a Rust String.
fn cstr_from_ptr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_string_lossy().into_owned()
}