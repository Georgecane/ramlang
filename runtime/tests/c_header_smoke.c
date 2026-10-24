#include "ramlang_runtime.h"

#include <string.h>

int main(void) {
    if (ramlang_tcp_request_is_ready(NULL) != -1) {
        return 1;
    }

    const char *error = ramlang_last_error();
    if (error == NULL || strstr(error, "request must not be null") == NULL) {
        return 2;
    }

    if (ramlang_tcp_request_wait(NULL, 0) != -1) {
        return 3;
    }
    if (ramlang_tcp_request_read_result(NULL, NULL, 0) != -1) {
        return 4;
    }
    if (ramlang_tcp_request_cancel(NULL) != -1) {
        return 5;
    }

    if (ramlang_udp_request_is_ready(NULL) != -1) {
        return 6;
    }
    if (ramlang_udp_request_wait(NULL, 0) != -1) {
        return 7;
    }
    if (ramlang_udp_request_read_result(NULL, NULL, 0) != -1) {
        return 8;
    }
    if (ramlang_udp_request_cancel(NULL) != -1) {
        return 9;
    }
    if (ramlang_future_await_owned_i64(NULL) != -1) {
        return 16;
    }
    if (ramlang_future_spawn_owned_i64(NULL, NULL, NULL) != NULL) {
        return 17;
    }
    if (ramlang_task_scope_track_tcp(NULL, NULL) != -1) {
        return 10;
    }
    if (ramlang_task_scope_track_udp(NULL, NULL) != -1) {
        return 11;
    }

    RamlangTaskScope *scope = ramlang_task_scope_new();
    if (scope == NULL || ramlang_task_scope_len(scope) != 0) {
        return 12;
    }
    if (ramlang_task_scope_cancel(scope) != 0) {
        return 13;
    }

    if (ramlang_byte_buffer_len(NULL) != -1) {
        return 14;
    }
    if (ramlang_byte_buffer_data(NULL) != NULL) {
        return 15;
    }

    if (ramlang_tcp_read_bytes_future(NULL) != NULL) {
        return 18;
    }
    if (ramlang_tcp_write_bytes_future(NULL, NULL, 0) != NULL) {
        return 19;
    }
    if (ramlang_udp_recv_bytes_future(NULL) != NULL) {
        return 20;
    }
    if (ramlang_udp_packet_bytes(NULL) != NULL) {
        return 21;
    }
    if (ramlang_udp_send_bytes_future(NULL, NULL, NULL, 0) != NULL) {
        return 22;
    }

    /* Closing null opaque handles is explicitly supported as a no-op. */
    ramlang_byte_buffer_close(NULL);
    ramlang_task_scope_close(scope);
    ramlang_task_scope_close(NULL);
    ramlang_tcp_request_close(NULL);
    ramlang_udp_request_close(NULL);
    ramlang_tcp_close(NULL);
    ramlang_tcp_close(NULL);
    ramlang_udp_close(NULL);
    return 0;
}
