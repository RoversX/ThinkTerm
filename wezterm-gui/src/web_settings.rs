//! What the Web settings section knows, and how it asks the mux server.
//!
//! The settings window paints synchronously; the mux server answers over an
//! async client. So every answer lands in this cache first and the next
//! paint reads it, the way the update section reads its own cache rather
//! than blocking the UI thread on an HTTP request.
//!
//! One cache for the process: there is one mux connection, and a second
//! settings window would be asking the same server the same questions.

use codec::{WebServerStatus, WebTokenInfo};
use mux::Mux;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wezterm_client::client::Client;
use wezterm_client::domain::ClientDomain;
use window::{Clipboard, Window, WindowOps};

#[derive(Default, Clone)]
pub struct WebState {
    pub status: Option<WebServerStatus>,
    pub tokens: Vec<WebTokenInfo>,
    /// The domain everything else here is about, when it is not this
    /// machine's own session host: `client()` falls back to an attached
    /// remote when there is no host to prefer, and then "Web" means that
    /// server's port and not this computer's. `None` is the ordinary case.
    pub elsewhere: Option<String>,
    /// The last failure, shown until an answer replaces it.
    pub error: Option<String>,
    /// A user's request is out. Controls stay where they are rather than
    /// jumping between "on" and "off" while the answer is in flight.
    pub busy: bool,
    /// The background poll's own gate. Separate from `busy` because they
    /// mean different things: a poll in flight must never be a reason to
    /// drop a click, and a click must never wait for a poll.
    pub polling: bool,
    /// A link was just put on the clipboard. The URL itself is not kept:
    /// it carries a live token, and nothing here needs to read it back.
    pub copied: bool,
    /// A minted link as a QR code (rows of dark modules), shown until the
    /// section is left. The code carries the token, as a copied link does.
    pub qr: Option<Vec<Vec<bool>>>,
    /// Whether anything has been asked yet, so the first paint of the
    /// section can ask without a button.
    pub loaded: bool,
    /// When the last answer landed, so the page can go and ask again.
    pub refreshed_at: Option<Instant>,
    /// A wake-up is already scheduled; a second would double the rate
    /// every time the window repainted for an unrelated reason.
    pub poll_armed: bool,
    /// Bumped by every user action that lands. A poll that was already in
    /// flight when it did carries a picture from before the click, and is
    /// thrown away rather than painted over the result.
    pub generation: u64,
}

/// How often the open page re-reads the server.
///
/// "Is anyone looking at my terminal right now" is the question this page
/// exists to answer, and an answer that only updates when you leave and
/// come back is not one. Two seconds is slow enough to be invisible in
/// cost -- two small RPCs on an already-open connection -- and fast enough
/// that plugging in a phone shows up while you are still holding it.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

static STATE: Mutex<Option<WebState>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut WebState) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(WebState::default))
}

pub fn state() -> WebState {
    with_state(|s| s.clone())
}

