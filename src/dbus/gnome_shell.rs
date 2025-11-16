//! Minimal `org.gnome.Shell` DBus interface stub.
//!
//! This allows xdg-desktop-portal-gnome to detect niri as a compatible compositor.

use zbus::fdo;
use zbus::interface;

use super::Start;

pub struct GnomeShell;

impl Start for GnomeShell {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        let flags = fdo::RequestNameFlags::AllowReplacement
            | fdo::RequestNameFlags::ReplaceExisting
            | fdo::RequestNameFlags::DoNotQueue;

        conn.object_server().at("/org/gnome/Shell", self)?;
        conn.request_name_with_flags("org.gnome.Shell", flags)?;

        Ok(conn)
    }
}

#[interface(name = "org.gnome.Shell", spawn = false)]
impl GnomeShell {
    /// Returns the GNOME Shell version.
    /// We report a recent version to ensure portal compatibility.
    #[zbus(property)]
    async fn shell_version(&self) -> String {
        "47.0".to_string()
    }

    /// Returns the current mode (typically "user").
    #[zbus(property)]
    async fn mode(&self) -> String {
        "user".to_string()
    }

    /// Eval method - required by some portal implementations.
    /// We don't actually support JavaScript evaluation, so we return an error.
    async fn eval(&self, script: String) -> fdo::Result<(bool, String)> {
        debug!("GNOME Shell Eval called with script (not supported): {}", script);
        Err(fdo::Error::NotSupported(
            "JavaScript evaluation not supported in niri".to_string(),
        ))
    }
}
