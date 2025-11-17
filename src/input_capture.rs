//      ! Module containing the parts of the implementation of input capture that need global state.

use std::thread;

use reis::eis;
use reis::handshake::EisHandshaker;
use reis::request::{DeviceCapability, EisRequest, EisRequestConverter};
use reis::PendingRequestResult;

use std::collections::HashMap;
use std::sync::Arc;

use smithay::utils::{Logical, Point};
use zbus::object_server::SignalEmitter;

use crate::dbus::mutter_input_capture::InputCaptureDBusToCalloop;
use crate::niri::State;

/// Check if two line segments intersect
fn line_segments_intersect(
    x1: f64, y1: f64, x2: f64, y2: f64,  // First line segment
    x3: f64, y3: f64, x4: f64, y4: f64,  // Second line segment
) -> bool {
    let denom = (x1 - x2) * (y3 - y4) - (y1 - y2) * (x3 - x4);
    if denom.abs() < 1e-10 {
        return false; // Lines are parallel
    }
    
    let t = ((x1 - x3) * (y3 - y4) - (y1 - y3) * (x3 - x4)) / denom;
    let u = -((x1 - x2) * (y1 - y3) - (y1 - y2) * (x1 - x3)) / denom;
    
    t >= 0.0 && t <= 1.0 && u >= 0.0 && u <= 1.0
}

// Re-export Barrier from mutter_input_capture to avoid type duplication
pub use crate::dbus::mutter_input_capture::Barrier;

pub struct ActiveSession {
    pub session_id: usize,
    pub barriers: Arc<std::sync::Mutex<HashMap<u32, Barrier>>>,
    pub signal_emitter: Arc<std::sync::Mutex<Option<SignalEmitter<'static>>>>,
    pub signal_sender: Option<calloop::channel::Sender<crate::dbus::mutter_input_capture::CalloopToInputCaptureDBus>>,
    pub enabled: bool,  // True when EIS context is active
    pub activated: bool,
    pub current_activation_id: u32,
    pub current_barrier_id: Option<u32>,
}

/// Child struct of the global `State` struct
#[derive(Default)]
pub struct InputCaptureState {
    /// Active input capture sessions with their barriers
    pub sessions: HashMap<usize, ActiveSession>,
    /// Whether pointer input is currently suppressed (captured by remote)
    pub input_suppressed: bool,
}

