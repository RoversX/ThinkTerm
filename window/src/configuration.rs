/// Whether this process runs in a remote desktop session, where OpenGL is
/// always the software renderer. Unlike prefer_swrast it reads no
/// configuration, so it is safe while the configuration is being evaluated.
pub fn in_remote_session() -> bool {
    #[cfg(windows)]
    {
        if crate::os::windows::is_running_in_rdp_session() {
            return true;
        }
    }
    false
}

pub(crate) fn prefer_swrast() -> bool {
    #[cfg(windows)]
    {
        if crate::os::windows::is_running_in_rdp_session() {
            // Using OpenGL in RDP has problematic behavior upon
            // disconnect, so we force the use of software rendering.
            log::trace!("Running in an RDP session, use SWRAST");
            return true;
        }
    }
    config::configuration().front_end == config::FrontEndSelection::Software
}
