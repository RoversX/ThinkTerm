use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smithay_client_toolkit::compositor::SurfaceData;
use smithay_client_toolkit::reexports::csd_frame::{DecorationsFrame, FrameClick};
use smithay_client_toolkit::seat::pointer::{
    PointerData, PointerDataExt, PointerEvent, PointerEventKind, PointerHandler,
};
use wayland_client::protocol::wl_pointer::{ButtonState, WlPointer};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Proxy, QueueHandle};
use wezterm_input_types::MousePress;

use crate::wayland::SurfaceUserData;

use super::copy_and_paste::CopyAndPaste;
use super::drag_and_drop::DragAndDrop;
use super::scroll::{PendingScroll, ScrollFrame};
use super::state::WaylandState;
use super::WaylandConnection;

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let mut scroll_frame = ScrollFrame::default();
        for evt in events {
            if let PointerEventKind::Enter { .. } = &evt.kind {
                *self.active_surface_id.borrow_mut() = Some(evt.surface.id());
                scroll_frame = ScrollFrame::default();
            }
            if let Some(serial) = event_serial(evt) {
                *self.last_serial.borrow_mut() = serial;
            }
            if let Some(pending) = self.surface_to_pending.get(&evt.surface.id()) {
                let mut pending = pending.lock().unwrap();
                if pending.queue(&evt.kind, evt.position, &mut scroll_frame) {
                    WaylandConnection::with_window_inner(pending.window_id, move |inner| {
                        inner.dispatch_pending_mouse();
                        Ok(())
                    });
                }
            }
        }
        self.pointer_window_frame(pointer, events);
    }
}

pub(super) struct PointerUserData {
    pub(super) pdata: PointerData,
    pub(super) state: Mutex<PointerState>,
}

impl PointerUserData {
    pub(super) fn new(seat: WlSeat) -> Self {
        Self {
            pdata: PointerData::new(seat),
            state: Default::default(),
        }
    }
}

#[derive(Default)]
pub(super) struct PointerState {
    pub(super) drag_and_drop: DragAndDrop,
}

impl PointerDataExt for PointerUserData {
    fn pointer_data(&self) -> &PointerData {
        &self.pdata
    }
}

#[derive(Clone, Debug)]
pub(super) struct PendingButton {
    pub button: MousePress,
    pub state: ButtonState,
    pub serial: u32,
    pub position: (f64, f64),
}

#[derive(Clone, Debug)]
pub struct PendingMouse {
    window_id: usize,
    pub(super) copy_and_paste: Arc<Mutex<CopyAndPaste>>,
    surface_coords: Option<(f64, f64)>,
    buttons: VecDeque<PendingButton>,
    scroll: Option<PendingScroll>,
    in_window: bool,
}

impl PendingMouse {
    pub(super) fn create(
        window_id: usize,
        copy_and_paste: &Arc<Mutex<CopyAndPaste>>,
    ) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            window_id,
            copy_and_paste: Arc::clone(copy_and_paste),
            buttons: VecDeque::new(),
            scroll: None,
            surface_coords: None,
            in_window: false,
        }))
    }

    fn queue(
        &mut self,
        kind: &PointerEventKind,
        position: (f64, f64),
        scroll_frame: &mut ScrollFrame,
    ) -> bool {
        match *kind {
            PointerEventKind::Enter { .. } => {
                self.in_window = true;
                self.surface_coords = Some(position);
                true
            }
            PointerEventKind::Leave { .. } => {
                let changed = self.in_window;
                self.surface_coords = None;
                self.in_window = false;
                changed
            }
            PointerEventKind::Motion { .. } => {
                let changed = self.surface_coords.is_none();
                self.surface_coords.replace(position);
                changed
            }
            PointerEventKind::Press { button, serial, .. }
            | PointerEventKind::Release { button, serial, .. } => {
                fn linux_button(b: u32) -> Option<MousePress> {
                    // See BTN_LEFT and friends in <linux/input-event-codes.h>
                    match b {
                        0x110 => Some(MousePress::Left),
                        0x111 => Some(MousePress::Right),
                        0x112 => Some(MousePress::Middle),
                        _ => None,
                    }
                }
                let button = match linux_button(button) {
                    Some(button) => button,
                    None => return false,
                };
                let changed = self.buttons.is_empty();
                let state = if matches!(kind, PointerEventKind::Press { .. }) {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                self.buttons.push_back(PendingButton {
                    button,
                    state,
                    serial,
                    position,
                });
                changed
            }
            PointerEventKind::Axis {
                horizontal,
                vertical,
                ..
            } => {
                let changed = self.scroll.is_none();
                let scroll = self.scroll.get_or_insert_with(PendingScroll::default);
                scroll_frame.horizontal.accumulate(
                    &mut scroll.horizontal,
                    horizontal.absolute,
                    horizontal.discrete,
                );
                scroll_frame.vertical.accumulate(
                    &mut scroll.vertical,
                    vertical.absolute,
                    vertical.discrete,
                );
                changed
            }
        }
    }

    pub(super) fn next_button(pending: &Arc<Mutex<Self>>) -> Option<PendingButton> {
        pending.lock().unwrap().buttons.pop_front()
    }

    pub(super) fn coords(pending: &Arc<Mutex<Self>>) -> Option<(f64, f64)> {
        pending.lock().unwrap().surface_coords.take()
    }

    pub(super) fn scroll(pending: &Arc<Mutex<Self>>) -> Option<PendingScroll> {
        pending.lock().unwrap().scroll.take()
    }

    pub(super) fn in_window(pending: &Arc<Mutex<Self>>) -> bool {
        pending.lock().unwrap().in_window
    }
}

