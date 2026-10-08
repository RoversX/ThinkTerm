//! DOM events to the app. A hidden textarea owns keyboard focus so the
//! IME composes into it; keys the IME is not handling go to the pane as
//! key events, composed text goes as bytes, and paste arrives as paste.

use crate::keymap::{map_key, DomKey};
use crate::page::WebApp;
use crate::platform::{PointerInput, WheelDelta, WheelInput};
use wezterm_term::KeyModifiers;
use std::cell::Cell;
use std::rc::Rc;
use web_sys::{
    ClipboardEvent, CompositionEvent, Event, HtmlCanvasElement, HtmlTextAreaElement, InputEvent,
    KeyboardEvent, PointerEvent, WheelEvent,
};

fn mouse_modifiers(ev: &web_sys::MouseEvent) -> KeyModifiers {
    let mut m = KeyModifiers::NONE;
    if ev.shift_key() {
        m |= KeyModifiers::SHIFT;
    }
    if ev.ctrl_key() {
        m |= KeyModifiers::CTRL;
    }
    if ev.alt_key() {
        m |= KeyModifiers::ALT;
    }
    if ev.meta_key() {
        m |= KeyModifiers::SUPER;
    }
    m
}

/// How long after an input method put its text in a Return marked 229 is
/// still its own, in milliseconds: WebKit sends the Return that ended it
/// then (as `PluginPanel.svelte` knows too).
const ENDED_COMPOSING_MS: f64 = 500.0;

fn pointer_input(ev: &PointerEvent) -> PointerInput {
    PointerInput {
        x: ev.client_x() as f64,
        y: ev.client_y() as f64,
        // Pointer Events give -1 on a move or a cancel: no button of its
        // own. 255 says so; the App fills in the one held.
        button: if ev.button() < 0 { 255 } else { ev.button().min(255) as u8 },
        buttons_down: ev.buttons() != 0,
        mods: mouse_modifiers(ev),
    }
}

fn wheel_input(ev: &WheelEvent) -> WheelInput {
    let delta = match ev.delta_mode() {
        WheelEvent::DOM_DELTA_LINE => WheelDelta::Lines(ev.delta_y()),
        WheelEvent::DOM_DELTA_PAGE => WheelDelta::Pages(ev.delta_y()),
        _ => WheelDelta::Pixels(ev.delta_y()),
    };
    WheelInput {
        x: ev.client_x() as f64,
        y: ev.client_y() as f64,
        delta,
        mods: mouse_modifiers(ev),
        ctrl: ev.ctrl_key(),
        trusted: ev.is_trusted(),
    }
}

