#ifndef SPANBOLT_SPANNER_FUNCTIONS_H_
#define SPANBOLT_SPANNER_FUNCTIONS_H_

#include "googlesql/public/simple_catalog.h"
#include "googlesql/public/types/type_factory.h"

namespace spanbolt {

// Registers Spanner-only scalar functions (signatures only; Rust evaluates).
void AddSpannerFunctions(googlesql::SimpleCatalog* catalog,
                         googlesql::TypeFactory* types);

}  // namespace spanbolt

#endif  // SPANBOLT_SPANNER_FUNCTIONS_H_
