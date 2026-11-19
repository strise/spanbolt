//! Streaming results as PartialResultSets: bounded memory, chunked values and resume tokens.
//!
//! A streaming query or read runs on a blocking thread and hands each row to a
//! [`PartialWriter`], which packs values into PartialResultSets of about [`CHUNK_BYTES`] and sends
//! them through a small bounded channel. When the client reads slowly, the executor waits, so a
//! result of any size holds at most [`IN_FLIGHT`] + 2 partials in server memory.
//!
//! **Encoded as they are built.** The writer encodes each value into the partial's wire bytes as
//! it arrives and hands on finished partials as [`Bytes`]; [`crate::wire`] sends them through
//! gRPC as they are. A partial held as a prost `PartialResultSet` takes several times its encoded
//! size (a 1 MiB partial of small values is around 10 MiB of `Value`s and strings), and the
//! allocator kept those blocks after the stream ended.
//!
//! **Chunked values.** A string (or bytes, which travel as base64 strings) or a list (ARRAY,
//! STRUCT) that does not fit the space left in a partial is split: the partial ends with the head
//! and `chunked_value = true`, and the next one starts with the rest. Clients merge the two the
//! way Spanner defines it: strings concatenate; lists concatenate, merging the last element of the
//! first with the first element of the second when both are strings or both lists. So a list
//! split between two whole elements whose boundary element is a string or a list starts its tail
//! with an empty one, which the merge absorbs.
//!
//! **Resume tokens.** Every partial that ends at a row boundary carries a token naming the number
//! of rows sent so far and, for a single-use read-only read, the read timestamp. A client that
//! resends the request with the token gets the rest of the rows from the same snapshot: the server
//! runs the statement again and skips the rows already sent. Execution is deterministic for a
//! given snapshot and plan, so the skipped rows are exactly the ones the client has.

use std::pin::Pin;

use prost::Message;
use prost::bytes::Bytes;
use prost_types::Value;
use prost_types::value::Kind;
use spanbolt_proto::spanner as pb;
use spanbolt_storage::Timestamp;
use tokio_stream::Stream;
use tonic::Status;

use crate::sql::RowSink;

pub type ResultStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Receives each finished PartialResultSet, encoded.
pub type Emit<'a> = dyn FnMut(Bytes) -> Result<(), Status> + 'a;

/// Target size of one PartialResultSet.
pub const CHUNK_BYTES: usize = 1 << 20;

/// Partials queued between the executor and gRPC.
const IN_FLIGHT: usize = 2;

/// A value is only split when at least this much room is left; otherwise the partial is sent
/// first and the value starts the next one.
const MIN_SPLIT: usize = 4 << 10;

const TOKEN_MAGIC: &[u8; 3] = b"sb1";

/// Where a resumed stream restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeToken {
    /// Rows the client already has.
    pub rows: u64,
    /// The snapshot of a single-use read-only read; `None` inside a transaction, which pins its
    /// own snapshot.
    pub read_ts: Option<Timestamp>,
}

impl ResumeToken {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(23);
        out.extend_from_slice(TOKEN_MAGIC);
        out.extend_from_slice(&self.rows.to_be_bytes());
        if let Some(ts) = self.read_ts {
            out.extend_from_slice(&ts.secs.to_be_bytes());
            out.extend_from_slice(&ts.nanos.to_be_bytes());
        }
        out
    }

    /// `None` for an empty token (a fresh request).
    pub fn decode(b: &[u8]) -> Result<Option<ResumeToken>, Status> {
        if b.is_empty() {
            return Ok(None);
        }
        let bad = || Status::invalid_argument("Invalid resume token.");
        let rest = b.strip_prefix(TOKEN_MAGIC.as_slice()).ok_or_else(bad)?;
        let (rows, rest) = rest.split_first_chunk::<8>().ok_or_else(bad)?;
        let read_ts = match rest.len() {
            0 => None,
            12 => {
                let (secs, nanos) = rest.split_at(8);
                let secs = i64::from_be_bytes(secs.try_into().map_err(|_| bad())?);
                let nanos = u32::from_be_bytes(nanos.try_into().map_err(|_| bad())?);
                Some(Timestamp { secs, nanos })
            }
            _ => return Err(bad()),
        };
        Ok(Some(ResumeToken {
            rows: u64::from_be_bytes(*rows),
            read_ts,
        }))
    }
}