impl State {
    /// Check if pointer movement from old_pos to new_pos crosses any barriers
    /// Returns (session_id, barrier_id) if a barrier was crossed
    pub fn check_barrier_crossing(
        &self,
        old_pos: Point<f64, Logical>,
        new_pos: Point<f64, Logical>,
    ) -> Option<(usize, u32, Point<f64, Logical>)> {
        // Only check if we have sessions and input is not already suppressed
        if self.niri.input_capture.sessions.is_empty() {
            return None;
        }
        if self.niri.input_capture.input_suppressed {
            warn!("check_barrier_crossing: input already suppressed, skipping");
            return None;
        }
        
        let delta: Point<f64, Logical> = Point::from((new_pos.x - old_pos.x, new_pos.y - old_pos.y));
        
        for (session_id, session) in &self.niri.input_capture.sessions {
            // Only check enabled and inactive sessions
            if !session.enabled {
                warn!("Skipping session {} - not enabled", session_id);
                continue;
            }
            if session.activated {
                warn!("Skipping session {} - already activated", session_id);
                continue;
            }
            
            let barriers = match session.barriers.lock() {
                Ok(b) => b,
                Err(e) => {
                    warn!("Failed to lock barriers for session {}: {:?}", session_id, e);
                    continue;
                }
            };
            
            if barriers.is_empty() {
                warn!("Session {} has no barriers", session_id);
                continue;
            }
            
            warn!("Checking {} barriers for session {}", barriers.len(), session_id);
            
            for (barrier_id, barrier) in barriers.iter() {
                // Barriers are often placed 1 pixel outside the valid coordinate space
                // (e.g., x=6400 when max is 6399). Check if we're at the edge and trying 
                // to move further in that direction.
                const EDGE_THRESHOLD: f64 = 1.5;
                
                // Debug: log ALL barriers being checked
                warn!("Checking barrier {}: ({},{}) -> ({},{}), pos={:?}, delta={:?}", 
                      barrier_id, barrier.x1, barrier.y1, barrier.x2, barrier.y2, new_pos, delta);
                
                let at_or_near_barrier = if barrier.x1 == barrier.x2 {
                    // Vertical barrier
                    let barrier_x = barrier.x1 as f64;
                    (new_pos.x - barrier_x).abs() < EDGE_THRESHOLD &&
                    new_pos.y >= barrier.y1.min(barrier.y2) as f64 &&
                    new_pos.y <= barrier.y1.max(barrier.y2) as f64 &&
                    ((delta.x > 0.0 && new_pos.x >= barrier_x - EDGE_THRESHOLD) ||
                     (delta.x < 0.0 && new_pos.x <= barrier_x + EDGE_THRESHOLD))
                } else if barrier.y1 == barrier.y2 {
                    // Horizontal barrier
                    let barrier_y = barrier.y1 as f64;
                    (new_pos.y - barrier_y).abs() < EDGE_THRESHOLD &&
                    new_pos.x >= barrier.x1.min(barrier.x2) as f64 &&
                    new_pos.x <= barrier.x1.max(barrier.x2) as f64 &&
                    ((delta.y > 0.0 && new_pos.y >= barrier_y - EDGE_THRESHOLD) ||
                     (delta.y < 0.0 && new_pos.y <= barrier_y + EDGE_THRESHOLD))
                } else {
                    // Diagonal barrier - use line intersection
                    line_segments_intersect(
                        old_pos.x, old_pos.y, new_pos.x, new_pos.y,
                        barrier.x1 as f64, barrier.y1 as f64,
                        barrier.x2 as f64, barrier.y2 as f64,
                    )
                };
                
                if at_or_near_barrier {
                    warn!("Barrier crossing detected: session {} barrier {} from {:?} to {:?}", 
                          session_id, barrier_id, old_pos, new_pos);
                    warn!("Barrier line: ({},{}) -> ({},{})", barrier.x1, barrier.y1, barrier.x2, barrier.y2);
                    return Some((*session_id, *barrier_id, new_pos));
                }
            }
        }
        None
    }

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
                exposed_device_types: _,
                screen_width,
                screen_height,
                event_sender,
            } => {
                warn!("InputCapture: received NewEisContext for session {} (screen {}x{})", 
                      session_id, screen_width, screen_height);
                
                // Mark session as enabled now that EIS context is active
                if let Some(session) = self.niri.input_capture.sessions.get_mut(&session_id) {
                    session.enabled = true;
                    warn!("InputCapture: Session {} marked as enabled", session_id);
                }
                
                // WORKAROUND: Handle EIS connection in a separate thread instead of using calloop.
                // The reis library's EisRequestSource causes calloop to freeze on disconnect.
                // By handling the connection in a thread, we avoid the calloop integration entirely.
                let event_sender_clone = event_sender.clone();
                thread::Builder::new()
                    .name(format!("eis-session-{}", session_id))
                    .spawn(move || {
                        warn!("EIS session {} thread started", session_id);
                        
                        if let Err(e) = handle_eis_connection(session_id, ctx, screen_width, screen_height, event_sender) {
                            warn!("EIS session {} error: {}", session_id, e);
                        }
                        
                        warn!("EIS session {} thread exiting - sending UnregisterSession", session_id);
                        // Clean up the session when the thread exits
                        let _ = event_sender_clone.send(InputCaptureDBusToCalloop::UnregisterSession { session_id });
                    })
                    .expect("Failed to spawn EIS session thread");
                
                warn!("InputCapture: NewEisContext complete (thread-based handler)")
            }
            InputCaptureDBusToCalloop::RegisterSession { session_id, barriers, signal_sender } => {
                warn!("InputCapture: Registering session {} for barrier detection", session_id);
                self.niri.input_capture.sessions.insert(session_id, ActiveSession {
                    session_id,
                    barriers,
                    signal_emitter: Arc::new(std::sync::Mutex::new(None)),
                    signal_sender: Some(signal_sender),
                    enabled: true,  // Session is registered when client is ready
                    activated: false,
                    current_activation_id: 0,
                    current_barrier_id: None,
                });
            }
            InputCaptureDBusToCalloop::UnregisterSession { session_id } => {
                warn!("InputCapture: Unregistering session {}", session_id);
                self.niri.input_capture.sessions.remove(&session_id);
                self.niri.input_capture.input_suppressed = false;
            }
            InputCaptureDBusToCalloop::ActivateSession { session_id, barrier_id, cursor_position } => {
                warn!("InputCapture: Activating session {} barrier {} at {:?}", 
                      session_id, barrier_id, cursor_position);
                // TODO: Emit activated signal through DBus
                if let Some(session) = self.niri.input_capture.sessions.get_mut(&session_id) {
                    session.activated = true;
                    session.current_activation_id += 1;
                    session.current_barrier_id = Some(barrier_id);
                    self.niri.input_capture.input_suppressed = true;
                }
            }
            InputCaptureDBusToCalloop::DeactivateSession { session_id, cursor_position } => {
                warn!("InputCapture: Deactivating session {} at {:?}", session_id, cursor_position);
                // TODO: Emit deactivated signal through DBus
                if let Some(session) = self.niri.input_capture.sessions.get_mut(&session_id) {
                    session.activated = false;
                    session.current_barrier_id = None;
                    self.niri.input_capture.input_suppressed = false;
                }
            }
            InputCaptureDBusToCalloop::EisInputEvent { session_id, request } => {
                use reis::request::*;
                use smithay::backend::input::ButtonState as SmithayButtonState;
                
                // Convert EIS requests to simulated input by directly manipulating the pointer
                match request {
                    EisRequest::PointerMotion(PointerMotion { dx, dy, .. }) => {
                        // Relative motion - move pointer by delta
                        use smithay::utils::{Logical, Point};
                        if let Some(pointer) = self.niri.seat.get_pointer() {
                            let current = pointer.current_location();
                            let delta = Point::<f64, Logical>::from((dx as f64, dy as f64));
                            let new_pos = current + delta;
                            warn!("EIS session {}: Moving pointer from {:?} by ({}, {}) to {:?}", 
                                  session_id, current, dx, dy, new_pos);
                            
                            // Update pointer location and redraw
                            pointer.motion(
                                self,
                                None,
                                &smithay::input::pointer::MotionEvent {
                                    location: new_pos,
                                    serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                                    time: 0,
                                },
                            );
                            pointer.frame(self);
                            self.niri.queue_redraw_all();
                        }
                    }
                    EisRequest::PointerMotionAbsolute(PointerMotionAbsolute { dx_absolute, dy_absolute, .. }) => {
                        // Absolute position
                        use smithay::utils::{Logical, Point};
                        if let Some(pointer) = self.niri.seat.get_pointer() {
                            let new_pos = Point::<f64, Logical>::from((dx_absolute as f64, dy_absolute as f64));
                            warn!("EIS session {}: Setting pointer to absolute position {:?}", session_id, new_pos);
                            
                            let under = self.niri.contents_under(new_pos);
                            pointer.motion(
                                self,
                                under.surface,
                                &smithay::input::pointer::MotionEvent {
                                    location: new_pos,
                                    serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                                    time: 0,
                                },
                            );
                            pointer.frame(self);
                            self.niri.queue_redraw_all();
                        }
                    }
                    EisRequest::Button(Button { button, state, .. }) => {
                        if let Some(pointer) = self.niri.seat.get_pointer() {
                            use reis::ei::button::ButtonState as EisButtonState;
                            let button_state = match state {
                                EisButtonState::Press => SmithayButtonState::Pressed,
                                EisButtonState::Released => SmithayButtonState::Released,
                            };
                            warn!("EIS session {}: Button {} {:?}", session_id, button, button_state);
                            
                            pointer.button(
                                self,
                                &smithay::input::pointer::ButtonEvent {
                                    serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                                    time: 0,
                                    button,
                                    state: button_state,
                                },
                            );
                            pointer.frame(self);
                        }
                    }
                    EisRequest::KeyboardKey(KeyboardKey { key, state, .. }) => {
                        warn!("EIS session {}: Key {} state={:?}", session_id, key, state);
                        // TODO: Keyboard support later
                    }
                    EisRequest::Frame(_) => {
                        // Frame events group other events together
                    }
                    EisRequest::DeviceStartEmulating(_) => {
                        warn!("EIS session {}: Device start emulating - suppressing local input", session_id);
                        // Now that Input-Leap is ready to send input, suppress local input
                        self.niri.input_capture.input_suppressed = true;
                    }
                    EisRequest::DeviceStopEmulating(_) => {
                        warn!("EIS session {}: Device stop emulating - deactivating session", session_id);
                        // Deactivate the session and restore normal input
                        if let Some(session) = self.niri.input_capture.sessions.get_mut(&session_id) {
                            session.activated = false;
                            session.current_barrier_id = None;
                            self.niri.input_capture.input_suppressed = false;
                            warn!("Session {} deactivated, input restored", session_id);
                        }
                    }
                    _ => {
                        // Ignore other request types for now
                    }
                }
            }
        }
    }
}

