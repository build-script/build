#include <core/greeting.h>
#include <gtest/gtest.h>

TEST(Greeting, NamesTheCore) {
    EXPECT_EQ(core::greeting("x"), "Hello, x! (core 1.0)");
}

TEST(Greeting, AlignsTheTable) {
    EXPECT_EQ(core::table({{"a", 1}}), "a             1\n");
}
