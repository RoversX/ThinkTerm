use crate::screen::Screens;
use crate::{Appearance, Connection, GeometryOrigin, RequestedWindowGeometry, ResolvedGeometry};
use anyhow::Result as Fallible;
use config::keyassignment::KeyAssignment;
use config::DimensionContext;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;

thread_local! {
    static CONN: RefCell<Option<Rc<Connection>>> = RefCell::new(None);
}

fn nop_event_handler(_event: ApplicationEvent) {}

static EVENT_HANDLER: Mutex<fn(ApplicationEvent)> = Mutex::new(nop_event_handler);

/// The appearance the application has been asked to present, overriding
/// whatever the system is set to. `None` follows the system.
///
/// A `Mutex` rather than a `thread_local!` on purpose: it is written from
/// wherever the setting is applied and read from the platform's UI thread,
/// and those are not always the same thread.
static PREFERRED_APPEARANCE: Mutex<Option<Appearance>> = Mutex::new(None);

/// The appearance the application has been asked to present, if it has been
/// asked for one at all.
pub fn preferred_appearance() -> Option<Appearance> {
    *PREFERRED_APPEARANCE.lock().unwrap()
}

/// Record the preferred appearance, reporting whether it actually changed.
///
/// Pure, over the slot rather than the static, so that the tests below never
/// touch process-wide state: `cargo test` runs them in parallel threads of one
/// process and they would otherwise clobber each other.
fn store_appearance(slot: &mut Option<Appearance>, appearance: Option<Appearance>) -> bool {
    if *slot == appearance {
        return false;
    }
    *slot = appearance;
    true
}

pub fn shutdown() {
    CONN.with(|m| drop(m.borrow_mut().take()));
}

#[derive(Debug)]
pub enum ApplicationEvent {
    /// The system wants to open a command in the terminal
    OpenCommandScript(String),
    PerformKeyAssignment(KeyAssignment),
}

pub trait ConnectionOps {
    fn get() -> Option<Rc<Connection>> {
        let mut res = None;
        CONN.with(|m| {
            if let Some(mux) = &*m.borrow() {
                res = Some(Rc::clone(mux));
            }
        });
        res
    }

    fn name(&self) -> String;

    fn set_event_handler(&self, func: fn(ApplicationEvent)) {
        let mut handler = EVENT_HANDLER.lock().unwrap();
        *handler = func;
    }

    fn dispatch_app_event(&self, event: ApplicationEvent) {
        let func = EVENT_HANDLER.lock().unwrap();
        func(event);
    }

    fn default_dpi(&self) -> f64 {
        crate::DEFAULT_DPI
    }

    fn init() -> Fallible<Rc<Connection>> {
        let conn = Rc::new(Connection::create_new()?);
        CONN.with(|m| *m.borrow_mut() = Some(Rc::clone(&conn)));
        crate::spawn::SPAWN_QUEUE.register_promise_schedulers();
        Ok(conn)
    }

    fn terminate_message_loop(&self);
    fn run_message_loop(&self) -> Fallible<()>;

    /// Retrieve the current appearance for the application.
    fn get_appearance(&self) -> Appearance {
        if let Some(appearance) = preferred_appearance() {
            return appearance;
        }
        Appearance::Light
    }

    /// Override the application appearance. Passing `None` follows the system.
    ///
    /// The value is stored synchronously, so a `get_appearance()` on the way
    /// back out of this call already reports it. Startup depends on that: the
    /// lua configuration is evaluated right after this runs, and
    /// `wezterm.gui.get_appearance()` has to agree with what the user picked.
    ///
    /// Handing it to the OS is deferred instead; see `reapply_appearance`.
    fn set_preferred_appearance(&self, appearance: Option<Appearance>) {
        let changed = {
            let mut slot = PREFERRED_APPEARANCE.lock().unwrap();
            store_appearance(&mut slot, appearance)
        };
        // The guard is gone before anything below can read the slot back.
        if !changed {
            return;
        }
        promise::spawn::spawn(async move {
            if let Some(conn) = Connection::get() {
                conn.reapply_appearance();
            }
        })
        .detach();
    }

    /// Present the appearance recorded by `set_preferred_appearance`: tell the
    /// OS about it, and let the windows know it changed.
    ///
    /// Runs from the spawn queue, **never** synchronously from inside an event
    /// dispatch. Every caller of `set_preferred_appearance` is one: the theme
    /// is picked with the mouse, and each platform holds the dispatching
    /// window's inner state for the duration of that dispatch. Re-entering it
    /// here is a `BorrowMutError` on Windows -- which `wnd_proc` turns into
    /// `exit(1)` -- and a deadlock on X11. Implementations may therefore
    /// assume nothing is borrowed.
    fn reapply_appearance(&self) {}

