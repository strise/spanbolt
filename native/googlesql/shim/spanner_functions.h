#ifndef SPANBOLT_SPANNER_FUNCTIONS_H_
#define SPANBOLT_SPANNER_FUNCTIONS_H_

#include "googlesql/public/simple_catalog.h"
#include "googlesql/public/types/type_factory.h"

namespace spanbolt {

// Registers Spanner-only scalar functions (signatures only; Rust evaluates).
// `production` picks Cloud Spanner's signatures where they differ from the
// emulator's.
void AddSpannerFunctions(googlesql::SimpleCatalog* catalog,
                         googlesql::TypeFactory* types, bool production);

}  // namespace spanbolt

#endif  // SPANBOLT_SPANNER_FUNCTIONS_H_
