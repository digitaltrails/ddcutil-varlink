// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/main.rs

//! # ddcutil-varlink
//!
//! A varlink service for _libddcutil_.
//!
//! Start a service on a unix socket, in priority order use:
//!
//! 1. If the environment variable `LISTEN_FDS` is set, accept
//!    the socket set by systemd (assumed to be on fd 3).
//! 2. If the environment variable `XDG_RUNTIME_DIR` is set,
//!
//!    use `unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket`,
//! 3. Fallback to `/tmp/ddcutil-varlink.socket`.

use log::{error, info, warn};
use std::os::unix::net::UnixListener;
use std::os::unix::io::FromRawFd;
use varlink::*;

use varlink_impl::DdcutilService;

// Our varlink generated interface for com_ddcutil_service.
#[allow(nonstandard_style, dead_code, clippy::all, clippy::nursery)]
mod com_ddcutil_service {
    include!(concat!(env!("OUT_DIR"), "/com.ddcutil.service.rs"));
}

// Our FFI wrapper around the generated bindings for libddcutil.
mod ffi;

// Our modules
mod ddcutil;
mod polling;
mod service;
mod subscribers;
mod varlink_impl;

/// Start the service on its unix socket.
///
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

    env_logger::init();
    info!(
        "Running with user privileges (UID: {})",
        rustix::process::getuid().as_raw()
    );

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

    // Check for systemd Socket Activation (LISTEN_FDS environment variable)
    // Will most likely be bound to unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket
    if let Ok(fds) = std::env::var("LISTEN_FDS") {
        // Systemd handles binding the file descriptor for us.
        // We pass an empty/dummy address string because varlink crate
        // automatically prioritizes the systemd FD when LISTEN_FDS exists.


        // SAFETY: We assume fd 3 is a valid socket passed by systemd.
        info!("LISTEN_FDS is set to {}. Activated via systemd. Assuming file descriptor 3.", fds);

        let listener = unsafe { UnixListener::from_raw_fd(3) };
        if let Ok(addr) = listener.local_addr() {
            if let Some(path) = addr.as_pathname() {
                info!("Listening on systemd assigned socket: {}", path.display());  // prints the path
            } else {
                warn!("Listening on abstract or unnamed socket.");
            }
        }

        varlink::listen(
            varlink_service,
            "unix:",
            &varlink::ListenConfig {
                idle_timeout: 600,
                ..Default::default()
            },
        )?;
    } else {
        // Fallback for manual local debugging/development
        // Dynamically build the path using XDG_RUNTIME_DIR safely

        // Determine socket address
        // Default to unix:$XDG_RUNTIME_DIR/ddcutil-varlink.socket or /tmp/ddcutil-varlink.socket
        // if XDG_RUNTIME_DIR isn't set.
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
        let socket_address = format!("unix:{}/ddcutil-varlink.socket", runtime_dir);

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
