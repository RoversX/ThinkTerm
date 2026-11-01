//! What macOS privacy permissions the programs in a pane get (macOS).
//!
//! macOS grants them to whoever it holds responsible for a process: for
//! everything in a pane, the server that spawned the pane's shell, which is
//! responsible for itself (`exec_as_own_tcc_identity` in the server binary).
//! The GUI cannot read another program's permissions, so the server binary
//! answers for itself: run with [`REPORT_OPTION`], it takes on its own
//! identity as a server does, reports, and exits. Asking never makes macOS
//! ask the person anything.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// What the GUI runs the server binary with to ask.
pub const REPORT_OPTION: &str = "--privacy-status";
/// What that run re-executes itself with once it is responsible for
/// itself, and only then reports.
pub const REPORT_AS_SELF_OPTION: &str = "--privacy-status-as-self";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyStatus {
    Granted,
    /// Not granted, where macOS does not tell refused from never asked
    /// (Full Disk Access, screen recording), or where a profile or Screen
    /// Time keeps it from being granted.
    NotGranted,
    /// Refused when macOS asked.
    Denied,
    /// Never asked for: macOS asks the first time a program wants it.
    NotAsked,
    /// Could not be told, such as when the file it is told by is missing.
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyReport {
    pub full_disk_access: PrivacyStatus,
    /// Screen & System Audio Recording.
    pub screen_recording: PrivacyStatus,
    /// Unknown when read from a server binary older than these two.
    #[serde(default)]
    pub microphone: PrivacyStatus,
    #[serde(default)]
    pub camera: PrivacyStatus,
}

/// Which stage of a report this run of the server binary is, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Take on the server's own identity, then report.
    TakeIdentity,
    /// Report as this process stands.
    Report,
}

pub fn requested() -> Option<Request> {
    match std::env::args_os().nth(1)?.to_str()? {
        REPORT_OPTION => Some(Request::TakeIdentity),
        REPORT_AS_SELF_OPTION => Some(Request::Report),
        _ => None,
    }
}

/// Print this process's report as one line of JSON.
pub fn print_report() {
    match serde_json::to_string(&probe()) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("privacy report: {err:#}"),
    }
}

/// The permissions this process gets, as it stands.
pub fn probe() -> PrivacyReport {
    PrivacyReport {
        full_disk_access: full_disk_access(),
        screen_recording: screen_recording(),
        microphone: capture_access(CaptureDevice::Microphone),
        camera: capture_access(CaptureDevice::Camera),
    }
}

/// Full Disk Access has no question to ask, so it is told by opening a file
/// only it lets a program open: the person's own privacy database.
fn full_disk_access() -> PrivacyStatus {
    let Some(home) = std::env::var_os("HOME") else {
        return PrivacyStatus::Unknown;
    };
    full_disk_access_by(
        &Path::new(&home).join("Library/Application Support/com.apple.TCC/TCC.db"),
    )
}

fn full_disk_access_by(witness: &Path) -> PrivacyStatus {
    match std::fs::File::open(witness) {
        Ok(_) => PrivacyStatus::Granted,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            PrivacyStatus::NotGranted
        }
        Err(_) => PrivacyStatus::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn screen_recording() -> PrivacyStatus {
    if objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        PrivacyStatus::Granted
    } else {
        PrivacyStatus::NotGranted
    }
}

#[cfg(not(target_os = "macos"))]
fn screen_recording() -> PrivacyStatus {
    PrivacyStatus::Unknown
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureDevice {
    Microphone,
    Camera,
}

/// Microphone and camera access, as AVFoundation tells it without asking.
#[cfg(target_os = "macos")]
fn capture_access(device: CaptureDevice) -> PrivacyStatus {
    use objc2_av_foundation::{
        AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio, AVMediaTypeVideo,
    };
    // SAFETY: both media types are AVFoundation's own constant strings, set
    // once it is loaded (this binary links it) and never changed, and
    // `authorizationStatusForMediaType:` takes exactly these two. The only
    // misuse Apple documents is another media type, which would throw.
    let status = unsafe {
        let media_type = match device {
            CaptureDevice::Microphone => AVMediaTypeAudio,
            CaptureDevice::Camera => AVMediaTypeVideo,
        };
        match media_type {
            Some(media_type) => AVCaptureDevice::authorizationStatusForMediaType(media_type),
            None => return PrivacyStatus::Unknown,
        }
    };
    match status {
        AVAuthorizationStatus::Authorized => PrivacyStatus::Granted,
        AVAuthorizationStatus::Denied => PrivacyStatus::Denied,
        AVAuthorizationStatus::NotDetermined => PrivacyStatus::NotAsked,
        AVAuthorizationStatus::Restricted => PrivacyStatus::NotGranted,
        _ => PrivacyStatus::Unknown,
    }
}

#[cfg(not(target_os = "macos"))]
fn capture_access(_device: CaptureDevice) -> PrivacyStatus {
    PrivacyStatus::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_witness_is_unknown_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            full_disk_access_by(&dir.path().join("TCC.db")),
            PrivacyStatus::Unknown
        );
    }

    #[test]
    fn a_readable_witness_means_granted() {
        let dir = tempfile::tempdir().unwrap();
        let witness = dir.path().join("TCC.db");
        std::fs::write(&witness, b"").unwrap();
        assert_eq!(full_disk_access_by(&witness), PrivacyStatus::Granted);
    }

    #[test]
    fn the_report_reads_back_as_written() {
        let report = PrivacyReport {
            full_disk_access: PrivacyStatus::NotGranted,
            screen_recording: PrivacyStatus::Granted,
            microphone: PrivacyStatus::NotAsked,
            camera: PrivacyStatus::Denied,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            json,
            r#"{"full_disk_access":"not_granted","screen_recording":"granted","microphone":"not_asked","camera":"denied"}"#
        );
        assert_eq!(serde_json::from_str::<PrivacyReport>(&json).unwrap(), report);
    }

    #[test]
    fn an_older_server_leaves_the_microphone_and_camera_unknown() {
        let report: PrivacyReport =
            serde_json::from_str(r#"{"full_disk_access":"granted","screen_recording":"not_granted"}"#)
                .unwrap();
        assert_eq!(report.microphone, PrivacyStatus::Unknown);
        assert_eq!(report.camera, PrivacyStatus::Unknown);
    }
}
