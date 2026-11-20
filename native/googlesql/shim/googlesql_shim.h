#ifndef SPANBOLT_GOOGLESQL_SHIM_H_
#define SPANBOLT_GOOGLESQL_SHIM_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct gsql_catalog gsql_catalog;
typedef struct gsql_table gsql_table;

// Bytes owned by the shim. Free with gsql_buf_free.
typedef struct {
  uint8_t* data;
  size_t len;
} gsql_buf;

// Result of an analyse call. `code` is an absl::StatusCode (0 = OK).
// On OK, `proto` holds a serialized googlesql.AnyResolvedStatementProto.
// Otherwise `error` holds a UTF-8 message.
typedef struct {
  int32_t code;
  gsql_buf proto;
  gsql_buf error;
} gsql_result;

// Builds a catalog from a JSON document:
// {"tables":[{"name":"T","columns":[{"name":"c","type":"ARRAY<STRING(MAX)>"}]}],
//  "extra_features":["FEATURE_QUALIFY"]}
// Returns NULL on failure and sets *error (free with gsql_buf_free).
gsql_catalog* gsql_catalog_new(const char* json, size_t json_len,
                               gsql_buf* error);
void gsql_catalog_free(gsql_catalog* catalog);

// Builds one table from {"name":"schema.T","columns":[...],"primary_key":[0]}
// (column format as above; primary_key, column positions, is optional). Tables are immutable and may be shared by any number of catalogs.
// Returns NULL on failure and sets *error.
gsql_table* gsql_table_new(const char* json, size_t json_len, gsql_buf* error);
// Frees a table. Every catalog that refers to it must be freed first.
void gsql_table_free(gsql_table* table);

// Builds a catalog over `tables` (not owned: they must outlive the catalog)
// and the process-wide builtin functions for `features_json`, a JSON array of
// extra language features (empty or NULL for none). Cheap: no table or
// function is built. Returns NULL on failure and sets *error.
gsql_catalog* gsql_catalog_from_tables(const gsql_table* const* tables,
                                       size_t n, const char* features_json,
                                       size_t features_len, gsql_buf* error);

// Analyses one statement. params_json maps parameter names to type names:
// {"ids":"ARRAY<STRING>","q":"STRING"}. Undeclared parameters are an error.
gsql_result gsql_analyze(const gsql_catalog* catalog, const char* sql,
                         size_t sql_len, const char* params_json,
                         size_t params_len);

// Adds a property graph to `catalog` (call before the catalog is shared). The
// graph JSON names element tables of the catalog:
// {"name":"G","properties":[{"name":"id","type":"STRING"}],
//  "labels":[{"name":"Person","properties":["id"]}],
//  "nodes":[{"name":"Person","table":"Person","key":["id"],
//            "labels":["Person"],"properties":[{"name":"id","sql":"id"}]}],
//  "edges":[{<node fields>,
//            "source":{"node":"Person","edge_columns":["src"],
//                      "node_columns":["id"]},"destination":{...}}]}
// Returns an absl::StatusCode (0 = OK) and sets *error otherwise.
int32_t gsql_catalog_add_property_graph(gsql_catalog* catalog,
                                        const char* json, size_t json_len,
                                        gsql_buf* error);

// Analyses a CREATE [OR REPLACE] PROPERTY GRAPH statement against the
// catalog's tables. On OK, `proto` holds an AnyResolvedStatementProto with a
// ResolvedCreatePropertyGraphStmt.
gsql_result gsql_analyze_ddl(const gsql_catalog* catalog, const char* sql,
                             size_t sql_len);

void gsql_buf_free(gsql_buf buf);

#ifdef __cplusplus
}
#endif

#endif  // SPANBOLT_GOOGLESQL_SHIM_H_
