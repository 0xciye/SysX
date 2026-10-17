/* SysX ownership shim. Upstream structures remain opaque to Rust. */
#include "abc_online.h"
#include <stdlib.h>
#include <string.h>

typedef struct sysx_abc_profile {
    abc_online_model_t model;
    abc_online_renderer_t prepared;
} sysx_abc_profile;

void *sysx_abc_prepare(const void *blob, size_t bytes, const int16_t *reports, size_t count) {
    sysx_abc_profile *p;
    size_t i;
    if (!reports || count != 256) return NULL;
    p = (sysx_abc_profile *)calloc(1, sizeof(*p));
    if (!p) return NULL;
    if (abc_online_model_init(&p->model, blob, bytes) != ABC_FIXED_OK ||
        abc_online_reset(&p->prepared, &p->model) != ABC_FIXED_OK) { free(p); return NULL; }
    for (i = 0; i < count; ++i) {
        /* Physical and SysX counts use Y down; ABCurves uses canonical Y up.
         * -32768 has no positive int16 counterpart: reject, never wrap it. */
        if (reports[2*i+1] == INT16_MIN || abc_online_observe_raw(
            &p->prepared, reports[2*i], (int16_t)-reports[2*i+1]) != ABC_FIXED_OK) {
            free(p); return NULL;
        }
    }
    return p;
}
void sysx_abc_free(void *p) { free(p); }
void *sysx_abc_begin(const void *profile, uint64_t seed) {
    const sysx_abc_profile *p = (const sysx_abc_profile *)profile;
    abc_online_renderer_t *event;
    if (!p) return NULL;
    event = (abc_online_renderer_t *)malloc(sizeof(*event));
    if (!event) return NULL;
    memcpy(event, &p->prepared, sizeof(*event));
    if (abc_online_begin(event, seed) != ABC_FIXED_OK) { free(event); return NULL; }
    return event;
}
int sysx_abc_step(void *event, int32_t x, int32_t y, int16_t *out) {
    abc_fixed_report_t report;
    int status;
    if (!event || !out || y == INT32_MIN) return ABC_FIXED_ERR_ARGUMENT;
    status = abc_online_step((abc_online_renderer_t *)event, x, -y, &report);
    if (status == ABC_FIXED_OK) { out[0] = report.dx; out[1] = (int16_t)-report.dy; }
    return status;
}
