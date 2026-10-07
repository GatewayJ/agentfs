//! Portable identities, file semantics and durable format types.

mod entities;
mod error;
mod id;
mod object;
mod path;

pub use entities::*;
pub use error::*;
pub use id::*;
pub use object::*;
pub use path::*;

pub const FORMAT_VERSION: u32 = 1;
pub const INDEX_PAGE_ENTRIES: usize = 128;
pub const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
pub const IO_BUFFER_BYTES: usize = 256 * 1024;
