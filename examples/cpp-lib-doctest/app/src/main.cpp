#include <util/text.h>

#include <iostream>

int main() {
    for (const auto& word : util::words("build script builds modules")) {
        std::cout << util::fingerprint(word) << "  " << word << "\n";
    }

    return 0;
}
