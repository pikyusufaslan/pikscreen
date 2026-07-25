#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <drm/drm_fourcc.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <vulkan/vulkan.h>

static int vk_ok(VkResult result, const char *operation) {
    if (result == VK_SUCCESS) return 1;
    fprintf(stderr, "%s failed: Vulkan error %d\n", operation, result);
    return 0;
}

static int va_ok(VAStatus status, const char *operation) {
    if (status == VA_STATUS_SUCCESS) return 1;
    fprintf(stderr, "%s failed: %s\n", operation, vaErrorStr(status));
    return 0;
}

static uint32_t find_memory_type(VkPhysicalDevice device, uint32_t bits) {
    VkPhysicalDeviceMemoryProperties properties;
    vkGetPhysicalDeviceMemoryProperties(device, &properties);
    for (uint32_t index = 0; index < properties.memoryTypeCount; index++) {
        if (bits & (1u << index)) return index;
    }
    return UINT32_MAX;
}

int main(void) {
    VkApplicationInfo application = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "PikScreen Vulkan VAAPI export probe",
        .apiVersion = VK_API_VERSION_1_1,
    };
    VkInstanceCreateInfo instance_info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &application,
    };
    VkInstance instance = VK_NULL_HANDLE;
    if (!vk_ok(vkCreateInstance(&instance_info, NULL, &instance), "vkCreateInstance")) return 1;
    uint32_t count = 0;
    vkEnumeratePhysicalDevices(instance, &count, NULL);
    VkPhysicalDevice *devices = calloc(count, sizeof(*devices));
    vkEnumeratePhysicalDevices(instance, &count, devices);
    VkPhysicalDevice physical = VK_NULL_HANDLE;
    for (uint32_t index = 0; index < count; index++) {
        VkPhysicalDeviceProperties properties;
        vkGetPhysicalDeviceProperties(devices[index], &properties);
        if (properties.vendorID == 0x1002 && properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU) {
            physical = devices[index];
            break;
        }
    }
    free(devices);
    if (physical == VK_NULL_HANDLE) {
        fprintf(stderr, "AMD discrete Vulkan device was not found.\n");
        return 1;
    }
    VkFormatProperties format_properties;
    vkGetPhysicalDeviceFormatProperties(physical, VK_FORMAT_G8_B8R8_2PLANE_420_UNORM, &format_properties);
    if (!(format_properties.linearTilingFeatures & VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT)) {
        fprintf(stderr, "Vulkan driver does not expose linear NV12 sampling.\n");
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
    if (family == UINT32_MAX) return 1;
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
        .format = VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
        .extent = {.width = 1920, .height = 1080, .depth = 1},
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_LINEAR,
        .usage = VK_IMAGE_USAGE_SAMPLED_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkImage image = VK_NULL_HANDLE;
    if (!vk_ok(vkCreateImage(device, &image_info, NULL, &image), "vkCreateImage(NV12 external DMA-BUF)")) return 1;
    VkMemoryRequirements requirements;
    vkGetImageMemoryRequirements(device, image, &requirements);
    uint32_t memory_type = find_memory_type(physical, requirements.memoryTypeBits);
    if (memory_type == UINT32_MAX) return 1;
    VkExportMemoryAllocateInfo export = {
        .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    VkMemoryAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &export,
        .allocationSize = requirements.size,
        .memoryTypeIndex = memory_type,
    };
    VkDeviceMemory memory = VK_NULL_HANDLE;
    if (!vk_ok(vkAllocateMemory(device, &allocation, NULL, &memory), "vkAllocateMemory(export DMA-BUF)")) return 1;
    if (!vk_ok(vkBindImageMemory(device, image, memory, 0), "vkBindImageMemory(NV12)")) return 1;
    VkImageSubresource luma = {.aspectMask = VK_IMAGE_ASPECT_PLANE_0_BIT, .mipLevel = 0, .arrayLayer = 0};
    VkImageSubresource chroma = {.aspectMask = VK_IMAGE_ASPECT_PLANE_1_BIT, .mipLevel = 0, .arrayLayer = 0};
    VkSubresourceLayout luma_layout;
    VkSubresourceLayout chroma_layout;
    vkGetImageSubresourceLayout(device, image, &luma, &luma_layout);
    vkGetImageSubresourceLayout(device, image, &chroma, &chroma_layout);
    PFN_vkGetMemoryFdKHR get_memory_fd =
        (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(device, "vkGetMemoryFdKHR");
    if (!get_memory_fd) {
        fprintf(stderr, "vkGetMemoryFdKHR is unavailable.\n");
        return 1;
    }
    VkMemoryGetFdInfoKHR fd_info = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
        .memory = memory,
        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    int exported_fd = -1;
    if (!vk_ok(get_memory_fd(device, &fd_info, &exported_fd), "vkGetMemoryFdKHR(DMA-BUF)")) return 1;

    int drm_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    VADisplay va_display = vaGetDisplayDRM(drm_fd);
    int major = 0, minor = 0;
    if (!va_display || !va_ok(vaInitialize(va_display, &major, &minor), "vaInitialize")) return 1;
    VADRMPRIMESurfaceDescriptor descriptor;
    memset(&descriptor, 0, sizeof(descriptor));
    descriptor.fourcc = VA_FOURCC_NV12;
    descriptor.width = 1920;
    descriptor.height = 1080;
    descriptor.num_objects = 1;
    descriptor.objects[0].fd = exported_fd;
    descriptor.objects[0].size = requirements.size;
    descriptor.objects[0].drm_format_modifier = DRM_FORMAT_MOD_LINEAR;
    descriptor.num_layers = 1;
    descriptor.layers[0].drm_format = DRM_FORMAT_NV12;
    descriptor.layers[0].num_planes = 2;
    descriptor.layers[0].object_index[0] = 0;
    descriptor.layers[0].object_index[1] = 0;
    descriptor.layers[0].offset[0] = luma_layout.offset;
    descriptor.layers[0].offset[1] = chroma_layout.offset;
    descriptor.layers[0].pitch[0] = luma_layout.rowPitch;
    descriptor.layers[0].pitch[1] = chroma_layout.rowPitch;
    VASurfaceAttrib attributes[2];
    memset(attributes, 0, sizeof(attributes));
    attributes[0].type = VASurfaceAttribMemoryType;
    attributes[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attributes[0].value.type = VAGenericValueTypeInteger;
    attributes[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    attributes[1].type = VASurfaceAttribExternalBufferDescriptor;
    attributes[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attributes[1].value.type = VAGenericValueTypePointer;
    attributes[1].value.value.p = &descriptor;
    VASurfaceID imported_surface = VA_INVALID_ID;
    if (!va_ok(vaCreateSurfaces(
            va_display, VA_RT_FORMAT_YUV420, 1920, 1080, &imported_surface, 1, attributes, 2),
            "vaCreateSurfaces(import Vulkan DMA-BUF)")) return 1;
    printf(
        "VAAPI imported Vulkan NV12 DMA-BUF: luma pitch=%llu offset=%llu, chroma pitch=%llu offset=%llu.\n",
        (unsigned long long)luma_layout.rowPitch,
        (unsigned long long)luma_layout.offset,
        (unsigned long long)chroma_layout.rowPitch,
        (unsigned long long)chroma_layout.offset);
    vaDestroySurfaces(va_display, &imported_surface, 1);
    vaTerminate(va_display);
    close(drm_fd);
    close(exported_fd);
    vkDestroyImage(device, image, NULL);
    vkFreeMemory(device, memory, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    return 0;
}
