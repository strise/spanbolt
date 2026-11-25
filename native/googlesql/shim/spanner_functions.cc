// Signatures follow cloud-spanner-emulator backend/query/function_catalog.cc
// and backend/query/search/search_function_catalog.cc (Apache-2.0).
#include "shim/spanner_functions.h"

#include <memory>
#include <string>
#include <utility>
#include <vector>

#include "absl/status/status.h"
#include "absl/strings/cord.h"
#include "absl/strings/str_cat.h"
#include "absl/strings/str_replace.h"
#include "googlesql/public/function.h"
#include "googlesql/public/function_signature.h"
#include "googlesql/public/input_argument_type.h"
#include "googlesql/public/language_options.h"
#include "googlesql/public/types/type_factory.h"
#include "googlesql/public/value.h"
#include "re2/re2.h"

namespace spanbolt {
namespace {

using googlesql::Function;
using googlesql::FunctionArgumentType;
using googlesql::FunctionArgumentTypeList;
using googlesql::FunctionArgumentTypeOptions;
using googlesql::FunctionSignature;
using googlesql::Type;

constexpr char kGroup[] = "spanner";

FunctionArgumentTypeOptions Arg(const char* name,
                                googlesql::FunctionEnums::NamedArgumentKind kind,
                                bool required, bool must_be_constant) {
  FunctionArgumentTypeOptions o;
  o.set_argument_name(name, kind);
  o.set_cardinality(required ? FunctionArgumentType::REQUIRED
                             : FunctionArgumentType::OPTIONAL);
  o.set_must_be_constant(must_be_constant);
  return o;
}
FunctionArgumentTypeOptions Req(const char* name, bool constant = true) {
  return Arg(name, googlesql::kPositionalOrNamed, true, constant);
}
FunctionArgumentTypeOptions Opt(const char* name, bool constant = true) {
  return Arg(name, googlesql::kNamedOnly, false, constant);
}
FunctionArgumentTypeOptions Pos(const char* name) {
  return Arg(name, googlesql::kPositionalOnly, true, false);
}

std::unique_ptr<Function> Scalar(const std::string& name,
                                 std::vector<FunctionSignature> sigs,
                                 bool coercible = false) {
  googlesql::FunctionOptions options;
  options.set_arguments_are_coercible(coercible);
  options.set_supports_safe_error_mode(false);
  return std::make_unique<Function>(name, kGroup, Function::SCALAR, sigs,
                                    options);
}

// Runs GoogleSQL's FORMAT check on the literal pattern with every `%` escape
// written as `%%` (conformance/cases/sql/format_percent_escapes.case), and
// names the pattern as written in its errors.
absl::Status CheckSpannerFormat(
    const googlesql::PostResolutionArgumentConstraintsCallback& check,
    const FunctionSignature& signature,
    const std::vector<googlesql::InputArgumentType>& args,
    const googlesql::LanguageOptions& language) {
  if (args.empty() || !args[0].is_literal() || args[0].is_literal_null() ||
      !args[0].type()->IsString()) {
    return check(signature, args, language);
  }
  const std::string& pattern = args[0].literal_value()->string_value();
  // GoogleSQL's "%[flags][width][.precision]specifier" with specifier `%`.
  static const LazyRE2 kEscape = {
      R"(%[-+ #'0]*(?:\*|[0-9]*)(?:\.(?:\*|[0-9]*))?%)"};
  std::string escapes = pattern;
  RE2::GlobalReplace(&escapes, *kEscape, "%%");
  if (escapes == pattern) return check(signature, args, language);
  const googlesql::Value value = googlesql::Value::String(escapes);
  std::vector<googlesql::InputArgumentType> rewritten = args;
  rewritten[0] = googlesql::InputArgumentType(value, /*annotation_map=*/nullptr);
  absl::Status status = check(signature, rewritten, language);
  if (status.ok()) return status;
  std::string message = absl::StrReplaceAll(
      status.message(), {{absl::StrCat("\"", escapes, "\""),
                          absl::StrCat("\"", pattern, "\"")}});
  absl::Status out(status.code(), message);
  status.ForEachPayload([&out](absl::string_view url, const absl::Cord& payload) {
    out.SetPayload(url, payload);
  });
  return out;
}

}  // namespace

void UseSpannerFormatCheck(googlesql::SimpleCatalog* catalog) {
  const Function* format = nullptr;
  if (!catalog->GetFunction("format", &format).ok() || format == nullptr) return;
  googlesql::PostResolutionArgumentConstraintsCallback check =
      format->PostResolutionConstraints();
  if (!check) return;
  googlesql::FunctionOptions options = format->function_options();
  options.set_post_resolution_argument_constraint(
      [check](const FunctionSignature& signature,
              const std::vector<googlesql::InputArgumentType>& args,
              const googlesql::LanguageOptions& language) {
        return CheckSpannerFormat(check, signature, args, language);
      });
  auto replacement = std::make_unique<Function>(
      format->Name(), format->GetGroup(), format->mode(), format->signatures(),
      std::move(options));
  catalog->RemoveFunctions([format](const Function* f) { return f == format; });
  catalog->AddOwnedFunction(std::move(replacement));
}

void AddSpannerFunctions(googlesql::SimpleCatalog* catalog,
                         googlesql::TypeFactory* types, bool production) {
  const Type* str = types->get_string();
  const Type* bytes = types->get_bytes();
  const Type* b = types->get_bool();
  const Type* i64 = types->get_int64();
  const Type* f64 = types->get_double();
  const Type* js = types->get_json();
  const Type* tok = types->get_tokenlist();
  const Type* ts = types->get_timestamp();
  const googlesql::ArrayType* str_arr = nullptr;
  const googlesql::ArrayType* tok_arr = nullptr;
  (void)types->MakeArrayType(str, &str_arr);
  (void)types->MakeArrayType(tok, &tok_arr);

  catalog->AddOwnedFunction(
      Scalar("pending_commit_timestamp", {FunctionSignature{ts, {}, nullptr}}));
  catalog->AddOwnedFunction(Scalar(
      "bit_reverse",
      {FunctionSignature{i64, {{i64, Req("value", false)}, {b, Req("preserve_sign", false)}}, nullptr}}));

  // Search functions: backend/query/search/search_function_catalog.cc.
  const Type* u64 = types->get_uint64();
  const googlesql::ArrayType* bytes_arr = nullptr;
  (void)types->MakeArrayType(bytes, &bytes_arr);
  auto with = [](FunctionArgumentTypeList head,
                 const FunctionArgumentTypeList& tail) {
    head.insert(head.end(), tail.begin(), tail.end());
    return head;
  };

  // Errors list the signatures in this order: Cloud Spanner's starts with BYTES.
  std::vector<FunctionSignature> token_sigs;
  const std::vector<const Type*> token_types =
      production ? std::vector<const Type*>{bytes, bytes_arr, str, str_arr}
                 : std::vector<const Type*>{str, bytes, str_arr, bytes_arr};
  for (const Type* t : token_types) {
    token_sigs.push_back(FunctionSignature{tok, {{t, Pos("value")}}, nullptr});
  }
  catalog->AddOwnedFunction(Scalar("token", token_sigs));

  std::vector<FunctionSignature> number_sigs;
  for (const char* precision : {"ieee_precision", "precision"}) {
    for (const Type* t : {i64, u64, f64}) {
      FunctionArgumentTypeList opts = {
          {str, Opt("comparison_type")}, {str, Opt("algorithm")},
          {t, Opt("min")},               {t, Opt("max")},
          {t, Opt("granularity")},       {i64, Opt("tree_base")},
          {i64, Opt(precision)}};
      number_sigs.push_back(FunctionSignature{
          tok, with({{t, Req("value", false)}}, opts), nullptr});
      const googlesql::ArrayType* arr = nullptr;
      if (types->MakeArrayType(t, &arr).ok()) {
        number_sigs.push_back(FunctionSignature{
            tok, with({{arr, Req("value", false)}}, opts), nullptr});
      }
    }
  }
  catalog->AddOwnedFunction(Scalar("tokenize_number", number_sigs));
  catalog->AddOwnedFunction(Scalar(
      "tokenize_bool",
      {FunctionSignature{tok, {{b, Req("value", false)}}, nullptr}},
      /*coercible=*/true));

  FunctionArgumentTypeList fulltext_opts = {
      {str, Opt("language_tag", false)},
      {str, Opt("content_type")},
      {str, Opt("token_category")},
  };
  catalog->AddOwnedFunction(Scalar(
      "tokenize_fulltext",
      {FunctionSignature{tok, with({{str, Req("value", false)}}, fulltext_opts),
                         nullptr},
       FunctionSignature{tok,
                         with({{str_arr, Req("value", false)}}, fulltext_opts),
                         nullptr}}));

  FunctionArgumentTypeList substring_opts = {
      {i64, Opt("ngram_size_max")},
      {i64, Opt("ngram_size_min")},
      {b, Opt("support_relative_search")},
      {str, Opt("content_type")},
      {str_arr, Opt("relative_search_types")},
      {b, Opt("remove_diacritics")},
      {b, Opt("short_tokens_only_for_anchors")},
      {str, Opt("language_tag", false)}};
  catalog->AddOwnedFunction(Scalar(
      "tokenize_substring",
      {FunctionSignature{tok, with({{str, Req("value", false)}}, substring_opts),
                         nullptr},
       FunctionSignature{tok,
                         with({{str_arr, Req("value", false)}}, substring_opts),
                         nullptr}}));

  catalog->AddOwnedFunction(Scalar(
      "tokenlist_concat",
      {FunctionSignature{tok, {{tok_arr, Req("tokens", false)}}, nullptr}}));

  // Cloud Spanner's SEARCH and SCORE take `enhance_query_options => JSON`
  // before `dictionary`, and SCORE only a STRING query
  // (conformance/cases/sql/search_signatures.case). The emulator's SCORE also
  // takes BYTES and fails while running.
  FunctionArgumentTypeList search_args = {
      {tok, Req("tokens", false)}, {str, Req("query")},
      {b, Opt("enhance_query")},   {str, Opt("language_tag")},
      {str, Opt("dialect")}};
  if (production) search_args.push_back({js, Opt("enhance_query_options")});
  search_args.push_back({str, Opt("dictionary")});
  catalog->AddOwnedFunction(
      Scalar("search", {FunctionSignature{b, search_args, nullptr}}));
  catalog->AddOwnedFunction(Scalar(
      "search_substring", {FunctionSignature{b,
                                             {{tok, Req("tokens", false)},
                                              {str, Req("query")},
                                              {str, Opt("relative_search_type")},
                                              {str, Opt("language_tag")}},
                                             nullptr}}));
  FunctionArgumentTypeList score_tail = {{b, Opt("enhance_query")},
                                         {str, Opt("language_tag")},
                                         {str, Opt("dialect")}};
  if (production) score_tail.push_back({js, Opt("enhance_query_options")});
  score_tail.push_back({js, Opt("options", false)});
  score_tail.push_back({str, Opt("dictionary")});
  std::vector<FunctionSignature> score_sigs = {FunctionSignature{
      f64, with({{tok, Req("tokens", false)}, {str, Req("query")}}, score_tail),
      nullptr}};
  if (!production) {
    score_sigs.push_back(FunctionSignature{
        f64,
        with({{tok, Req("tokens", false)}, {bytes, Req("query")}}, score_tail),
        nullptr});
  }
  catalog->AddOwnedFunction(Scalar("score", score_sigs));
  catalog->AddOwnedFunction(Scalar(
      "snippet", {FunctionSignature{js,
                                    {{str, Req("value", false)},
                                     {str, Req("query")},
                                     {b, Opt("enhance_query")},
                                     {str, Opt("language_tag")},
                                     {i64, Opt("max_snippet_width", false)},
                                     {i64, Opt("max_snippets", false)},
                                     {str, Opt("content_type")},
                                     {str, Opt("dictionary")}},
                                    nullptr}}));

  FunctionArgumentTypeList ngram_opts = {{i64, Opt("ngram_size_max")},
                                         {i64, Opt("ngram_size_min")},
                                         {b, Opt("remove_diacritics")}};
  catalog->AddOwnedFunction(Scalar(
      "tokenize_ngrams",
      {FunctionSignature{tok, with({{str, Req("value", false)}}, ngram_opts),
                         nullptr},
       FunctionSignature{tok, with({{str_arr, Req("value", false)}}, ngram_opts),
                         nullptr}}));
  catalog->AddOwnedFunction(Scalar(
      "search_ngrams",
      {FunctionSignature{b,
                         {{tok, Req("tokens", false)},
                          {str, Req("ngrams_query")},
                          {i64, Opt("min_ngrams")},
                          {f64, Opt("min_ngrams_percent")},
                          {str, Opt("language_tag")}},
                         nullptr}},
      /*coercible=*/true));
  catalog->AddOwnedFunction(Scalar(
      "score_ngrams",
      {FunctionSignature{f64,
                         {{tok, Req("tokens", false)},
                          {str, Req("ngrams_query")},
                          {str, Opt("algorithm")},
                          {str, Opt("language_tag")}},
                         nullptr}},
      /*coercible=*/true));
  catalog->AddOwnedFunction(Scalar(
      "tokenize_json", {FunctionSignature{tok, {{js, Req("value", false)}}, nullptr}}));

  // Only Cloud Spanner has DEBUG_TOKENLIST: the emulator answers "Function not found".
  if (production) {
    catalog->AddOwnedFunction(Scalar(
        "debug_tokenlist",
        {FunctionSignature{str, {{tok, Req("tokenlist", false)}}, nullptr}}));
  }
}

}  // namespace spanbolt
