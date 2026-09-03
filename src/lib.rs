//! Factory for building Wasm Components with Wasm Components.
//!
//! An implementor provides a [`ComponentBuilder`] that builds a world and then
//! builds each exported function declared in that world.
//!
//! The factory's [`build`] drives both and returns the component's bytes.

mod abi;
mod component;
mod factory;
mod module;
mod values;

pub mod emitter;
pub mod schema;
pub mod wit;
pub mod world;

pub use factory::{ComponentBuilder, World, build};
