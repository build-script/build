#include <core/greeting.h>
#include <spdlog/spdlog.h>

#include <format>
#include <iostream>
#include <ranges>
#include <vector>

#include "stats.h"

int main() {
    spdlog::info("{}", core::greeting("build-script"));

    std::vector<int> values{5, 3, 8, 1};
    auto squares = values | std::views::transform([](int value) { return value * value; });

    std::cout << std::format("sum of squares {}, mean {:.2f}\n", app::sum(squares), app::mean(values));
    std::cout << core::table({{"apples", 3}, {"pears", 12}});

    std::cout << std::format("lol");
    return 0;
}