fn event_serial(event: &PointerEvent) -> Option<u32> {
    Some(match event.kind {
        PointerEventKind::Enter { serial, .. } => serial,
        PointerEventKind::Leave { serial, .. } => serial,
        PointerEventKind::Press { serial, .. } => serial,
        PointerEventKind::Release { serial, .. } => serial,
        _ => return None,
    })
}

impl WaylandState {
    fn pointer_window_frame(&mut self, pointer: &WlPointer, events: &[PointerEvent]) {
        let windows = self.windows.borrow();

        for evt in events {
            let surface = &evt.surface;
            let Some(window) = surface
                .data::<SurfaceData>()
                .and_then(SurfaceData::parent_surface)
                .and_then(SurfaceUserData::try_from_wl)
                .and_then(|data| windows.get(&data.window_id))
            else {
                continue;
            };
            let mut inner = window.borrow_mut();
            let (x, y) = evt.position;

            match evt.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    inner
                        .window_frame
                        .click_point_moved(Duration::ZERO, &surface.id(), x, y);
                }
                PointerEventKind::Leave { .. } => inner.window_frame.click_point_left(),
                PointerEventKind::Press { button, serial, .. }
                | PointerEventKind::Release { button, serial, .. } => {
                    let pressed = matches!(evt.kind, PointerEventKind::Press { .. });
                    let click = match button {
                        0x110 => FrameClick::Normal,
                        0x111 => FrameClick::Alternate,
                        _ => continue,
                    };
                    if let Some(action) =
                        inner.window_frame.on_click(Duration::ZERO, click, pressed)
                    {
                        inner.frame_action(pointer, serial, action);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_keep_their_own_positions_across_motion_and_leave() {
        let pending = PendingMouse::create(1, &CopyAndPaste::create());
        let mut frame = ScrollFrame::default();
        let press = PointerEventKind::Press {
            time: 1,
            button: 0x112,
            serial: 2,
        };
        let release = PointerEventKind::Release {
            time: 3,
            button: 0x112,
            serial: 3,
        };
        {
            let mut mouse = pending.lock().unwrap();
            mouse.queue(
                &PointerEventKind::Enter { serial: 1 },
                (85.0, 45.0),
                &mut frame,
            );
            mouse.queue(&press, (85.0, 45.0), &mut frame);
            mouse.queue(
                &PointerEventKind::Motion { time: 2 },
                (95.0, 55.0),
                &mut frame,
            );
            mouse.queue(&release, (95.0, 55.0), &mut frame);
            mouse.queue(
                &PointerEventKind::Leave { serial: 4 },
                (95.0, 55.0),
                &mut frame,
            );
        }
        assert!(PendingMouse::coords(&pending).is_none());
        for (state, serial, position) in [
            (ButtonState::Pressed, 2, (85.0, 45.0)),
            (ButtonState::Released, 3, (95.0, 55.0)),
        ] {
            let event = PendingMouse::next_button(&pending).unwrap();
            assert_eq!(event.button, MousePress::Middle);
            assert_eq!(
                (event.state, event.serial, event.position),
                (state, serial, position)
            );
        }
        assert!(PendingMouse::next_button(&pending).is_none());
    }
}
