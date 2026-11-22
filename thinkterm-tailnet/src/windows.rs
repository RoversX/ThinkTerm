//! What the crate needs of Windows itself: whether a process still runs,
//! and whose process is at the other end of the node's socket.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

/// Whether process `pid` has not ended. One that has ended while its
/// number is still held -- by the process that started it, until that one
/// lets go -- has ended all the same. One this user may not open counts as
/// running, as one that refuses a signal does on unix.
pub(crate) fn process_is_running(pid: u32) -> bool {
    use winapi::shared::winerror::{ERROR_ACCESS_DENIED, WAIT_TIMEOUT};
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winnt::SYNCHRONIZE;
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        let err = std::io::Error::last_os_error();
        return err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    }
    let process = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
    unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) == WAIT_TIMEOUT }
}

/// Whether the process at the other end of `stream`, a connection to the
/// node's socket, is this user's: it runs as this user, at medium integrity
/// or above -- or at this process's own, should this one run below that.
/// A sandboxed process of the same user (low integrity, an app container)
/// is turned away, as is one that cannot be looked at. The same check as
/// the peer's uid on unix, where the socket's mode says so too.
#[cfg(feature = "node")]
pub(crate) fn is_this_users(stream: &wezterm_uds::UnixStream) -> bool {
    match peer_is_this_users(stream) {
        Ok(true) => true,
        Ok(false) => {
            log::warn!("tailnet: turned away another user's process, or a sandboxed one");
            false
        }
        Err(err) => {
            log::warn!("tailnet: turned away a process it could not look at: {err:#}");
            false
        }
    }
}

#[cfg(feature = "node")]
fn peer_is_this_users(stream: &wezterm_uds::UnixStream) -> anyhow::Result<bool> {
    use anyhow::Context;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess};
    use winapi::um::winnt::{PROCESS_QUERY_LIMITED_INFORMATION, SECURITY_MANDATORY_MEDIUM_RID};
    let pid = peer_pid(stream)?;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("opening process {pid}"));
    }
    let peer = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
    let (own_user, own_level) = token_identity(unsafe { GetCurrentProcess() } as RawHandle)?;
    let (user, level) =
        token_identity(peer.as_raw_handle()).with_context(|| format!("process {pid}"))?;
    Ok(user == own_user && level >= own_level.min(SECURITY_MANDATORY_MEDIUM_RID))
}

/// The pid of the process at the other end of an AF_UNIX stream: the one
/// that connected, which waits on it for its answer.
#[cfg(feature = "node")]
fn peer_pid(stream: &wezterm_uds::UnixStream) -> anyhow::Result<u32> {
    use std::os::windows::io::AsRawSocket;
    use winapi::um::winsock2::{WSAGetLastError, WSAIoctl, SOCKET};
    /// `_WSAIOR(IOC_VENDOR, 256)` from afunix.h.
    const SIO_AF_UNIX_GETPEERPID: u32 = 0x5800_0100;
    let mut pid: u32 = 0;
    let mut returned: u32 = 0;
    let rc = unsafe {
        WSAIoctl(
            stream.as_raw_socket() as SOCKET,
            SIO_AF_UNIX_GETPEERPID,
            std::ptr::null_mut(),
            0,
            &mut pid as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 || pid == 0 {
        anyhow::bail!(
            "asking for the other process's pid failed: WSA error {}",
            unsafe { WSAGetLastError() }
        );
    }
    Ok(pid)
}

/// Who a process runs as: its user's SID, and its integrity level.
#[cfg(feature = "node")]
fn token_identity(process: RawHandle) -> anyhow::Result<(Vec<u8>, u32)> {
    use anyhow::Context;
    use winapi::um::processthreadsapi::OpenProcessToken;
    use winapi::um::securitybaseapi::{
        GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    };
    use winapi::um::winnt::{
        TokenIntegrityLevel, TokenUser, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TOKEN_USER,
    };

    fn information(
        token: &OwnedHandle,
        class: TOKEN_INFORMATION_CLASS,
    ) -> anyhow::Result<Vec<u64>> {
        let mut len = 0u32;
        unsafe {
            GetTokenInformation(
                token.as_raw_handle() as _,
                class,
                std::ptr::null_mut(),
                0,
                &mut len,
            )
        };
        // u64s, for the alignment of the structures read from it.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle() as _,
                class,
                buf.as_mut_ptr() as *mut _,
                len,
                &mut len,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("reading a process token");
        }
        Ok(buf)
    }

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process as _, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("opening a process token");
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };

    let user = information(&token, TokenUser)?;
    let sid = unsafe { (*(user.as_ptr() as *const TOKEN_USER)).User.Sid };
    let sid_bytes =
        unsafe { std::slice::from_raw_parts(sid as *const u8, GetLengthSid(sid) as usize) }
            .to_vec();

    let label = information(&token, TokenIntegrityLevel)?;
    let level_sid = unsafe {
        (*(label.as_ptr() as *const TOKEN_MANDATORY_LABEL))
            .Label
            .Sid
    };
    let level = unsafe {
        let count = *GetSidSubAuthorityCount(level_sid);
        *GetSidSubAuthority(level_sid, u32::from(count.saturating_sub(1)))
    };
    Ok((sid_bytes, level))
}
