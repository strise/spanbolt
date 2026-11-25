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
#include "absl/container/flat_hash_set.h"
#include "absl/strings/ascii.h"
#include "absl/strings/match.h"
#include "absl/strings/str_cat.h"
#include "absl/strings/str_join.h"
#include "absl/time/time.h"
#include "google/protobuf/descriptor.pb.h"
#include "googlesql/public/analyzer.h"
#include "googlesql/public/analyzer_options.h"
#include "googlesql/public/analyzer_output.h"
#include "googlesql/public/builtin_function_options.h"
#include "googlesql/public/language_options.h"
#include "googlesql/public/multi_catalog.h"
#include "googlesql/public/options.pb.h"
#include "googlesql/public/simple_catalog.h"
#include "googlesql/public/simple_property_graph.h"
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
  // The shared builtins (not owned).
  googlesql::Catalog* builtins = nullptr;
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
      googlesql::FEATURE_SQL_GRAPH,
      googlesql::FEATURE_SQL_GRAPH_ADVANCED_QUERY,
      googlesql::FEATURE_SQL_GRAPH_BOUNDED_PATH_QUANTIFICATION,
      googlesql::FEATURE_SQL_GRAPH_PATH_TYPE,
      googlesql::FEATURE_SQL_GRAPH_PATH_MODE,
      googlesql::FEATURE_SQL_GRAPH_DYNAMIC_ELEMENT_TYPE,
      googlesql::FEATURE_SQL_GRAPH_RETURN_EXTENSIONS,
      googlesql::FEATURE_SQL_GRAPH_CALL,
  });
  // Spanner reserves GRAPH_TABLE, so `FROM GRAPH_TABLE(g MATCH ...)` parses.
  options.EnableReservableKeyword("GRAPH_TABLE").IgnoreError();
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

// Not a language feature: in `extra_features` it picks Cloud Spanner's
// function signatures where they differ from the emulator's.
constexpr char kProductionMarker[] = "SPANBOLT_PRODUCTION";

// The shared base for a feature set (the catalog JSON's `extra_features`).
absl::StatusOr<SharedBase*> BaseFor(const json& features) {
  static std::mutex mu;
  static auto* bases = new std::map<std::string, SharedBase*>();
  std::string key = features.is_null() ? "[]" : features.dump();
  std::lock_guard<std::mutex> lock(mu);
  if (auto it = bases->find(key); it != bases->end()) return it->second;

  googlesql::LanguageOptions language = SpannerLanguageOptions();
  bool production = false;
  if (!features.is_null()) {
    for (const auto& f : features) {
      if (f == kProductionMarker) {
        production = true;
        continue;
      }
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
  spanbolt::AddSpannerFunctions(&base->builtins, GlobalTypes(), production);
  SharedBase* out = base.release();
  bases->emplace(key, out);
  return out;
}

// The base used to parse column types: column types never depend on extra
// language features.
absl::StatusOr<SharedBase*> DefaultBase() { return BaseFor(json()); }

constexpr char kFileDescriptorSetType[] =
    "PROTO<google.protobuf.FileDescriptorSet>";

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
  const googlesql::Type* type = nullptr;
  if (name == kFileDescriptorSetType) {
    // INFORMATION_SCHEMA.SCHEMATA.PROTO_BUNDLE, the one PROTO column. Made
    // directly: Spanner's language options name no proto types, so the
    // analyser can't parse it, and user SQL still can't name it.
    absl::Status made = GlobalTypes()->MakeProtoType(
        google::protobuf::FileDescriptorSet::descriptor(), &type);
    if (!made.ok()) return made;
  } else {
    auto base = DefaultBase();
    if (!base.ok()) return base.status();
    auto parsed = ParseType(name, (*base)->options, &(*base)->builtins,
                            GlobalTypes());
    if (!parsed.ok()) return parsed.status();
    type = *parsed;
  }
  std::lock_guard<std::mutex> lock(mu);
  types->emplace(name, type);
  return type;
}

// {"name":"schema.T","columns":[{"name":"c","type":"INT64","hidden":false,
//  "writable":true}],"primary_key":[0]}
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
  if (t.contains("primary_key")) {
    std::vector<int> pk = t["primary_key"].get<std::vector<int>>();
    absl::Status set = out->table->SetPrimaryKey(pk);
    if (!set.ok()) return set;
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
  out->builtins = &(*base)->builtins;
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
                                    const std::string& params_text,
                                    bool ddl = false,
                                    googlesql::Catalog* view_catalog = nullptr) {
  // Per-call factory: the catalog's factory stays read-only so analyses can
  // run concurrently.
  googlesql::TypeFactory call_factory;
  googlesql::AnalyzerOptions options = cat.options;
  if (ddl) {
    options.mutable_language()->SetSupportedStatementKinds(
        {googlesql::RESOLVED_CREATE_PROPERTY_GRAPH_STMT});
  }
  if (view_catalog != nullptr) {
    // How Spanner and the emulator analyse a view's query.
    options.mutable_language()->set_name_resolution_mode(
        googlesql::NAME_RESOLUTION_STRICT);
  }
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
      sql, options,
      view_catalog != nullptr ? view_catalog : cat.catalog.get(),
      &call_factory, &output);
  if (!status.ok()) return status;

  googlesql::AnyResolvedStatementProto proto;
  googlesql::Type::FileDescriptorSetMap fdsets;
  status = output->resolved_statement()->SaveTo(&fdsets, &proto);
  if (!status.ok()) return status;
  return proto.SerializeAsString();
}

