#pragma once

#include <string>
#include <string_view>
#include <vector>

namespace util {

std::string fingerprint(std::string_view text);
std::vector<std::string> words(std::string_view text);

}
