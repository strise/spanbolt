//! Snapshot files: every instance and database (schema + rows), compact binary, zstd compressed.
//!
//! Layout: `MAGIC`, then a zstd stream of: format version, instances, and per database its schema
//! (GetDatabaseDdl statements) and each table's rows keyed by column name (so column order changes stay loadable).
//! Indexes are rebuilt on load.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use spanbolt_ddl::Schema;

use crate::error::{Error, Result};
use crate::key::encode_key;
use crate::state::{DbState, Row, RowMap};
use crate::value::{Date, Interval, Numeric, Timestamp, Value};

const MAGIC: &[u8; 8] = b"SPANBOLT";
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default)]
pub struct InstanceImage {
    pub name: String,
    pub config: String,
    pub display_name: String,
    pub node_count: i32,
    pub processing_units: i32,
    pub labels: Vec<(String, String)>,
    pub databases: Vec<DatabaseImage>,
}

#[derive(Debug, Clone)]
pub struct DatabaseImage {
    pub name: String,
    pub commit_ts: Timestamp,
    pub state: Arc<DbState>,
}

fn io_err(e: std::io::Error) -> Error {
    Error::internal(format!("snapshot I/O error: {e}"))
}

fn corrupt() -> Error {
    Error::failed_precondition("Snapshot file is corrupt or truncated")
}

pub fn save(path: &Path, instances: &[InstanceImage]) -> Result<u64> {
    let tmp = path.with_extension("tmp");
    let file = std::fs::File::create(&tmp).map_err(io_err)?;
    let mut w = std::io::BufWriter::new(file);
    write_to(&mut w, instances)?;
    w.flush().map_err(io_err)?;
    drop(w);
    std::fs::rename(&tmp, path).map_err(io_err)?;
    Ok(std::fs::metadata(path).map_err(io_err)?.len())
}

pub fn load(path: &Path) -> Result<Vec<InstanceImage>> {
    let file = std::fs::File::open(path).map_err(io_err)?;
    read_from(std::io::BufReader::new(file))
}

pub fn write_to<W: Write>(mut w: W, instances: &[InstanceImage]) -> Result<()> {
    w.write_all(MAGIC).map_err(io_err)?;
    let mut z = zstd::stream::Encoder::new(w, 3).map_err(io_err)?;
    let mut e = Enc {
        buf: Vec::with_capacity(1 << 16),
    };
    e.u32(FORMAT_VERSION);
    e.len(instances.len());
    for inst in instances {
        e.str(&inst.name);
        e.str(&inst.config);
        e.str(&inst.display_name);
        e.i64(inst.node_count as i64);
        e.i64(inst.processing_units as i64);
        e.pairs(&inst.labels);
        e.len(inst.databases.len());
        for db in &inst.databases {
            e.str(&db.name);
            e.i64(db.commit_ts.secs);
            e.u32(db.commit_ts.nanos);
            e.strs(&db.state.schema.to_ddl());
            let cat = &db.state.catalog;
            e.len(cat.tables.len());
            for (t, tm) in cat.tables.iter().enumerate() {
                e.str(&tm.name);
                e.len(tm.columns.len());
                for c in &tm.columns {
                    e.str(&c.name);
                }
                let rows = db.state.rows(t);
                e.len(rows.len());
                for (_, row) in rows.iter() {
                    for v in row.iter() {
                        e.value(v);
                    }
                    if e.buf.len() > 1 << 20 {
                        z.write_all(&e.buf).map_err(io_err)?;
                        e.buf.clear();
                    }
                }
            }
        }
    }
    z.write_all(&e.buf).map_err(io_err)?;
    z.finish().map_err(io_err)?.flush().map_err(io_err)?;
    Ok(())
}