/// Every listener holds the App weakly, and is held by the platform rather
/// than leaked: the page can close a machine's terminal (machines.svelte.ts)
/// and stay open, and then `WebPlatform::release` takes them all down.
pub fn install(app: Rc<WebApp>, canvas: &HtmlCanvasElement, textarea: &HtmlTextAreaElement) {
    let platform = Rc::clone(&app.platform);
    if let Some(document) = web_sys::window().and_then(|window| window.document()) {
        app.visibility_changed(!document.hidden());
        let app = Rc::downgrade(&app);
        let target = document.clone();
        platform.listen::<Event>(&target, "visibilitychange", move |_| {
            if let Some(app) = app.upgrade() {
                app.visibility_changed(!document.hidden());
            }
        });
    }
    let window = web_sys::window().expect("window");
    // When an input method last put its text in.
    let composed = Rc::new(Cell::new(f64::NEG_INFINITY));
    // Only WebKit (Safari, and every browser on iOS) ends a composition
    // before the key that ended it. Elsewhere that key comes first, marked
    // composing, and a Return or Backspace just after is the user's own --
    // on an Android keyboard, marked 229 like every other key.
    let webkit = js_sys::Reflect::get(&window.navigator(), &"vendor".into())
        .ok()
        .and_then(|vendor| vendor.as_string())
        .is_some_and(|vendor| vendor.starts_with("Apple"));

    {
        let app = Rc::downgrade(&app);
        let composed = Rc::clone(&composed);
        platform.listen::<KeyboardEvent>(textarea, "keydown", move |ev| {
            let Some(app) = app.upgrade() else { return };
            let key = ev.key();
            let code = ev.code();
            // WebKit ends a composition before the key that ended it
            // arrives, unmarked but for keyCode 229: the Return that picked
            // a candidate is not the pane's.
            if webkit
                && ev.key_code() == 229
                && key == "Enter"
                && ev.time_stamp() - composed.get() < ENDED_COMPOSING_MS
            {
                composed.set(f64::NEG_INFINITY);
                return;
            }
            let dom = DomKey {
                key: &key,
                code: &code,
                ctrl: ev.ctrl_key(),
                alt: ev.alt_key(),
                shift: ev.shift_key(),
                meta: ev.meta_key(),
                // Chrome on macOS names the key the IME took (`h`, not
                // `Process`) and only marks it by keyCode 229; a Pinyin
                // `htop` would otherwise send `h` and then `htop`.
                // Only a printable key: an IME that is on but idle (an
                // Android keyboard, a CJK IME in ASCII mode) marks every
                // key 229, and Backspace or Enter must still get through.
                composing: ev.is_composing() || (ev.key_code() == 229 && key.chars().count() == 1),
            };
            match map_key(&dom) {
                Some((key, mods)) => {
                    if app.key_down(key, mods, ev.shift_key()) {
                        ev.prevent_default();
                    }
                }
                None => {}
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<CompositionEvent>(textarea, "compositionstart", move |_| {
            if let Some(app) = app.upgrade() {
                app.composing(true);
            }
        });
    }
    {
        // What the input method has so far ("ni hao" before it is 你好),
        // drawn at the cursor: the field it is typed into is out of sight.
        let app = Rc::downgrade(&app);
        platform.listen::<CompositionEvent>(textarea, "compositionupdate", move |ev| {
            if let Some(app) = app.upgrade() {
                app.preedit(&ev.data().unwrap_or_default());
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        let textarea = textarea.clone();
        let composed = Rc::clone(&composed);
        platform.listen::<CompositionEvent>(&textarea.clone(), "compositionend", move |ev| {
            let Some(app) = app.upgrade() else { return };
            composed.set(ev.time_stamp());
            app.composing(false);
            if let Some(text) = ev.data() {
                if !text.is_empty() {
                    app.text(&text);
                }
            }
            textarea.set_value("");
        });
    }
    {
        let app = Rc::downgrade(&app);
        let textarea = textarea.clone();
        platform.listen::<InputEvent>(&textarea.clone(), "input", move |ev| {
            let Some(app) = app.upgrade() else { return };
            // Composition text arrives through compositionend; anything
            // else typed straight into the textarea (a virtual keyboard,
            // autocorrect) is sent as bytes.
            if ev.is_composing() {
                return;
            }
            let value = textarea.value();
            if !value.is_empty() {
                app.text(&value);
                textarea.set_value("");
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<ClipboardEvent>(textarea, "paste", move |ev| {
            let Some(app) = app.upgrade() else { return };
            ev.prevent_default();
            if let Some(data) = ev.clipboard_data() {
                if let Ok(text) = data.get_data("text/plain") {
                    if !text.is_empty() {
                        app.paste(&text);
                    }
                }
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        let canvas = canvas.clone();
        platform.listen::<PointerEvent>(&canvas.clone(), "pointerdown", move |ev| {
            let Some(app) = app.upgrade() else { return };
            let _ = canvas.set_pointer_capture(ev.pointer_id());
            app.pointer(&pointer_input(&ev), crate::app::Pointer::Down);
            ev.prevent_default();
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<PointerEvent>(canvas, "pointermove", move |ev| {
            if let Some(app) = app.upgrade() {
                app.pointer(&pointer_input(&ev), crate::app::Pointer::Move);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<PointerEvent>(canvas, "pointerup", move |ev| {
            if let Some(app) = app.upgrade() {
                app.pointer(&pointer_input(&ev), crate::app::Pointer::Up);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<PointerEvent>(canvas, "pointercancel", move |ev| {
            if let Some(app) = app.upgrade() {
                app.pointer(&pointer_input(&ev), crate::app::Pointer::Up);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<WheelEvent>(canvas, "wheel", move |ev| {
            if app.upgrade().is_some_and(|app| app.wheel(&wheel_input(&ev))) {
                ev.prevent_default();
            }
        });
    }
    {
        platform.listen::<Event>(canvas, "contextmenu", move |ev| ev.prevent_default());
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<Event>(&window, "focus", move |_| {
            if let Some(app) = app.upgrade() {
                app.focus(true);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<Event>(&window, "blur", move |_| {
            if let Some(app) = app.upgrade() {
                app.focus(false);
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<Event>(&window, "resize", move |_| {
            if let Some(app) = app.upgrade() {
                app.resize();
            }
        });
    }
    {
        let app = Rc::downgrade(&app);
        platform.listen::<Event>(&window, "scroll", move |_| {
            if let Some(app) = app.upgrade() {
                app.resize();
            }
        });
    }
    {
        // The canvas's own box, not just the window: CSS can resize it.
        let app = Rc::downgrade(&app);
        platform.observe_resize(canvas, move || {
            if let Some(app) = app.upgrade() {
                app.resize();
            }
        });
    }
}
