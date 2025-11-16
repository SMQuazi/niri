//! `org.gnome.Mutter.InputCapture` implementation. `xdg-desktop-portal-gnome` implements the
//! Input Capture portal on top of this.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use enumflags2::BitFlags;
use serde::{Deserialize, Serialize};
use zbus::fdo::{self, RequestNameFlags};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, DeserializeDict, OwnedObjectPath, Type};
use zbus::{interface, ObjectServer};

use super::mutter_remote_desktop::MutterXdpDeviceType;
use super::Start;
use crate::backend::IpcOutputMap;

pub(super) mod shared {
    use std::collections::HashMap;
    use std::sync::Arc;

    use futures_util::lock::Mutex;
    use zbus::object_server::InterfaceRef;

    /// Data shared between sessions
    #[derive(Default)]
    pub struct InputCaptureShared {
        pub(in super::super) sessions: HashMap<usize, InterfaceRef<super::Session>>,
    }
    impl InputCaptureShared {
        pub fn new_arc_mutex() -> Arc<Mutex<Self>> {
            Arc::new(Mutex::new(Self::default()))
        }
    }
}

pub enum InputCaptureDBusToCalloop {
    RemoveEisHandler {
        session_id: usize,
    },
    NewEisContext {
        session_id: usize,
        ctx: reis::eis::Context,
        exposed_device_types: BitFlags<MutterXdpDeviceType>,
    },
}

// == MAIN INTERFACE ==

/// D-Bus object for the input capture portal's implementation
pub(super) struct InputCapture {
    pub(super) to_calloop: calloop::channel::Sender<InputCaptureDBusToCalloop>,
    pub(super) shared: Arc<futures_util::lock::Mutex<shared::InputCaptureShared>>,
    pub(super) ipc_outputs: Arc<std::sync::Mutex<IpcOutputMap>>,
}

impl Start for InputCapture {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        info!("Starting InputCapture DBus interface...");
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Mutter/InputCapture", self)?;
        conn.request_name_with_flags("org.gnome.Mutter.InputCapture", flags)?;

        info!("InputCapture DBus interface started successfully");
        Ok(conn)
    }
}

#[interface(
    name = "org.gnome.Mutter.InputCapture",
    spawn = false,
    introspection_docs = false
)]
impl InputCapture {
    async fn create_session(
        &mut self,
        capabilities: u32,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        warn!("InputCapture.CreateSession START (capabilities: {})", capabilities);
        
        static NUMBER: AtomicUsize = AtomicUsize::new(0);

        let session_id = NUMBER.fetch_add(1, Ordering::SeqCst);
        let path = format!("/org/gnome/Mutter/InputCapture/Session/u{}", session_id);
        let path = OwnedObjectPath::try_from(path).unwrap();

        warn!(
            "InputCapture.CreateSession: created session ID {} at path {}",
            session_id, path
        );

        let session = Session {
            id: session_id,
            id_str: session_id.to_string(),
            to_calloop: self.to_calloop.clone(),
            ipc_outputs: self.ipc_outputs.clone(),
            active: true,  // Session is active immediately upon creation
            enabled: false,
            using_eis: false,
            zone_set: Arc::new(AtomicU32::new(0)),
            activation_id: Arc::new(AtomicU32::new(0)),
            barriers: Arc::new(Mutex::new(HashMap::new())),
        };

        warn!("InputCapture.CreateSession: registering session object");
        match server.at(&path, session).await {
            Ok(true) => {
                warn!("InputCapture.CreateSession: getting interface reference");
                let iface: zbus::object_server::InterfaceRef<Session> = server.interface(&path).await.unwrap();
                warn!("InputCapture.CreateSession: storing session reference");
                self.shared.lock().await.sessions.insert(session_id, iface.clone());
                
                // Also store globally for testing
                SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
                    .lock().unwrap().insert(path.to_string(), iface);
                
                warn!("InputCapture.CreateSession: session stored");
            }
            Ok(false) => return Err(fdo::Error::Failed("session path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )))
            }
        }

        warn!("InputCapture.CreateSession COMPLETE: {}", path);
        Ok(path)
    }

    /// Bitmask of supported capabilities: KEYBOARD(1), POINTER(2), TOUCHSCREEN(4)
    #[zbus(property)]
    async fn supported_capabilities(&self) -> u32 {
        BitFlags::<MutterXdpDeviceType>::all().bits()
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        1
    }
}

// == SESSION ==

/// D-Bus object for an input capture session
#[derive(Debug, Clone)]
struct Barrier {
    id: u32,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
}

pub(super) struct Session {
    id: usize,
    id_str: String,
    to_calloop: calloop::channel::Sender<InputCaptureDBusToCalloop>,
    ipc_outputs: Arc<std::sync::Mutex<IpcOutputMap>>,
    pub active: bool,
    pub enabled: bool,
    using_eis: bool,
    zone_set: Arc<AtomicU32>,
    activation_id: Arc<AtomicU32>,
    barriers: Arc<Mutex<HashMap<u32, Barrier>>>,
}

impl Session {
    fn get_zones_impl(&self) -> (u32, Vec<Zone>) {
        let outputs = match self.ipc_outputs.lock() {
            Ok(outputs) => outputs,
            Err(poisoned) => {
                warn!("InputCapture: ipc_outputs lock poisoned, recovering");
                poisoned.into_inner()
            }
        };
        
        let zones: Vec<Zone> = outputs
            .values()
            .filter_map(|output| {
                output.logical.map(|l| {
                    warn!("Zone from output: {}x{} @ ({},{})", l.width, l.height, l.x, l.y);
                    Zone(l.width as u32, l.height as u32, l.x, l.y)
                })
            })
            .collect();

        warn!("GetZones returning {} zones", zones.len());
        for (i, zone) in zones.iter().enumerate() {
            warn!("  Zone {}: {}x{} @ ({},{})", i, zone.0, zone.1, zone.2, zone.3);
        }

        let zone_set = self.zone_set.load(Ordering::SeqCst);
        (zone_set, zones)
    }

