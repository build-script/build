#include "base/checksum.h"

uint32_t base_checksum(const char *bytes, size_t length) {
    uint32_t hash = 2166136261u;

    for (size_t i = 0; i < length; i++) {
        hash = (hash ^ (unsigned char) bytes[i]) * 16777619u;
    }

    return hash;
}