// The tables of a catalog, where the ones named in `exact` (by full name) are
// found only by that spelling. Cloud Spanner resolves table names in a view's
// query case-sensitively; other lookups find nothing here and fall through to
// the builtins behind it.
class ExactTables : public googlesql::Catalog {
 public:
  ExactTables(googlesql::Catalog* tables,
              absl::flat_hash_set<std::string> exact)
      : tables_(tables), exact_(std::move(exact)) {}

  std::string FullName() const override { return tables_->FullName(); }

  absl::Status FindTable(const absl::Span<const std::string>& path,
                         const googlesql::Table** table,
                         const FindOptions& options) override {
    absl::Status s = tables_->FindTable(path, table, options);
    if (s.ok() && !Spelled(path, path.size(), *table)) {
      *table = nullptr;
      return TableNotFoundError(path);
    }
    return s;
  }

  absl::Status FindTableWithPathPrefix(absl::Span<const std::string> path,
                                       const FindOptions& options,
                                       int* num_names_consumed,
                                       const googlesql::Table** table) override {
    absl::Status s = tables_->FindTableWithPathPrefix(
        path, options, num_names_consumed, table);
    if (s.ok() && *table != nullptr &&
        !Spelled(path, *num_names_consumed, *table)) {
      *table = nullptr;
      *num_names_consumed = 0;
      return TableNotFoundError(path);
    }
    return s;
  }

  absl::Status FindPropertyGraph(absl::Span<const std::string> path,
                                 const googlesql::PropertyGraph*& graph,
                                 const FindOptions& options) override {
    return tables_->FindPropertyGraph(path, graph, options);
  }

 private:
  // Whether the first `n` names of `path` spell `table` as declared, when it
  // has to be.
  bool Spelled(absl::Span<const std::string> path, size_t n,
               const googlesql::Table* table) const {
    std::string name = table->FullName();
    if (!exact_.contains(name)) return true;
    return absl::StrJoin(path.subspan(0, n), ".") == name;
  }

  googlesql::Catalog* tables_;
  absl::flat_hash_set<std::string> exact_;
};

absl::StatusOr<const googlesql::Table*> FindTable(const gsql_catalog& cat,
                                                  const std::string& name) {
  std::vector<std::string> path;
  if (auto dot = name.find('.'); dot != std::string::npos) {
    path = {name.substr(0, dot), name.substr(dot + 1)};
  } else {
    path = {name};
  }
  const googlesql::Table* table = nullptr;
  absl::Status status = cat.tables->FindTable(path, &table);
  if (!status.ok()) {
    return absl::InvalidArgumentError(absl::StrCat("Table not found: ", name));
  }
  return table;
}

absl::StatusOr<std::vector<int>> ColumnIndexes(const googlesql::Table& table,
                                               const json& names) {
  std::vector<int> out;
  for (const auto& n : names) {
    const std::string& name = n.get_ref<const std::string&>();
    int found = -1;
    for (int i = 0; i < table.NumColumns(); ++i) {
      if (absl::EqualsIgnoreCase(table.GetColumn(i)->Name(), name)) {
        found = i;
        break;
      }
    }
    if (found < 0) {
      return absl::InvalidArgumentError(
          absl::StrCat("Column not found: ", table.Name(), ".", name));
    }
    out.push_back(found);
  }
  return out;
}

