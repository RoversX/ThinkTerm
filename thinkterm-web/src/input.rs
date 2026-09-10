//! DOM events to the app. A hidden textarea owns keyboard focus so the
//! IME composes into it; keys the IME is not handling go to the pane as
//! key events, composed text goes as bytes, and paste arrives as paste.

use crate::app::App;
use crate::keymap::{map_key, DomKey};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    ClipboardEvent, CompositionEvent, Event, HtmlCanvasElement, HtmlTextAreaElement, InputEvent,
    KeyboardEvent, PointerEvent, WheelEvent,
};

pub(crate) fn listen<E: JsCast + 'static>(
    target: &web_sys::EventTarget,
    name: &str,
    handler: impl FnMut(E) + 'static,
) {
    let mut handler = handler;
    let closure = Closure::<dyn FnMut(Event)>::new(move |ev: Event| {
        if let Ok(ev) = ev.dyn_into::<E>() {
            handler(ev);
        }
    });
    target
        .add_event_listener_with_callback(name, closure.as_ref().unchecked_ref())
        .expect("addEventListener");
    // The listeners live as long as the page.
    closure.forget();
}

pub fn install(app: Rc<App>, canvas: &HtmlCanvasElement, textarea: &HtmlTextAreaElement) {
    let window = web_sys::window().expect("window");

    {
        let app = app.clone();
        listen::<KeyboardEvent>(textarea, "keydown", move |ev| {
            let key = ev.key();
            let code = ev.code();
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
        let app = app.clone();
        listen::<CompositionEvent>(textarea, "compositionstart", move |_| app.composing(true));
    }
    {
        let app = app.clone();
        let textarea = textarea.clone();
        listen::<CompositionEvent>(&textarea.clone(), "compositionend", move |ev| {
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
        let app = app.clone();
        let textarea = textarea.clone();
        listen::<InputEvent>(&textarea.clone(), "input", move |ev| {
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
        let app = app.clone();
        listen::<ClipboardEvent>(textarea, "paste", move |ev| {
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
        let app = app.clone();
        listen::<PointerEvent>(canvas, "pointerdown", move |ev| {
            app.pointer(&ev, crate::app::Pointer::Down);
            ev.prevent_default();
        });
    }
    {
        let app = app.clone();
        listen::<PointerEvent>(canvas, "pointermove", move |ev| {
            app.pointer(&ev, crate::app::Pointer::Move);
        });
    }
    {
        let app = app.clone();
        listen::<PointerEvent>(canvas, "pointerup", move |ev| {
            app.pointer(&ev, crate::app::Pointer::Up);
        });
    }
    {
        let app = app.clone();
        listen::<PointerEvent>(canvas, "pointercancel", move |ev| {
            app.pointer(&ev, crate::app::Pointer::Up);
        });
    }
    {
        let app = app.clone();
        listen::<WheelEvent>(canvas, "wheel", move |ev| {
            if app.wheel(&ev) {
                ev.prevent_default();
            }
        });
    }
    {
        listen::<Event>(canvas, "contextmenu", move |ev| ev.prevent_default());
    }
    {
        let app = app.clone();
        listen::<Event>(&window, "focus", move |_| app.focus(true));
    }
    {
        let app = app.clone();
        listen::<Event>(&window, "blur", move |_| app.focus(false));
    }
    {
        let app = app.clone();
        listen::<Event>(&window, "resize", move |_| app.resize());
    }
    {
        let app = app.clone();
        listen::<Event>(&window, "scroll", move |_| app.resize());
    }
    {
        // The canvas's own box, not just the window: CSS can resize it.
        let app = app.clone();
        let closure = Closure::<dyn FnMut(js_sys::Array)>::new(move |_entries: js_sys::Array| app.resize());
        if let Ok(observer) = web_sys::ResizeObserver::new(closure.as_ref().unchecked_ref()) {
            observer.observe(canvas);
            closure.forget();
        }
    }
}
