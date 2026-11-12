//! Build a component that calls HTTP endpoints, one typed function per
//! configured URL template.

use anyhow::Result;

pub mod config;
pub mod deserializer;
pub mod factory;
pub mod names;
pub mod response;
pub mod template;

pub use config::Config;
pub use factory::Builder;

/// Build the component from config `entries`, with nested tables flattened
/// into dot-delimited keys as the runtime delivers them.
pub fn build(entries: impl IntoIterator<Item = (String, String)>) -> Result<Vec<u8>> {
    composable_factory::build(&Builder::new(Config::parse(entries)?))
}
