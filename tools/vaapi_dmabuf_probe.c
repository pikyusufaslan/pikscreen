#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>

static int va_ok(VAStatus status, const char *operation) {
    if (status == VA_STATUS_SUCCESS) {
        return 1;
    }
    fprintf(stderr, "%s failed: %s\n", operation, vaErrorStr(status));
    return 0;
}

int main(void) {
    const int drm_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (drm_fd < 0) {
        fprintf(stderr, "Could not open render node: %s\n", strerror(errno));
        return 1;
    }
    VADisplay display = vaGetDisplayDRM(drm_fd);
    if (!display) {
        fprintf(stderr, "vaGetDisplayDRM returned no display.\n");
        close(drm_fd);
        return 1;
    }
    int major = 0;
    int minor = 0;
    if (!va_ok(vaInitialize(display, &major, &minor), "vaInitialize")) {
        close(drm_fd);
        return 1;
    }
    VASurfaceID surface = VA_INVALID_ID;
    if (!va_ok(
            vaCreateSurfaces(
                display,
                VA_RT_FORMAT_YUV420,
                1920,
                1080,
                &surface,
                1,
                NULL,
                0),
            "vaCreateSurfaces")) {
        vaTerminate(display);
        close(drm_fd);
        return 1;
    }
    VADRMPRIMESurfaceDescriptor descriptor;
    memset(&descriptor, 0, sizeof(descriptor));
    if (!va_ok(
            vaExportSurfaceHandle(
                display,
                surface,
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS,
                &descriptor),
            "vaExportSurfaceHandle(DRM PRIME 2)")) {
        vaDestroySurfaces(display, &surface, 1);
        vaTerminate(display);
        close(drm_fd);
        return 1;
    }
    printf(
        "VAAPI DMA-BUF export works: %ux%u, fourcc=0x%08x, objects=%u, layers=%u, modifier=0x%016llx\n",
        descriptor.width,
        descriptor.height,
        descriptor.fourcc,
        descriptor.num_objects,
        descriptor.num_layers,
        (unsigned long long)descriptor.objects[0].drm_format_modifier);
    for (unsigned int index = 0; index < descriptor.num_objects; index++) {
        printf(
            "  object %u: fd=%d size=%u modifier=0x%016llx\n",
            index,
            descriptor.objects[index].fd,
            descriptor.objects[index].size,
            (unsigned long long)descriptor.objects[index].drm_format_modifier);
        close(descriptor.objects[index].fd);
    }
    for (unsigned int layer = 0; layer < descriptor.num_layers; layer++) {
        printf(
            "  layer %u: fourcc=0x%08x planes=%u object=%u offset=%u pitch=%u\n",
            layer,
            descriptor.layers[layer].drm_format,
            descriptor.layers[layer].num_planes,
            descriptor.layers[layer].object_index[0],
            descriptor.layers[layer].offset[0],
            descriptor.layers[layer].pitch[0]);
    }
    vaDestroySurfaces(display, &surface, 1);
    vaTerminate(display);
    close(drm_fd);
    return 0;
}
