//! The page as a [`Platform`]: the browser's clocks and timers, the
//! canvas as the viewport, the textarea as the keyboard's field, the
//! document title and clipboard.

use crate::platform::{LocalFuture, Platform, Timeout, Viewport};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

pub struct WebPlatform {
    canvas: web_sys::HtmlCanvasElement,
    textarea: web_sys::HtmlTextAreaElement,
    performance: Option<web_sys::Performance>,
    frame: RefCell<Option<Closure<dyn FnMut()>>>,
    ime_anchor: RefCell<Option<crate::ime::Anchor>>,
}

impl WebPlatform {
    pub fn new(canvas: web_sys::HtmlCanvasElement, textarea: web_sys::HtmlTextAreaElement) -> Self {
        Self {
            canvas,
            textarea,
            performance: web_sys::window().and_then(|w| w.performance()),
            frame: RefCell::new(None),
            ime_anchor: RefCell::new(None),
        }
    }

    pub fn canvas(&self) -> &web_sys::HtmlCanvasElement {
        &self.canvas
    }

    pub fn textarea(&self) -> &web_sys::HtmlTextAreaElement {
        &self.textarea
    }

    /// Milliseconds from `performance.now`, falling back to the wall clock
    /// where there is none.
    pub fn now_ms() -> f64 {
        web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or_else(js_sys::Date::now)
    }
}

impl Platform for WebPlatform {
    fn monotonic_ms(&self) -> f64 {
        self.performance
            .as_ref()
            .map(|p| p.now())
            .unwrap_or_else(js_sys::Date::now)
    }

    fn wall_ms(&self) -> f64 {
        js_sys::Date::now()
    }

    fn random_u32(&self) -> u32 {
        (js_sys::Math::random() * u32::MAX as f64) as u32
    }

    fn spawn(&self, fut: LocalFuture<()>) {
        wasm_bindgen_futures::spawn_local(fut);
    }

    fn set_timeout(&self, delay_ms: f64, cb: Box<dyn FnOnce()>) {
        let closure = Closure::once_into_js(cb);
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                closure.as_ref().unchecked_ref(),
                delay_ms.clamp(0.0, i32::MAX as f64) as i32,
            );
        }
    }

    fn set_interval(&self, every_ms: f64, cb: Box<dyn FnMut()>) {
        let closure = Closure::<dyn FnMut()>::new(cb);
        if let Some(window) = web_sys::window() {
            if window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    every_ms.clamp(0.0, i32::MAX as f64) as i32,
                )
                .is_ok()
            {
                // One per page, for the life of the page.
                closure.forget();
            }
        }
    }

    fn cancellable_timeout(&self, delay_ms: f64, cb: Box<dyn FnOnce()>) -> Timeout {
        let Some(window) = web_sys::window() else { return Timeout::default() };
        let pending = Rc::new(Cell::new(None));
        let fired = Rc::clone(&pending);
        let closure = Closure::once(move || {
            fired.set(None);
            cb();
        });
        match window.set_timeout_with_callback_and_timeout_and_arguments_0(
            closure.as_ref().unchecked_ref(),
            delay_ms.clamp(0.0, i32::MAX as f64) as i32,
        ) {
            Ok(id) => pending.set(Some(id)),
            Err(_) => return Timeout::default(),
        }
        Timeout::new(move || {
            if let Some(id) = pending.take() {
                window.clear_timeout_with_handle(id);
            }
            drop(closure);
        })
    }

    fn set_frame_handler(&self, cb: Box<dyn Fn()>) {
        *self.frame.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || cb()));
    }

    fn request_frame(&self) {
        let frame = self.frame.borrow();
        if let (Some(window), Some(closure)) = (web_sys::window(), frame.as_ref()) {
            let _ = window.request_animation_frame(closure.as_ref().unchecked_ref());
        }
    }

    fn viewport(&self) -> Viewport {
        let rect = self.canvas.get_bounding_client_rect();
        Viewport {
            left: rect.left(),
            top: rect.top(),
            width: rect.width(),
            height: rect.height(),
            dpr: web_sys::window()
                .map(|w| w.device_pixel_ratio())
                .unwrap_or(1.0),
        }
    }

    fn set_backing_size(&self, width: u32, height: u32) -> bool {
        // Setting the backing store's size clears it.
        let cleared = self.canvas.width() != width || self.canvas.height() != height;
        if cleared {
            self.canvas.set_width(width);
            self.canvas.set_height(height);
        }
        cleared
    }

    fn is_mobile(&self) -> bool {
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
            .is_some_and(|body| body.has_attribute("data-mobile"))
    }

    fn set_title(&self, title: &str) {
        if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
            doc.set_title(&format!("{title} — ThinkTerm"));
        }
    }

    fn clipboard_write(&self, text: &str) {
        if let Some(window) = web_sys::window() {
            let clipboard = window.navigator().clipboard();
            let promise = clipboard.write_text(text);
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(err) = wasm_bindgen_futures::JsFuture::from(promise).await {
                    log::warn!("clipboard write refused: {err:?}");
                }
            });
        }
    }

    /// Focus the field the terminal types through -- unless the page says
    /// the soft keyboard is not wanted. On a phone (`body[data-mobile]`)
    /// focusing it raises the keyboard over half the screen, so only the
    /// key bar's keyboard button asks for it (`body[data-keyboard]`, kept
    /// by mobile.svelte.ts); a press or a finished rename must not.
    fn focus_input(&self) {
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            if body.has_attribute("data-mobile") && !body.has_attribute("data-keyboard") {
                return;
            }
        }
        let el: &web_sys::HtmlElement = self.textarea.as_ref();
        let _ = el.focus();
    }

    fn set_cursor(&self, cursor: &str) {
        let el: &web_sys::HtmlElement = self.canvas.as_ref();
        let _ = el.style().set_property("cursor", cursor);
    }

    fn set_ime_anchor(&self, anchor: crate::ime::Anchor) {
        let mut previous = self.ime_anchor.borrow_mut();
        if let Err(err) = crate::ime::update_field(&self.textarea, &mut previous, Some(anchor)) {
            log::warn!("IME anchor: {err:?}");
        }
    }

    fn publish(&self, key: &str, value: &str) {
        let _ = self.canvas.set_attribute(&format!("data-{key}"), value);
    }
}
