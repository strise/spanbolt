//! FFI bindings to the GoogleSQL analyser shim (`native/googlesql`).
//!
//! The shim parses and type-checks one statement against a catalog and returns the resolved AST
//! as a serialized `googlesql.AnyResolvedStatementProto`; [`proto`] holds the prost types.

use std::ffi::c_char;
use std::ptr::NonNull;
use std::sync::Arc;

#[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/googlesql.rs"));
}

/// The serialized `FileDescriptorSet` of `resolved_ast.proto` and its imports, for code that walks
/// a serialized resolved AST without decoding it into the prost types. Empty on docs.rs.
pub const RESOLVED_AST_DESCRIPTORS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/resolved_ast.fds"));

#[repr(C)]
struct Buf {
    data: *mut u8,
    len: usize,
}

// SAFETY: a `Buf` owns bytes the shim allocated; any thread may read and free them (`take`).
unsafe impl Send for Buf {}

#[repr(C)]
struct RawResult {
    code: i32,
    proto: Buf,
    error: Buf,
}

#[repr(C)]
struct RawCatalog {
    _private: [u8; 0],
}

#[repr(C)]
struct RawTable {
    _private: [u8; 0],
}

#[repr(C)]
struct RawProtos {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn gsql_catalog_new(json: *const c_char, len: usize, error: *mut Buf) -> *mut RawCatalog;
    fn gsql_catalog_free(catalog: *mut RawCatalog);
    fn gsql_table_new(json: *const c_char, len: usize, error: *mut Buf) -> *mut RawTable;
    fn gsql_table_free(table: *mut RawTable);
    fn gsql_protos_new(
        descriptors: *const u8,
        descriptors_len: usize,
        types: *const c_char,
        types_len: usize,
        error: *mut Buf,
    ) -> *mut RawProtos;
    fn gsql_protos_free(protos: *mut RawProtos);
    fn gsql_table_new_with_protos(
        json: *const c_char,
        len: usize,
        protos: *const RawProtos,
        error: *mut Buf,
    ) -> *mut RawTable;
    fn gsql_catalog_from_tables_with_protos(
        tables: *const *const RawTable,
        n: usize,
        features: *const c_char,
        features_len: usize,
        protos: *const RawProtos,
        error: *mut Buf,
    ) -> *mut RawCatalog;
    fn gsql_analyze(
        catalog: *const RawCatalog,
        sql: *const c_char,
        sql_len: usize,
        params: *const c_char,
        params_len: usize,
    ) -> RawResult;
    fn gsql_catalog_add_property_graph(
        catalog: *mut RawCatalog,
        json: *const c_char,
        len: usize,
        error: *mut Buf,
    ) -> i32;
    fn gsql_analyze_ddl(
        catalog: *const RawCatalog,
        sql: *const c_char,
        sql_len: usize,
    ) -> RawResult;
    fn gsql_analyze_view(
        catalog: *const RawCatalog,
        sql: *const c_char,
        sql_len: usize,
        exact: *const c_char,
        exact_len: usize,
    ) -> RawResult;
    fn gsql_buf_free(buf: Buf);
}

fn take(buf: Buf) -> Vec<u8> {
    if buf.data.is_null() {
        return Vec::new();
    }
    // SAFETY: the shim allocated `len` bytes at `data`; we copy then free them exactly once.
    let v = unsafe { std::slice::from_raw_parts(buf.data, buf.len).to_vec() };
    unsafe { gsql_buf_free(buf) };
    v
}

/// An analyser error: an `absl::StatusCode` (same numbering as gRPC codes) and its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "googlesql status {}: {}", self.code, self.message)
    }
}

impl std::error::Error for AnalyzeError {}

fn empty_buf() -> Buf {
    Buf {
        data: std::ptr::null_mut(),
        len: 0,
    }
}

fn build_error(err: Buf) -> AnalyzeError {
    AnalyzeError {
        code: 3,
        message: String::from_utf8_lossy(&take(err)).into_owned(),
    }
}

/// The types of a proto bundle (`CREATE PROTO BUNDLE`), immutable. Tables and catalogs built
/// with them hold an `Arc`, so they outlive every user.
pub struct Protos(NonNull<RawProtos>);

// SAFETY: the shim never mutates bundle types after construction except through GoogleSQL's
// TypeFactory, which is thread-safe.
unsafe impl Send for Protos {}
unsafe impl Sync for Protos {}