    /// Stops the session.
    pub async fn stop(&mut self, _server: &ObjectServer, ctxt: &SignalEmitter<'_>) {
        if !self.active {
            return;
        }
        self.active = false;
        self.enabled = false;

        // Remove from SESSIONS map to prevent stale references
        let path = ctxt.path().to_string();
        if let Some(sessions_lock) = SESSIONS.get() {
            if let Ok(mut sessions) = sessions_lock.try_lock() {
                sessions.remove(&path);
                warn!("InputCapture session removed from SESSIONS map (id={})", self.id);
            } else {
                warn!("InputCapture session could not acquire SESSIONS lock for cleanup (id={})", self.id);
            }
        }

        // DO NOT send Closed signal or remove from object server
        // These operations can block/hang when the DBus connection is in a bad state
        // The session will be garbage collected by zbus when the connection dies
        
        warn!("InputCapture session stopped (id={})", self.id);
    }
}

// Zone is serialized as a DBus struct (tuple): (width, height, x, y)
#[derive(Debug, Clone, Serialize, Type)]
struct Zone(u32, u32, i32, i32);

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct SetPointerBarriersOptions {}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
#[allow(dead_code)]
struct PointerBarrier {
    barrier_id: u32,
    position: (i32, i32, i32, i32), // x1, y1, x2, y2
}

#[interface(
    name = "org.gnome.Mutter.InputCapture.Session",
    spawn = false,
    introspection_docs = false
)]
impl Session {
    #[zbus(property)]
    async fn session_id(&self) -> &str {
        &self.id_str
    }

    async fn start(&mut self) -> fdo::Result<()> {
        debug!("InputCapture.Start id={}", self.id);

        if self.active {
            return Err(fdo::Error::Failed("Already started".to_owned()));
        }
        self.active = true;

        Ok(())
    }

    #[zbus(name = "Stop")]
    pub async fn stop_dbus(
        &mut self,
        #[zbus(object_server)] _server: &ObjectServer,
        #[zbus(signal_context)] _ctxt: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        debug!("InputCapture.Stop id={}", self.id);

        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }

        // DO NOT call stop() - just mark as inactive
        self.active = false;
        self.enabled = false;

