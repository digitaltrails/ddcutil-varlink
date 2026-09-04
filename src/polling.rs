// SPDX-FileCopyrightText: 2026 Contributors to ddcutil-varlink <https://github.com/digitaltrails/ddcutil-varlink>
// SPDX-License-Identifier: GPL-2.0-or-later
// src/polling.rs

use crate::ddcutil::{DdcutilEvent, DisplayRef};
use crate::service::DdcutilSharedState;
use crossbeam_channel::{Receiver, Sender};
use std::sync::{Arc, Mutex};

// ============================================================================
// Polling loop (runs in a background thread)
// Alternative way of detecting connectivity changes and DPMS events.
// (libddcutil does not handle DPMS and on some hardware cannot detect
// connectivity changes)
// ============================================================================

/// State of a single display for the polling loop.
#[derive(Debug, Clone, Copy)]
struct DisplayState {
    display_number: i32,
    #[allow(dead_code)]
    display_ref: DisplayRef, // for potential future use
    awake: bool,
}

/// The main polling loop. Runs in its own thread.
pub fn polling_loop(
    state: Arc<Mutex<DdcutilSharedState>>,
    event_dispatcher: Sender<DdcutilEvent>,
    shutdown_listener: Receiver<()>,
) {
    use crate::ddcutil::{
        get_display_info_list, is_dpms_awake, redetect, sleep_interruptible, DdcutilEventKind,
    };
    use base64::{engine::general_purpose, Engine as _};
    use log::{debug, error, info};
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    let mut previous_states: HashMap<String, DisplayState> = HashMap::new();
    let mut initializing = true;

    loop {
        // Check for shutdown signal
        if shutdown_listener.try_recv().is_ok() {
            info!("Polling thread received shutdown signal, stopping polling thread.");
            break;
        }

        // ---- Acquire the lock and read config ----
        let guard = state.lock().unwrap();
        let (interval, cascade, do_detect, events_enabled) = {
            let cfg = &*guard;
            (
                cfg.poll_interval_secs,
                cfg.poll_cascade_secs,
                cfg.poll_do_detect,
                cfg.events_enabled,
            )
        };

        if interval == 0 {
            info!("Polling interval set to zero, stopping polling thread.");
            break;
        }
        
        if !events_enabled {
            drop(guard);
            sleep_interruptible(Duration::from_secs(5));
            continue;
        }

        // ---- Call libddcutil (safe because we hold the lock) ----
        if do_detect {
            if let Err(e) = redetect() {
                error!("redetect failed: {}", e);
                drop(guard);
                sleep_interruptible(Duration::from_secs(interval as u64));
                continue;
            }
        }

        let current_displays = match get_display_info_list(false) {
            Ok(list) => list,
            Err(e) => {
                error!("get_display_info_list failed: {}", e);
                drop(guard);
                sleep_interruptible(Duration::from_secs(interval as u64));
                continue;
            }
        };

        // Build current state (also needs libddcutil for DPMS check)
        let mut current_states = HashMap::with_capacity(current_displays.len());
        for display in &current_displays {
            let edid = general_purpose::STANDARD.encode(display.edid_bytes);
            let awake = match is_dpms_awake(display.display_ref) {
                Ok(a) => a,
                Err(e) => {
                    debug!(
                        "DPMS query failed for display {}: {}",
                        display.display_number, e
                    );
                    false  // assume its asleep.
                }
            };
            current_states.insert(
                edid,
                DisplayState {
                    display_number: display.display_number,
                    display_ref: display.display_ref,
                    awake,
                },
            );
        }

        // ---- Release the lock before comparing states and sending events ----
        drop(guard);

        // Compare states (no lock needed)
        let current_edids: HashSet<_> = current_states.keys().collect();
        let previous_edids: HashSet<_> = previous_states.keys().collect();

        let some_newly_detected = current_edids.difference(&previous_edids).next().is_some();
        let some_lost = previous_edids.difference(&current_edids).next().is_some();
        let connection_change = some_newly_detected || some_lost;

        if connection_change && !initializing {
            let event_type = if some_newly_detected {
                DdcutilEventKind::Connected.as_str()
            } else {
                DdcutilEventKind::Disconnected.as_str()
            };
            let data = serde_json::json!({
                "event_type": event_type,
                "flags": 0,
            })
            .to_string();
            let event = DdcutilEvent {
                kind: DdcutilEventKind::ConnectedDisplaysChanged,
                data,
            };
            debug!("poll: sending connection change event");
            let _ = event_dispatcher.send(event);
        }

        // Detect DPMS changes
        for (edid, state) in &current_states {
            if let Some(prev_state) = previous_states.get(edid) {
                if prev_state.awake != state.awake && !initializing {
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
                    })
                    .to_string();
                    let event = DdcutilEvent { kind, data };
                    debug!("poll: sending DPMS change event");
                    let _ = event_dispatcher.send(event);
                }
            }
        }

        previous_states = current_states;
        initializing = false;

        // Sleep without holding the lock
        let sleep_duration = if connection_change {
            Duration::from_millis((cascade * 1000.0) as u64)
        } else {
            Duration::from_secs(interval as u64)
        };
        sleep_interruptible(sleep_duration);
    }
}
