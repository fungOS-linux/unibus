/* Build library with cargo build -p unibus-ffi, then on macOS:
 * cc -I crates/unibus-ffi/include crates/unibus-ffi/examples/poll.c 
 *    -L target/debug -lunibus_ffi -Wl,-rpath,"$PWD/target/debug" -o /tmp/unibus-poll
 * Supply a ClientConfig JSON string as argv[1]. No token value in config. */
#include "unibus.h"
#include <stdio.h>
#include <string.h>
#include <time.h>
int main(int argc, char **argv) {
    UnibusSession *session = NULL;
    if (argc != 2 || unibus_sdk_connect(argv[1], &session) != 0) {
        fputs("ClientConfig required or connection failed\n", stderr);
        return 1;
    }
    int result = 0;
    for (;;) {
        char *json = NULL;
        int code = unibus_sdk_poll(session, &json);
        if (code < 0) { result = 1; break; }
        if (code == 1) { puts(json); unibus_sdk_string_free(json); }
        if (unibus_sdk_status(session, &json) != 0) { result = 1; break; }
        int disconnected = strstr(json, "\"connected\":false") != NULL;
        unibus_sdk_string_free(json);
        if (disconnected) { fputs("Disconnected\n", stderr); result = 1; break; }
        const struct timespec delay = {0, 16000000};
        nanosleep(&delay, NULL);
    }
    unibus_sdk_close(session);
    return result;
}
