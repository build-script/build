#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* FNV-1a over the bytes */
uint32_t base_checksum(const char *bytes, size_t length);

#ifdef __cplusplus
}
#endif
