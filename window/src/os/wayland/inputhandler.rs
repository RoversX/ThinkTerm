//! Implements zwp_text_input_v3 for handling IME
use std::collections::HashMap;
use std::sync::Mutex;

use smithay_client_toolkit::globals::GlobalData;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{BindError, GlobalList};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    Event as TextInputEvent, ZwpTextInputV3,
};
use wezterm_input_types::{KeyCode, KeyEvent, KeyboardLedStatus, Modifiers};

use crate::{DeadKeyStatus, WindowEvent};

use super::state::WaylandState;
use super::window::SurfaceUserData;

#[derive(Default)]
struct PendingState {
    surface: Option<WlSurface>,
    pre_edit: Option<String>,
    commit: Option<String>,
}

pub(super) struct TextInputState {
    text_input_manager: ZwpTextInputManagerV3,
    input_by_seat: HashMap<ObjectId, ZwpTextInputV3>,
}

impl TextInputState {
    pub(super) fn bind(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WaylandState>,
    ) -> Result<Self, BindError> {
        let text_input_manager = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self {
            text_input_manager,
            input_by_seat: HashMap::new(),
        })
    }

    pub(super) fn get_text_input_for_surface(&self, surface: &WlSurface) -> Option<ZwpTextInputV3> {
        self.input_by_seat
            .values()
            .find(|input| {
                input.data::<TextInputData>().is_some_and(|data| {
                    data.pending.lock().unwrap().surface.as_ref() == Some(surface)
                })
            })
            .cloned()
    }

    pub(super) fn advise_seat(&mut self, seat: &WlSeat, qh: &QueueHandle<WaylandState>) {
        let manager = &self.text_input_manager;
        self.input_by_seat
            .entry(seat.id())
            .or_insert_with(|| manager.get_text_input(seat, qh, TextInputData::default()));
    }

    /// Workaround for <https://gitlab.gnome.org/GNOME/gnome-shell/-/issues/4776>
    /// If we make sure to disable things before we close the app,
    /// mutter is less likely to get in a bad state
    pub fn shutdown(&self) {
        for input in self.input_by_seat.values() {
            input.disable();
            input.commit();
        }
    }
}

#[derive(Default)]
pub(super) struct TextInputData {
    pending: Mutex<PendingState>,
}

impl Dispatch<ZwpTextInputManagerV3, GlobalData, WaylandState> for TextInputState {
    fn event(
        _state: &mut WaylandState,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<WaylandState>,
    ) {
        // No events from ZwpTextInputMangerV3
        unreachable!();
    }
}

impl Dispatch<ZwpTextInputV3, TextInputData, WaylandState> for TextInputState {
    fn event(
        state: &mut WaylandState,
        input: &ZwpTextInputV3,
        event: <ZwpTextInputV3 as Proxy>::Event,
        data: &TextInputData,
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<WaylandState>,
    ) {
        log::trace!(
            "ZwpTextInputEvent: {}",
            ZwpTextInputV3::interface().events[usize::from(event.opcode())].name
        );
        match event {
            // enter establishes a new text-input focus interval. Requests
            // before it may have been ignored, even with keyboard focus.
            TextInputEvent::Enter { surface } => {
                *data.pending.lock().unwrap() = PendingState {
                    surface: Some(surface.clone()),
                    ..PendingState::default()
                };
                input.enable();
                if let Some(surface_data) = SurfaceUserData::try_from_wl(&surface) {
                    if let Some(window) = state.window_by_id(surface_data.window_id) {
                        window
                            .borrow()
                            .send_text_cursor_rectangle(input, surface_data);
                    }
                }
                input.commit();
            }
            TextInputEvent::Leave { surface } => {
                *data.pending.lock().unwrap() = PendingState::default();
                state.dispatch_text_input_event(
                    &surface,
                    WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None),
                );
            }
            TextInputEvent::PreeditString {
                text,
                cursor_begin: _,
                cursor_end: _,
            } => {
                data.pending.lock().unwrap().pre_edit = text;
            }
            TextInputEvent::CommitString { text } => {
                data.pending.lock().unwrap().commit = text;
            }
            TextInputEvent::Done { serial } => {
                *state.last_serial.borrow_mut() = serial;
                let (surface, commit, pre_edit) = {
                    let mut pending = data.pending.lock().unwrap();
                    (
                        pending.surface.clone(),
                        pending.commit.take(),
                        pending.pre_edit.take(),
                    )
                };
                let Some(surface) = surface else { return };
                if let Some(text) = commit {
                    state.dispatch_text_input_event(
                        &surface,
                        WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None),
                    );
                    state.dispatch_text_input_event(&surface, WindowEvent::KeyEvent(KeyEvent {
                        key: KeyCode::composed(&text),
                        modifiers: Modifiers::NONE,
                        leds: KeyboardLedStatus::empty(),
                        repeat_count: 1,
                        key_is_down: true,
                        raw: None,
                    }));
                }
                let status = if let Some(text) = pre_edit {
                    DeadKeyStatus::Composing(text)
                } else {
                    DeadKeyStatus::None
                };
                state.dispatch_text_input_event(&surface, WindowEvent::AdviseDeadKeyStatus(status));
            }
            _ => {}
        }
    }
}

impl WaylandState {
    fn dispatch_text_input_event(&self, surface: &WlSurface, event: WindowEvent) {
        if let Some(win) = SurfaceUserData::try_from_wl(surface)
            .and_then(|surface| self.window_by_id(surface.window_id))
        {
            win.borrow_mut().events.dispatch(event);
        }
    }
}

impl Drop for WaylandState {
    fn drop(&mut self) {
        if let Some(text_input) = self.text_input.as_mut() {
            text_input.shutdown();
        }
    }
}
