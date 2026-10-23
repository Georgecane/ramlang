#include <stdarg.h>
#include <stdint.h>

int32_t variadic_sum(int32_t base, ...) {
    va_list arguments;
    va_start(arguments, base);
    int small = va_arg(arguments, int);
    int medium = va_arg(arguments, int);
    int enabled = va_arg(arguments, int);
    double floating = va_arg(arguments, double);
    va_end(arguments);
    return base + small + medium + enabled + (int)floating;
}