fn handle_eis_connection(
    session_id: usize, 
    ctx: eis::Context,
    screen_width: u32,
    screen_height: u32,
    event_sender: calloop::channel::Sender<InputCaptureDBusToCalloop>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Phase 1: Handshake
    warn!("EIS session {}: Starting handshake", session_id);
    let mut handshaker = EisHandshaker::new(&ctx, 1);
    
    let handshake_resp = 'handshake: loop {
        ctx.read()?;
        
        while let Some(result) = ctx.pending_request() {
            let request = match result {
                PendingRequestResult::Request(req) => req,
                PendingRequestResult::ParseError(e) => {
                    return Err(format!("Parse error: {:?}", e).into());
                }
                PendingRequestResult::InvalidObject(_) => continue,
            };
            
            match handshaker.handle_request(request) {
                Ok(Some(resp)) => {
                    warn!("EIS session {}: Handshake complete, client: {:?}", session_id, resp.name);
                    ctx.flush()?;
                    break 'handshake resp;
                }
                Ok(std::option::Option::None) => {
                    ctx.flush()?;
                }
                Err(e) => {
                    return Err(format!("Handshake error: {:?}", e).into());
                }
            }
        }
        
        thread::sleep(std::time::Duration::from_millis(10));
    };
    
    // Phase 2: Create request converter and advertise seat
    warn!("EIS session {}: Setting up seat and devices (screen {}x{})", session_id, screen_width, screen_height);
    let mut request_converter = EisRequestConverter::new(&ctx, handshake_resp, 1);
    let connection = request_converter.handle().clone();
    
    // Create a seat with all capabilities
    let seat = connection.add_seat(
        Some("default"),
        DeviceCapability::Pointer
            | DeviceCapability::PointerAbsolute
            | DeviceCapability::Keyboard
            | DeviceCapability::Touch
            | DeviceCapability::Scroll
            | DeviceCapability::Button,
    );
    
    connection.flush()?;
    warn!("EIS session {}: Seat advertised", session_id);
    
    // Phase 3: Handle client requests
    
    loop {
        match ctx.read() {
            Ok(_) => {
                while let Some(result) = ctx.pending_request() {
                    let request = match result {
                        PendingRequestResult::Request(req) => req,
                        PendingRequestResult::ParseError(e) => {
                            warn!("EIS session {}: Parse error: {:?}", session_id, e);
                            continue;
                        }
                        PendingRequestResult::InvalidObject(_) => continue,
                    };
                    
                    if let Err(e) = request_converter.handle_request(request) {
                        warn!("EIS session {}: Request error: {:?}", session_id, e);
                        continue;
                    }
                    
                    while let Some(eis_request) = request_converter.next_request() {
                        warn!("EIS session {}: Received request: {:?}", session_id, eis_request);
                        match &eis_request {
                            EisRequest::Bind(bind_req) => {
                                warn!("EIS session {}: Client bound to capabilities: {:?}", session_id, bind_req.capabilities);
                                
                                // Create devices based on requested capabilities
                                let mut created_devices = Vec::new();
                                
                                if bind_req.capabilities.contains(DeviceCapability::Pointer) {
                                    let device = seat.add_device(
                                        Some("pointer"),
                                        eis::device::DeviceType::Virtual,
                                        DeviceCapability::Pointer.into(),
                                        |_| {},
                                    );
                                    created_devices.push(device);
                                }
                                
                                if bind_req.capabilities.contains(DeviceCapability::PointerAbsolute) {
                                    let device = seat.add_device(
                                        Some("pointer-absolute"),
                                        eis::device::DeviceType::Virtual,
                                        DeviceCapability::PointerAbsolute.into(),
                                        |device| {
                                            // Set the screen region for absolute positioning
                                            // Virtual devices use regions (logical pixels) not dimensions (mm)
                                            device.device().region(
                                                0,  // offset_x
                                                0,  // offset_y
                                                screen_width,
                                                screen_height,
                                                1.0,  // scale
                                            );
                                        },
                                    );
                                    created_devices.push(device);
                                }
                                
                                if bind_req.capabilities.contains(DeviceCapability::Keyboard) {
                                    let device = seat.add_device(
                                        Some("keyboard"),
                                        eis::device::DeviceType::Virtual,
                                        DeviceCapability::Keyboard.into(),
                                        |_| {},
                                    );
                                    created_devices.push(device);
                                }
                                
                                connection.flush()?;
                                warn!("EIS session {}: Devices created with screen dimensions {}x{}", session_id, screen_width, screen_height);
                                
                                // Resume all devices so they can accept input
                                // Devices start in paused state and must be resumed before they can send/receive events
                                for device in created_devices {
                                    device.resumed();
                                }
                                connection.flush()?;
                                warn!("EIS session {}: All devices resumed and ready for input", session_id);
                            }
                            EisRequest::Disconnect => {
                                warn!("EIS session {}: Client requested disconnect", session_id);
                                return Ok(());
                            }
                            _ => {
                                // Send all other requests to the main loop for processing
                                let _ = event_sender.send(InputCaptureDBusToCalloop::EisInputEvent {
                                    session_id,
                                    request: eis_request,
                                });
                            }
                        }
                    }
                }
                
                ctx.flush()?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                warn!("EIS session {}: Client disconnected", session_id);
                return Ok(());
            }
            Err(err) => {
                return Err(err.into());
            }
        }
        
        thread::sleep(std::time::Duration::from_millis(10));
    }
}
