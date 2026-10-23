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

    /* Closing null opaque handles is explicitly supported as a no-op. */
    ramlang_tcp_request_close(NULL);
    ramlang_tcp_close(NULL);
    ramlang_udp_close(NULL);
    return 0;
}