/// Packs rows into PartialResultSets and hands each finished one, encoded, to `emit`.
pub struct PartialWriter<'a> {
    emit: &'a mut Emit<'a>,
    chunk: usize,
    /// Every field of the current partial except `values`.
    cur: pb::PartialResultSet,
    /// The current partial's `values`, encoded (field 2, repeated).
    values: Vec<u8>,
    /// Number of values in `values`.
    count: usize,
    size: usize,
    /// Set into the metadata: the transaction an inline `begin` started.
    begun: Option<pb::Transaction>,
    read_ts: Option<Timestamp>,
    /// Rows to drop before sending (a resumed stream).
    skip: u64,
    /// Rows produced so far, skipped ones included.
    rows: u64,
}

impl<'a> PartialWriter<'a> {
    pub fn new(emit: &'a mut Emit<'a>) -> Self {
        PartialWriter {
            emit,
            chunk: CHUNK_BYTES,
            cur: pb::PartialResultSet::default(),
            values: Vec::new(),
            count: 0,
            size: 0,
            begun: None,
            read_ts: None,
            skip: 0,
            rows: 0,
        }
    }

    #[cfg(test)]
    pub fn chunk_bytes(mut self, n: usize) -> Self {
        self.chunk = n.max(1);
        self
    }

    pub fn begun(mut self, t: Option<pb::Transaction>) -> Self {
        self.begun = t;
        self
    }

    /// Resume after `token`, and stamp later tokens with `read_ts`.
    pub fn resume(mut self, token: Option<ResumeToken>, read_ts: Option<Timestamp>) -> Self {
        self.skip = token.map_or(0, |t| t.rows);
        self.read_ts = read_ts;
        self
    }

    fn token(&self) -> Vec<u8> {
        ResumeToken {
            rows: self.rows,
            read_ts: self.read_ts,
        }
        .encode()
    }

    fn flush(&mut self, at_row_boundary: bool) -> Result<(), Status> {
        if at_row_boundary {
            self.cur.resume_token = self.token();
        }
        self.size = 0;
        let part = self.encode()?;
        (self.emit)(part)
    }

    fn put(&mut self, v: &Value) {
        prost::encoding::message::encode(2, v, &mut self.values);
        self.count += 1;
    }

    /// The current partial's wire bytes, in field order; resets it, keeping the value buffer.
    fn encode(&mut self) -> Result<Bytes, Status> {
        use prost::encoding::message;
        let meta = self.cur.metadata.take();
        let rest = std::mem::take(&mut self.cur);
        let meta_len = meta.as_ref().map_or(0, |m| message::encoded_len(1, m));
        let mut out = Vec::with_capacity(meta_len + self.values.len() + rest.encoded_len());
        if let Some(m) = &meta {
            message::encode(1, m, &mut out);
        }
        out.extend_from_slice(&self.values);
        rest.encode(&mut out)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.values.clear();
        self.count = 0;
        Ok(Bytes::from(out))
    }

    fn push(&mut self, mut v: Value, row_start: bool) -> Result<(), Status> {
        let mut at_boundary = row_start;
        loop {
            let len = v.encoded_len() + 2;
            let room = self.chunk.saturating_sub(self.size);
            if len <= room || !splittable(&v) {
                self.size += len;
                self.put(&v);
                return Ok(());
            }
            if room < MIN_SPLIT.min(self.chunk) && self.count > 0 {
                self.flush(at_boundary)?;
                continue;
            }
            let (head, tail) = split(v, room.max(1));
            self.put(&head);
            let Some(tail) = tail else {
                self.size += len;
                return Ok(());
            };
            self.cur.chunked_value = true;
            self.flush(false)?;
            at_boundary = false;
            v = tail;
        }
    }

    /// Sends the last partial, with the statement's stats and the precommit token.
    pub fn finish(
        mut self,
        stats: Option<pb::ResultSetStats>,
        precommit_token: Option<pb::MultiplexedSessionPrecommitToken>,
    ) -> Result<(), Status> {
        self.cur.stats = stats;
        self.cur.precommit_token = precommit_token;
        self.cur.last = true;
        self.cur.resume_token = self.token();
        let part = self.encode()?;
        (self.emit)(part)
    }
}

