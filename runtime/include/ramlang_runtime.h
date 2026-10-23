#ifndef RAMLANG_RUNTIME_H
#define RAMLANG_RUNTIME_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle owned by the runtime. */
typedef struct RamlangTcpConnection RamlangTcpConnection;
typedef struct RamlangTcpRequest RamlangTcpRequest;
typedef struct RamlangUdpEndpoint RamlangUdpEndpoint;
typedef struct RamlangUdpRequest RamlangUdpRequest;
typedef struct RamlangTaskScope RamlangTaskScope;

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
/* Native printing builtins used by generated RamLang programs. */
void ramlang_print_i32(int32_t value);
void ramlang_print_i64(int64_t value);
void ramlang_print_u32(uint32_t value);
void ramlang_print_u64(uint64_t value);
void ramlang_print_f64(double value);
void ramlang_print_str(const char *value);

/* Rust implementations of allocation, byte-memory, and string primitives. */
void *ramlang_malloc(size_t size);
void *ramlang_calloc(size_t count, size_t size);
void *ramlang_realloc(void *pointer, size_t new_size);
void ramlang_free(void *pointer);
void *ramlang_memcpy(void *destination, const void *source, size_t count);
void *ramlang_memmove(void *destination, const void *source, size_t count);
void *ramlang_memset(void *destination, int byte, size_t count);
int32_t ramlang_memcmp(const void *left, const void *right, size_t count);
size_t ramlang_strlen(const char *value);
int32_t ramlang_strcmp(const char *left, const char *right);
int32_t ramlang_strncmp(const char *left, const char *right, size_t count);
void *ramlang_memchr(const void *value, int byte, size_t count);

/* Rust-owned allocator used by compiler-generated async argument frames. */
void *ramlang_alloc(uint64_t size);
void ramlang_free(void *pointer);

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
 * Native RamLang Future bridge. Generated async functions use a small Rust runtime
 * handle so their callbacks can run concurrently. Awaiting i64 blocks the current caller
 * in this initial backend; state-machine lowering will remove that blocking phase.
 */
typedef struct RamlangFuture RamlangFuture;
typedef int64_t (*RamlangFutureCallback)(void *context);
RamlangFuture *ramlang_future_spawn_i64(RamlangFutureCallback callback, void *context);
int64_t ramlang_future_await_i64(RamlangFuture *future);
int32_t ramlang_future_is_ready(RamlangFuture *future);
void ramlang_future_close(RamlangFuture *future);
/* Start a framed TCP text request. Await it through ramlang_future_await_i64; the returned response string is released with ramlang_free. */
RamlangFuture *ramlang_tcp_request_future(const char *address, const char *payload, int64_t timeout_ms);

/*
 * Explicit cancellation scope for background network requests.
 *
 * Track each request you create. Cancelling/closing the scope wakes pending waits and
 * cancels unfinished tracked requests. The scope does not free request handles; the
 * caller remains responsible for closing each request handle.
 */
RamlangTaskScope *ramlang_task_scope_new(void);
int32_t ramlang_task_scope_track_tcp(RamlangTaskScope *scope, RamlangTcpRequest *request);
int32_t ramlang_task_scope_track_udp(RamlangTaskScope *scope, RamlangUdpRequest *request);
int32_t ramlang_task_scope_cancel(RamlangTaskScope *scope);
int64_t ramlang_task_scope_len(RamlangTaskScope *scope);
void ramlang_task_scope_close(RamlangTaskScope *scope);

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

/*
 * Non-blocking UDP request API. Starts a datagram send followed by one response receive
 * on a shared multithread runtime, returning a request handle immediately. It resolves
 * the destination and accepts a reply only from the selected destination address.
 * Payload and response limits are 65,507 bytes.
 */
RamlangUdpRequest *ramlang_udp_request_start(
    const char *destination,
    const uint8_t *data,
    uint64_t length,
    uint64_t response_capacity,
    uint64_t timeout_ms
);
int32_t ramlang_udp_request_is_ready(RamlangUdpRequest *request);
int32_t ramlang_udp_request_wait(RamlangUdpRequest *request, uint64_t wait_timeout_ms);
int64_t ramlang_udp_request_read_result(
    RamlangUdpRequest *request,
    uint8_t *output,
    uint64_t capacity
);
int32_t ramlang_udp_request_cancel(RamlangUdpRequest *request);
void ramlang_udp_request_close(RamlangUdpRequest *request);

#ifdef __cplusplus
}
#endif

#endif
