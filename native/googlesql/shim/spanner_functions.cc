// Signatures follow cloud-spanner-emulator backend/query/function_catalog.cc
// and backend/query/search/search_function_catalog.cc (Apache-2.0).
#include "shim/spanner_functions.h"

#include <memory>
#include <string>
#include <vector>

#include "googlesql/public/function.h"
#include "googlesql/public/function_signature.h"
#include "googlesql/public/types/type_factory.h"

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

}  // namespace

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

  catalog->AddOwnedFunction(Scalar(
      "token",
      {FunctionSignature{tok, {{str, Pos("value")}}, nullptr},
       FunctionSignature{tok, {{bytes, Pos("value")}}, nullptr},
       FunctionSignature{tok, {{str_arr, Pos("value")}}, nullptr},
       FunctionSignature{tok, {{bytes_arr, Pos("value")}}, nullptr}}));

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
}

}  // namespace spanbolt
