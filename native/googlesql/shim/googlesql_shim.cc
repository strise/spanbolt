#include "shim/googlesql_shim.h"

#include <cstdlib>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <utility>
#include <vector>

#include "absl/container/flat_hash_map.h"
#include "absl/status/status.h"
#include "absl/status/statusor.h"
#include "absl/strings/str_cat.h"
#include "absl/time/time.h"
#include "googlesql/public/analyzer.h"
#include "googlesql/public/analyzer_options.h"
#include "googlesql/public/analyzer_output.h"
#include "googlesql/public/builtin_function_options.h"
#include "googlesql/public/language_options.h"
#include "googlesql/public/multi_catalog.h"
#include "googlesql/public/options.pb.h"
#include "googlesql/public/simple_catalog.h"
#include "googlesql/public/types/type_factory.h"
#include "googlesql/resolved_ast/resolved_ast.h"
#include "googlesql/resolved_ast/resolved_ast.pb.h"
#include "googlesql/resolved_ast/resolved_node_kind.pb.h"
#include "nlohmann/json.hpp"
#include "re2/re2.h"
#include "shim/spanner_functions.h"

// One table definition. Catalogs refer to tables without owning them, so a
// table unchanged by a DDL is shared by every schema version that has it.
struct gsql_table {
  std::unique_ptr<googlesql::SimpleTable> table;
  std::string full_name;
};

// Process-wide state for one language feature set: the builtin and Spanner
// functions, built once instead of per catalog, and the analyser options.
// Never freed: there are only a handful of feature sets.
struct SharedBase {
  googlesql::SimpleCatalog builtins;
  googlesql::AnalyzerOptions options;
  explicit SharedBase(googlesql::TypeFactory* types)
      : builtins("spanner_builtins", types) {}
};

struct gsql_catalog {
  // Named schemas (`monitoring.alerts`) are nested catalogs of `tables`.
  std::unique_ptr<googlesql::SimpleCatalog> tables;
  // `tables` first, then the shared builtins.
  std::unique_ptr<googlesql::MultiCatalog> catalog;
  // Tables of the JSON constructor; shared tables belong to the caller.
  std::vector<std::unique_ptr<gsql_table>> owned;
  googlesql::AnalyzerOptions options;
};