    /// Hide the application.
    /// This actions hides all of the windows of the application and switches
    /// focus away from it.
    fn hide_application(&self) {}

    /// Perform the system beep/notification sound
    fn beep(&self) {}

    /// Play a WAV held in memory, without blocking.
    ///
    /// This is fire-and-forget on purpose: a prompt that fails to sound must
    /// never disturb the work it was reporting on, so every implementation
    /// swallows its errors into the log. A platform with no way to play one
    /// stays silent, which is the same outcome the user gets by turning the
    /// sounds off.
    ///
    /// The bytes are `'static` because playback outlives this call on every
    /// platform: Windows keeps reading the caller's buffer for the duration of
    /// an async `PlaySound`, and the unix implementation hands it to a thread.
    /// `include_bytes!` already yields exactly this, so the bound costs the
    /// callers nothing and removes the lifetime hazard entirely.
    fn play_sound(&self, _wav: &'static [u8]) {}

    /// Returns information about the screens
    fn screens(&self) -> anyhow::Result<Screens> {
        anyhow::bail!("Unable to query screen information");
    }

    fn resolve_geometry(&self, geometry: RequestedWindowGeometry) -> ResolvedGeometry {
        let bounds = match self.screens() {
            Ok(screens) => {
                log::trace!("{screens:?}");

                match geometry.origin {
                    GeometryOrigin::ScreenCoordinateSystem => screens.virtual_rect,
                    GeometryOrigin::MainScreen => screens.main.rect,
                    GeometryOrigin::ActiveScreen => screens.active.rect,
                    GeometryOrigin::Named(name) => match screens.by_name.get(&name) {
                        Some(info) => info.rect,
                        None => {
                            log::error!(
                                "Requested display {} was not found; available displays are: {:?}. \
                             Using primary display instead",
                                name,
                                screens.by_name,
                            );
                            screens.main.rect
                        }
                    },
                }
            }
            Err(_) => euclid::rect(0, 0, 65535, 65535),
        };

        let dpi = self.default_dpi();
        let width_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: bounds.width() as f32,
            pixel_cell: bounds.width() as f32,
        };
        let height_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: bounds.height() as f32,
            pixel_cell: bounds.height() as f32,
        };
        let width = geometry.width.evaluate_as_pixels(width_context) as usize;
        let height = geometry.height.evaluate_as_pixels(height_context) as usize;
        let x = geometry
            .x
            .map(|x| x.evaluate_as_pixels(width_context) as i32 + bounds.origin.x as i32);
        let y = geometry
            .y
            .map(|y| y.evaluate_as_pixels(height_context) as i32 + bounds.origin.y as i32);

        ResolvedGeometry {
            x,
            y,
            width,
            height,
        }
    }
}

#[cfg(test)]
mod appearance_override_tests {
    use super::{store_appearance, Appearance};

    #[test]
    fn the_first_pick_is_a_change() {
        let mut slot = None;
        assert!(
            store_appearance(&mut slot, Some(Appearance::Dark)),
            "picking a theme where none was set has to reach the windows"
        );
        assert_eq!(slot, Some(Appearance::Dark));
    }

    /// Startup under "System" stores `None` over `None`. Treating that as a
    /// change would queue a window sweep, and a `setAppearance: nil`, for a
    /// setting nobody touched.
    #[test]
    fn following_the_system_over_and_over_is_not_a_change() {
        let mut slot = None;
        assert!(!store_appearance(&mut slot, None));
        assert_eq!(slot, None);
    }

    /// Saving any other setting re-applies the whole settings struct, theme
    /// included; that must not re-theme every window each time.
    #[test]
    fn repeating_the_same_choice_is_not_a_change() {
        let mut slot = Some(Appearance::Dark);
        assert!(!store_appearance(&mut slot, Some(Appearance::Dark)));
        assert_eq!(slot, Some(Appearance::Dark));
    }

    /// Going back to "System" has to fire, or the title bar and the config
    /// reload never catch up with the system's own theme.
    #[test]
    fn giving_the_choice_back_to_the_system_is_a_change() {
        let mut slot = Some(Appearance::Dark);
        assert!(store_appearance(&mut slot, None));
        assert_eq!(slot, None);
    }

    #[test]
    fn swapping_one_choice_for_the_other_is_a_change() {
        let mut slot = Some(Appearance::Dark);
        assert!(store_appearance(&mut slot, Some(Appearance::Light)));
        assert_eq!(slot, Some(Appearance::Light));
    }
}
