// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/main.rs

use log::{error, info, warn};
use std::env;
use varlink::*;

use varlink_impl::DdcutilService;

// ============================================================================
// Imports from generated interface
// ============================================================================

mod com_ddcutil_service;

// ============================================================================
// Our modules
// ============================================================================
mod ddcutil;
mod polling;
mod service;
mod subscribers;
mod varlink_impl;

// ============================================================================
// Main entry point
// ============================================================================

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Set up panic hook for better logging
    std::panic::set_hook(Box::new(|panic_info| {
        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown"
        };
        let location = panic_info
            .location()
            .unwrap_or_else(|| std::panic::Location::caller());
        error!(
            "PANIC at {}:{}: {}",
            location.file(),
            location.line(),
            payload
        );
    }));

    info!(
        "Running with user privileges (UID: {})",
        rustix::process::getuid().as_raw()
    );
    env_logger::init();

    // Create the service
    let (service, event_listener) = DdcutilService::new();

    // Spawn thread to forward ddcutil events to Varlink subscribers
    std::thread::spawn(move || {
        subscribers::forward_events(event_listener);
    });

    // Build the Varlink interface
    let interface = com_ddcutil_service::new(Box::new(service));
    let varlink_service = VarlinkService::new(
        "com.ddcutil",
        "ddcutil-varlink",
        "1.0.0",
        "https://github.com/digitaltrails/ddcutil-varlink",
        vec![Box::new(interface)],
    );

    // Determine socket address
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let socket_address = format!("unix:{}/ddcutil-varlink.socket", runtime_dir);

    // Check for systemd Socket Activation (LISTEN_FDS environment variable)
    // Will be on unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket
    if let Ok(fds) = env::var("LISTEN_FDS") {
        // Systemd handles binding the file descriptor for us.
        // We pass an empty/dummy address string because varlink crate
        // automatically prioritizes the systemd FD when LISTEN_FDS exists.
        info!("LISTEN_FDS is set {}. Activated via systemd.", fds);
        info!(
            "Listening on systemd assigned socket - which might be: {}",
            socket_address
        );
        varlink::listen(
            varlink_service,
            "systemd:",
            &varlink::ListenConfig {
                idle_timeout: 600,
                ..Default::default()
            },
        )?;
    } else {
        // Fallback for manual local debugging/development
        // Dynamically build the path using XDG_RUNTIME_DIR safely
        warn!("LISTEN_FDS is not set. Running in manual mode.");
        info!("Listening on socket: {}", socket_address);
        varlink::listen(
            varlink_service,
            &socket_address,
            &varlink::ListenConfig {
                idle_timeout: 0,
                ..Default::default()
            },
        )?;
    }

    Ok(())
}