namespace {

using json = nlohmann::json;

gsql_buf CopyBytes(const std::string& s) {
  gsql_buf buf{nullptr, s.size()};
  if (!s.empty()) {
    buf.data = static_cast<uint8_t*>(std::malloc(s.size()));
    std::memcpy(buf.data, s.data(), s.size());
  }
  return buf;
}

gsql_result Error(const absl::Status& status) {
  return gsql_result{static_cast<int32_t>(status.code()), gsql_buf{nullptr, 0},
                     CopyBytes(std::string(status.message()))};
}

// Spanner's language options, copied from cloud-spanner-emulator
// backend/query/analyzer_options.cc (Apache-2.0).
googlesql::LanguageOptions SpannerLanguageOptions() {
  googlesql::LanguageOptions options;
  options.set_name_resolution_mode(googlesql::NAME_RESOLUTION_DEFAULT);
  options.set_product_mode(googlesql::PRODUCT_EXTERNAL);
  options.SetEnabledLanguageFeatures({
      googlesql::FEATURE_EXTENDED_TYPES,
      googlesql::FEATURE_FUNCTION_ARGUMENTS_WITH_DEFAULTS,
      googlesql::FEATURE_NAMED_ARGUMENTS,
      googlesql::FEATURE_NUMERIC_TYPE,
      googlesql::FEATURE_TABLESAMPLE,
      googlesql::FEATURE_TIMESTAMP_NANOS,
      googlesql::FEATURE_HAVING_IN_AGGREGATE,
      googlesql::FEATURE_NULL_HANDLING_MODIFIER_IN_AGGREGATE,
      googlesql::FEATURE_ORDER_BY_IN_AGGREGATE,
      googlesql::FEATURE_LIMIT_IN_AGGREGATE,
      googlesql::FEATURE_ORDER_BY_COLLATE,
      googlesql::FEATURE_SELECT_STAR_EXCEPT_REPLACE,
      googlesql::FEATURE_SAFE_FUNCTION_CALL,
      googlesql::FEATURE_JSON_KEYS_FUNCTION,
      googlesql::FEATURE_JSON_TYPE,
      googlesql::FEATURE_JSON_ARRAY_FUNCTIONS,
      googlesql::FEATURE_JSON_CONSTRUCTOR_FUNCTIONS,
      googlesql::FEATURE_JSON_CONTAINS_FUNCTION,
      googlesql::FEATURE_JSON_MUTATOR_FUNCTIONS,
      googlesql::FEATURE_JSON_STRICT_NUMBER_PARSING,
      googlesql::FEATURE_JSON_VALUE_EXTRACTION_FUNCTIONS,
      googlesql::FEATURE_JSON_LAX_VALUE_EXTRACTION_FUNCTIONS,
      googlesql::FEATURE_DML_RETURNING,
      googlesql::FEATURE_WITH_EXPRESSION,
      googlesql::FEATURE_TABLE_VALUED_FUNCTIONS,
      googlesql::FEATURE_TOKENIZED_SEARCH,
      googlesql::FEATURE_ADDITIONAL_STRING_FUNCTIONS,
      googlesql::FEATURE_SEQUENCE_ARG,
      googlesql::FEATURE_JSON_ARRAY_VALUE_EXTRACTION_FUNCTIONS,
      googlesql::FEATURE_JSON_MORE_VALUE_EXTRACTION_FUNCTIONS,
      googlesql::FEATURE_JSON_TYPE_COMPARISON,
      googlesql::FEATURE_JSON_TYPE_COMPARISON_COERCION,
      googlesql::FEATURE_CAST_TO_JSON_TYPE,
      googlesql::FEATURE_ENABLE_FLOAT_DISTANCE_FUNCTIONS,
      googlesql::FEATURE_DOT_PRODUCT,
      googlesql::FEATURE_INTERVAL_TYPE,
      googlesql::FEATURE_UUID_TYPE,
      googlesql::FEATURE_FOR_UPDATE,
      googlesql::FEATURE_INLINE_LAMBDA_ARGUMENT,
      googlesql::FEATURE_IS_DISTINCT,
  });
  options.SetSupportedStatementKinds({
      googlesql::RESOLVED_QUERY_STMT,
      googlesql::RESOLVED_INSERT_STMT,
      googlesql::RESOLVED_UPDATE_STMT,
      googlesql::RESOLVED_DELETE_STMT,
      googlesql::RESOLVED_CALL_STMT,
  });
  return options;
}

googlesql::BuiltinFunctionOptions SpannerBuiltinFunctionOptions(
    const googlesql::LanguageOptions& language) {
  googlesql::BuiltinFunctionOptions options(language);
  for (auto id : {googlesql::FN_COSINE_DISTANCE_SPARSE_INT64,
                  googlesql::FN_COSINE_DISTANCE_SPARSE_STRING,
                  googlesql::FN_EUCLIDEAN_DISTANCE_SPARSE_INT64,
                  googlesql::FN_EUCLIDEAN_DISTANCE_SPARSE_STRING,
                  googlesql::FN_INSTR_STRING, googlesql::FN_INSTR_BYTES,
                  googlesql::FN_TRANSLATE_STRING,
                  googlesql::FN_TRANSLATE_BYTES, googlesql::FN_INITCAP_STRING}) {
    options.exclude_function_ids.insert(id);
  }
  return options;
}

// Spanner DDL spells STRING(MAX) / BYTES(1024); GoogleSQL types have no length.
std::string NormalizeTypeName(std::string type) {
  static const LazyRE2 kLength = {R"((?i)\b(STRING|BYTES)\s*\(\s*(MAX|\d+)\s*\))"};
  RE2::GlobalReplace(&type, *kLength, R"(\1)");
  return type;
}

// Types are made by one process-wide factory, so tables and builtin functions
// built at different times can be mixed in one catalog. TypeFactory is
// thread-safe. Never freed.
googlesql::TypeFactory* GlobalTypes() {
  static googlesql::TypeFactory* types = new googlesql::TypeFactory();
  return types;
}

absl::StatusOr<const googlesql::Type*> ParseType(
    const std::string& name, const googlesql::AnalyzerOptions& options,
    googlesql::Catalog* catalog, googlesql::TypeFactory* factory) {
  const googlesql::Type* type = nullptr;
  absl::Status status = googlesql::AnalyzeType(NormalizeTypeName(name), options,
                                               catalog, factory, &type);
  if (!status.ok()) {
    return absl::InvalidArgumentError(
        absl::StrCat("bad type '", name, "': ", status.message()));
  }
  return type;
}

// The shared base for a feature set (the catalog JSON's `extra_features`).
absl::StatusOr<SharedBase*> BaseFor(const json& features) {
  static std::mutex mu;
  static auto* bases = new std::map<std::string, SharedBase*>();
  std::string key = features.is_null() ? "[]" : features.dump();
  std::lock_guard<std::mutex> lock(mu);
  if (auto it = bases->find(key); it != bases->end()) return it->second;

  googlesql::LanguageOptions language = SpannerLanguageOptions();
  if (!features.is_null()) {
    for (const auto& f : features) {
      googlesql::LanguageFeature feature;
      if (!f.is_string() ||
          !googlesql::LanguageFeature_Parse(f.get<std::string>(), &feature)) {
        return absl::InvalidArgumentError(
            absl::StrCat("unknown language feature ", f.dump()));
      }
      language.EnableLanguageFeature(feature);
    }
  }
  auto base = std::make_unique<SharedBase>(GlobalTypes());
  googlesql::AnalyzerOptions& options = base->options;
  options.set_language(language);
  options.set_parameter_mode(googlesql::PARAMETER_NAMED);
  options.set_error_message_mode(
      googlesql::ERROR_MESSAGE_ONE_LINE);
  options.set_allow_undeclared_parameters(false);
  options.set_prune_unused_columns(true);
  // Built-ins arrive as calls, not as GoogleSQL's SQL bodies (DOT_PRODUCT,
  // ARRAY_FIRST, ARRAY_FILTER, ...): Rust evaluates them with Spanner's errors.
  options.enable_rewrite(googlesql::REWRITE_BUILTIN_FUNCTION_INLINER, false);
  // Spanner's default time zone; absl falls back to UTC if zoneinfo is missing.
  absl::TimeZone zone;
  if (!absl::LoadTimeZone("America/Los_Angeles", &zone)) zone = absl::UTCTimeZone();
  options.set_default_time_zone(zone);

  absl::Status status = base->builtins.AddBuiltinFunctionsAndTypes(
      SpannerBuiltinFunctionOptions(language));
  if (!status.ok()) return status;
  spanbolt::AddSpannerFunctions(&base->builtins, GlobalTypes());
  SharedBase* out = base.release();
  bases->emplace(key, out);
  return out;
}

// The base used to parse column types: column types never depend on extra
// language features.
absl::StatusOr<SharedBase*> DefaultBase() { return BaseFor(json()); }

// Parses each distinct type string once per process: AnalyzeType copies the
// analyser and language options and runs the parser per call, and a schema of
// hundreds of tables has only a few dozen distinct column types.
absl::StatusOr<const googlesql::Type*> ColumnType(const std::string& name) {
  static std::mutex mu;
  static auto* types =
      new absl::flat_hash_map<std::string, const googlesql::Type*>();
  {
    std::lock_guard<std::mutex> lock(mu);
    if (auto it = types->find(name); it != types->end()) return it->second;
  }
  auto base = DefaultBase();
  if (!base.ok()) return base.status();
  auto parsed = ParseType(name, (*base)->options, &(*base)->builtins,
                          GlobalTypes());
  if (!parsed.ok()) return parsed.status();
  std::lock_guard<std::mutex> lock(mu);
  types->emplace(name, *parsed);
  return *parsed;
}

// {"name":"schema.T","columns":[{"name":"c","type":"INT64","hidden":false,
//  "writable":true}]}
absl::StatusOr<std::unique_ptr<gsql_table>> BuildTable(const json& t) {
  std::string table_name = t.at("name").get<std::string>();
  std::string local_name = table_name;
  if (auto dot = table_name.find('.'); dot != std::string::npos) {
    local_name = table_name.substr(dot + 1);
  }
  auto out = std::make_unique<gsql_table>();
  out->full_name = table_name;
  out->table = std::make_unique<googlesql::SimpleTable>(local_name);
  for (const auto& c : t.at("columns")) {
    std::string column_name = c.at("name").get<std::string>();
    auto type = ColumnType(c.at("type").get_ref<const std::string&>());
    if (!type.ok()) {
      return absl::InvalidArgumentError(absl::StrCat(
          table_name, ".", column_name, ": ", type.status().message()));
    }
    // Hidden columns are pseudo columns: excluded from SELECT *.
    bool hidden = c.value("hidden", false);
    bool writable = c.value("writable", true);
    absl::Status added =
        out->table->AddColumn(std::make_unique<googlesql::SimpleColumn>(
            table_name, column_name, *type, hidden, writable));
    if (!added.ok()) return added;
  }
  out->table->set_full_name(table_name).IgnoreError();
  return out;
}

// A catalog over `tables` (not owned) and the shared builtins for `features`.
absl::StatusOr<std::unique_ptr<gsql_catalog>> AssembleCatalog(
    const gsql_table* const* tables, size_t n, const json& features) {
  auto base = BaseFor(features);
  if (!base.ok()) return base.status();
  auto out = std::make_unique<gsql_catalog>();
  out->options = (*base)->options;
  out->tables =
      std::make_unique<googlesql::SimpleCatalog>("spanner", GlobalTypes());
  absl::flat_hash_map<std::string, googlesql::SimpleCatalog*> schemas;
  for (size_t i = 0; i < n; ++i) {
    const gsql_table* t = tables[i];
    googlesql::SimpleCatalog* target = out->tables.get();
    if (auto dot = t->full_name.find('.'); dot != std::string::npos) {
      std::string schema = t->full_name.substr(0, dot);
      googlesql::SimpleCatalog*& sub = schemas[schema];
      if (sub == nullptr) sub = out->tables->MakeOwnedSimpleCatalog(schema);
      target = sub;
    }
    target->AddTable(t->table.get());
  }
  std::unique_ptr<googlesql::MultiCatalog> multi;
  absl::Status status = googlesql::MultiCatalog::Create(
      "spanner", {out->tables.get(), &(*base)->builtins}, &multi);
  if (!status.ok()) return status;
  out->catalog = std::move(multi);
  return out;
}

absl::StatusOr<std::unique_ptr<gsql_catalog>> BuildCatalog(
    const std::string& text) {
  json doc = json::parse(text, nullptr, /*allow_exceptions=*/false);
  if (doc.is_discarded() || !doc.is_object()) {
    return absl::InvalidArgumentError("catalog JSON is not an object");
  }
  std::vector<std::unique_ptr<gsql_table>> owned;
  if (doc.contains("tables")) {
    for (const auto& t : doc["tables"]) {
      auto table = BuildTable(t);
      if (!table.ok()) return table.status();
      owned.push_back(*std::move(table));
    }
  }
  std::vector<const gsql_table*> refs;
  for (const auto& t : owned) refs.push_back(t.get());
  auto out = AssembleCatalog(refs.data(), refs.size(),
                             doc.contains("extra_features")
                                 ? doc["extra_features"]
                                 : json());
  if (!out.ok()) return out.status();
  (*out)->owned = std::move(owned);
  return out;
}

absl::StatusOr<std::string> Analyze(const gsql_catalog& cat,
                                    const std::string& sql,
                                    const std::string& params_text) {
  // Per-call factory: the catalog's factory stays read-only so analyses can
  // run concurrently.
  googlesql::TypeFactory call_factory;
  googlesql::AnalyzerOptions options = cat.options;
  if (!params_text.empty()) {
    json params = json::parse(params_text, nullptr, false);
    if (params.is_discarded() || !params.is_object()) {
      return absl::InvalidArgumentError("params JSON is not an object");
    }
    for (const auto& [name, type_name] : params.items()) {
      // null = untyped parameter; the analyser infers its type.
      if (type_name.is_null()) {
        options.set_allow_undeclared_parameters(true);
        continue;
      }
      auto type = ParseType(type_name.get<std::string>(), cat.options,
                            cat.catalog.get(), &call_factory);
      if (!type.ok()) return type.status();
      absl::Status s = options.AddQueryParameter(name, *type);
      if (!s.ok()) return s;
    }
  }
  std::unique_ptr<const googlesql::AnalyzerOutput> output;
  absl::Status status = googlesql::AnalyzeStatement(
      sql, options, cat.catalog.get(), &call_factory, &output);
  if (!status.ok()) return status;

  googlesql::AnyResolvedStatementProto proto;
  googlesql::Type::FileDescriptorSetMap fdsets;
  status = output->resolved_statement()->SaveTo(&fdsets, &proto);
  if (!status.ok()) return status;
  return proto.SerializeAsString();
}

}  // namespace

