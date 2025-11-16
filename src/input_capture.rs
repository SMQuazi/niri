//! Module containing the parts of the implementation of input capture that need global state.

use std::thread;

use reis::eis;
use reis::handshake::EisHandshaker;
use reis::request::{DeviceCapability, EisRequest, EisRequestConverter};
use reis::PendingRequestResult;

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
                exposed_device_types: _,
            } => {
                warn!("InputCapture: received NewEisContext for session {}", session_id);
                
                // WORKAROUND: Handle EIS connection in a separate thread instead of using calloop.
                // The reis library's EisRequestSource causes calloop to freeze on disconnect.
                // By handling the connection in a thread, we avoid the calloop integration entirely.
                thread::Builder::new()
                    .name(format!("eis-session-{}", session_id))
                    .spawn(move || {
                        warn!("EIS session {} thread started", session_id);
                        
                        if let Err(e) = handle_eis_connection(session_id as u32, ctx) {
                            warn!("EIS session {} error: {}", session_id, e);
                        }
                        
                        warn!("EIS session {} thread exiting", session_id);
                    })
                    .expect("Failed to spawn EIS session thread");
                
                warn!("InputCapture: NewEisContext complete (thread-based handler)")
            }
        }
    }
}

fn handle_eis_connection(session_id: u32, ctx: eis::Context) -> Result<(), Box<dyn std::error::Error>> {
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
    warn!("EIS session {}: Setting up seat and devices", session_id);
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
                        match eis_request {
                            EisRequest::Bind(bind_req) => {
                                warn!("EIS session {}: Client bound to capabilities: {:?}", session_id, bind_req.capabilities);
                                
                                // Create devices based on requested capabilities
                                if bind_req.capabilities.contains(DeviceCapability::Pointer) {
                                    seat.add_device(
                                        Some("pointer"),
                                        eis::device::DeviceType::Virtual,
                                        DeviceCapability::Pointer.into(),
                                        |_| {},
                                    );
                                }
                                
                                if bind_req.capabilities.contains(DeviceCapability::PointerAbsolute) {
                                    seat.add_device(
                                        Some("pointer-absolute"),
                                        eis::device::DeviceType::Virtual,
                                        DeviceCapability::PointerAbsolute.into(),
                                        |_| {},
                                    );
                                }
                                
                                if bind_req.capabilities.contains(DeviceCapability::Keyboard) {
                                    seat.add_device(
                                        Some("keyboard"),
                                        eis::device::DeviceType::Virtual,
                                        DeviceCapability::Keyboard.into(),
                                        |_| {},
                                    );
                                }
                                
                                connection.flush()?;
                                warn!("EIS session {}: Devices created", session_id);
                            }
                            EisRequest::Disconnect => {
                                warn!("EIS session {}: Client requested disconnect", session_id);
                                return Ok(());
                            }
                            _ => {
                                // Handle other requests (frame, start_emulating, etc.) here
                                // For now, we just acknowledge them
                                warn!("EIS session {}: Received request: {:?}", session_id, eis_request);
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
