use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use spanbolt_storage::Timestamp;

const SYSTEM: i64 = i64::MIN;

/// Commit timestamps: microsecond granularity, strictly increasing across every database.
/// Tests can freeze the clock; commits still advance by at least 1µs.
#[derive(Debug)]
pub struct Clock {
    fixed_micros: AtomicI64,
    last_micros: AtomicI64,
}

impl Default for Clock {
    fn default() -> Self {
        Clock {
            fixed_micros: AtomicI64::new(SYSTEM),
            last_micros: AtomicI64::new(0),
        }
    }
}

fn system_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}

impl Clock {
    pub fn now(&self) -> Timestamp {
        Timestamp::from_micros(self.now_micros())
    }

    fn now_micros(&self) -> i64 {
        match self.fixed_micros.load(Ordering::Relaxed) {
            SYSTEM => system_micros(),
            fixed => fixed,
        }
    }

    /// `CURRENT_TIMESTAMP()` for a statement in a read-write transaction: every later commit
    /// timestamp is greater, so the value can be written to a commit-timestamp column.
    pub fn statement_ts(&self) -> Timestamp {
        let now = self.now_micros();
        self.last_micros.fetch_max(now, Ordering::AcqRel);
        Timestamp::from_micros(now)
    }

    /// A strong read's timestamp: now, and no earlier than any commit timestamp handed out; every
    /// later commit timestamp is greater.
    pub fn read_ts(&self) -> Timestamp {
        let now = self.now_micros();
        let prev = self.last_micros.fetch_max(now, Ordering::AcqRel);
        Timestamp::from_micros(now.max(prev))
    }

    /// The latest timestamp a write may hold before its commit timestamp is chosen: no earlier
    /// than any `statement_ts` or commit timestamp handed out, and no later than the next commit.
    pub fn write_horizon(&self) -> Timestamp {
        Timestamp::from_micros(
            self.now_micros()
                .max(self.last_micros.load(Ordering::Acquire)),
        )
    }

    /// The next commit timestamp: `max(now, last + 1µs)`.
    pub fn next_commit_ts(&self) -> Timestamp {
        let now = self.now_micros();
        let mut last = self.last_micros.load(Ordering::Relaxed);
        loop {
            let next = now.max(last + 1);
            match self.last_micros.compare_exchange_weak(
                last,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Timestamp::from_micros(next),
                Err(cur) => last = cur,
            }
        }
    }

    /// Guarantees every later commit timestamp is greater than `ts`.
    pub fn observe(&self, ts: Timestamp) {
        let micros = ts.as_micros() + i64::from(!ts.nanos.is_multiple_of(1000));
        self.last_micros.fetch_max(micros, Ordering::AcqRel);
    }

    /// Freezes the clock at `ts`, or returns to the system clock with `None`.
    pub fn set(&self, ts: Option<Timestamp>) {
        self.fixed_micros
            .store(ts.map_or(SYSTEM, |t| t.as_micros()), Ordering::Relaxed);
    }
}
