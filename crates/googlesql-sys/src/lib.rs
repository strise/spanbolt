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

unsafe extern "C" {
    fn gsql_catalog_new(json: *const c_char, len: usize, error: *mut Buf) -> *mut RawCatalog;
    fn gsql_catalog_free(catalog: *mut RawCatalog);
    fn gsql_table_new(json: *const c_char, len: usize, error: *mut Buf) -> *mut RawTable;
    fn gsql_table_free(table: *mut RawTable);
    fn gsql_catalog_from_tables(
        tables: *const *const RawTable,
        n: usize,
        features: *const c_char,
        features_len: usize,
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

/// One GoogleSQL table definition, immutable. Any number of catalogs can share it, so a table
/// that a DDL leaves unchanged is built once for every schema version that has it.
pub struct Table(NonNull<RawTable>);

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
        NonNull::new(raw).map(Self).ok_or_else(|| build_error(err))
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        // SAFETY: created by gsql_table_new, freed once. Catalogs that refer to it hold an Arc.
        unsafe { gsql_table_free(self.0.as_ptr()) }
    }
}

/// A GoogleSQL catalog (tables + built-in and Spanner functions). Safe to share across threads:
/// analysis only reads it.
pub struct Catalog {
    raw: NonNull<RawCatalog>,
    /// Tables the catalog refers to; dropped after `raw` (see `Drop`).
    _tables: Vec<Arc<Table>>,
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
            }),
            None => Err(build_error(err)),
        }
    }

    /// A catalog over shared `tables` plus the builtin and Spanner functions, which the shim
    /// builds once per process for each `extra_features` set (language feature names). Cheap: it
    /// builds no table and no function. Tables in named schemas (`schema.T`) go in nested
    /// catalogs. Names must be unique.
    pub fn from_tables(
        tables: Vec<Arc<Table>>,
        extra_features: &[&str],
    ) -> Result<Self, AnalyzeError> {
        let features = if extra_features.is_empty() {
            String::new()
        } else {
            let quoted: Vec<String> = extra_features.iter().map(|f| format!("\"{f}\"")).collect();
            format!("[{}]", quoted.join(","))
        };
        let ptrs: Vec<*const RawTable> = tables.iter().map(|t| t.0.as_ptr().cast_const()).collect();
        let mut err = empty_buf();
        // SAFETY: `ptrs` holds live tables (kept alive by `tables`, stored in the catalog);
        // string pointers valid for their lengths for the call.
        let raw = unsafe {
            gsql_catalog_from_tables(
                ptrs.as_ptr(),
                ptrs.len(),
                features.as_ptr().cast(),
                features.len(),
                &mut err,
            )
        };
        match NonNull::new(raw) {
            Some(raw) => Ok(Self {
                raw,
                _tables: tables,
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
        // SAFETY: catalog pointer is live for &self; sql valid for its length.
        let r = unsafe { gsql_analyze_ddl(self.raw.as_ptr(), sql.as_ptr().cast(), sql.len()) };
        decode_result(r)
    }

    /// Analyses one statement; `params_json` maps parameter names to GoogleSQL type names.
    /// Returns the serialized `AnyResolvedStatementProto`.
    pub fn analyze_raw(&self, sql: &str, params_json: &str) -> Result<Vec<u8>, AnalyzeError> {
        // SAFETY: catalog pointer is live for &self; string pointers valid for their lengths.
        let r = unsafe {
            gsql_analyze(
                self.raw.as_ptr(),
                sql.as_ptr().cast(),
                sql.len(),
                params_json.as_ptr().cast(),
                params_json.len(),
            )
        };
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
        prost::Message::decode(bytes.as_slice()).map_err(|e| AnalyzeError {
            code: 13,
            message: format!("decode AnyResolvedStatementProto: {e}"),
        })
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
    prost::Message::decode(bytes.as_slice()).map_err(|e| AnalyzeError {
        code: 13,
        message: format!("decode AnyResolvedStatementProto: {e}"),
    })
}

impl Drop for Catalog {
    fn drop(&mut self) {
        // SAFETY: created by gsql_catalog_new/from_tables, freed once, before the tables it
        // refers to (fields drop after this).
        unsafe { gsql_catalog_free(self.raw.as_ptr()) }
    }
}
