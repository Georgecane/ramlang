#ifndef RAMLANG_RUNTIME_H
#define RAMLANG_RUNTIME_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle owned by the runtime. */
typedef struct RamlangTcpConnection RamlangTcpConnection;
typedef struct RamlangTcpRequest RamlangTcpRequest;
typedef struct RamlangUdpEndpoint RamlangUdpEndpoint;

/*
 * Blocking C ABI for the current RamLang compiler.
 *
 * Frames use a four-byte big-endian unsigned length followed by exactly that many
 * bytes. The C ABI limits each TCP frame to 16 MiB. The per-operation timeout is
 * configured at connect time.
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

/*
 * Non-blocking TCP request API. start copies the address and payload, then launches
 * connect -> framed write -> framed read on a shared multithread Tokio runtime.
 * Each network operation has timeout_ms. Frames are capped at 16 MiB.
 *
 * wait returns 1 when complete, 0 when only the wait timed out, or -1 on error.
 * Completion can represent either success or a network error; read_result returns
 * -1 and updates ramlang_last_error() when the request failed.
 * close cancels unfinished work. Do not use a handle concurrently with close.
 */
RamlangTcpRequest *ramlang_tcp_request_start(
    const char *address,
    const uint8_t *data,
    uint64_t length,
    uint64_t timeout_ms
);
int32_t ramlang_tcp_request_is_ready(RamlangTcpRequest *request);
int32_t ramlang_tcp_request_wait(RamlangTcpRequest *request, uint64_t wait_timeout_ms);
int64_t ramlang_tcp_request_read_result(
    RamlangTcpRequest *request,
    uint8_t *output,
    uint64_t capacity
);
int32_t ramlang_tcp_request_cancel(RamlangTcpRequest *request);
void ramlang_tcp_request_close(RamlangTcpRequest *request);

/*
 * UDP operations are also blocking at the RamLang call site.
 * Datagram payloads are limited to 65,507 bytes. The peer buffer must be large enough
 * for a NUL-terminated textual socket address; 64 bytes is sufficient for IPv4/IPv6.
 */
RamlangUdpEndpoint *ramlang_udp_bind(const char *local_address, uint64_t timeout_ms);
int64_t ramlang_udp_local_addr(
    RamlangUdpEndpoint *endpoint,
    char *output,
    uint64_t capacity
);
int64_t ramlang_udp_send_to(
    RamlangUdpEndpoint *endpoint,
    const uint8_t *data,
    uint64_t length,
    const char *destination
);
int64_t ramlang_udp_recv_from(
    RamlangUdpEndpoint *endpoint,
    uint8_t *output,
    uint64_t capacity,
    char *peer_output,
    uint64_t peer_capacity
);
void ramlang_udp_close(RamlangUdpEndpoint *endpoint);

#ifdef __cplusplus
}
#endif

#endif