extern "C" {

gsql_table* gsql_table_new(const char* json_text, size_t json_len,
                           gsql_buf* error) {
  absl::StatusOr<std::unique_ptr<gsql_table>> result;
  try {
    json doc = json::parse(std::string(json_text, json_len), nullptr,
                           /*allow_exceptions=*/false);
    if (doc.is_discarded() || !doc.is_object()) {
      result = absl::InvalidArgumentError("table JSON is not an object");
    } else {
      result = BuildTable(doc);
    }
  } catch (const std::exception& e) {
    result = absl::InvalidArgumentError(absl::StrCat("table JSON: ", e.what()));
  }
  if (!result.ok()) {
    if (error != nullptr) {
      *error = CopyBytes(std::string(result.status().message()));
    }
    return nullptr;
  }
  if (error != nullptr) *error = gsql_buf{nullptr, 0};
  return result->release();
}

void gsql_table_free(gsql_table* table) { delete table; }

gsql_catalog* gsql_catalog_from_tables(const gsql_table* const* tables,
                                       size_t n, const char* features_json,
                                       size_t features_len, gsql_buf* error) {
  absl::StatusOr<std::unique_ptr<gsql_catalog>> result;
  try {
    json features;
    if (features_json != nullptr && features_len > 0) {
      features = json::parse(std::string(features_json, features_len), nullptr,
                             /*allow_exceptions=*/false);
      if (features.is_discarded() || !features.is_array()) {
        throw std::invalid_argument("extra features are not an array");
      }
    }
    result = AssembleCatalog(tables, n, features);
  } catch (const std::exception& e) {
    result = absl::InvalidArgumentError(absl::StrCat("catalog: ", e.what()));
  }
  if (!result.ok()) {
    if (error != nullptr) {
      *error = CopyBytes(std::string(result.status().message()));
    }
    return nullptr;
  }
  if (error != nullptr) *error = gsql_buf{nullptr, 0};
  return result->release();
}

gsql_catalog* gsql_catalog_new(const char* json_text, size_t json_len,
                               gsql_buf* error) {
  absl::StatusOr<std::unique_ptr<gsql_catalog>> result;
  try {
    result = BuildCatalog(std::string(json_text, json_len));
  } catch (const std::exception& e) {
    result = absl::InvalidArgumentError(absl::StrCat("catalog JSON: ", e.what()));
  }
  if (!result.ok()) {
    if (error != nullptr) {
      *error = CopyBytes(std::string(result.status().message()));
    }
    return nullptr;
  }
  if (error != nullptr) *error = gsql_buf{nullptr, 0};
  return result->release();
}

void gsql_catalog_free(gsql_catalog* catalog) { delete catalog; }

gsql_result gsql_analyze(const gsql_catalog* catalog, const char* sql,
                         size_t sql_len, const char* params_json,
                         size_t params_len) {
  absl::StatusOr<std::string> result;
  try {
    result = Analyze(*catalog, std::string(sql, sql_len),
                     params_json == nullptr
                         ? std::string()
                         : std::string(params_json, params_len));
  } catch (const std::exception& e) {
    result = absl::InvalidArgumentError(absl::StrCat("params JSON: ", e.what()));
  }
  if (!result.ok()) return Error(result.status());
  return gsql_result{0, CopyBytes(*result), gsql_buf{nullptr, 0}};
}

void gsql_buf_free(gsql_buf buf) { std::free(buf.data); }

}  // extern "C"