impl Protos {
    /// The bundle's `types` (full names: messages, nested messages and enums) from
    /// `descriptors`, a serialized `google.protobuf.FileDescriptorSet`. Imports the set leaves
    /// out (`google/protobuf/*.proto`) come from the protos compiled into the shim.
    pub fn new(descriptors: &[u8], types: &[&str]) -> Result<Self, AnalyzeError> {
        let types = serde_json_array(types);
        let mut err = empty_buf();
        // SAFETY: pointers are valid for the given lengths for the duration of the call.
        let raw = unsafe {
            gsql_protos_new(
                descriptors.as_ptr(),
                descriptors.len(),
                types.as_ptr().cast(),
                types.len(),
                &mut err,
            )
        };
        NonNull::new(raw).map(Self).ok_or_else(|| build_error(err))
    }
}

impl Drop for Protos {
    fn drop(&mut self) {
        // SAFETY: created by gsql_protos_new, freed once, after every table and catalog that
        // uses it (they hold an Arc).
        unsafe { gsql_protos_free(self.0.as_ptr()) }
    }
}

/// A JSON array of strings.
fn serde_json_array(items: &[&str]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        })
        .collect();
    format!("[{}]", quoted.join(","))
}

/// One GoogleSQL table definition, immutable. Any number of catalogs can share it, so a table
/// that a DDL leaves unchanged is built once for every schema version that has it.
pub struct Table {
    raw: NonNull<RawTable>,
    /// The bundle types its columns use; dropped after `raw`.
    _protos: Option<Arc<Protos>>,
}

// SAFETY: the shim never mutates a table after construction.
unsafe impl Send for Table {}
unsafe impl Sync for Table {}

impl Table {
    /// Builds a table from `{"name":"schema.T","columns":[...]}` (the column format of
    /// [`Catalog::new`]).
    pub fn new(json: &str) -> Result<Self, AnalyzeError> {
        let mut err = empty_buf();
        // SAFETY: pointers are valid for the given lengths for the duration of the call.
        let raw = unsafe { gsql_table_new(json.as_ptr().cast(), json.len(), &mut err) };
        NonNull::new(raw)
            .map(|raw| Self { raw, _protos: None })
            .ok_or_else(|| build_error(err))
    }

    /// [`Table::new`] where column types may also be `PROTO<x>`, `ENUM<x>` and arrays of them,
    /// for types of `protos`.
    pub fn with_protos(json: &str, protos: &Arc<Protos>) -> Result<Self, AnalyzeError> {
        let mut err = empty_buf();
        // SAFETY: pointers are valid for the given lengths for the duration of the call; the
        // table keeps `protos` alive.
        let raw = unsafe {
            gsql_table_new_with_protos(
                json.as_ptr().cast(),
                json.len(),
                protos.0.as_ptr().cast_const(),
                &mut err,
            )
        };
        NonNull::new(raw)
            .map(|raw| Self {
                raw,
                _protos: Some(protos.clone()),
            })
            .ok_or_else(|| build_error(err))
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        // SAFETY: created by gsql_table_new, freed once. Catalogs that refer to it hold an Arc.
        unsafe { gsql_table_free(self.raw.as_ptr()) }
    }
}

/// A GoogleSQL catalog (tables + built-in and Spanner functions). Safe to share across threads:
/// analysis only reads it.
pub struct Catalog {
    raw: NonNull<RawCatalog>,
    /// Tables the catalog refers to; dropped after `raw` (see `Drop`).
    _tables: Vec<Arc<Table>>,
    /// The bundle types SQL can name; dropped after `raw`.
    _protos: Option<Arc<Protos>>,
}

// SAFETY: the shim never mutates the catalog after construction; each analysis uses its own
// TypeFactory, and SimpleCatalog lookups are internally locked.
unsafe impl Send for Catalog {}
unsafe impl Sync for Catalog {}

impl Catalog {
    /// Builds a catalog from the shim's JSON format (see `native/googlesql/shim/googlesql_shim.h`).
    pub fn new(json: &str) -> Result<Self, AnalyzeError> {
        let mut err = empty_buf();
        // SAFETY: pointers are valid for the given lengths for the duration of the call.
        let raw = unsafe { gsql_catalog_new(json.as_ptr().cast(), json.len(), &mut err) };
        match NonNull::new(raw) {
            Some(raw) => Ok(Self {
                raw,
                _tables: Vec::new(),
                _protos: None,
            }),
            None => Err(build_error(err)),
        }
    }

