mod auto;
pub mod format;
pub mod io;
pub mod scope;
pub mod shared;
pub mod store;
pub mod views;

use crate::protocol::read_json;
use crate::Result;

pub use store::{Binding, Store, WriteOutcome};
