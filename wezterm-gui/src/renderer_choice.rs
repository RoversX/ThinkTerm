//! Which renderer a window comes up on. The selected backend goes first and,
//! unless the configuration asks for software rendering, the other one is
//! tried when it cannot start, so no selection can keep a window shut on a
//! machine where that backend fails. Each attempt covers the whole bring-up,
//! context and shaders both: an OpenGL old enough to create a context but not
//! to compile our shaders is as unusable as one that cannot create a context.
//!
//! A backend that failed once goes last for every later window of the
//! process, so a broken one is not paid for again on each window -- a failed
//! WGL attempt also leaves its probe window behind.

use crate::native_settings::NativeRendererBackend;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;

thread_local! {
    // Windows are created on the GUI thread; one record per thread also keeps
    // the unit tests below from seeing each other's failures.
    static FAILED_WEBGPU: Cell<bool> = const { Cell::new(false) };
    static FAILED_OPENGL: Cell<bool> = const { Cell::new(false) };
}

fn failure_record(backend: NativeRendererBackend) -> &'static std::thread::LocalKey<Cell<bool>> {
    match backend {
        NativeRendererBackend::WebGpu => &FAILED_WEBGPU,
        NativeRendererBackend::OpenGL => &FAILED_OPENGL,
    }
}

fn label(backend: NativeRendererBackend) -> &'static str {
    match backend {
        NativeRendererBackend::WebGpu => "WebGPU",
        NativeRendererBackend::OpenGL => "OpenGL",
    }
}

type Attempt<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + 'a>>;

/// Neither future is polled unless its backend is tried.
pub(crate) async fn bring_up<'a, T>(
    selected: NativeRendererBackend,
    allow_fallback: bool,
    webgpu: impl Future<Output = anyhow::Result<T>> + 'a,
    opengl: impl Future<Output = anyhow::Result<T>> + 'a,
) -> anyhow::Result<T> {
    let webgpu: Attempt<'a, T> = Box::pin(webgpu);
    let opengl: Attempt<'a, T> = Box::pin(opengl);
    let mut attempts = match selected {
        NativeRendererBackend::WebGpu => vec![
            (NativeRendererBackend::WebGpu, webgpu),
            (NativeRendererBackend::OpenGL, opengl),
        ],
        NativeRendererBackend::OpenGL => vec![
            (NativeRendererBackend::OpenGL, opengl),
            (NativeRendererBackend::WebGpu, webgpu),
        ],
    };
    if !allow_fallback {
        attempts.truncate(1);
    }
    // A stable sort: one that has failed before moves behind one that has not.
    attempts.sort_by_key(|(backend, _)| failure_record(*backend).with(Cell::get));

    let mut causes = Vec::new();
    for (backend, attempt) in attempts {
        match attempt.await {
            Ok(renderer) => return Ok(renderer),
            Err(err) => {
                failure_record(backend).with(|failed| failed.set(true));
                log::error!("{} initialization failed: {err:#}", label(backend));
                causes.push(format!("{} initialization failed: {err:#}", label(backend)));
            }
        }
    }
    Err(anyhow::anyhow!(causes.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn opengl_success_does_not_initialize_webgpu() {
        let result = smol::block_on(bring_up(
            NativeRendererBackend::OpenGL,
            true,
            async { panic!("WebGPU must not be polled") },
            async { Ok("OpenGL") },
        ));
        assert_eq!(result.unwrap(), "OpenGL");
    }

    #[test]
    fn webgpu_success_does_not_initialize_opengl() {
        let result = smol::block_on(bring_up(
            NativeRendererBackend::WebGpu,
            true,
            async { Ok("WebGPU") },
            async { panic!("OpenGL must not be polled") },
        ));
        assert_eq!(result.unwrap(), "WebGPU");
    }

    #[test]
    fn webgpu_failure_falls_back_to_opengl() {
        let attempts = RefCell::new(Vec::new());
        let result = smol::block_on(bring_up(
            NativeRendererBackend::WebGpu,
            true,
            async {
                attempts.borrow_mut().push("WebGPU");
                anyhow::bail!("adapter unavailable")
            },
            async {
                attempts.borrow_mut().push("OpenGL");
                Ok("OpenGL")
            },
        ));
        assert_eq!(result.unwrap(), "OpenGL");
        assert_eq!(*attempts.borrow(), vec!["WebGPU", "OpenGL"]);
    }

    #[test]
    fn opengl_failure_falls_back_to_webgpu() {
        let attempts = RefCell::new(Vec::new());
        let result = smol::block_on(bring_up(
            NativeRendererBackend::OpenGL,
            true,
            async {
                attempts.borrow_mut().push("WebGPU");
                Ok("WebGPU")
            },
            async {
                attempts.borrow_mut().push("OpenGL");
                anyhow::bail!("the OpenGL implementation is too old")
            },
        ));
        assert_eq!(result.unwrap(), "WebGPU");
        assert_eq!(*attempts.borrow(), vec!["OpenGL", "WebGPU"]);
    }

    #[test]
    fn both_renderer_failures_keep_both_causes() {
        let error = smol::block_on(bring_up::<()>(
            NativeRendererBackend::WebGpu,
            true,
            async { Err(anyhow::anyhow!("adapter unavailable").context("device creation")) },
            async { Err(anyhow::anyhow!("shader compilation failed")) },
        ))
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message
            .contains("WebGPU initialization failed: device creation: adapter unavailable"));
        assert!(message.contains("OpenGL initialization failed: shader compilation failed"));
    }

    #[test]
    fn explicit_opengl_failure_keeps_both_causes() {
        let error = smol::block_on(bring_up::<()>(
            NativeRendererBackend::OpenGL,
            true,
            async { anyhow::bail!("adapter unavailable") },
            async { anyhow::bail!("context creation failed") },
        ))
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "OpenGL initialization failed: context creation failed; \
             WebGPU initialization failed: adapter unavailable"
        );
    }

    #[test]
    fn software_rendering_never_falls_back_to_webgpu() {
        let error = smol::block_on(bring_up::<()>(
            NativeRendererBackend::OpenGL,
            false,
            async { panic!("WebGPU must not be polled") },
            async { anyhow::bail!("context creation failed") },
        ))
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "OpenGL initialization failed: context creation failed"
        );
    }

    #[test]
    fn a_backend_that_failed_goes_last_for_later_windows() {
        let first = smol::block_on(bring_up(
            NativeRendererBackend::OpenGL,
            true,
            async { Ok("WebGPU") },
            async { anyhow::bail!("the OpenGL implementation is too old") },
        ));
        assert_eq!(first.unwrap(), "WebGPU");

        let second = smol::block_on(bring_up(
            NativeRendererBackend::OpenGL,
            true,
            async { Ok("WebGPU") },
            async { panic!("OpenGL failed before and must not go first") },
        ));
        assert_eq!(second.unwrap(), "WebGPU");
    }
}