    /// A catalog over shared `tables` plus the builtin and Spanner functions, which the shim
    /// builds once per process for each `extra_features` set (language feature names, and
    /// `SPANBOLT_PRODUCTION` for Cloud Spanner's function signatures). Cheap: it
    /// builds no table and no function. Tables in named schemas (`schema.T`) go in nested
    /// catalogs. Names must be unique.
    pub fn from_tables(
        tables: Vec<Arc<Table>>,
        extra_features: &[&str],
    ) -> Result<Self, AnalyzeError> {
        Self::from_tables_with_protos(tables, extra_features, None)
    }

    /// [`Catalog::from_tables`] where SQL can also name the types of `protos`: in `CAST`, `NEW`
    /// and parameter types (`PROTO<x>`, `ENUM<x>`). Tables must use the same `protos`, if any.
    pub fn from_tables_with_protos(
        tables: Vec<Arc<Table>>,
        extra_features: &[&str],
        protos: Option<Arc<Protos>>,
    ) -> Result<Self, AnalyzeError> {
        let features = if extra_features.is_empty() {
            String::new()
        } else {
            serde_json_array(extra_features)
        };
        let ptrs: Vec<*const RawTable> =
            tables.iter().map(|t| t.raw.as_ptr().cast_const()).collect();
        let protos_ptr = protos
            .as_ref()
            .map_or(std::ptr::null(), |p| p.0.as_ptr().cast_const());
        let mut err = empty_buf();
        // SAFETY: `ptrs` holds live tables (kept alive by `tables`, stored in the catalog);
        // string pointers valid for their lengths for the call.
        let raw = unsafe {
            gsql_catalog_from_tables_with_protos(
                ptrs.as_ptr(),
                ptrs.len(),
                features.as_ptr().cast(),
                features.len(),
                protos_ptr,
                &mut err,
            )
        };
        match NonNull::new(raw) {
            Some(raw) => Ok(Self {
                raw,
                _tables: tables,
                _protos: protos,
            }),
            None => Err(build_error(err)),
        }
    }

    /// Adds a property graph (the shim's graph JSON, see `googlesql_shim.h`) over this catalog's
    /// tables. Takes `&mut self`: graphs are added before the catalog is shared.
    pub fn add_property_graph(&mut self, json: &str) -> Result<(), AnalyzeError> {
        let mut err = empty_buf();
        // SAFETY: catalog pointer is live and exclusively borrowed; json valid for its length.
        let code = unsafe {
            gsql_catalog_add_property_graph(
                self.raw.as_ptr(),
                json.as_ptr().cast(),
                json.len(),
                &mut err,
            )
        };
        let message = String::from_utf8_lossy(&take(err)).into_owned();
        if code == 0 {
            Ok(())
        } else {
            Err(AnalyzeError { code, message })
        }
    }

    /// Analyses a `CREATE [OR REPLACE] PROPERTY GRAPH` statement against this catalog's tables.
    pub fn analyze_property_graph_ddl(
        &self,
        sql: &str,
    ) -> Result<proto::AnyResolvedStatementProto, AnalyzeError> {
        let raw = RawPtr(self.raw);
        // SAFETY: catalog pointer is live for &self; sql valid for its length.
        let r = with_stack_for(sql.len(), move || unsafe {
            gsql_analyze_ddl(raw.get(), sql.as_ptr().cast(), sql.len())
        });
        decode_result(r)
    }

    /// Analyses the query of a view as Spanner does: in strict name resolution mode (column names
    /// need a qualifier, no `SELECT *`), without parameters. The tables in `exact_json` (a JSON
    /// array of full names) are found only by that spelling.
    pub fn analyze_view(
        &self,
        sql: &str,
        exact_json: &str,
    ) -> Result<proto::AnyResolvedStatementProto, AnalyzeError> {
        let raw = RawPtr(self.raw);
        // SAFETY: catalog pointer is live for &self; string pointers valid for their lengths.
        let r = with_stack_for(sql.len(), move || unsafe {
            gsql_analyze_view(
                raw.get(),
                sql.as_ptr().cast(),
                sql.len(),
                exact_json.as_ptr().cast(),
                exact_json.len(),
            )
        });
        decode_result(r)
    }

