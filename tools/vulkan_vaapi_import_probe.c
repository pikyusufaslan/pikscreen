#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <vulkan/vulkan.h>

static int va_ok(VAStatus status, const char *operation) {
    if (status == VA_STATUS_SUCCESS) return 1;
    fprintf(stderr, "%s failed: %s\n", operation, vaErrorStr(status));
    return 0;
}

static int vk_ok(VkResult result, const char *operation) {
    if (result == VK_SUCCESS) return 1;
    fprintf(stderr, "%s failed: Vulkan error %d\n", operation, result);
    return 0;
}

static uint32_t memory_type_index(
    VkPhysicalDevice device,
    uint32_t type_bits,
    VkMemoryPropertyFlags wanted) {
    VkPhysicalDeviceMemoryProperties properties;
    vkGetPhysicalDeviceMemoryProperties(device, &properties);
    for (uint32_t index = 0; index < properties.memoryTypeCount; index++) {
        if ((type_bits & (1u << index)) &&
            (properties.memoryTypes[index].propertyFlags & wanted) == wanted) {
            return index;
        }
    }
    return UINT32_MAX;
}

int main(void) {
    int drm_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (drm_fd < 0) {
        fprintf(stderr, "Could not open render node: %s\n", strerror(errno));
        return 1;
    }
    VADisplay va_display = vaGetDisplayDRM(drm_fd);
    int va_major = 0;
    int va_minor = 0;
    if (!va_display || !va_ok(vaInitialize(va_display, &va_major, &va_minor), "vaInitialize")) {
        close(drm_fd);
        return 1;
    }
    VASurfaceID surface = VA_INVALID_ID;
    if (!va_ok(vaCreateSurfaces(
            va_display, VA_RT_FORMAT_YUV420, 1920, 1080, &surface, 1, NULL, 0),
            "vaCreateSurfaces")) {
        vaTerminate(va_display);
        close(drm_fd);
        return 1;
    }
    VADRMPRIMESurfaceDescriptor prime;
    memset(&prime, 0, sizeof(prime));
    if (!va_ok(vaExportSurfaceHandle(
            va_display, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS, &prime),
            "vaExportSurfaceHandle")) {
        vaDestroySurfaces(va_display, &surface, 1);
        vaTerminate(va_display);
        close(drm_fd);
        return 1;
    }
    int import_fd = fcntl(prime.objects[0].fd, F_DUPFD_CLOEXEC, 3);
    if (import_fd < 0) {
        fprintf(stderr, "Could not duplicate VAAPI DMA-BUF: %s\n", strerror(errno));
        return 1;
    }

    VkApplicationInfo application = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "PikScreen VAAPI import probe",
        .apiVersion = VK_API_VERSION_1_1,
    };
    VkInstanceCreateInfo instance_info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &application,
    };
    VkInstance instance = VK_NULL_HANDLE;
    if (!vk_ok(vkCreateInstance(&instance_info, NULL, &instance), "vkCreateInstance")) return 1;
    uint32_t device_count = 0;
    vkEnumeratePhysicalDevices(instance, &device_count, NULL);
    VkPhysicalDevice *devices = calloc(device_count, sizeof(*devices));
    vkEnumeratePhysicalDevices(instance, &device_count, devices);
    VkPhysicalDevice physical = VK_NULL_HANDLE;
    for (uint32_t index = 0; index < device_count; index++) {
        VkPhysicalDeviceProperties properties;
        vkGetPhysicalDeviceProperties(devices[index], &properties);
        if (properties.vendorID == 0x1002 && properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) {
            physical = devices[index];
            break;
        }
    }
    free(devices);
    if (physical == VK_NULL_HANDLE) {
        fprintf(stderr, "Could not find the AMD discrete Vulkan device.\n");
        return 1;
    }
    uint32_t family_count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &family_count, NULL);
    VkQueueFamilyProperties *families = calloc(family_count, sizeof(*families));
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &family_count, families);
    uint32_t family = UINT32_MAX;
    for (uint32_t index = 0; index < family_count; index++) {
        if (families[index].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            family = index;
            break;
        }
    }
    free(families);
    if (family == UINT32_MAX) {
        fprintf(stderr, "Could not find a Vulkan graphics queue.\n");
        return 1;
    }
    const float priority = 1.0f;
    VkDeviceQueueCreateInfo queue = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = family,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    const char *extensions[] = {
        VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
        VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
    };
    VkDeviceCreateInfo device_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &queue,
        .enabledExtensionCount = 2,
        .ppEnabledExtensionNames = extensions,
    };
    VkDevice device = VK_NULL_HANDLE;
    if (!vk_ok(vkCreateDevice(physical, &device_info, NULL, &device), "vkCreateDevice")) return 1;

    VkExternalMemoryImageCreateInfo external_image = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    VkImageCreateInfo image_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .pNext = &external_image,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_R8_UNORM,
        .extent = {.width = prime.width, .height = prime.height, .depth = 1},
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_LINEAR,
        .usage = VK_IMAGE_USAGE_SAMPLED_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkImage image = VK_NULL_HANDLE;
    if (!vk_ok(vkCreateImage(device, &image_info, NULL, &image), "vkCreateImage(R8 external DMA-BUF)")) return 1;
    VkMemoryRequirements requirements;
    vkGetImageMemoryRequirements(device, image, &requirements);
    uint32_t memory_type = memory_type_index(physical, requirements.memoryTypeBits, 0);
    if (memory_type == UINT32_MAX) {
        fprintf(stderr, "Could not find a Vulkan memory type for the imported DMA-BUF.\n");
        return 1;
    }
    VkImportMemoryFdInfoKHR import = {
        .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        .fd = import_fd,
    };
    VkMemoryAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &import,
        .allocationSize = prime.objects[0].size,
        .memoryTypeIndex = memory_type,
    };
    VkDeviceMemory memory = VK_NULL_HANDLE;
    if (!vk_ok(vkAllocateMemory(device, &allocation, NULL, &memory), "vkAllocateMemory(import DMA-BUF)")) return 1;
    if (!vk_ok(vkBindImageMemory(device, image, memory, 0), "vkBindImageMemory(import DMA-BUF)")) return 1;
    printf(
        "Vulkan imported the VAAPI DMA-BUF as an R8 image: %ux%u, VA pitch=%u, allocation=%llu bytes.\n",
        prime.width,
        prime.height,
        prime.layers[0].pitch[0],
        (unsigned long long)prime.objects[0].size);
    vkDestroyImage(device, image, NULL);
    vkFreeMemory(device, memory, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    for (uint32_t index = 0; index < prime.num_objects; index++) close(prime.objects[index].fd);
    vaDestroySurfaces(va_display, &surface, 1);
    vaTerminate(va_display);
    close(drm_fd);
    return 0;
}
