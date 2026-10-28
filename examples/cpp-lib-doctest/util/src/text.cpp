#include "util/text.h"

#include <base/checksum.h>

#include <cstdio>

namespace util {

std::string fingerprint(std::string_view text) {
    char hex[9];
    std::snprintf(hex, sizeof hex, "%08x", base_checksum(text.data(), text.size()));
    return hex;
}

std::vector<std::string> words(std::string_view text) {
    std::vector<std::string> result;
    std::string word;

    for (char c : text) {
        if (c == ' ') {
            if (!word.empty()) result.push_back(word);
            word.clear();
        } else {
            word += c;
        }
    }

    if (!word.empty()) result.push_back(word);
    return result;
}

}
