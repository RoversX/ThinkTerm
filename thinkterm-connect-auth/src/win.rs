//! The Windows window: what is being allowed, then Windows Hello, or the
//! account's password where Hello is not set up. The session watcher here
//! also serves the host's monitor.
//!
//! This is the one place reverse access uses `unsafe`: Hello, the credential
//! prompt and the window exist only as system calls.

use super::{emit, Intent, Text};
use anyhow::{bail, Context};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::io::{IsTerminal, Read};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;
use windows::core::{factory, w, HRESULT, HSTRING, PCWSTR, PWSTR};
use windows::Security::Credentials::UI::{
    UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_LOGON_FAILURE, ERROR_SUCCESS, HANDLE, HWND, LPARAM,
    LRESULT, S_FALSE, S_OK, WPARAM,
};
use windows::Win32::Security::Credentials::{
    CredPackAuthenticationBufferW, CredUIPromptForWindowsCredentialsW,
    CredUnPackAuthenticationBufferW, CREDUIWIN_GENERIC, CREDUI_INFOW, CRED_PACK_FLAGS,
    CRED_PACK_GENERIC_CREDENTIALS,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, IsWellKnownSid, LogonUserW, LookupAccountSidW, TokenUser,
    WinLocalSystemSid, LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT, PSID, SID_NAME_USE,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::WinRT::IUserConsentVerifierInterop;
use windows::Win32::System::WindowsProgramming::GetComputerNameW;
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOG_BUTTON,
    TASKDIALOG_NOTIFICATIONS, TDF_ALLOW_DIALOG_CANCELLATION, TDM_CLICK_BUTTON, TDM_ENABLE_BUTTON,
    TDN_BUTTON_CLICKED, TDN_CREATED, TD_WARNING_ICON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW,
    SendMessageW, SetForegroundWindow, SetWindowPos, HWND_TOPMOST, IDCANCEL, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, SWP_NOMOVE, SWP_NOSIZE,
    WINDOW_EX_STYLE, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED,
};
use windows_future::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncOperation};
use zeroize::{Zeroize, Zeroizing};

/// Hello's window interface needs this Windows 11 build.
const HELLO_BUILD: u32 = 22000;
const APPROVE: i32 = 100;
const PASSWORD_ATTEMPTS: usize = 3;
/// In UTF-16 units, past anything Windows accepts as a name or a password.
const CREDENTIAL_LIMIT: usize = 1024;

pub(super) fn run() -> anyhow::Result<()> {
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        bail!("a private control pipe is required");
    }
    let mut args = std::env::args().skip(1);
    let mode = args.next().context("mode is required")?;
    if args.next().is_some() {
        bail!("unexpected argument");
    }
    match mode.as_str() {
        "--monitor" => monitor(),
        "--authenticate" => {
            // In a statement of its own, so that this lock ends here: the
            // thread watching the control pipe needs it.
            let intent = Intent::read(&mut std::io::stdin().lock()).context("invalid intent")?;
            authenticate(intent)
        }
        _ => bail!("unknown mode"),
    }
}

fn monitor() -> anyhow::Result<()> {
    fn unavailable() {
        let _ = emit("unavailable", None);
        std::process::exit(0);
    }
    watch_session(unavailable)?;
    emit("ready", None)?;
    std::thread::spawn(|| {
        let _ = std::io::stdin().read(&mut [0]);
        unavailable();
    });
    loop {
        std::thread::sleep(Duration::from_secs(1));
        emit("alive", None)?;
    }
}

fn authenticate(intent: Intent) -> anyhow::Result<()> {
    // Each of these ends the attempt by ending this process: whatever window
    // is up goes with it, and no result is ever reported.
    fn abandon() {
        std::process::exit(2);
    }
    watch_session(abandon)?;
    std::thread::spawn(|| {
        let _ = std::io::stdin().read(&mut [0]);
        abandon();
    });
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        abandon();
    });
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
    let approved = match confirm(&intent)? {
        Confirmation::Declined => false,
        Confirmation::Verified => true,
        Confirmation::NeedsPassword => verify_password(&intent.text)?,
    };
    emit(
        if approved { "approved" } else { "rejected" },
        Some(&intent.id),
    )?;
    Ok(())
}

static SESSION_ENDED: OnceLock<fn()> = OnceLock::new();

/// Calls `ended` on any change of the session, such as a lock, and on suspend
/// or resume. Returns once that is in place.
fn watch_session(ended: fn()) -> anyhow::Result<()> {
    SESSION_ENDED
        .set(ended)
        .ok()
        .context("the session is already watched")?;
    let (ready, started) = mpsc::channel();
    std::thread::Builder::new()
        .name("session-watch".into())
        .spawn(move || {
            let window = session_window();
            let watching = window.is_ok();
            let _ = ready.send(window);
            if watching {
                let mut message = MSG::default();
                while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
                    unsafe { DispatchMessageW(&message) };
                }
            }
        })?;
    started.recv()?.context("watching the desktop session")
}

