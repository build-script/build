#include "core/greeting.h"
#include "detail.h"

#include <fmt/format.h>

namespace core {
    std::string greeting(const std::string& name) {
        return fmt::format("Hello, {}! (core {})", name, detail::version);
    }

    std::string table(const std::vector<std::pair<std::string, int>>& rows) {
        std::string out;
        for (const auto& [key, value] : rows) {
            out += fmt::format("{:<10}{:>5}\n", key, value);
        }
        return out;
    }
}
