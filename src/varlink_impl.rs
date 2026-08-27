// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/varlink_impl.rs

use crate::com_ddcutil_service::*;
pub(crate) use crate::service::DdcutilService;
use crate::{com_ddcutil_service, ddcutil};
use crossbeam_channel::unbounded;
use log::{error, info};
use std::sync::atomic::Ordering;
use varlink::StringHashMap;

// ============================================================================
// Varlink Interface Implementation
// ============================================================================

const DDCUTIL_VARLINK_VERSION: &str = "1.0.0";

/// Logs the Varlink call method name and parameters for debugging.
macro_rules! debug_varlink_call {
    ($call:expr) => {{
        let req = $call.get_request().expect("Varlink call missing request");
        log::debug!("VARLINK CALL: {:?}: {:?}", req.method, req.parameters);
    }};
}

impl VarlinkInterface for DdcutilService {
    fn detect(&self, call: &mut dyn Call_Detect, include_offline: bool) -> varlink::Result<()> {
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
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let edid_ref = edid_base64.as_deref();
            let dref =
                ddcutil::find_display(display_number, edid_ref, is_edid_prefix_allowed(&options))?;
            let handle = ddcutil::open_display(dref)?;
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
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let edid_ref = edid_base64.as_deref();
            let dref =
                ddcutil::find_display(display_number, edid_ref, is_edid_prefix_allowed(&options))?;
            let handle = ddcutil::open_display(dref)?;
            ddcutil::get_capabilities_string(&handle)
        };

        match ddc_operation() {
            Ok(caps_str) => call.reply(caps_str),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn get_ddcutil_dynamic_sleep(
        &self,
        call: &mut dyn Call_GetDdcutilDynamicSleep,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();
        call.reply(ddcutil::is_dynamic_sleep_enabled())
    }

    fn get_ddcutil_output_level(
        &self,
        call: &mut dyn Call_GetDdcutilOutputLevel,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();
        call.reply(ddcutil::get_output_level() as i64)
    }

    fn get_ddcutil_version(&self, call: &mut dyn Call_GetDdcutilVersion) -> varlink::Result<()> {
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
    ) -> varlink::Result<()> {
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
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let dref = match ddcutil::find_display(
            display_number,
            edid_base64.as_deref(),
            is_edid_prefix_allowed(&options),
        ) {
            Ok(d) => d,
            Err(e) => return send_ddc_error(call, None, display_number, edid_base64, None, &e),
        };
        let mut handle = match ddcutil::open_display(dref) {
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

    fn get_service_interface_version(
        &self,
        call: &mut dyn Call_GetServiceInterfaceVersion,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        call.reply(DDCUTIL_VARLINK_VERSION.to_owned())
    }

    fn get_service_poll_cascade_interval(
        &self,
        call: &mut dyn Call_GetServicePollCascadeInterval,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        // No lock needed for a simple read – but we acquire it anyway for consistency
        let guard = self.state.lock().unwrap();
        call.reply(guard.poll_cascade_secs)
    }

    fn get_service_poll_interval(
        &self,
        call: &mut dyn Call_GetServicePollInterval,
    ) -> varlink::Result<()> {
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
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(
                display_number,
                edid_base64.as_deref(),
                is_edid_prefix_allowed(&options),
            )?;
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
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(
                display_number,
                edid_base64.as_deref(),
                is_edid_prefix_allowed(&options),
            )?;
            let mut handle = ddcutil::open_display(dref)?;
            let (current, max, formatted) = ddcutil::get_vcp(&mut handle, vcp_code as u8)?;
            Ok((current as u32, max as u32, formatted))
        };

        match ddc_operation() {
            Ok((current, max, formatted)) => call.reply(current as i64, max as i64, formatted),
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
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();

        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(
                display_number,
                edid_base64.as_deref(),
                is_edid_prefix_allowed(&options),
            )?;
            let handle = ddcutil::open_display(dref)?;
            ddcutil::get_vcp_metadata(&handle, vcp_code)
        };

        match ddc_operation() {
            Ok(metadata) => call.reply(
                metadata.feature_name,
                metadata.description,
                metadata.is_read_only,
                metadata.is_write_only,
                metadata.is_rw,
                metadata.is_complex,
                metadata.is_continuous,
            ),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, None, &e),
        }
    }

    fn list_detected(
        &self,
        call: &mut dyn Call_ListDetected,
        include_offline: bool,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        let _guard = self.state.lock().unwrap();
        let displays = ddcutil::list_displays(include_offline)?;
        let detect_entries: Vec<DetectEntry> = displays.iter().map(Into::into).collect();
        call.reply(detect_entries.len() as i64, detect_entries)
    }

    fn set_ddcutil_dynamic_sleep(
        &self,
        call: &mut dyn Call_SetDdcutilDynamicSleep,
        enabled: bool,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(
                varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into(),
            );
        }
        let _guard = self.state.lock().unwrap();
        _ = ddcutil::enable_dynamic_sleep(enabled);
        call.reply()
    }