impl RowSink for PartialWriter<'_> {
    fn metadata(&mut self, mut m: pb::ResultSetMetadata) -> Result<(), Status> {
        if let Some(t) = self.begun.take() {
            m.transaction = Some(t);
        }
        self.cur.metadata = Some(m);
        Ok(())
    }

    fn row(&mut self, values: Vec<Value>) -> Result<(), Status> {
        if self.rows < self.skip {
            self.rows += 1;
            return Ok(());
        }
        // Start a new partial at this row boundary (with a resume token) rather than inside the
        // row when the row does not fit; only rows larger than a chunk end partials mid-row.
        let len: usize = values.iter().map(|v| v.encoded_len() + 2).sum();
        if self.count > 0 && self.size + len > self.chunk {
            self.flush(true)?;
        }
        for (i, v) in values.into_iter().enumerate() {
            self.push(v, i == 0)?;
        }
        self.rows += 1;
        if self.size >= self.chunk {
            self.flush(true)?;
        }
        Ok(())
    }
}

fn splittable(v: &Value) -> bool {
    match &v.kind {
        Some(Kind::StringValue(s)) => s.len() > 1,
        Some(Kind::ListValue(l)) => !l.values.is_empty(),
        _ => false,
    }
}

fn is_empty(v: &Value) -> bool {
    match &v.kind {
        Some(Kind::StringValue(s)) => s.is_empty(),
        Some(Kind::ListValue(l)) => l.values.is_empty(),
        _ => false,
    }
}

/// An empty value of the same mergeable kind, or `None` when `v` does not merge.
fn empty_like(v: &Value) -> Option<Value> {
    let kind = match &v.kind {
        Some(Kind::StringValue(_)) => Kind::StringValue(String::new()),
        Some(Kind::ListValue(_)) => Kind::ListValue(Default::default()),
        _ => return None,
    };
    Some(Value { kind: Some(kind) })
}

/// Splits `v` into a head of about `budget` encoded bytes and the rest, such that a client
/// merging the two (see the module docs) gets `v` back. `None` when `v` cannot be split.
fn split(v: Value, budget: usize) -> (Value, Option<Value>) {
    match v.kind {
        Some(Kind::StringValue(mut s)) => {
            let mut at = budget.min(s.len().saturating_sub(1)).max(1);
            while at > 0 && !s.is_char_boundary(at) {
                at -= 1;
            }
            if at == 0 {
                at = (1..s.len())
                    .find(|&i| s.is_char_boundary(i))
                    .unwrap_or(s.len());
            }
            if at >= s.len() {
                return (string(s), None);
            }
            let tail = s.split_off(at);
            (string(s), Some(string(tail)))
        }
        Some(Kind::ListValue(l)) => {
            let mut vals = l.values;
            let mut used = 0;
            for i in 0..vals.len() {
                let len = vals[i].encoded_len() + 2;
                if used + len <= budget {
                    used += len;
                    continue;
                }
                let room = budget.saturating_sub(used);
                // A head of nothing but empty values (a merge pad) would make no progress.
                let trivial = vals[..i].iter().all(is_empty);
                if splittable(&vals[i]) && (room >= MIN_SPLIT.min(budget) || trivial) {
                    let rest = vals.split_off(i + 1);
                    let Some(e) = vals.pop() else { break };
                    let (h, t) = split(e, room.max(1));
                    vals.push(h);
                    if let Some(t) = t {
                        let mut tail = Vec::with_capacity(rest.len() + 1);
                        tail.push(t);
                        tail.extend(rest);
                        return (list(vals), Some(list(tail)));
                    }
                    vals.extend(rest);
                }
                // Split between whole elements, with a non-empty element in the head.
                let at = if trivial { i + 1 } else { i };
                if at >= vals.len() {
                    break;
                }
                let mut tail = vals.split_off(at);
                if let Some(pad) = vals.last().and_then(empty_like) {
                    tail.insert(0, pad);
                }
                return (list(vals), Some(list(tail)));
            }
            (list(vals), None)
        }
        kind => (Value { kind }, None),
    }
}

fn string(s: String) -> Value {
    Value {
        kind: Some(Kind::StringValue(s)),
    }
}

fn list(values: Vec<Value>) -> Value {
    Value {
        kind: Some(Kind::ListValue(prost_types::ListValue { values })),
    }
}