    /// Analyses one statement; `params_json` maps parameter names to GoogleSQL type names.
    /// Returns the serialized `AnyResolvedStatementProto`.
    pub fn analyze_raw(&self, sql: &str, params_json: &str) -> Result<Vec<u8>, AnalyzeError> {
        let raw = RawPtr(self.raw);
        // SAFETY: catalog pointer is live for &self; string pointers valid for their lengths.
        let r = with_stack_for(sql.len(), move || unsafe {
            gsql_analyze(
                raw.get(),
                sql.as_ptr().cast(),
                sql.len(),
                params_json.as_ptr().cast(),
                params_json.len(),
            )
        });
        let proto = take(r.proto);
        let error = take(r.error);
        if r.code == 0 {
            Ok(proto)
        } else {
            Err(AnalyzeError {
                code: r.code,
                message: String::from_utf8_lossy(&error).into_owned(),
            })
        }
    }

    /// Analyses and decodes into prost types.
    pub fn analyze(
        &self,
        sql: &str,
        params_json: &str,
    ) -> Result<proto::AnyResolvedStatementProto, AnalyzeError> {
        let bytes = self.analyze_raw(sql, params_json)?;
        decode_statement(&bytes)
    }
}

/// The catalog pointer, sent to the thread [`with_stack_for`] may run the analyser on.
#[derive(Clone, Copy)]
struct RawPtr(NonNull<RawCatalog>);
// SAFETY: the catalog is `Sync` (see above), and the pointer outlives the scoped thread.
unsafe impl Send for RawPtr {}

impl RawPtr {
    /// A method, so that a closure captures the whole `RawPtr` (which is `Send`), not its field.
    fn get(self) -> *mut RawCatalog {
        self.0.as_ptr()
    }
}

/// Statements up to this long analyse on the calling thread: at most 512 levels of nesting.
const INLINE_SQL_BYTES: usize = 1024;