pub fn read_from<R: Read>(mut r: R) -> Result<Vec<InstanceImage>> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(|_| corrupt())?;
    if &magic != MAGIC {
        return Err(Error::failed_precondition("Not a spanbolt snapshot file"));
    }
    let mut data = Vec::new();
    zstd::stream::Decoder::new(r)
        .map_err(io_err)?
        .read_to_end(&mut data)
        .map_err(|_| corrupt())?;
    let mut d = Dec { buf: &data, pos: 0 };
    let version = d.u32()?;
    if version != FORMAT_VERSION {
        return Err(Error::failed_precondition(format!(
            "Unsupported snapshot format version {version}"
        )));
    }
    let n = d.len()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut inst = InstanceImage {
            name: d.str()?,
            config: d.str()?,
            display_name: d.str()?,
            node_count: d.i64()? as i32,
            processing_units: d.i64()? as i32,
            labels: d.pairs()?,
            databases: Vec::new(),
        };
        for _ in 0..d.len()? {
            let name = d.str()?;
            let commit_ts = Timestamp {
                secs: d.i64()?,
                nanos: d.u32()?,
            };
            let sch = schema_from_ddl(&d.strs()?)?;
            let mut state = DbState::new(sch)?;
            for _ in 0..d.len()? {
                let tname = d.str()?;
                let ncols = d.len()?;
                let mut names = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    names.push(d.str()?);
                }
                let tid = state.catalog.table_id(&tname).ok_or_else(corrupt)?;
                let tm = state.catalog.table(tid).clone();
                let mapping: Vec<Option<usize>> = names.iter().map(|n| tm.column(n)).collect();
                let nrows = d.len()?;
                let mut map = RowMap::new();
                let mut key_vals = Vec::with_capacity(tm.pk.len());
                for _ in 0..nrows {
                    let mut row = vec![Value::Null; tm.columns.len()];
                    for m in &mapping {
                        let v = d.value()?;
                        if let Some(i) = m {
                            row[*i] = v;
                        }
                    }
                    key_vals.clear();
                    key_vals.extend(tm.pk.iter().map(|&c| row[c].clone()));
                    let key = encode_key(&key_vals, &tm.pk_desc)?;
                    map.insert(Arc::from(key), Row::from(row));
                }
                state.tables.set(tid, map);
            }
            state.rebuild_indexes()?;
            inst.databases.push(DatabaseImage {
                name,
                commit_ts,
                state: Arc::new(state),
            });
        }
        out.push(inst);
    }
    Ok(out)
}

struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    fn len(&mut self, n: usize) {
        self.varint(n as u64);
    }
    fn u32(&mut self, v: u32) {
        self.varint(v as u64);
    }
    fn i64(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.buf.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn pairs(&mut self, p: &[(String, String)]) {
        self.len(p.len());
        for (a, b) in p {
            self.str(a);
            self.str(b);
        }
    }
    fn strs(&mut self, v: &[String]) {
        self.len(v.len());
        for s in v {
            self.str(s);
        }
    }
    fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.buf.push(0),
            Value::Bool(b) => self.buf.extend_from_slice(&[1, *b as u8]),
            Value::Int64(i) => {
                self.buf.push(2);
                self.i64(*i);
            }
            Value::Float32(f) => {
                self.buf.push(3);
                self.buf.extend_from_slice(&f.to_le_bytes());
            }
            Value::Float64(f) => {
                self.buf.push(4);
                self.buf.extend_from_slice(&f.to_le_bytes());
            }
            Value::Numeric(n) => {
                self.buf.push(5);
                self.buf.extend_from_slice(&n.0.to_le_bytes());
            }
            Value::String(s) => {
                self.buf.push(6);
                self.str(s);
            }
            Value::Bytes(b) => {
                self.buf.push(7);
                self.bytes(b);
            }
            Value::Date(d) => {
                self.buf.push(8);
                self.i64(d.0 as i64);
            }
            Value::Timestamp(t) => {
                self.buf.push(9);
                self.i64(t.secs);
                self.u32(t.nanos);
            }
            Value::Json(s) => {
                self.buf.push(10);
                self.str(s);
            }
            Value::Interval(iv) => {
                self.buf.push(11);
                self.i64(iv.months as i64);
                self.i64(iv.days as i64);
                self.buf.extend_from_slice(&iv.nanos.to_le_bytes());
            }
            Value::Uuid(u) => {
                self.buf.push(12);
                self.buf.extend_from_slice(&u.to_le_bytes());
            }
            Value::TokenList(b) => {
                self.buf.push(13);
                self.bytes(b);
            }
            Value::Array(items) | Value::Struct(items) => {
                self.buf
                    .push(if matches!(v, Value::Array(_)) { 14 } else { 15 });
                self.len(items.len());
                for i in items.iter() {
                    self.value(i);
                }
            }
        }
    }
}

struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Dec<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or_else(corrupt)?;
        self.pos += 1;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(corrupt)?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| corrupt())
    }
    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(corrupt())
    }
    fn len(&mut self) -> Result<usize> {
        let n = self.varint()? as usize;
        if n > self.buf.len() * 8 + 1024 {
            return Err(corrupt());
        }
        Ok(n)
    }
    fn u32(&mut self) -> Result<u32> {
        u32::try_from(self.varint()?).map_err(|_| corrupt())
    }
    fn i64(&mut self) -> Result<i64> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }
    fn bool(&mut self) -> Result<bool> {
        Ok(self.byte()? != 0)
    }
    fn bytes(&mut self) -> Result<&[u8]> {
        let n = self.len()?;
        self.take(n)
    }
    fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| corrupt())
    }
    fn pairs(&mut self) -> Result<Vec<(String, String)>> {
        let n = self.len()?;
        (0..n).map(|_| Ok((self.str()?, self.str()?))).collect()
    }
    fn strs(&mut self) -> Result<Vec<String>> {
        let n = self.len()?;
        (0..n).map(|_| self.str()).collect()
    }
    fn value(&mut self) -> Result<Value> {
        Ok(match self.byte()? {
            0 => Value::Null,
            1 => Value::Bool(self.bool()?),
            2 => Value::Int64(self.i64()?),
            3 => Value::Float32(f32::from_le_bytes(self.arr()?)),
            4 => Value::Float64(f64::from_le_bytes(self.arr()?)),
            5 => Value::Numeric(Numeric(i128::from_le_bytes(self.arr()?))),
            6 => Value::String(Arc::from(
                std::str::from_utf8(self.bytes()?).map_err(|_| corrupt())?,
            )),
            7 => Value::Bytes(Arc::from(self.bytes()?)),
            8 => Value::Date(Date(self.i64()? as i32)),
            9 => Value::Timestamp(Timestamp {
                secs: self.i64()?,
                nanos: self.u32()?,
            }),
            10 => Value::Json(Arc::from(
                std::str::from_utf8(self.bytes()?).map_err(|_| corrupt())?,
            )),
            11 => Value::Interval(Interval {
                months: self.i64()? as i32,
                days: self.i64()? as i32,
                nanos: i128::from_le_bytes(self.arr()?),
            }),
            12 => Value::Uuid(u128::from_le_bytes(self.arr()?)),
            13 => Value::TokenList(Arc::from(self.bytes()?)),
            t @ (14 | 15) => {
                let n = self.len()?;
                let items: Vec<Value> = (0..n).map(|_| self.value()).collect::<Result<_>>()?;
                if t == 14 {
                    Value::Array(Arc::from(items))
                } else {
                    Value::Struct(Arc::from(items))
                }
            }
            _ => return Err(corrupt()),
        })
    }
}

/// Rebuilds a schema from its GetDatabaseDdl statements.
pub fn schema_from_ddl(statements: &[String]) -> Result<Schema> {
    let mut schema = Schema::default();
    for sql in statements {
        let stmt = spanbolt_ddl::parse_statement(sql).map_err(|e| {
            Error::failed_precondition(format!(
                "Snapshot schema statement does not parse: {e}: {sql}"
            ))
        })?;
        // The more lenient rules, so a schema the emulator-compat server accepted loads back.
        schema.apply_emulator_compat(&stmt)?;
    }
    Ok(schema)
}