/// Runs `produce` on a blocking thread and streams the partials it emits. An error before the
/// first partial is returned as the RPC's status; a later one ends the stream.
pub async fn spawn<F>(produce: F) -> Result<ResultStream<Bytes>, Status>
where
    F: FnOnce(&mut Emit<'_>) -> Result<(), Status> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel(IN_FLIGHT);
    tokio::task::spawn_blocking(move || {
        let mut emit = |p: Bytes| {
            tx.blocking_send(Ok(p))
                .map_err(|_| Status::cancelled("The client cancelled the stream."))
        };
        if let Err(s) = produce(&mut emit) {
            let _ = tx.blocking_send(Err(s));
        }
    });
    match rx.recv().await {
        None => Err(Status::internal(
            "The result stream ended without a result.",
        )),
        Some(Err(s)) => Err(s),
        Some(Ok(first)) => {
            let rest = tokio_stream::wrappers::ReceiverStream::new(rx);
            Ok(Box::pin(tokio_stream::StreamExt::chain(
                tokio_stream::once(Ok(first)),
                rest,
            )))
        }
    }
}

/// Decodes an encoded stream, for callers that want the prost messages.
pub fn decoded(s: ResultStream<Bytes>) -> ResultStream<pb::PartialResultSet> {
    Box::pin(tokio_stream::StreamExt::map(s, |r| {
        r.and_then(|b| pb::PartialResultSet::decode(b).map_err(|e| Status::internal(e.to_string())))
    }))
}

