#ifndef SPANBOLT_SPANNER_FUNCTIONS_H_
#define SPANBOLT_SPANNER_FUNCTIONS_H_

#include "googlesql/public/simple_catalog.h"
#include "googlesql/public/types/type_factory.h"

namespace spanbolt {

// Registers Spanner-only scalar functions (signatures only; Rust evaluates).
// `production` picks Cloud Spanner's signatures where they differ from the
// emulator's, and adds DEBUG_TOKENLIST, which only Cloud Spanner has.
void AddSpannerFunctions(googlesql::SimpleCatalog* catalog,
                         googlesql::TypeFactory* types, bool production);

// Gives the builtin FORMAT Cloud Spanner's analysis of a literal pattern: a
// `%` escape with flags, a width or a precision (`%5%`, `%*%`) takes no
// argument, where GoogleSQL rejects the pattern. Spanner rejects it only when
// FORMAT runs (Rust).
void UseSpannerFormatCheck(googlesql::SimpleCatalog* catalog);

}  // namespace spanbolt

#endif  // SPANBOLT_SPANNER_FUNCTIONS_H_
