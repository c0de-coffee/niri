// Test-only shim: hide DRM_CAP_SYNCOBJ_TIMELINE so gnome-remote-desktop skips explicit sync.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#define DRM_CAP_SYNCOBJ_TIMELINE 0x14
int drmGetCap(int fd, uint64_t cap, uint64_t *value) {
    static int (*real)(int, uint64_t, uint64_t *);
    if (!real) real = dlsym(RTLD_NEXT, "drmGetCap");
    int ret = real(fd, cap, value);
    if (cap == DRM_CAP_SYNCOBJ_TIMELINE && ret == 0) *value = 0;
    return ret;
}
