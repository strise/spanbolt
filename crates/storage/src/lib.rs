//! Values, key encoding, tables and indexes as persistent maps, immutable database states and
//! snapshot files.

pub mod catalog;
pub mod error;
pub mod expr;
pub mod key;
pub mod pending;
pub mod snapshot;
pub mod state;
pub mod testing;
pub mod value;
pub mod wire;

pub use catalog::{Catalog, IndexId, IndexMeta, TableId, TableMeta};
pub use error::{Code, Error, Result};
pub use key::{Key, KeyRange, KeySet, Span};
pub use state::{
    DbState, Mutation, Row, RowMap, Target, WriteKind, Writer, decode_key, decode_point_key,
    index_prefix, scan,
};
pub use value::{Date, Interval, Numeric, Timestamp, Value};