/// Hidden, and top-level because power broadcasts do not reach a message-only
/// window.
fn session_window() -> windows::core::Result<()> {
    let instance = unsafe { GetModuleHandleW(None) }?.into();
    let class = w!("ThinkTermConnectSession");
    let description = WNDCLASSW {
        lpfnWndProc: Some(session_event),
        hInstance: instance,
        lpszClassName: class,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&description) } == 0 {
        return Err(windows::core::Error::from_thread());
    }
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }?;
    unsafe { WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) }
}

unsafe extern "system" fn session_event(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let changed = match message {
        WM_WTSSESSION_CHANGE => true,
        WM_POWERBROADCAST => matches!(
            wparam.0 as u32,
            PBT_APMSUSPEND | PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND
        ),
        _ => false,
    };
    if changed {
        if let Some(ended) = SESSION_ENDED.get() {
            ended();
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

enum Confirmation {
    Declined,
    Verified,
    NeedsPassword,
}

/// Hello's outcome, which arrives on a thread of its own.
#[derive(Default)]
struct HelloResult {
    answered: AtomicBool,
    verified: AtomicBool,
    declined: AtomicBool,
}

/// The dialog's state, used only on its thread, for as long as it is up.
struct Prompt {
    hello: Cell<bool>,
    reason: HSTRING,
    result: Arc<HelloResult>,
    operation: RefCell<Option<IAsyncOperation<UserConsentVerificationResult>>>,
}

impl Prompt {
    /// Hello's own window opens over the dialog, which stays until it answers.
    fn begin_hello(&self, dialog: HWND) -> windows::core::Result<()> {
        let interop = factory::<UserConsentVerifier, IUserConsentVerifierInterop>()?;
        let operation: IAsyncOperation<UserConsentVerificationResult> =
            unsafe { interop.RequestVerificationForWindowAsync(dialog, &self.reason) }?;
        unsafe {
            SendMessageW(
                dialog,
                TDM_ENABLE_BUTTON.0 as u32,
                Some(WPARAM(APPROVE as usize)),
                Some(LPARAM(0)),
            )
        };
        let result = Arc::clone(&self.result);
        let dialog = dialog.0 as isize;
        operation.SetCompleted(&AsyncOperationCompletedHandler::new(
            move |operation, status| {
                let verified = status == AsyncStatus::Completed
                    && operation.as_ref().is_some_and(|operation| {
                        operation.GetResults() == Ok(UserConsentVerificationResult::Verified)
                    });
                result.verified.store(verified, Ordering::SeqCst);
                result.answered.store(true, Ordering::SeqCst);
                // Back on the dialog's thread, which closes it.
                unsafe {
                    PostMessageW(
                        Some(HWND(dialog as *mut c_void)),
                        TDM_CLICK_BUTTON.0 as u32,
                        WPARAM(IDCANCEL.0 as usize),
                        LPARAM(0),
                    )
                }
            },
        ))?;
        *self.operation.borrow_mut() = Some(operation);
        Ok(())
    }
}

unsafe extern "system" fn dialog_event(
    dialog: HWND,
    notification: TASKDIALOG_NOTIFICATIONS,
    wparam: WPARAM,
    _: LPARAM,
    data: isize,
) -> HRESULT {
    // `data` is the `Prompt` in `confirm`, which outlives the dialog.
    let prompt = unsafe { &*(data as *const Prompt) };
    match notification {
        // Started by a background process, it would otherwise open behind
        // whatever the user is doing.
        TDN_CREATED => unsafe {
            let _ = SetWindowPos(
                dialog,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
            let _ = SetForegroundWindow(dialog);
        },
        TDN_BUTTON_CLICKED if wparam.0 as i32 == APPROVE => {
            if prompt.hello.get() {
                if prompt.begin_hello(dialog).is_ok() {
                    return S_FALSE;
                }
                prompt.hello.set(false);
            }
        }
        TDN_BUTTON_CLICKED if !prompt.result.answered.load(Ordering::SeqCst) => {
            prompt.result.declined.store(true, Ordering::SeqCst);
        }
        _ => {}
    }
    S_OK
}

fn confirm(intent: &Intent) -> anyhow::Result<Confirmation> {
    let text = &intent.text;
    let title = HSTRING::from(text.title.as_str());
    let heading = HSTRING::from(text.heading.as_str());
    let content = HSTRING::from(format!(
        "{}\n\n{}\n{}\n\n{}",
        text.connection, text.terminals, intent.targets, text.detail
    ));
    let approve = HSTRING::from(text.approve.as_str());
    let cancel = HSTRING::from(text.cancel.as_str());
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: APPROVE,
            pszButtonText: PCWSTR(approve.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: IDCANCEL.0,
            pszButtonText: PCWSTR(cancel.as_ptr()),
        },
    ];
    let prompt = Prompt {
        hello: Cell::new(hello_available()),
        reason: HSTRING::from(text.reason.as_str()),
        result: Arc::default(),
        operation: RefCell::new(None),
    };
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        Anonymous1: TASKDIALOGCONFIG_0 {
            pszMainIcon: TD_WARNING_ICON,
        },
        pszMainInstruction: PCWSTR(heading.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        // A stray Enter must not be the thing that allows this.
        nDefaultButton: IDCANCEL.0,
        pfCallback: Some(dialog_event),
        lpCallbackData: &prompt as *const Prompt as isize,
        ..Default::default()
    };
    let mut button = 0;
    unsafe { TaskDialogIndirect(&config, Some(&mut button), None, None) }?;
    let result = &prompt.result;
    Ok(if button == APPROVE && !prompt.hello.get() {
        Confirmation::NeedsPassword
    } else if result.verified.load(Ordering::SeqCst) && !result.declined.load(Ordering::SeqCst) {
        Confirmation::Verified
    } else {
        Confirmation::Declined
    })
}

fn hello_available() -> bool {
    let build = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .and_then(|key| key.get_value::<String, _>("CurrentBuildNumber"))
        .ok()
        .and_then(|build| build.parse::<u32>().ok());
    if !build.is_some_and(|build| build >= HELLO_BUILD) {
        return false;
    }
    // Waiting here would stall the window's thread, so it is not done there.
    std::thread::spawn(|| {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        UserConsentVerifier::CheckAvailabilityAsync()?.join()
    })
    .join()
    .is_ok_and(|availability| {
        matches!(availability, Ok(UserConsentVerifierAvailability::Available))
    })
}

/// This process's account. The password is only ever tried against it, under
/// its own name and on this machine, whatever is typed in the name field.
struct Account {
    /// A `TOKEN_USER`, whose SID points into the same buffer.
    token_user: Vec<usize>,
    name: Vec<u16>,
    domain: Vec<u16>,
}

impl Account {
    fn current() -> anyhow::Result<Self> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
        let token_user = token_user(token);
        unsafe { CloseHandle(token) }?;
        let token_user = token_user?;
        let sid = sid_of(&token_user);
        if unsafe { IsWellKnownSid(sid, WinLocalSystemSid) }.as_bool() {
            bail!("the system account cannot authenticate");
        }
        let (mut name_size, mut domain_size) = (0, 0);
        let mut kind = SID_NAME_USE::default();
        // Asked with no room, this reports how much it needs and fails.
        let _ = unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                None,
                &mut name_size,
                None,
                &mut domain_size,
                &mut kind,
            )
        };
        let mut name = vec![0; name_size as usize];
        let mut domain = vec![0; domain_size as usize];
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_size,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_size,
                &mut kind,
            )
        }?;
        Ok(Self {
            token_user,
            name,
            domain,
        })
    }

    /// Other account types use Hello: this path never sends a password to
    /// another machine.
    fn is_local(&self) -> bool {
        let mut computer = [0; 256];
        let mut size = computer.len() as u32;
        unsafe { GetComputerNameW(Some(PWSTR(computer.as_mut_ptr())), &mut size) }.is_ok()
            && same_name(&computer[..size as usize], &self.domain)
    }

    fn accepts(&self, password: &[u16]) -> bool {
        // An empty password would not be proof of anything.
        if password.first().is_none_or(|first| *first == 0) {
            return false;
        }
        let mut token = HANDLE::default();
        let logged_on = unsafe {
            LogonUserW(
                PCWSTR(self.name.as_ptr()),
                PCWSTR(self.domain.as_ptr()),
                PCWSTR(password.as_ptr()),
                LOGON32_LOGON_INTERACTIVE,
                LOGON32_PROVIDER_DEFAULT,
                &mut token,
            )
        };
        if logged_on.is_err() {
            return false;
        }
        let same = token_user(token).is_ok_and(|logged_on| {
            unsafe { EqualSid(sid_of(&logged_on), sid_of(&self.token_user)) }.is_ok()
        });
        let _ = unsafe { CloseHandle(token) };
        same
    }
}