/// Runs `f`, which analyses (or walks the analysis of) `sql_len` bytes of SQL, on a stack deep
/// enough for it. GoogleSQL recurses once per level of a nested expression, up to about 2.2 KiB a
/// level, and its stack guard doesn't stop it before a 2 MiB thread overflows (about 1000 levels),
/// which aborts the process. A level takes at least 3 bytes of SQL (`-(`...`)`; a `+` chain, 2 bytes
/// a level, stops at the parser's 20000), so a longer statement runs on a scoped thread with
/// 1 KiB of stack per byte: address space, used only as deep as the recursion goes.
pub fn with_stack_for<R: Send>(sql_len: usize, f: impl FnOnce() -> R + Send) -> R {
    if sql_len <= INLINE_SQL_BYTES {
        return f();
    }
    let size = (16 << 20) + sql_len.min(2 << 20) * 1024;
    std::thread::scope(|s| {
        let thread = std::thread::Builder::new()
            .name("googlesql-deep".into())
            .stack_size(size)
            .spawn_scoped(s, f)
            .expect("spawn an analyser thread");
        thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// Decodes a serialized resolved AST. Expressions, scans and STRUCT types nest past prost's limit of
/// 100 messages (build.rs), and a level of them takes up to tens of KiB of stack to decode: an AST
/// deeper than [`deep::INLINE`] decodes again on a thread with a stack for [`deep::MAX`].
pub fn decode_statement(bytes: &[u8]) -> Result<proto::AnyResolvedStatementProto, AnalyzeError> {
    decode_statement_within(bytes, deep::MAX.types)
}

/// [`decode_statement`], with types nested at most `types` levels (at most [`deep::MAX`]'s).
pub fn decode_statement_within(
    bytes: &[u8],
    types: u32,
) -> Result<proto::AnyResolvedStatementProto, AnalyzeError> {
    let max = deep::Limits {
        types: types.min(deep::MAX.types),
        ..deep::MAX
    };
    let inline = deep::Limits {
        types: deep::INLINE.types.min(max.types),
        ..deep::INLINE
    };
    let (mut r, mut too_deep) = deep::decode(bytes, inline);
    if too_deep.is_some() {
        (r, too_deep) = std::thread::scope(|s| {
            std::thread::Builder::new()
                .name("googlesql-decode".into())
                .stack_size(deep::STACK)
                .spawn_scoped(s, || deep::decode(bytes, max))
                .expect("spawn a decoder thread")
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    }
    r.map_err(|e| match too_deep {
        // What the emulator's analyser and Spanner's answer when they run out of stack on a deep
        // expression or type.
        Some(_) => AnalyzeError {
            code: 8,
            message: "Out of stack space due to deeply nested query expression during query \
                      resolution"
                .into(),
        },
        // The error names every message it is nested in: too long for a gRPC status.
        None => AnalyzeError {
            code: 13,
            message: format!("decode AnyResolvedStatementProto: {e}"),
        },
    })
}

/// Depth of the expression and scan nodes, and of the STRUCT types, being decoded on this thread,
/// for the wrappers build.rs generates.
#[doc(hidden)]
pub mod deep {
    use std::cell::Cell;

    /// How deep a decode goes: nested expression and scan nodes, and nested STRUCT types.
    #[derive(Clone, Copy)]
    pub struct Limits {
        pub nodes: u32,
        pub types: u32,
    }

    /// Decoded on the caller's stack: ordinary statements.
    pub const INLINE: Limits = Limits {
        nodes: 16,
        types: 32,
    };
    /// The most decoded at all. Spanner's limits stop a query at 75 nested calls, 25 nested
    /// subqueries and 15 nested STRUCT parameters well before `nodes`, but not a chain of field
    /// accesses (`(STRUCT(STRUCT(1 AS f) AS f)).f.f`): Spanner answers 400 of them and runs out of
    /// stack before 500, the emulator answers 5000 (and stops before 6000). Deeper than `nodes`, a
    /// statement fails the way both do when they run out of stack. `--emulator-compat` keeps the
    /// same `nodes`: every node of the serialized AST carries its whole type, so N nested STRUCT
    /// constructors serialize N²/2 levels of type. Without the limit, 1000 constructors under 1000
    /// field accesses took 1.5 GB to analyse and decode, 5000 over 4 GB (#172). A type has no limit
    /// of its own in the emulator
    /// (`CAST(NULL AS STRUCT<f STRUCT<...>>)` answers 5000 levels, until its analyser runs out of
    /// stack; Spanner about 1200), but the engine converts and drops types recursively, on 2 MiB
    /// handler threads: about 6500 nested STRUCT and ARRAY types overflow one.
    pub const MAX: Limits = Limits {
        nodes: 500,
        types: 2000,
    };
    /// Stack for [`MAX`] (address space, used only as deep as the AST goes).
    pub const STACK: usize = 64 << 20;
    /// What [`Level::enter_type`] counts, as [`decode`] reports it.
    pub const TYPES: &str = "types";

    thread_local! {
        static NODES: Cell<u32> = const { Cell::new(0) };
        static TYPE_LEVELS: Cell<u32> = const { Cell::new(0) };
        static LIMITS: Cell<Limits> = const { Cell::new(INLINE) };
        static TOO_DEEP: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    /// One nested expression or scan node, or STRUCT field type, for as long as it decodes.
    pub struct Level(&'static std::thread::LocalKey<Cell<u32>>);

    impl Level {
        /// An expression or scan node.
        pub fn enter() -> Result<Level, prost::DecodeError> {
            Self::enter_in(&NODES, LIMITS.get().nodes, "expressions")
        }

        /// The type of a STRUCT field or ARRAY element, or a STRUCT value's fields.
        pub fn enter_type() -> Result<Level, prost::DecodeError> {
            Self::enter_in(&TYPE_LEVELS, LIMITS.get().types, TYPES)
        }

        fn enter_in(
            counter: &'static std::thread::LocalKey<Cell<u32>>,
            limit: u32,
            what: &'static str,
        ) -> Result<Level, prost::DecodeError> {
            let depth = counter.get() + 1;
            if depth > limit {
                TOO_DEEP.set(Some(what));
                #[allow(deprecated)]
                return Err(prost::DecodeError::new("nested too deep"));
            }
            counter.set(depth);
            Ok(Level(counter))
        }
    }

    impl Drop for Level {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    /// Decodes `bytes` within `limits`, and what went past them.
    pub(crate) fn decode<M: prost::Message + Default>(
        bytes: &[u8],
        limits: Limits,
    ) -> (Result<M, prost::DecodeError>, Option<&'static str>) {
        LIMITS.set(limits);
        TOO_DEEP.set(None);
        let r = M::decode(bytes);
        LIMITS.set(INLINE);
        (r, TOO_DEEP.replace(None))
    }
}

fn decode_result(r: RawResult) -> Result<proto::AnyResolvedStatementProto, AnalyzeError> {
    let bytes = take(r.proto);
    let error = take(r.error);
    if r.code != 0 {
        return Err(AnalyzeError {
            code: r.code,
            message: String::from_utf8_lossy(&error).into_owned(),
        });
    }
    decode_statement(&bytes)
}

impl Drop for Catalog {
    fn drop(&mut self) {
        // SAFETY: created by gsql_catalog_new/from_tables, freed once, before the tables it
        // refers to (fields drop after this).
        unsafe { gsql_catalog_free(self.raw.as_ptr()) }
    }
}