    fn set_ddcutil_output_level(
        &self,
        call: &mut dyn Call_SetDdcutilOutputLevel,
        level: i64,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(
                varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into(),
            );
        }
        let _guard = self.state.lock().unwrap();
        ddcutil::set_output_level(level as u32);

        call.reply()
    }

    fn set_service_poll_cascade_interval(
        &self,
        call: &mut dyn Call_SetServicePollCascadeInterval,
        seconds: f64,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(
                varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into(),
            );
        }
        if seconds < 0.0 || (seconds > 0.0 && seconds < 1.0) {
            return Err(
                varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_owned()).into(),
            );
        }
        let mut state = self.state.lock().unwrap();
        state.poll_cascade_secs = seconds;
        call.reply()
    }

    fn set_service_poll_interval(
        &self,
        call: &mut dyn Call_SetServicePollInterval,
        seconds: i64,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(
                varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into(),
            );
        }
        if seconds < 0 || (seconds > 0 && seconds < 10) {
            return Err(
                varlink::ErrorKind::InvalidParameter("InvalidPollInterval".to_owned()).into(),
            );
        }
        let mut state = self.state.lock().unwrap();
        state.poll_interval_secs = seconds as u32;
        call.reply()
    }

    fn set_sleep_multiplier(
        &self,
        call: &mut dyn Call_SetSleepMultiplier,
        display_number: Option<i64>,
        edid_base64: Option<String>,
        new_multiplier: f64,
        options: Option<CallOptions>,
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(
                varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into(),
            );
        }

        let _guard = self.state.lock().unwrap();
        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(
                display_number,
                edid_base64.as_deref(),
                is_edid_prefix_allowed(&options),
            )?;
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
    ) -> varlink::Result<()> {
        debug_varlink_call!(call);
        if self.configuration_locked.load(Ordering::SeqCst) {
            return Err(
                varlink::ErrorKind::InvalidParameter("ConfigurationLocked".to_owned()).into(),
            );
        }

        let _guard = self.state.lock().unwrap();
        let ddc_operation = || -> std::result::Result<_, ddcutil::Error> {
            let dref = ddcutil::find_display(
                display_number,
                edid_base64.as_deref(),
                is_edid_prefix_allowed(&options),
            )?;
            let mut handle = ddcutil::open_display(dref)?;
            let verify = is_setvcp_verifying(&options);

            ddcutil::set_vcp(&mut handle, vcp_code as u8, new_value as u16, verify)?;

            Self::broadcast_set_vcp(
                display_number,
                edid_base64.as_deref(),
                vcp_code,
                new_value,
                client_context,
            );

            Ok(())
        };

        match ddc_operation() {
            Ok(()) => call.reply(),
            Err(e) => send_ddc_error(call, None, display_number, edid_base64, Some(vcp_code), &e),
        }
    }

    fn subscribe(&self, call: &mut dyn Call_Subscribe, use_polling: bool) -> varlink::Result<()> {
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
            // Possibly not serious - might already be watching, which is an error?
            error!("Failed to enable events: {}", e);
        }

        // Create a channel for this subscriber
        let (event_listener, event_receiver) = unbounded::<Event>();

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
        let subscriber_id = Self::subscribe_to_events(event_listener);

        // Main loop: forward events from the channel
        // Loops while client is still listening.
        loop {
            match event_receiver.recv() {
                Ok(event) => {
                    if let Err(_) = call.reply(event) {
                        // Client disconnected
                        break;
                    }
                    call.set_continues(true);  // Is this necessary?
                }
                Err(_) => {
                    // All senders dropped
                    break;
                }
            }
        }

        // Client has gone - cleanup
        Self::unsubscribe_from_events(subscriber_id);

        // Close the stream - client will never receive this.
        call.set_continues(false);
        let _ = call.reply(Event {
            kind: Event_kind::stream_closed,
            data: "{}".to_owned(),
        });

        Ok(())
    }
}

fn is_edid_prefix_allowed(options: &Option<CallOptions>) -> bool {
    options
        .as_ref()
        .map_or(false, |o| o.allow_edid_prefix.unwrap_or(false))
}

fn is_setvcp_verifying(options: &Option<CallOptions>) -> bool {
    options
        .as_ref()
        .map_or(true, |o| !o.no_verify.unwrap_or(false))
}

/// Convert ddcutil capabilities data to Varlink format.
fn convert_capabilities_data(
    data: ddcutil::CapabilitiesData,
) -> (
    String,
    i64,
    i64,
    StringHashMap<String>,
    StringHashMap<CapabilitiesFeature>,
) {
    let commands = data
        .commands
        .into_iter()
        .map(|cmd| (format!("{:02X}", cmd.code), cmd.description))
        .collect();

    let capabilities = data
        .features
        .into_iter()
        .map(|feature| {
            let values = feature
                .values
                .into_iter()
                .map(|val| (format!("{:02X}", val.code), val.name))
                .collect();
            (
                format!("{:02X}", feature.code),
                CapabilitiesFeature {
                    feature_name: feature.name,
                    feature_description: feature.description,
                    values,
                },
            )
        })
        .collect();

    (
        data.model_name,
        data.mccs_major as i64,
        data.mccs_minor as i64,
        commands,
        capabilities,
    )
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
