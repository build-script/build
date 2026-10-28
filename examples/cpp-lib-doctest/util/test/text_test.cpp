#define DOCTEST_CONFIG_IMPLEMENT_WITH_MAIN
#include <doctest/doctest.h>

#include <util/text.h>

TEST_CASE("words splits at spaces") {
    CHECK(util::words("a bb  c") == std::vector<std::string>{"a", "bb", "c"});
}

TEST_CASE("the fingerprint is stable") {
    CHECK(util::fingerprint("abc") == util::fingerprint("abc"));
    CHECK(util::fingerprint("abc") != util::fingerprint("abd"));
}