/// Forget everything, so reopening the section asks again rather than
/// showing what was true before the server was restarted.
pub fn reset() {
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The mux client the switch is about, if this GUI has one. A GUI with
/// only local panes and no mux server has nothing to ask, and the section
/// says so.
///
/// The server that owns this machine's sessions is the one the page means
/// -- the local session host, when local sessions run in one, the same
/// rule `local_sessions` applies to spawning. Only without a host does it
/// fall back to a remote domain, and it says so when it had to pick among
/// several, because "Web" then reads as that server's port.
fn client() -> Option<Client> {
    let mut clients = Vec::new();
    for domain in Mux::get().iter_domains() {
        if domain.downcast_ref::<ClientDomain>().is_none() {
            continue;
        }
        match ClientDomain::get_client_inner_for_domain(domain.domain_id()) {
            Ok(inner) => clients.push((
                domain.domain_id(),
                domain.domain_name().to_string(),
                inner.client.clone(),
            )),
            Err(err) => log::debug!("web settings: domain has no live client: {err:#}"),
        }
    }
    if let Some((_, _, client)) = clients
        .iter()
        .find(|(id, _, _)| crate::local_sessions::is_host_domain_id(*id))
    {
        with_state(|s| s.elsewhere = None);
        return Some(client.clone());
    }
    if clients.len() > 1 {
        log::warn!(
            "web settings: no local session host; the Web section is about {} (attached: {})",
            clients[0].1,
            clients
                .iter()
                .map(|(_, name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let chosen = clients.into_iter().next();
    // Which server was picked is recorded rather than only logged. The
    // warning above fires at two attached domains and up, so the case that
    // needed saying most -- a single remote, on a platform with no session
    // host of its own, silently becoming what "Allow browser access" opens
    // a port on -- said nothing at all. The section reads this to name the
    // machine instead of claiming to be about this one.
    with_state(|s| s.elsewhere = chosen.as_ref().map(|(_, name, _)| name.clone()));
    chosen.map(|(_, _, client)| client)
}

/// Which gate an answer releases. They are separate on purpose: a poll
/// finishing must not release a user's action that is still out (a
/// "Copy link" could then be clicked twice and mint two tokens), and an
/// action finishing must not release a poll.
#[derive(Clone, Copy)]
enum Gate {
    Poll,
    Action,
}

fn finish(window: &Window, error: Option<String>, gate: Gate) {
    with_state(|s| {
        match gate {
            Gate::Poll => s.polling = false,
            Gate::Action => {
                s.busy = false;
                s.generation += 1;
            }
        }
        s.loaded = true;
        s.refreshed_at = Some(Instant::now());
        // A quiet poll does not clear what an action just said went wrong:
        // "the listener refused to start because ..." has to outlive the
        // next two-second tick, or it is never read.
        match (gate, error) {
            (Gate::Poll, None) => {}
            (_, error) => s.error = error,
        }
    });
    window.invalidate();
}

/// Whether enough time has passed to ask the server again.
pub fn poll_is_due() -> bool {
    with_state(|s| {
        !s.busy
            && !s.polling
            && s.refreshed_at
                .is_none_or(|at| at.elapsed() >= POLL_INTERVAL)
    })
}

/// Come back and repaint once the poll is due.
///
/// The page has nothing else to make it repaint -- a browser connecting is
/// news from the server, not from this window -- so the timer is what turns
/// a static page into a live one. Armed from the Web section's paint and
/// nowhere else, so leaving the section stops it after one last tick.
pub fn arm_poll(window: Window) {
    // Wait only for the time still owed. Sleeping a whole interval here and
    // then finding the poll due meant the pair took two intervals: a "2
    // second" refresh that actually ran every four, with every other wake
    // painting an identical frame.
    //
    // Nothing is armed while a request is out: `refreshed_at` is then the
    // *previous* answer's time, the wait owed reads as zero, and the window
    // would wake, paint, find the poll not due, and arm again at display
    // rate for as long as the request took. `finish` repaints when the
    // answer lands, and that paint arms the next wait.
    let due_in = with_state(|s| {
        if s.busy || s.polling || std::mem::replace(&mut s.poll_armed, true) {
            return None;
        }
        Some(
            s.refreshed_at
                .map(|at| POLL_INTERVAL.saturating_sub(at.elapsed()))
                .unwrap_or(POLL_INTERVAL),
        )
    });
    let Some(due_in) = due_in else {
        return;
    };
    promise::spawn::spawn_into_main_thread(async move {
        smol::Timer::after(due_in).await;
        with_state(|s| s.poll_armed = false);
        window.invalidate();
    })
    .detach();
}

/// Read the listener's state and the token list.
///
/// Both in one task: two answers arriving separately would paint a frame
/// where the listener is on and the tokens are still the old ones.
pub fn refresh(window: Window) {
    // The poll's gate, not the user's: a click that lands while a refresh
    // is in flight still goes through, and a refresh that never answers
    // does not lock the page.
    if with_state(|s| std::mem::replace(&mut s.polling, true)) {
        return;
    }
    let Some(client) = client() else {
        with_state(|s| {
            s.status = None;
            s.tokens.clear();
        });
        finish(&window, Some(crate::i18n::tr("settings-web-no-server")), Gate::Poll);
        return;
    };
    let asked_at = with_state(|s| s.generation);
    promise::spawn::spawn_into_main_thread(async move {
        let outcome = async {
            let status = client.get_web_server_status().await?;
            let tokens = client.web_token_list().await?;
            Ok::<_, anyhow::Error>((status, tokens.tokens))
        }
        .await;
        // An action landed while this was out. Its answer is the newer
        // truth and this one would paint the switch back the way it was;
        // ask again instead, now that the poll gate is free.
        if with_state(|s| s.generation) != asked_at {
            finish(&window, None, Gate::Poll);
            refresh(window);
            return;
        }
        match outcome {
            Ok((status, tokens)) => {
                log::debug!(
                    "web settings: listening={:?} configured={:?} tokens={}",
                    status.listening,
                    status.configured,
                    tokens.len()
                );
                with_state(|s| {
                    s.status = Some(status);
                    s.tokens = tokens;
                });
                finish(&window, None, Gate::Poll);
            }
            Err(err) => finish(&window, Some(format!("{err:#}")), Gate::Poll),
        }
    })
    .detach();
}

/// Turn the listener on or off, then re-read: the answer to `SetWebServer`
/// is the new status, but the token list can change with it (a listener
/// that has just come up configures the token store).
pub fn set_enabled(window: Window, enabled: bool, bind_address: Option<String>) {
    if with_state(|s| std::mem::replace(&mut s.busy, true)) {
        return;
    }
    let Some(client) = client() else {
        finish(&window, Some(crate::i18n::tr("settings-web-no-server")), Gate::Action);
        return;
    };
    promise::spawn::spawn_into_main_thread(async move {
        match client
            .set_web_server(codec::SetWebServer {
                enabled,
                bind_address,
            })
            .await
        {
            Ok(status) => {
                with_state(|s| s.status = Some(status));
                finish(&window, None, Gate::Action);
                refresh(window);
            }
            Err(err) => finish(&window, Some(format!("{err:#}")), Gate::Action),
        }
    })
    .detach();
}

/// Turn the listener off and on again at another address: the settings
/// switch flipped where it listens while it was up.
pub fn restart(window: Window, bind_address: String) {
    if with_state(|s| std::mem::replace(&mut s.busy, true)) {
        return;
    }
    let Some(client) = client() else {
        finish(&window, Some(crate::i18n::tr("settings-web-no-server")), Gate::Action);
        return;
    };
    promise::spawn::spawn_into_main_thread(async move {
        let off = client
            .set_web_server(codec::SetWebServer { enabled: false, bind_address: None })
            .await;
        let outcome = match off {
            Ok(_) => client
                .set_web_server(codec::SetWebServer { enabled: true, bind_address: Some(bind_address) })
                .await,
            Err(err) => Err(err),
        };
        match outcome {
            Ok(status) => {
                with_state(|s| {
                    s.status = Some(status);
                    s.qr = None;
                });
                finish(&window, None, Gate::Action);
                refresh(window);
            }
            Err(err) => finish(&window, Some(format!("{err:#}")), Gate::Action),
        }
    })
    .detach();
}

/// Mint a link and show it as a QR code, for a phone to scan. Prefers a
/// URL another device can use; a loopback-only listener gets a remark.
pub fn show_qr(window: Window, ttl_secs: Option<u64>) {
    if with_state(|s| std::mem::replace(&mut s.busy, true)) {
        return;
    }
    let Some(client) = client() else {
        finish(&window, Some(crate::i18n::tr("settings-web-no-server")), Gate::Action);
        return;
    };
    promise::spawn::spawn_into_main_thread(async move {
        match client.web_token_mint(codec::WebTokenMint { label: None, ttl_secs }).await {
            Ok(minted) => {
                let reachable = minted.urls.iter().find(|url| {
                    url.split("://")
                        .nth(1)
                        .and_then(|rest| rest.split('/').next())
                        .and_then(config::split_authority)
                        .is_some_and(|(host, _)| !config::is_loopback_host(&host))
                });
                match reachable.or(minted.urls.first()) {
                    Some(url) => {
                        let code = qrcode::QrCode::new(url.as_bytes());
                        match code {
                            Ok(code) => {
                                let width = code.width();
                                let colors = code.to_colors();
                                let rows: Vec<Vec<bool>> = (0..width)
                                    .map(|y| (0..width).map(|x| colors[y * width + x] == qrcode::Color::Dark).collect())
                                    .collect();
                                let remark = reachable.is_none().then(|| crate::i18n::tr("settings-web-qr-loopback"));
                                with_state(|s| s.qr = Some(rows));
                                finish(&window, remark, Gate::Action);
                                refresh(window);
                            }
                            Err(err) => finish(&window, Some(format!("{err}")), Gate::Action),
                        }
                    }
                    None => {
                        let _ = client
                            .web_token_revoke(codec::WebTokenRevoke { id: Some(minted.id.clone()) })
                            .await;
                        finish(&window, Some(crate::i18n::tr("settings-web-no-listener")), Gate::Action);
                    }
                }
            }
            Err(err) => finish(&window, Some(format!("{err:#}")), Gate::Action),
        }
    })
    .detach();
}

pub fn hide_qr() {
    with_state(|s| s.qr = None);
}

/// Mint a link and put it on the clipboard.
///
/// Straight to the clipboard rather than onto the screen: a token on screen
/// is a token in every screenshot and screen share of this window.
///
/// `ttl_secs` is `None` for a link that lasts until it is revoked; the
/// section's dropdown chooses it and the server takes it at face value.
pub fn mint(window: Window, ttl_secs: Option<u64>) {
    if with_state(|s| std::mem::replace(&mut s.busy, true)) {
        return;
    }
    let Some(client) = client() else {
        finish(&window, Some(crate::i18n::tr("settings-web-no-server")), Gate::Action);
        return;
    };
    promise::spawn::spawn_into_main_thread(async move {
        match client
            .web_token_mint(codec::WebTokenMint {
                // Deliberately unnamed. There is nothing true to call it
                // yet -- the browser that will use it has not connected --
                // and a constant like "Settings" made every row in the list
                // identical. The row names itself from the device once one
                // turns up.
                label: None,
                ttl_secs,
            })
            .await
        {
            Ok(minted) => match minted.urls.first() {
                Some(url) => {
                    window.set_clipboard(Clipboard::Clipboard, url.clone());
                    with_state(|s| s.copied = true);
                    finish(&window, None, Gate::Action);
                    // The flash clears itself. The settings window schedules
                    // its other copy-button timers when the click happens,
                    // but this flag is set when the *answer* lands -- often
                    // after that timer has already fired, which left the
                    // button reading "Copied" on every later visit.
                    clear_copied_soon(window.clone());
                    refresh(window);
                }
                None => {
                    // No listener, so no URL. Leaving the token alive would
                    // be a credential nobody can see and nobody revokes.
                    let _ = client
                        .web_token_revoke(codec::WebTokenRevoke {
                            id: Some(minted.id.clone()),
                        })
                        .await;
                    finish(&window, Some(crate::i18n::tr("settings-web-no-listener")), Gate::Action);
                }
            },
            Err(err) => finish(&window, Some(format!("{err:#}")), Gate::Action),
        }
    })
    .detach();
}

/// Revoke one token, or every token when `id` is None.
pub fn revoke(window: Window, id: Option<String>) {
    if with_state(|s| std::mem::replace(&mut s.busy, true)) {
        return;
    }
    let Some(client) = client() else {
        finish(&window, Some(crate::i18n::tr("settings-web-no-server")), Gate::Action);
        return;
    };
    promise::spawn::spawn_into_main_thread(async move {
        match client.web_token_revoke(codec::WebTokenRevoke { id }).await {
            Ok(_) => {
                finish(&window, None, Gate::Action);
                refresh(window);
            }
            Err(err) => finish(&window, Some(format!("{err:#}")), Gate::Action),
        }
    })
    .detach();
}

/// Clear the "copied" flash. The window schedules this the way it does for
/// the other copy buttons.
pub fn clear_copied() {
    with_state(|s| s.copied = false);
}

/// How long "Copied" stays up. The same beat as the settings window's other
/// copy buttons.
const COPIED_FLASH: Duration = Duration::from_millis(1450);

/// Take the flash down again, counted from when it went up.
fn clear_copied_soon(window: Window) {
    promise::spawn::spawn_into_main_thread(async move {
        smol::Timer::after(COPIED_FLASH).await;
        clear_copied();
        window.invalidate();
    })
    .detach();
}
