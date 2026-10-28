#include <vector>

#include "stats.h"

namespace app {

double mean(const std::vector<int>& values) {
    return values.empty() ? 0 : static_cast<double>(sum(values)) / values.size();
}

}