/// Merges a stream of partials back into rows the way the Java client does (tests only).
#[cfg(test)]
pub(crate) fn reassemble(parts: &[pb::PartialResultSet], width: usize) -> Vec<Vec<Value>> {
    fn merge(a: Value, b: Value) -> Value {
        match (a.kind, b.kind) {
            (Some(Kind::StringValue(x)), Some(Kind::StringValue(y))) => string(x + &y),
            (Some(Kind::ListValue(x)), Some(Kind::ListValue(y))) => {
                let mut xs = x.values;
                let mut ys = y.values.into_iter();
                if let (Some(last), Some(first)) = (xs.last(), ys.as_slice().first())
                    && empty_like(last).is_some()
                    && std::mem::discriminant(&last.kind) == std::mem::discriminant(&first.kind)
                {
                    let last = xs.pop().unwrap_or_default();
                    let first = ys.next().unwrap_or_default();
                    xs.push(merge(last, first));
                }
                xs.extend(ys);
                list(xs)
            }
            (a, _) => panic!("unmergeable chunk {a:?}"),
        }
    }
    let mut values: Vec<Value> = Vec::new();
    let mut pending = false;
    for p in parts {
        let mut it = p.values.iter().cloned();
        if pending && let Some(first) = it.next() {
            let last = values.pop().unwrap_or_default();
            values.push(merge(last, first));
        }
        values.extend(it);
        pending = p.chunked_value;
    }
    values.chunks(width).map(|c| c.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        rows: &[Vec<Value>],
        chunk: usize,
        skip: Option<ResumeToken>,
    ) -> Vec<pb::PartialResultSet> {
        let mut out = Vec::new();
        let mut emit = |p: Bytes| {
            let part = pb::PartialResultSet::decode(p.clone()).unwrap();
            // Same bytes, in the same field order, as prost encodes the message.
            assert_eq!(part.encode_to_vec(), p);
            out.push(part);
            Ok(())
        };
        let mut w = PartialWriter::new(&mut emit)
            .chunk_bytes(chunk)
            .resume(skip, None);
        w.metadata(pb::ResultSetMetadata::default()).unwrap();
        for r in rows {
            w.row(r.clone()).unwrap();
        }
        w.finish(None, None).unwrap();
        out
    }

    fn big(n: usize, c: char) -> Value {
        string(c.to_string().repeat(n))
    }

    #[test]
    fn token_round_trips() {
        for t in [
            ResumeToken {
                rows: 42,
                read_ts: None,
            },
            ResumeToken {
                rows: 7,
                read_ts: Some(Timestamp {
                    secs: 1_700_000_000,
                    nanos: 5,
                }),
            },
        ] {
            assert_eq!(ResumeToken::decode(&t.encode()).unwrap(), Some(t));
        }
        assert_eq!(ResumeToken::decode(b"").unwrap(), None);
        assert!(ResumeToken::decode(b"garbage").is_err());
    }

    #[test]
    fn large_strings_split_and_merge_back() {
        let rows = vec![
            vec![big(50_000, 'a'), string("x".into())],
            vec![big(3, 'é'), big(30_000, 'ø')],
        ];
        let parts = run(&rows, 8 << 10, None);
        assert!(parts.len() > 5, "{} partials", parts.len());
        assert!(parts.iter().any(|p| p.chunked_value));
        for p in &parts {
            assert!(
                p.encoded_len() < (8 << 10) + 64,
                "partial of {}",
                p.encoded_len()
            );
        }
        assert_eq!(reassemble(&parts, 2), rows);
    }

    #[test]
    fn arrays_of_strings_keep_element_boundaries() {
        let arr = list(
            (0..400)
                .map(|i| string(format!("element-{i:05}")))
                .collect(),
        );
        let nested = list(vec![
            big(20_000, 'q'),
            list(vec![string("s".into()), big(10_000, 'z')]),
            Value {
                kind: Some(Kind::NullValue(0)),
            },
        ]);
        let rows = vec![vec![arr.clone(), nested.clone()], vec![arr, nested]];
        let parts = run(&rows, MIN_SPLIT, None);
        assert!(parts.iter().filter(|p| p.chunked_value).count() > 4);
        assert_eq!(reassemble(&parts, 2), rows);
    }

    #[test]
    fn tokens_only_at_row_boundaries_and_resume_exactly_once() {
        let rows: Vec<Vec<Value>> = (0..200)
            .map(|i| vec![string(format!("{i}")), big(300 + i * 7, 'r')])
            .collect();
        let parts = run(&rows, MIN_SPLIT, None);
        // Resume from every token: the resumed stream holds exactly the remaining rows.
        let mut tokens = 0;
        for (n, p) in parts.iter().enumerate() {
            if p.resume_token.is_empty() {
                // Ends inside a row (a chunked value, or between two values of one row).
                continue;
            }
            assert!(!p.chunked_value);
            tokens += 1;
            let t = ResumeToken::decode(&p.resume_token).unwrap();
            let got = reassemble(&parts[..=n], 2);
            assert_eq!(got.len() as u64, t.unwrap().rows);
            let resumed = run(&rows, MIN_SPLIT, t);
            let mut all = got;
            all.extend(reassemble(&resumed, 2));
            assert_eq!(all, rows);
        }
        assert!(tokens > 10);
    }

    /// A slow client holds the producer back: memory stays at a few partials however long the
    /// result is.
    #[tokio::test]
    async fn slow_reader_bounds_partials_in_flight() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        use tokio_stream::StreamExt;

        const ROWS: u64 = 1_000_000;
        let produced = Arc::new(AtomicU64::new(0));
        let p = produced.clone();
        let mut out = spawn(move |emit| {
            let mut counting = |part: Bytes| {
                p.fetch_add(1, Ordering::SeqCst);
                emit(part)
            };
            let mut w = PartialWriter::new(&mut counting).chunk_bytes(16 << 10);
            w.metadata(pb::ResultSetMetadata::default())?;
            for i in 0..ROWS {
                w.row(vec![string(format!("{i:0100}"))])?;
            }
            w.finish(None, None)
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let early = produced.load(Ordering::SeqCst);
        assert!(
            early <= IN_FLIGHT as u64 + 2,
            "{early} partials built ahead of the reader"
        );
        let mut rows = 0;
        let mut parts = 1;
        while let Some(part) = out.next().await {
            rows += pb::PartialResultSet::decode(part.unwrap())
                .unwrap()
                .values
                .len() as u64;
            parts += 1;
        }
        assert_eq!(rows, ROWS);
        assert!(parts > 1000);
    }

    #[test]
    fn metadata_on_first_partial_only() {
        let rows: Vec<Vec<Value>> = (0..100).map(|_| vec![big(1000, 'm')]).collect();
        let parts = run(&rows, MIN_SPLIT, None);
        assert!(parts[0].metadata.is_some());
        assert!(parts[1..].iter().all(|p| p.metadata.is_none()));
        assert!(parts.last().unwrap().last);
    }
}
