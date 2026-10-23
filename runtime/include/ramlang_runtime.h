#ifndef RAMLANG_RUNTIME_H
#define RAMLANG_RUNTIME_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle owned by the runtime. */
typedef struct RamlangTcpConnection RamlangTcpConnection;

/*
 * Blocking C ABI for the current RamLang compiler.
 *
 * Frames use a four-byte big-endian unsigned length followed by exactly that many
 * bytes. The per-operation timeout is configured at connect time.
 *
 * Error convention:
 *   connect returns NULL on failure;
 *   read/write return -1 on failure;
 *   last_error returns a thread-local diagnostic string.
 *
 * Copy ramlang_last_error() before another runtime call on the same thread.
 * A connection must not be closed while another thread is using that handle.
 */
const char *ramlang_last_error(void);
RamlangTcpConnection *ramlang_tcp_connect(const char *address, uint64_t timeout_ms);
int64_t ramlang_tcp_write_frame(
    RamlangTcpConnection *connection,
    const uint8_t *data,
    uint64_t length
);
int64_t ramlang_tcp_read_frame(
    RamlangTcpConnection *connection,
    uint8_t *output,
    uint64_t capacity
);
void ramlang_tcp_close(RamlangTcpConnection *connection);

#ifdef __cplusplus
}
#endif

#endif
