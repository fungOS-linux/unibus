#ifndef UNIBUS_SDK_H
#define UNIBUS_SDK_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef struct UnibusSession UnibusSession;
uint32_t unibus_sdk_abi_version(void);
/* All handle calls must be serialized. -1 error, -2 caught panic.
 * connect/publish/status: 0 success. poll: 0 empty, 1 event.
 * Publish success is queued, NOT router acknowledgement.
 * Strings are UTF-8 JSON. Returned strings must use string_free, not free(). */
int32_t unibus_sdk_connect(const char *config, UnibusSession **out);
/* publish returns an allocated request ID in out, for reply correlation. */
int32_t unibus_sdk_publish(UnibusSession *, const char *json, char **out);
int32_t unibus_sdk_poll(UnibusSession *, char **out);
int32_t unibus_sdk_status(UnibusSession *, char **out);
void unibus_sdk_string_free(char *);
void unibus_sdk_close(UnibusSession *);
#ifdef __cplusplus
}
#endif
#endif
