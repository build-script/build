#include <catch2/catch_test_macros.hpp>

#include <vector>

#include "stats.h"

TEST_CASE("sum adds the values") {
    REQUIRE(app::sum(std::vector<int>{1, 2, 3}) == 6);
}

TEST_CASE("the mean of nothing is zero") {
    REQUIRE(app::mean({}) == 0);
}