fn token_user(token: HANDLE) -> anyhow::Result<Vec<usize>> {
    let mut size = 0;
    // Asked with no room, this reports how much it needs and fails.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
    }?;
    Ok(buffer)
}

fn sid_of(token_user: &[usize]) -> PSID {
    // `token_user` is what `GetTokenInformation` filled, aligned for this.
    unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}

/// Windows compares names without regard to case. Either may end in NUL.
fn same_name(first: &[u16], second: &[u16]) -> bool {
    let name = |units: &[u16]| {
        String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .to_lowercase()
    };
    name(first) == name(second)
}

fn verify_password(text: &Text) -> anyhow::Result<bool> {
    let account = Account::current()?;
    if !account.is_local() {
        return Ok(false);
    }
    let mut failure = ERROR_SUCCESS;
    for _ in 0..PASSWORD_ATTEMPTS {
        let Some(password) = ask_password(text, &account, failure.0)? else {
            return Ok(false);
        };
        if account.accepts(&password) {
            return Ok(true);
        }
        failure = ERROR_LOGON_FAILURE;
    }
    Ok(false)
}

/// What the credential prompt returned, gone again with this.
struct PackedCredentials {
    bytes: *mut u8,
    size: usize,
}

impl Drop for PackedCredentials {
    fn drop(&mut self) {
        unsafe {
            std::slice::from_raw_parts_mut(self.bytes, self.size).zeroize();
            CoTaskMemFree(Some(self.bytes.cast()));
        }
    }
}