        Ok(())
    }

    #[zbus(signal)]
    async fn closed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    async fn get_zones(&self) -> fdo::Result<(u32, Vec<Zone>)> {
        warn!("InputCapture.GetZones START id={}", self.id);

        // GetZones can be called before Start() to query available zones
        let result = self.get_zones_impl();
        warn!("InputCapture.GetZones COMPLETE id={} zones={}", self.id, result.1.len());
        Ok(result)
    }

    async fn set_pointer_barriers(
        &mut self,
        _options: SetPointerBarriersOptions,
        barriers: Vec<PointerBarrier>,
        zone_set: u32,
    ) -> fdo::Result<Vec<u32>> {
        debug!(
            "InputCapture.SetPointerBarriers id={} zone_set={} barriers={:?}",
            self.id, zone_set, barriers
        );

        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }

        let current_zone_set = self.zone_set.load(Ordering::SeqCst);
        if zone_set != current_zone_set {
            return Err(fdo::Error::Failed(format!(
                "Zone set mismatch: expected {}, got {}",
                current_zone_set, zone_set
            )));
        }

        // Setting barriers suspends the session
        self.enabled = false;

        // For now, accept all barriers. In a full implementation, we would:
        // 1. Validate that barriers are at zone boundaries
        // 2. Validate that barriers are fully contained within zones
        // 3. Return barrier_ids that failed validation
        let failed_barriers: Vec<u32> = Vec::new();

        Ok(failed_barriers)
    }

    async fn enable(&mut self) -> fdo::Result<()> {
        warn!("InputCapture.Enable START id={}", self.id);

        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }

        if !self.using_eis {
            return Err(fdo::Error::Failed("Must call ConnectToEIS first".to_owned()));
        }

        self.enabled = true;

        // In a full implementation, we would start monitoring for pointer barrier crossings
        // and emit Activated signal when appropriate

        warn!("InputCapture.Enable COMPLETE id={}", self.id);
        Ok(())
    }

    async fn disable(&mut self) -> fdo::Result<()> {
        warn!("InputCapture.Disable START id={}", self.id);

        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }

        self.enabled = false;

        warn!("InputCapture.Disable COMPLETE id={}", self.id);
        Ok(())
    }

    #[zbus(name = "AddBarrier")]
    async fn add_barrier(
        &mut self,
        serial: u32,
        position: (i32, i32, i32, i32),
    ) -> fdo::Result<u32> {
        warn!("InputCapture.AddBarrier id={} serial={} position={:?}", self.id, serial, position);
        
        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }
        
        static BARRIER_ID: AtomicU32 = AtomicU32::new(1);
        let barrier_id = BARRIER_ID.fetch_add(1, Ordering::SeqCst);
        
        let barrier = Barrier {
            id: barrier_id,
            x1: position.0,
            y1: position.1,
            x2: position.2,
            y2: position.3,
        };
        
        self.barriers.lock().unwrap().insert(barrier_id, barrier);
        
        warn!("InputCapture.AddBarrier: stored barrier_id={} ({},{}) -> ({},{})", 
              barrier_id, position.0, position.1, position.2, position.3);
        Ok(barrier_id)
    }

    #[zbus(name = "ClearBarriers")]
    async fn clear_barriers(&mut self) -> fdo::Result<()> {
        warn!("InputCapture.ClearBarriers id={}", self.id);
        
        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }
        
        self.barriers.lock().unwrap().clear();
        warn!("InputCapture.ClearBarriers: all barriers cleared");
        Ok(())
    }

    #[zbus(name = "Close")]
    async fn close(
        &mut self,
        #[zbus(object_server)] _server: &ObjectServer,
        #[zbus(signal_context)] _ctxt: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        warn!("InputCapture.Close id={}", self.id);
        
        // DO NOT call stop() - it can block on async operations
        // Just mark as inactive
        self.active = false;
        self.enabled = false;
        
        warn!("InputCapture.Close: complete (cleanup skipped)");
        Ok(())
    }

    async fn release(
        &mut self,
        #[zbus(signal_context)] _ctxt: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        debug!("InputCapture.Release id={}", self.id);

        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }

        // In a full implementation, we would:
        // 1. Stop capturing input if currently active
        // 2. Emit Deactivated signal

        Ok(())
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(&mut self) -> fdo::Result<zvariant::OwnedFd> {
        warn!("InputCapture.ConnectToEIS START id={}", self.id);

        if !self.active {
            return Err(fdo::Error::Failed("Session not started".to_owned()));
        }

        if self.using_eis {
            return Err(fdo::Error::Failed("Already gave EIS socket".to_owned()));
        }

        self.using_eis = true;

        warn!("InputCapture.ConnectToEIS: creating socket pair");
        let (a, b) = UnixStream::pair().map_err(zbus::Error::from)?;
        
        warn!("InputCapture.ConnectToEIS: setting non-blocking");
        // Set non-blocking mode to prevent deadlocks
        a.set_nonblocking(true).map_err(zbus::Error::from)?;
        b.set_nonblocking(true).map_err(zbus::Error::from)?;
        
        warn!("InputCapture.ConnectToEIS: creating EIS context");
        let ctx = reis::eis::Context::new(a).map_err(zbus::Error::from)?;

        warn!("InputCapture.ConnectToEIS: sending to calloop");
        if let Err(err) =
            self.to_calloop
                .send(InputCaptureDBusToCalloop::NewEisContext {
                    session_id: self.id,
                    ctx,
                    exposed_device_types: BitFlags::all(), // All device types supported
                })
        {
            warn!("error sending NewEisContext to niri: {err:?}");
            return Err(fdo::Error::Failed(format!("Failed to register EIS context: {err:?}")));
        }

        warn!("InputCapture.ConnectToEIS: returning FD");
        Ok(OwnedFd::from(b).into())
    }

    #[zbus(signal)]
    async fn activated(
        ctxt: &SignalEmitter<'_>,
        barrier_id: u32,
        activation_id: u32,
        cursor_position: (f64, f64),
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn deactivated(
        ctxt: &SignalEmitter<'_>,
        barrier_id: u32,
        activation_id: u32,
        cursor_position: (f64, f64),
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn disabled(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn zones_changed(ctxt: &SignalEmitter<'_>, zone_set: u32) -> zbus::Result<()>;
}

impl Drop for Session {
    fn drop(&mut self) {
        // TEMPORARILY DISABLED for debugging
        // If freeze still happens, the issue is NOT in Drop
        warn!("Session Drop: cleanup DISABLED for debugging (session {})", self.id);
    }
}

// Store session interface references globally for testing
static SESSIONS: std::sync::OnceLock<Mutex<HashMap<String, zbus::object_server::InterfaceRef<Session>>>> =
    std::sync::OnceLock::new();

pub fn trigger_activation_test() -> anyhow::Result<()> {
    // Get the first active session
    let sessions_lock = SESSIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let sessions = sessions_lock.lock().unwrap();
    
    if let Some((session_path, iface_ref)) = sessions.iter().next() {
        warn!("TEST: Triggering activation for session {}", session_path);
        
        // Get current activation ID and increment
        // Note: This is synchronous, the actual signal emission happens async via zbus
        let activation_id = std::thread::spawn({
            let iface_ref = iface_ref.clone();
            move || {
                async_io::block_on(async {
                    let session = iface_ref.get().await;
                    let id = session.activation_id.fetch_add(1, Ordering::SeqCst);
                    
                    // Emit the signal with barrier_id=1 (first barrier) and dummy cursor position
                    let signal_ctxt = iface_ref.signal_emitter();
                    match Session::activated(&signal_ctxt, 1, id, (0.0, 0.0)).await {
                        Ok(_) => {
                            warn!("TEST: Activated signal emitted successfully with barrier_id=1, activation_id={}", id);
                            Ok(())
                        }
                        Err(err) => {
                            warn!("TEST: Error emitting Activated signal: {err:?}");
                            Err(anyhow::anyhow!("Signal emission failed: {err:?}"))
                        }
                    }
                })
            }
        }).join().unwrap()?;
        
        Ok(activation_id)
    } else {
        anyhow::bail!("No active InputCapture sessions found")
    }
}

// Thread-local flag to prevent recursive barrier checking
thread_local! {
    static CHECKING_BARRIERS: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

// Check if pointer crossed a barrier and trigger activation
// NOTE: In the GNOME implementation, barrier crossing is detected by the INPUT-LEAP CLIENT
// reading cursor position via EIS, not by the compositor. The compositor only:
// 1. Registers barriers (via DBus)
// 2. Sends cursor position updates (via EIS) 
// 3. Emits Activated/Deactivated signals when Input-Leap calls them
//
// This function is a no-op placeholder. Barrier detection happens client-side.
pub fn check_barrier_crossing(_old_pos: (f64, f64), _new_pos: (f64, f64)) {
    // No-op: Input-Leap checks barriers locally using EIS cursor position
}

// Helper function to check if two line segments intersect
fn line_segments_intersect(
    p1: (f64, f64),
    p2: (f64, f64),
    p3: (f64, f64),
    p4: (f64, f64),
) -> bool {
    // Using the cross product method to check intersection
    fn ccw(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> bool {
        (c.1 - a.1) * (b.0 - a.0) > (b.1 - a.1) * (c.0 - a.0)
    }
    
    ccw(p1, p3, p4) != ccw(p2, p3, p4) && ccw(p1, p2, p3) != ccw(p1, p2, p4)
}
