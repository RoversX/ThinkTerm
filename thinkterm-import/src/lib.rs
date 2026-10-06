//! Source-independent session import plans and native adapter registration.
//! The wire model also builds for wasm; adapters never depend on the mux.
mod model;
pub use model::*;

#[cfg(unix)]
mod source;
#[cfg(unix)]
pub use source::*;

#[cfg(test)]
mod tests;
