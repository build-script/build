#pragma once

#include <concepts>
#include <ranges>
#include <vector>

namespace app {

template <std::ranges::input_range Range>
    requires std::integral<std::ranges::range_value_t<Range>>
long sum(Range&& range) {
    long total = 0;
    for (auto value : range) {
        total += value;
    }
    return total;
}

double mean(const std::vector<int>& values);

}