// Adds a property graph built from a JSON document:
// {"name":"G","properties":[{"name":"id","type":"STRING"}],
//  "labels":[{"name":"Person","properties":["id"]}],
//  "nodes":[{"name":"Person","table":"Person","key":["id"],
//            "labels":["Person"],"properties":[{"name":"id","sql":"id"}]}],
//  "edges":[{... as nodes ...,
//            "source":{"node":"Person","edge_columns":["src"],
//                      "node_columns":["id"]},
//            "destination":{...}}]}
absl::Status AddPropertyGraph(gsql_catalog& cat, const json& g) {
  const std::string graph_name = g.at("name").get<std::string>();
  const std::vector<std::string> path = {graph_name};

  std::vector<std::unique_ptr<const googlesql::GraphPropertyDeclaration>> decls;
  absl::flat_hash_map<std::string, const googlesql::GraphPropertyDeclaration*>
      decl_by_name;
  for (const auto& p : g.at("properties")) {
    std::string name = p.at("name").get<std::string>();
    auto type = ColumnType(p.at("type").get<std::string>());
    if (!type.ok()) return type.status();
    auto decl = std::make_unique<googlesql::SimpleGraphPropertyDeclaration>(
        name, path, *type, /*type_annotation_map=*/nullptr,
        googlesql::GraphPropertyDeclaration::Kind::kScalar);
    decl_by_name[absl::AsciiStrToLower(name)] = decl.get();
    decls.push_back(std::move(decl));
  }
  auto decl_for = [&](const std::string& name)
      -> absl::StatusOr<const googlesql::GraphPropertyDeclaration*> {
    auto it = decl_by_name.find(absl::AsciiStrToLower(name));
    if (it == decl_by_name.end()) {
      return absl::InvalidArgumentError(
          absl::StrCat("Property not declared: ", name));
    }
    return it->second;
  };

  std::vector<std::unique_ptr<const googlesql::GraphElementLabel>> labels;
  absl::flat_hash_map<std::string, const googlesql::GraphElementLabel*>
      label_by_name;
  for (const auto& l : g.at("labels")) {
    std::string name = l.at("name").get<std::string>();
    absl::flat_hash_set<const googlesql::GraphPropertyDeclaration*> props;
    for (const auto& p : l.at("properties")) {
      auto decl = decl_for(p.get<std::string>());
      if (!decl.ok()) return decl.status();
      props.insert(*decl);
    }
    auto label =
        std::make_unique<googlesql::SimpleGraphElementLabel>(name, path, props);
    label_by_name[absl::AsciiStrToLower(name)] = label.get();
    labels.push_back(std::move(label));
  }

  struct Common {
    std::string name;
    const googlesql::Table* table;
    std::vector<int> key;
    absl::flat_hash_set<const googlesql::GraphElementLabel*> labels;
    std::vector<std::unique_ptr<const googlesql::GraphPropertyDefinition>> defs;
  };
  auto common = [&](const json& e) -> absl::StatusOr<Common> {
    Common c;
    c.name = e.at("name").get<std::string>();
    auto table = FindTable(cat, e.at("table").get<std::string>());
    if (!table.ok()) return table.status();
    c.table = *table;
    auto key = ColumnIndexes(*c.table, e.at("key"));
    if (!key.ok()) return key.status();
    c.key = *std::move(key);
    for (const auto& l : e.at("labels")) {
      auto it = label_by_name.find(absl::AsciiStrToLower(l.get<std::string>()));
      if (it == label_by_name.end()) {
        return absl::InvalidArgumentError(
            absl::StrCat("Label not declared: ", l.get<std::string>()));
      }
      c.labels.insert(it->second);
    }
    for (const auto& p : e.at("properties")) {
      auto decl = decl_for(p.at("name").get<std::string>());
      if (!decl.ok()) return decl.status();
      c.defs.push_back(std::make_unique<googlesql::SimpleGraphPropertyDefinition>(
          *decl, p.at("sql").get<std::string>()));
    }
    return c;
  };

  std::vector<std::unique_ptr<const googlesql::GraphNodeTable>> nodes;
  absl::flat_hash_map<std::string, const googlesql::GraphNodeTable*>
      node_by_name;
  for (const auto& n : g.at("nodes")) {
    auto c = common(n);
    if (!c.ok()) return c.status();
    auto node = std::make_unique<googlesql::SimpleGraphNodeTable>(
        c->name, path, c->table, c->key, c->labels, std::move(c->defs));
    node_by_name[absl::AsciiStrToLower(c->name)] = node.get();
    nodes.push_back(std::move(node));
  }

  std::vector<std::unique_ptr<const googlesql::GraphEdgeTable>> edges;
  for (const auto& e : g.at("edges")) {
    auto c = common(e);
    if (!c.ok()) return c.status();
    auto reference = [&](const json& r)
        -> absl::StatusOr<std::unique_ptr<const googlesql::GraphNodeTableReference>> {
      auto it =
          node_by_name.find(absl::AsciiStrToLower(r.at("node").get<std::string>()));
      if (it == node_by_name.end()) {
        return absl::InvalidArgumentError(absl::StrCat(
            "Node table not found: ", r.at("node").get<std::string>()));
      }
      auto edge_cols = ColumnIndexes(*c->table, r.at("edge_columns"));
      if (!edge_cols.ok()) return edge_cols.status();
      auto node_cols = ColumnIndexes(*it->second->GetTable(), r.at("node_columns"));
      if (!node_cols.ok()) return node_cols.status();
      return std::make_unique<googlesql::SimpleGraphNodeTableReference>(
          it->second, *std::move(edge_cols), *std::move(node_cols));
    };
    auto source = reference(e.at("source"));
    if (!source.ok()) return source.status();
    auto destination = reference(e.at("destination"));
    if (!destination.ok()) return destination.status();
    edges.push_back(std::make_unique<googlesql::SimpleGraphEdgeTable>(
        c->name, path, c->table, c->key, c->labels, std::move(c->defs),
        *std::move(source), *std::move(destination)));
  }

  cat.tables->AddOwnedPropertyGraph(
      graph_name, std::make_unique<googlesql::SimplePropertyGraph>(
                      path, std::move(nodes), std::move(edges),
                      std::move(labels), std::move(decls)));
  return absl::OkStatus();
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

int32_t gsql_catalog_add_property_graph(gsql_catalog* catalog,
                                        const char* json_text, size_t json_len,
                                        gsql_buf* error) {
  absl::Status status;
  try {
    json doc = json::parse(std::string(json_text, json_len), nullptr,
                           /*allow_exceptions=*/false);
    if (doc.is_discarded() || !doc.is_object()) {
      status = absl::InvalidArgumentError("graph JSON is not an object");
    } else {
      status = AddPropertyGraph(*catalog, doc);
    }
  } catch (const std::exception& e) {
    status = absl::InvalidArgumentError(absl::StrCat("graph JSON: ", e.what()));
  }
  if (error != nullptr) {
    *error = status.ok() ? gsql_buf{nullptr, 0}
                         : CopyBytes(std::string(status.message()));
  }
  return static_cast<int32_t>(status.code());
}

gsql_result gsql_analyze_ddl(const gsql_catalog* catalog, const char* sql,
                             size_t sql_len) {
  absl::StatusOr<std::string> result;
  try {
    result = Analyze(*catalog, std::string(sql, sql_len), std::string(),
                     /*ddl=*/true);
  } catch (const std::exception& e) {
    result = absl::InternalError(e.what());
  }
  if (!result.ok()) return Error(result.status());
  return gsql_result{0, CopyBytes(*result), gsql_buf{nullptr, 0}};
}

gsql_result gsql_analyze_view(const gsql_catalog* catalog, const char* sql,
                              size_t sql_len, const char* exact_json,
                              size_t exact_len) {
  absl::StatusOr<std::string> result;
  try {
    absl::flat_hash_set<std::string> exact;
    if (exact_json != nullptr && exact_len > 0) {
      json names = json::parse(std::string(exact_json, exact_len));
      for (const auto& n : names) exact.insert(n.get<std::string>());
    }
    ExactTables tables(catalog->tables.get(), std::move(exact));
    std::unique_ptr<googlesql::MultiCatalog> multi;
    absl::Status made = googlesql::MultiCatalog::Create(
        "spanner", {&tables, catalog->builtins}, &multi);
    if (!made.ok()) {
      result = made;
    } else {
      result = Analyze(*catalog, std::string(sql, sql_len), std::string(),
                       /*ddl=*/false, multi.get());
    }
  } catch (const std::exception& e) {
    result = absl::InvalidArgumentError(absl::StrCat("exact JSON: ", e.what()));
  }
  if (!result.ok()) return Error(result.status());
  return gsql_result{0, CopyBytes(*result), gsql_buf{nullptr, 0}};
}

void gsql_buf_free(gsql_buf buf) { std::free(buf.data); }

}  // extern "C"