/// The password typed, NUL-terminated, or None if the user declined.
fn ask_password(
    text: &Text,
    account: &Account,
    failure: u32,
) -> anyhow::Result<Option<Zeroizing<Vec<u16>>>> {
    let caption = HSTRING::from(text.password.as_str());
    let message = HSTRING::from(text.reason.as_str());
    let info = CREDUI_INFOW {
        cbSize: size_of::<CREDUI_INFOW>() as u32,
        pszMessageText: PCWSTR(message.as_ptr()),
        pszCaptionText: PCWSTR(caption.as_ptr()),
        ..Default::default()
    };
    // Shows whose password this is.
    let name = PCWSTR(account.name.as_ptr());
    let mut filled_size = 0;
    let _ = unsafe {
        CredPackAuthenticationBufferW(
            CRED_PACK_GENERIC_CREDENTIALS,
            name,
            w!(""),
            None,
            &mut filled_size,
        )
    };
    let mut filled = vec![0u8; filled_size as usize];
    unsafe {
        CredPackAuthenticationBufferW(
            CRED_PACK_GENERIC_CREDENTIALS,
            name,
            w!(""),
            Some(filled.as_mut_ptr()),
            &mut filled_size,
        )
    }?;
    let mut package = 0;
    let mut bytes: *mut c_void = std::ptr::null_mut();
    let mut size = 0;
    let answer = unsafe {
        CredUIPromptForWindowsCredentialsW(
            Some(&info),
            failure,
            &mut package,
            Some(filled.as_ptr().cast()),
            filled_size,
            &mut bytes,
            &mut size,
            None,
            CREDUIWIN_GENERIC,
        )
    };
    if answer == ERROR_CANCELLED.0 {
        return Ok(None);
    }
    if answer != ERROR_SUCCESS.0 {
        bail!("the credential prompt failed");
    }
    let packed = PackedCredentials {
        bytes: bytes.cast(),
        size: size as usize,
    };
    let mut user = [0; CREDENTIAL_LIMIT];
    let mut domain = [0; CREDENTIAL_LIMIT];
    let mut password = Zeroizing::new(vec![0; CREDENTIAL_LIMIT]);
    let (mut user_size, mut domain_size, mut password_size) = (
        CREDENTIAL_LIMIT as u32,
        CREDENTIAL_LIMIT as u32,
        CREDENTIAL_LIMIT as u32,
    );
    unsafe {
        CredUnPackAuthenticationBufferW(
            CRED_PACK_FLAGS(0),
            packed.bytes.cast(),
            packed.size as u32,
            Some(PWSTR(user.as_mut_ptr())),
            &mut user_size,
            Some(PWSTR(domain.as_mut_ptr())),
            Some(&mut domain_size),
            Some(PWSTR(password.as_mut_ptr())),
            &mut password_size,
        )
    }?;
    Ok(Some(password))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn account_domains_compare_as_windows_compares_them() {
        let mut terminated = wide("DEVBOX");
        terminated.push(0);
        assert!(same_name(&wide("devbox"), &terminated));
        assert!(!same_name(&wide("devbox"), &wide("server-a")));
        assert!(!same_name(&wide("devbox"), &wide("devbox2")));
    }
}
