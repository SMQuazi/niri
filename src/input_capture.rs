//! Module containing the parts of the implementation of input capture that need global state.

use std::thread;

use crate::dbus::mutter_input_capture::InputCaptureDBusToCalloop;
use crate::niri::State;

/// Child struct of the global `State` struct
#[derive(Default)]
pub struct InputCaptureState {
    // Currently empty - EIS sessions are handled in separate threads
    // and don't need tracking in the main event loop
}

impl State {
    pub fn on_input_capture_msg_from_dbus(&mut self, msg: InputCaptureDBusToCalloop) {
        match msg {
            InputCaptureDBusToCalloop::RemoveEisHandler { session_id } => {
                // With the thread-based approach, cleanup happens automatically
                // when the EIS context is dropped (when the thread exits).
                // No manual cleanup needed.
                warn!("InputCapture: session {} cleanup requested (automatic via thread)", session_id);
            }
            InputCaptureDBusToCalloop::NewEisContext {
                session_id,
                ctx,
                exposed_device_types,
            } => {
                warn!("InputCapture: received NewEisContext for session {}", session_id);
                
                // WORKAROUND: Handle EIS connection in a separate thread instead of using calloop.
                // The reis library's EisRequestSource causes calloop to freeze on disconnect.
                // By handling the connection in a thread, we avoid the calloop integration entirely.
                thread::Builder::new()
                    .name(format!("eis-session-{}", session_id))
                    .spawn(move || {
                        warn!("EIS session {} thread started", session_id);
                        
                        let ctx = ctx;
                        let _exposed_device_types = exposed_device_types;
                        
                        // Event loop: poll for events and handle disconnect
                        loop {
                            // Read incoming data from the socket
                            match ctx.read() {
                                Ok(_) => {
                                    // Process any pending requests
                                    while let Some(_result) = ctx.pending_request() {
                                        // We're acting as an EIS server (receiver context),
                                        // so we would process client requests here.
                                        // For now, we just drain the queue.
                                    }
                                },
                                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                                    // Client disconnected normally
                                    warn!("EIS session {} client disconnected", session_id);
                                    break;
                                },
                                Err(err) => {
                                    // Other I/O error
                                    warn!("EIS session {} I/O error: {}", session_id, err);
                                    break;
                                }
                            }
                            
                            // Small sleep to avoid busy-waiting
                            thread::sleep(std::time::Duration::from_millis(10));
                        }
                        
                        drop(ctx);
                        warn!("EIS session {} thread exiting", session_id);
                    })
                    .expect("Failed to spawn EIS session thread");
                
                warn!("InputCapture: NewEisContext complete (thread-based handler)")
            }
        }
    }
}
