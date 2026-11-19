//! Transactions: optimistic concurrency (first committer wins), MVCC versions and the clock.

mod clock;
mod database;
mod transaction;

pub use clock::Clock;
pub use database::{CommitInput, CommitOutcome, Database, ReadSet, TimestampBound, Version};
pub use transaction::{ReadOnlyTxn, ReadView, ReadWriteTxn};
