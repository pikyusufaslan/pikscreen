#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <drm/drm_fourcc.h>
#include <libavcodec/avcodec.h>
#include <libavutil/error.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>
#include <vulkan/vulkan.h>

enum { FRAME_WIDTH = 1920, FRAME_HEIGHT = 1080 };

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

static int av_ok(int result, const char *operation) {
    if (result >= 0) return 1;
    char error[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(result, error, sizeof(error));
    fprintf(stderr, "%s failed: %s\n", operation, error);
    return 0;
}

static uint32_t find_memory_type(VkPhysicalDevice device, uint32_t bits, VkMemoryPropertyFlags required) {
    VkPhysicalDeviceMemoryProperties properties;
    vkGetPhysicalDeviceMemoryProperties(device, &properties);
    for (uint32_t index = 0; index < properties.memoryTypeCount; index++) {
        if ((bits & (1u << index)) && (properties.memoryTypes[index].propertyFlags & required) == required) {
            return index;
        }
    }
    return UINT32_MAX;
}

static uint32_t *read_spirv(const char *path, size_t *word_count) {
    FILE *file = fopen(path, "rb");
    if (!file) {
        fprintf(stderr, "Could not open Vulkan shader %s: %s\n", path, strerror(errno));
        return NULL;
    }
    if (fseek(file, 0, SEEK_END) || ftell(file) <= 0) {
        fclose(file);
        return NULL;
    }
    long bytes = ftell(file);
    rewind(file);
    if (bytes % 4) {
        fprintf(stderr, "Vulkan shader %s is not valid SPIR-V.\n", path);
        fclose(file);
        return NULL;
    }
    uint32_t *words = malloc((size_t)bytes);
    if (!words || fread(words, 1, (size_t)bytes, file) != (size_t)bytes) {
        free(words);
        fclose(file);
        return NULL;
    }
    fclose(file);
    *word_count = (size_t)bytes / 4;
    return words;
}

typedef struct {
    VkDevice device;
    VkPhysicalDevice physical;
    uint32_t family;
    uint32_t width;
    uint32_t height;
    VkImage output;
    VkBuffer input;
    VkDeviceMemory input_memory;
    uint8_t *mapped_input;
    VkImageView output_view;
    VkDescriptorSetLayout set_layout;
    VkPipelineLayout pipeline_layout;
    VkPipeline pipeline;
    VkDescriptorPool descriptor_pool;
    VkDescriptorSet descriptor;
    VkCommandPool command_pool;
    VkCommandBuffer command;
    VkQueue queue;
    int initialized_layout;
} GpuCompositor;

static int read_file_exact(const char *path, void *destination, size_t bytes) {
    FILE *file = fopen(path, "rb");
    if (!file || fread(destination, 1, bytes, file) != bytes) {
        fprintf(stderr, "Could not read %zu bytes from %s.\n", bytes, path);
        if (file) fclose(file);
        return 0;
    }
    fclose(file);
    return 1;
}

static void gpu_compositor_destroy(GpuCompositor *compositor) {
    if (!compositor->device) return;
    if (compositor->mapped_input) vkUnmapMemory(compositor->device, compositor->input_memory);
    if (compositor->command_pool) vkDestroyCommandPool(compositor->device, compositor->command_pool, NULL);
    if (compositor->descriptor_pool) vkDestroyDescriptorPool(compositor->device, compositor->descriptor_pool, NULL);
    if (compositor->pipeline) vkDestroyPipeline(compositor->device, compositor->pipeline, NULL);
    if (compositor->pipeline_layout) vkDestroyPipelineLayout(compositor->device, compositor->pipeline_layout, NULL);
    if (compositor->set_layout) vkDestroyDescriptorSetLayout(compositor->device, compositor->set_layout, NULL);
    if (compositor->output_view) vkDestroyImageView(compositor->device, compositor->output_view, NULL);
    if (compositor->input_memory) vkFreeMemory(compositor->device, compositor->input_memory, NULL);
    if (compositor->input) vkDestroyBuffer(compositor->device, compositor->input, NULL);
    memset(compositor, 0, sizeof(*compositor));
}

static int gpu_compositor_init(
    GpuCompositor *compositor,
    VkDevice device,
    VkPhysicalDevice physical,
    uint32_t family,
    VkImage output,
    uint32_t width,
    uint32_t height,
    const char *shader_path,
    const char *canvas_path,
    const char *cursor_path) {
    memset(compositor, 0, sizeof(*compositor));
    compositor->device = device;
    compositor->physical = physical;
    compositor->family = family;
    compositor->width = width;
    compositor->height = height;
    compositor->output = output;
    const VkDeviceSize frame_bytes = (VkDeviceSize)width * height * 4;
    const VkDeviceSize cursor_bytes = 618 * 958 * 4;
    const VkDeviceSize buffer_bytes = frame_bytes * 2 + cursor_bytes;
    size_t word_count = 0;
    uint32_t *words = read_spirv(shader_path, &word_count);
    if (!words) goto fail;
    VkBufferCreateInfo input_info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = buffer_bytes,
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    if (!vk_ok(vkCreateBuffer(device, &input_info, NULL, &compositor->input), "vkCreateBuffer(stream input)")) goto fail;
    VkMemoryRequirements requirements;
    vkGetBufferMemoryRequirements(device, compositor->input, &requirements);
    uint32_t memory_type = find_memory_type(
        physical,
        requirements.memoryTypeBits,
        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    if (memory_type == UINT32_MAX) goto fail;
    VkMemoryAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = requirements.size,
        .memoryTypeIndex = memory_type,
    };
    if (!vk_ok(vkAllocateMemory(device, &allocation, NULL, &compositor->input_memory), "vkAllocateMemory(stream input)")) goto fail;
    if (!vk_ok(vkBindBufferMemory(device, compositor->input, compositor->input_memory, 0), "vkBindBufferMemory(stream input)")) goto fail;
    if (!vk_ok(vkMapMemory(device, compositor->input_memory, 0, buffer_bytes, 0, (void **)&compositor->mapped_input), "vkMapMemory(stream input)")) goto fail;
    if (!read_file_exact(canvas_path, compositor->mapped_input + frame_bytes, (size_t)frame_bytes)) goto fail;
    if (!read_file_exact(cursor_path, compositor->mapped_input + frame_bytes * 2, (size_t)cursor_bytes)) goto fail;
    VkImageViewCreateInfo view_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = output,
        .viewType = VK_IMAGE_VIEW_TYPE_2D,
        .format = VK_FORMAT_B8G8R8A8_UNORM,
        .subresourceRange = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .levelCount = 1, .layerCount = 1},
    };
    if (!vk_ok(vkCreateImageView(device, &view_info, NULL, &compositor->output_view), "vkCreateImageView(stream output)")) goto fail;
    VkDescriptorSetLayoutBinding bindings[2] = {
        {.binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT},
        {.binding = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT},
    };
    VkDescriptorSetLayoutCreateInfo set_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 2,
        .pBindings = bindings,
    };
    if (!vk_ok(vkCreateDescriptorSetLayout(device, &set_info, NULL, &compositor->set_layout), "vkCreateDescriptorSetLayout(stream)")) goto fail;
    VkPushConstantRange push = {.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT, .size = 28};
    VkPipelineLayoutCreateInfo layout_info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1,
        .pSetLayouts = &compositor->set_layout,
        .pushConstantRangeCount = 1,
        .pPushConstantRanges = &push,
    };
    if (!vk_ok(vkCreatePipelineLayout(device, &layout_info, NULL, &compositor->pipeline_layout), "vkCreatePipelineLayout(stream)")) goto fail;
    VkShaderModuleCreateInfo shader_info = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = word_count * 4, .pCode = words};
    VkShaderModule shader = VK_NULL_HANDLE;
    if (!vk_ok(vkCreateShaderModule(device, &shader_info, NULL, &shader), "vkCreateShaderModule(stream)")) goto fail;
    VkPipelineShaderStageCreateInfo stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = shader, .pName = "main"};
    VkComputePipelineCreateInfo pipeline_info = {.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, .stage = stage, .layout = compositor->pipeline_layout};
    VkResult pipeline_result = vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL, &compositor->pipeline);
    vkDestroyShaderModule(device, shader, NULL);
    if (!vk_ok(pipeline_result, "vkCreateComputePipelines(stream)")) goto fail;
    VkDescriptorPoolSize pool_sizes[2] = {{VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1}, {VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1}};
    VkDescriptorPoolCreateInfo pool_info = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1, .poolSizeCount = 2, .pPoolSizes = pool_sizes};
    if (!vk_ok(vkCreateDescriptorPool(device, &pool_info, NULL, &compositor->descriptor_pool), "vkCreateDescriptorPool(stream)")) goto fail;
    VkDescriptorSetAllocateInfo descriptor_info = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = compositor->descriptor_pool, .descriptorSetCount = 1, .pSetLayouts = &compositor->set_layout};
    if (!vk_ok(vkAllocateDescriptorSets(device, &descriptor_info, &compositor->descriptor), "vkAllocateDescriptorSets(stream)")) goto fail;
    VkDescriptorBufferInfo input_descriptor = {.buffer = compositor->input, .offset = 0, .range = buffer_bytes};
    VkDescriptorImageInfo output_descriptor = {.imageView = compositor->output_view, .imageLayout = VK_IMAGE_LAYOUT_GENERAL};
    VkWriteDescriptorSet writes[2] = {
        {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = compositor->descriptor, .dstBinding = 0, .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &input_descriptor},
        {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = compositor->descriptor, .dstBinding = 1, .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, .pImageInfo = &output_descriptor},
    };
    vkUpdateDescriptorSets(device, 2, writes, 0, NULL);
    VkCommandPoolCreateInfo command_pool_info = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = family};
    if (!vk_ok(vkCreateCommandPool(device, &command_pool_info, NULL, &compositor->command_pool), "vkCreateCommandPool(stream)")) goto fail;
    VkCommandBufferAllocateInfo command_info = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = compositor->command_pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    if (!vk_ok(vkAllocateCommandBuffers(device, &command_info, &compositor->command), "vkAllocateCommandBuffers(stream)")) goto fail;
    vkGetDeviceQueue(device, family, 0, &compositor->queue);
    free(words);
    return 1;
fail:
    free(words);
    gpu_compositor_destroy(compositor);
    return 0;
}

static int gpu_compositor_render(
    GpuCompositor *compositor,
    const uint8_t *frame,
    float scale,
    float crop_x,
    float crop_y,
    float cursor_x,
    float cursor_y) {
    const size_t frame_bytes = (size_t)compositor->width * compositor->height * 4;
    memcpy(compositor->mapped_input, frame, frame_bytes);
    if (!vk_ok(vkResetCommandBuffer(compositor->command, 0), "vkResetCommandBuffer(stream)")) return 0;
    VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    if (!vk_ok(vkBeginCommandBuffer(compositor->command, &begin), "vkBeginCommandBuffer(stream)")) return 0;
    if (!compositor->initialized_layout) {
        VkImageMemoryBarrier barrier = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_GENERAL,
            .dstAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
            .image = compositor->output,
            .subresourceRange = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .levelCount = 1, .layerCount = 1},
        };
        vkCmdPipelineBarrier(
            compositor->command,
            VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
            VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
            0, 0, NULL, 0, NULL, 1, &barrier);
    }
    vkCmdBindPipeline(compositor->command, VK_PIPELINE_BIND_POINT_COMPUTE, compositor->pipeline);
    vkCmdBindDescriptorSets(compositor->command, VK_PIPELINE_BIND_POINT_COMPUTE, compositor->pipeline_layout, 0, 1, &compositor->descriptor, 0, NULL);
    struct {
        uint32_t use_source;
        uint32_t use_canvas;
        float scale;
        float crop_x;
        float crop_y;
        float cursor_x;
        float cursor_y;
    } parameters = {1, 1, scale, crop_x, crop_y, cursor_x, cursor_y};
    vkCmdPushConstants(compositor->command, compositor->pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(parameters), &parameters);
    vkCmdDispatch(compositor->command, (compositor->width + 15) / 16, (compositor->height + 15) / 16, 1);
    if (!vk_ok(vkEndCommandBuffer(compositor->command), "vkEndCommandBuffer(stream)")) return 0;
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &compositor->command};
    if (!vk_ok(vkQueueSubmit(compositor->queue, 1, &submit, VK_NULL_HANDLE), "vkQueueSubmit(stream composition)")) return 0;
    /* VAAPI consumes this exported DMA-BUF immediately after this submission.
     * The DRM fence attached to the shared buffer provides producer/consumer
     * ordering; a CPU-side vkQueueWaitIdle here serialized every frame. */
    compositor->initialized_layout = 1;
    return 1;
}

static int render_frame(
    VkDevice device,
    VkPhysicalDevice physical,
    uint32_t family,
    VkImage image,
    uint32_t width,
    uint32_t height,
    const char *shader_path,
    const char *raw_path,
    const char *raw_canvas_path,
    const char *raw_cursor_path,
    float scale,
    float crop_x,
    float crop_y,
    float cursor_x,
    float cursor_y) {
    int success = 0;
    size_t word_count = 0;
    uint32_t *words = read_spirv(shader_path, &word_count);
    VkShaderModule shader = VK_NULL_HANDLE;
    VkImageView view = VK_NULL_HANDLE;
    VkDescriptorSetLayout set_layout = VK_NULL_HANDLE;
    VkPipelineLayout pipeline_layout = VK_NULL_HANDLE;
    VkPipeline pipeline = VK_NULL_HANDLE;
    VkDescriptorPool descriptor_pool = VK_NULL_HANDLE;
    VkCommandPool command_pool = VK_NULL_HANDLE;
    VkCommandBuffer command = VK_NULL_HANDLE;
    VkBuffer source = VK_NULL_HANDLE;
    VkDeviceMemory source_memory = VK_NULL_HANDLE;
    const VkDeviceSize frame_size = (VkDeviceSize)width * height * 4;
    const VkDeviceSize cursor_size = 618 * 958 * 4;
    const VkDeviceSize source_size = raw_path
        ? frame_size * (raw_canvas_path ? 2 : 1) + (raw_cursor_path ? cursor_size : 0)
        : 4;
    if (!words) goto cleanup;
    VkBufferCreateInfo source_info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = source_size,
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    if (!vk_ok(vkCreateBuffer(device, &source_info, NULL, &source), "vkCreateBuffer(source frame)")) goto cleanup;
    VkMemoryRequirements source_requirements;
    vkGetBufferMemoryRequirements(device, source, &source_requirements);
    uint32_t source_memory_type = find_memory_type(
        physical,
        source_requirements.memoryTypeBits,
        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    if (source_memory_type == UINT32_MAX) {
        fprintf(stderr, "No host-visible memory type exists for the source frame.\n");
        goto cleanup;
    }
    VkMemoryAllocateInfo source_allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = source_requirements.size,
        .memoryTypeIndex = source_memory_type,
    };
    if (!vk_ok(vkAllocateMemory(device, &source_allocation, NULL, &source_memory), "vkAllocateMemory(source frame)")) goto cleanup;
    if (!vk_ok(vkBindBufferMemory(device, source, source_memory, 0), "vkBindBufferMemory(source frame)")) goto cleanup;
    void *source_bytes = NULL;
    if (!vk_ok(vkMapMemory(device, source_memory, 0, source_size, 0, &source_bytes), "vkMapMemory(source frame)")) goto cleanup;
    memset(source_bytes, 0, (size_t)source_size);
    if (raw_path) {
        FILE *raw = fopen(raw_path, "rb");
        if (!raw || fread(source_bytes, 1, (size_t)frame_size, raw) != (size_t)frame_size) {
            fprintf(stderr, "Could not read a full %ux%u RGBA source frame from %s.\n", width, height, raw_path);
            if (raw) fclose(raw);
            vkUnmapMemory(device, source_memory);
            goto cleanup;
        }
        fclose(raw);
        if (raw_canvas_path) {
            FILE *canvas = fopen(raw_canvas_path, "rb");
            if (!canvas || fread((uint8_t *)source_bytes + frame_size, 1, (size_t)frame_size, canvas) != (size_t)frame_size) {
                fprintf(stderr, "Could not read a full %ux%u RGBA static canvas from %s.\n", width, height, raw_canvas_path);
                if (canvas) fclose(canvas);
                vkUnmapMemory(device, source_memory);
                goto cleanup;
            }
            fclose(canvas);
        }
        if (raw_cursor_path) {
            FILE *cursor = fopen(raw_cursor_path, "rb");
            if (!cursor || fread((uint8_t *)source_bytes + frame_size * 2, 1, (size_t)cursor_size, cursor) != (size_t)cursor_size) {
                fprintf(stderr, "Could not read the Tahoe cursor RGBA asset from %s.\n", raw_cursor_path);
                if (cursor) fclose(cursor);
                vkUnmapMemory(device, source_memory);
                goto cleanup;
            }
            fclose(cursor);
        }
    }
    vkUnmapMemory(device, source_memory);
    VkShaderModuleCreateInfo shader_info = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = word_count * sizeof(*words),
        .pCode = words,
    };
    if (!vk_ok(vkCreateShaderModule(device, &shader_info, NULL, &shader), "vkCreateShaderModule")) goto cleanup;
    VkImageViewCreateInfo view_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = image,
        .viewType = VK_IMAGE_VIEW_TYPE_2D,
        .format = VK_FORMAT_B8G8R8A8_UNORM,
        .subresourceRange = {
            .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
            .levelCount = 1,
            .layerCount = 1,
        },
    };
    if (!vk_ok(vkCreateImageView(device, &view_info, NULL, &view), "vkCreateImageView(RGBA)")) goto cleanup;
    VkDescriptorSetLayoutBinding bindings[2] = {
        {
            .binding = 0,
            .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
            .descriptorCount = 1,
            .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
        },
        {
            .binding = 1,
            .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
            .descriptorCount = 1,
            .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
        },
    };
    VkDescriptorSetLayoutCreateInfo set_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 2,
        .pBindings = bindings,
    };
    if (!vk_ok(vkCreateDescriptorSetLayout(device, &set_info, NULL, &set_layout), "vkCreateDescriptorSetLayout")) goto cleanup;
    VkPushConstantRange push_constant = {.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT, .size = 28};
    VkPipelineLayoutCreateInfo pipeline_layout_info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1,
        .pSetLayouts = &set_layout,
        .pushConstantRangeCount = 1,
        .pPushConstantRanges = &push_constant,
    };
    if (!vk_ok(vkCreatePipelineLayout(device, &pipeline_layout_info, NULL, &pipeline_layout), "vkCreatePipelineLayout")) goto cleanup;
    VkPipelineShaderStageCreateInfo stage = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_COMPUTE_BIT,
        .module = shader,
        .pName = "main",
    };
    VkComputePipelineCreateInfo pipeline_info = {
        .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = stage,
        .layout = pipeline_layout,
    };
    if (!vk_ok(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL, &pipeline), "vkCreateComputePipelines")) goto cleanup;
    VkDescriptorPoolSize pool_sizes[2] = {
        {.type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1},
        {.type = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, .descriptorCount = 1},
    };
    VkDescriptorPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 1,
        .poolSizeCount = 2,
        .pPoolSizes = pool_sizes,
    };
    if (!vk_ok(vkCreateDescriptorPool(device, &pool_info, NULL, &descriptor_pool), "vkCreateDescriptorPool")) goto cleanup;
    VkDescriptorSetAllocateInfo descriptor_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool = descriptor_pool,
        .descriptorSetCount = 1,
        .pSetLayouts = &set_layout,
    };
    VkDescriptorSet descriptor;
    if (!vk_ok(vkAllocateDescriptorSets(device, &descriptor_info, &descriptor), "vkAllocateDescriptorSets")) goto cleanup;
    VkDescriptorBufferInfo source_descriptor = {.buffer = source, .offset = 0, .range = source_size};
    VkDescriptorImageInfo image_descriptor = {.imageView = view, .imageLayout = VK_IMAGE_LAYOUT_GENERAL};
    VkWriteDescriptorSet writes[2] = {
        {
            .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            .dstSet = descriptor,
            .dstBinding = 0,
            .descriptorCount = 1,
            .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
            .pBufferInfo = &source_descriptor,
        },
        {
            .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            .dstSet = descriptor,
            .dstBinding = 1,
            .descriptorCount = 1,
            .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
            .pImageInfo = &image_descriptor,
        },
    };
    vkUpdateDescriptorSets(device, 2, writes, 0, NULL);
    VkCommandPoolCreateInfo command_pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = family,
    };
    if (!vk_ok(vkCreateCommandPool(device, &command_pool_info, NULL, &command_pool), "vkCreateCommandPool")) goto cleanup;
    VkCommandBufferAllocateInfo command_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = command_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    if (!vk_ok(vkAllocateCommandBuffers(device, &command_info, &command), "vkAllocateCommandBuffers")) goto cleanup;
    VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    if (!vk_ok(vkBeginCommandBuffer(command, &begin), "vkBeginCommandBuffer")) goto cleanup;
    VkImageMemoryBarrier barrier = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .newLayout = VK_IMAGE_LAYOUT_GENERAL,
        .dstAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
        .image = image,
        .subresourceRange = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .levelCount = 1, .layerCount = 1},
    };
    vkCmdPipelineBarrier(
        command, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
        0, 0, NULL, 0, NULL, 1, &barrier);
    vkCmdBindPipeline(command, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
    vkCmdBindDescriptorSets(command, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_layout, 0, 1, &descriptor, 0, NULL);
    struct {
        uint32_t use_source;
        uint32_t use_canvas;
        float scale;
        float crop_x;
        float crop_y;
        float cursor_x;
        float cursor_y;
    } parameters = {
        raw_path ? 1 : 0,
        raw_canvas_path ? 1 : 0,
        scale,
        crop_x,
        crop_y,
        raw_cursor_path ? cursor_x : -1.0f,
        raw_cursor_path ? cursor_y : -1.0f,
    };
    vkCmdPushConstants(command, pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(parameters), &parameters);
    vkCmdDispatch(command, (width + 15) / 16, (height + 15) / 16, 1);
    if (!vk_ok(vkEndCommandBuffer(command), "vkEndCommandBuffer")) goto cleanup;
    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &command};
    if (!vk_ok(vkQueueSubmit(queue, 1, &submit, VK_NULL_HANDLE), "vkQueueSubmit(compute composition)")) goto cleanup;
    if (!vk_ok(vkQueueWaitIdle(queue), "vkQueueWaitIdle(compute composition)")) goto cleanup;
    success = 1;

cleanup:
    if (source_memory != VK_NULL_HANDLE) vkFreeMemory(device, source_memory, NULL);
    if (source != VK_NULL_HANDLE) vkDestroyBuffer(device, source, NULL);
    if (command_pool != VK_NULL_HANDLE) vkDestroyCommandPool(device, command_pool, NULL);
    if (descriptor_pool != VK_NULL_HANDLE) vkDestroyDescriptorPool(device, descriptor_pool, NULL);
    if (pipeline != VK_NULL_HANDLE) vkDestroyPipeline(device, pipeline, NULL);
    if (pipeline_layout != VK_NULL_HANDLE) vkDestroyPipelineLayout(device, pipeline_layout, NULL);
    if (set_layout != VK_NULL_HANDLE) vkDestroyDescriptorSetLayout(device, set_layout, NULL);
    if (view != VK_NULL_HANDLE) vkDestroyImageView(device, view, NULL);
    if (shader != VK_NULL_HANDLE) vkDestroyShaderModule(device, shader, NULL);
    free(words);
    return success;
}

static int write_encoded_frame(AVCodecContext *encoder, AVFrame *frame, const char *output) {
    FILE *file = fopen(output, "wb");
    if (!file) {
        fprintf(stderr, "fopen(%s) failed: %s\n", output, strerror(errno));
        return 0;
    }
    if (!av_ok(avcodec_send_frame(encoder, frame), "avcodec_send_frame")) {
        fclose(file);
        return 0;
    }
    if (!av_ok(avcodec_send_frame(encoder, NULL), "avcodec_send_frame(flush)")) {
        fclose(file);
        return 0;
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        fclose(file);
        return 0;
    }
    int frames = 0;
    for (;;) {
        int receive = avcodec_receive_packet(encoder, packet);
        if (receive == AVERROR_EOF || receive == AVERROR(EAGAIN)) break;
        if (!av_ok(receive, "avcodec_receive_packet")) {
            av_packet_free(&packet);
            fclose(file);
            return 0;
        }
        if (fwrite(packet->data, 1, packet->size, file) != (size_t)packet->size) {
            fprintf(stderr, "Could not write encoded packet.\n");
            av_packet_free(&packet);
            fclose(file);
            return 0;
        }
        frames++;
        av_packet_unref(packet);
    }
    av_packet_free(&packet);
    fclose(file);
    if (!frames) {
        fprintf(stderr, "The VAAPI encoder produced no H.264 packets.\n");
        return 0;
    }
    return 1;
}

static int vpp_process_frame(VADisplay display, VAContextID context, VASurfaceID source, VASurfaceID target) {
    VARectangle source_region = {0, 0, FRAME_WIDTH, FRAME_HEIGHT};
    VARectangle output_region = {0, 0, FRAME_WIDTH, FRAME_HEIGHT};
    VAProcPipelineParameterBuffer pipeline = {0};
    pipeline.surface = source;
    pipeline.surface_region = &source_region;
    pipeline.output_region = &output_region;
    pipeline.pipeline_flags = VA_PIPELINE_FLAG_END;
    VABufferID buffer = VA_INVALID_ID;
    if (!va_ok(vaCreateBuffer(display, context, VAProcPipelineParameterBufferType, sizeof(pipeline), 1, &pipeline, &buffer), "vaCreateBuffer(VideoProc pipeline)")) return 0;
    VAStatus status = vaBeginPicture(display, context, target);
    if (status == VA_STATUS_SUCCESS) status = vaRenderPicture(display, context, &buffer, 1);
    if (status == VA_STATUS_SUCCESS) status = vaEndPicture(display, context);
    vaDestroyBuffer(display, buffer);
    return va_ok(status, "VAAPI RGBA to NV12 VideoProc");
}

static int write_available_packets(AVCodecContext *encoder, AVPacket *packet, FILE *file, int *written) {
    for (;;) {
        int receive = avcodec_receive_packet(encoder, packet);
        if (receive == AVERROR(EAGAIN) || receive == AVERROR_EOF) return 1;
        if (!av_ok(receive, "avcodec_receive_packet(stream)")) return 0;
        if (fwrite(packet->data, 1, packet->size, file) != (size_t)packet->size) return 0;
        (*written)++;
        av_packet_unref(packet);
    }
}

int main(int argc, char **argv) {
    const char *output = argc > 1 ? argv[1] : "/tmp/pikscreen-vulkan-vaapi-probe.h264";
    const char *shader_path = argc > 2 ? argv[2] : "/tmp/pikscreen-vulkan-rgba-fill.spv";
    const char *raw_source_path = argc > 3 ? argv[3] : NULL;
    const char *raw_canvas_path = argc > 4 ? argv[4] : NULL;
    const char *raw_cursor_path = argc > 5 ? argv[5] : NULL;
    const float scale = argc > 6 ? strtof(argv[6], NULL) : 1.0f;
    const float crop_x = argc > 7 ? strtof(argv[7], NULL) : 0.0f;
    const float crop_y = argc > 8 ? strtof(argv[8], NULL) : 0.0f;
    const float cursor_x = argc > 9 ? strtof(argv[9], NULL) : 0.5f;
    const float cursor_y = argc > 10 ? strtof(argv[10], NULL) : 0.5f;
    const int stream_mode = raw_source_path && strcmp(raw_source_path, "-") == 0;
    if (raw_cursor_path && !raw_canvas_path) {
        fprintf(stderr, "The Tahoe cursor compositor needs a static canvas.\n");
        return 1;
    }
    if (stream_mode && (!raw_canvas_path || !raw_cursor_path)) {
        fprintf(stderr, "Stream mode needs static RGBA canvas and Tahoe cursor files.\n");
        return 1;
    }
    if (scale < 1.0f || crop_x < 0.0f || crop_y < 0.0f || crop_x > 1.0f || crop_y > 1.0f) {
        fprintf(stderr, "Scale must be at least 1 and crop values must be normalized.\n");
        return 1;
    }
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    int exported_fd = -1;
    AVBufferRef *device_ctx = NULL;
    AVBufferRef *frames_ctx = NULL;
    AVCodecContext *encoder = NULL;
    AVFrame *frame = NULL;
    VASurfaceID imported_surface = VA_INVALID_ID;
    VASurfaceID converted_surface = VA_INVALID_ID;
    VAConfigID vpp_config = VA_INVALID_ID;
    VAContextID vpp_context = VA_INVALID_ID;
    GpuCompositor compositor = {0};
    int success = 0;

    VkApplicationInfo application = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "PikScreen Vulkan VAAPI encode probe",
        .apiVersion = VK_API_VERSION_1_1,
    };
    VkInstanceCreateInfo instance_info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &application,
    };
    if (!vk_ok(vkCreateInstance(&instance_info, NULL, &instance), "vkCreateInstance")) goto cleanup;
    uint32_t count = 0;
    vkEnumeratePhysicalDevices(instance, &count, NULL);
    VkPhysicalDevice *devices = calloc(count, sizeof(*devices));
    if (!devices) goto cleanup;
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
        goto cleanup;
    }
    uint32_t family_count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &family_count, NULL);
    VkQueueFamilyProperties *families = calloc(family_count, sizeof(*families));
    if (!families) goto cleanup;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &family_count, families);
    uint32_t family = UINT32_MAX;
    for (uint32_t index = 0; index < family_count; index++) {
        if (families[index].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            family = index;
            break;
        }
    }
    free(families);
    if (family == UINT32_MAX) goto cleanup;
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
    if (!vk_ok(vkCreateDevice(physical, &device_info, NULL, &device), "vkCreateDevice")) goto cleanup;
    VkExternalMemoryImageCreateInfo external_image = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    VkImageCreateInfo image_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .pNext = &external_image,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_B8G8R8A8_UNORM,
        .extent = {.width = FRAME_WIDTH, .height = FRAME_HEIGHT, .depth = 1},
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_LINEAR,
        .usage = VK_IMAGE_USAGE_STORAGE_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    if (!vk_ok(vkCreateImage(device, &image_info, NULL, &image), "vkCreateImage(RGBA external DMA-BUF)")) goto cleanup;
    VkMemoryRequirements requirements;
    vkGetImageMemoryRequirements(device, image, &requirements);
    uint32_t memory_type = find_memory_type(physical, requirements.memoryTypeBits, 0);
    if (memory_type == UINT32_MAX) goto cleanup;
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
    if (!vk_ok(vkAllocateMemory(device, &allocation, NULL, &memory), "vkAllocateMemory(export DMA-BUF)")) goto cleanup;
    if (!vk_ok(vkBindImageMemory(device, image, memory, 0), "vkBindImageMemory(RGBA)")) goto cleanup;
    VkImageSubresource color = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT};
    VkSubresourceLayout color_layout;
    vkGetImageSubresourceLayout(device, image, &color, &color_layout);
    if (stream_mode) {
        if (!gpu_compositor_init(
                &compositor, device, physical, family, image, FRAME_WIDTH, FRAME_HEIGHT,
                shader_path, raw_canvas_path, raw_cursor_path)) goto cleanup;
    } else if (!render_frame(
                   device, physical, family, image, FRAME_WIDTH, FRAME_HEIGHT,
                   shader_path, raw_source_path, raw_canvas_path, raw_cursor_path,
                   scale, crop_x, crop_y, cursor_x, cursor_y)) {
        goto cleanup;
    }
    PFN_vkGetMemoryFdKHR get_memory_fd =
        (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(device, "vkGetMemoryFdKHR");
    if (!get_memory_fd) goto cleanup;
    VkMemoryGetFdInfoKHR fd_info = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
        .memory = memory,
        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    if (!vk_ok(get_memory_fd(device, &fd_info, &exported_fd), "vkGetMemoryFdKHR(DMA-BUF)")) goto cleanup;

    if (!av_ok(av_hwdevice_ctx_create(&device_ctx, AV_HWDEVICE_TYPE_VAAPI, "/dev/dri/renderD128", NULL, 0), "av_hwdevice_ctx_create(VAAPI)")) goto cleanup;
    AVHWDeviceContext *hardware = (AVHWDeviceContext *)device_ctx->data;
    AVVAAPIDeviceContext *va_hardware = (AVVAAPIDeviceContext *)hardware->hwctx;
    VADRMPRIMESurfaceDescriptor descriptor;
    memset(&descriptor, 0, sizeof(descriptor));
    descriptor.fourcc = VA_FOURCC_BGRA;
    descriptor.width = FRAME_WIDTH;
    descriptor.height = FRAME_HEIGHT;
    descriptor.num_objects = 1;
    descriptor.objects[0].fd = exported_fd;
    descriptor.objects[0].size = requirements.size;
    descriptor.objects[0].drm_format_modifier = DRM_FORMAT_MOD_LINEAR;
    descriptor.num_layers = 1;
    descriptor.layers[0].drm_format = DRM_FORMAT_ARGB8888;
    descriptor.layers[0].num_planes = 1;
    descriptor.layers[0].object_index[0] = 0;
    descriptor.layers[0].offset[0] = color_layout.offset;
    descriptor.layers[0].pitch[0] = color_layout.rowPitch;
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
    if (!va_ok(vaCreateSurfaces(
            va_hardware->display, VA_RT_FORMAT_RGB32, FRAME_WIDTH, FRAME_HEIGHT,
            &imported_surface, 1, attributes, 2),
            "vaCreateSurfaces(import Vulkan RGBA DMA-BUF)")) goto cleanup;

    if (!va_ok(vaCreateSurfaces(
            va_hardware->display, VA_RT_FORMAT_YUV420, FRAME_WIDTH, FRAME_HEIGHT,
            &converted_surface, 1, NULL, 0),
            "vaCreateSurfaces(VAAPI NV12 conversion target)")) goto cleanup;
    if (!va_ok(vaCreateConfig(
            va_hardware->display, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &vpp_config),
            "vaCreateConfig(VideoProc)")) goto cleanup;
    if (!va_ok(vaCreateContext(
            va_hardware->display, vpp_config, FRAME_WIDTH, FRAME_HEIGHT, VA_PROGRESSIVE,
            &converted_surface, 1, &vpp_context),
            "vaCreateContext(VideoProc)")) goto cleanup;
    frames_ctx = av_hwframe_ctx_alloc(device_ctx);
    if (!frames_ctx) goto cleanup;
    AVHWFramesContext *frames = (AVHWFramesContext *)frames_ctx->data;
    frames->format = AV_PIX_FMT_VAAPI;
    frames->sw_format = AV_PIX_FMT_NV12;
    frames->width = FRAME_WIDTH;
    frames->height = FRAME_HEIGHT;
    if (!av_ok(av_hwframe_ctx_init(frames_ctx), "av_hwframe_ctx_init(VAAPI)")) goto cleanup;
    const AVCodec *codec = avcodec_find_encoder_by_name("h264_vaapi");
    if (!codec) {
        fprintf(stderr, "h264_vaapi is not available.\n");
        goto cleanup;
    }
    encoder = avcodec_alloc_context3(codec);
    if (!encoder) goto cleanup;
    encoder->width = FRAME_WIDTH;
    encoder->height = FRAME_HEIGHT;
    encoder->time_base = (AVRational){1, 120};
    encoder->framerate = (AVRational){120, 1};
    encoder->pix_fmt = AV_PIX_FMT_VAAPI;
    encoder->gop_size = 120;
    encoder->max_b_frames = 0;
    encoder->hw_frames_ctx = av_buffer_ref(frames_ctx);
    av_opt_set_int(encoder->priv_data, "qp", 18, 0);
    av_opt_set_int(encoder->priv_data, "async_depth", 1, 0);
    if (!av_ok(avcodec_open2(encoder, codec, NULL), "avcodec_open2(h264_vaapi)")) goto cleanup;
    frame = av_frame_alloc();
    if (!frame) goto cleanup;
    frame->format = AV_PIX_FMT_VAAPI;
    frame->width = FRAME_WIDTH;
    frame->height = FRAME_HEIGHT;
    frame->pts = 0;
    if (!av_ok(av_hwframe_get_buffer(frames_ctx, frame, 0), "av_hwframe_get_buffer(VAAPI carrier)")) goto cleanup;
    frame->data[3] = (uint8_t *)(uintptr_t)converted_surface;
    if (stream_mode) {
        const size_t frame_bytes = FRAME_WIDTH * FRAME_HEIGHT * 4;
        uint8_t *input_frame = malloc(frame_bytes);
        FILE *stream_output = fopen(output, "wb");
        AVPacket *packet = av_packet_alloc();
        int packets = 0;
        uint64_t frame_count = 0;
        if (!input_frame || !stream_output || !packet) {
            free(input_frame);
            if (stream_output) fclose(stream_output);
            av_packet_free(&packet);
            goto cleanup;
        }
        while (fread(input_frame, 1, frame_bytes, stdin) == frame_bytes) {
            if (!gpu_compositor_render(&compositor, input_frame, scale, crop_x, crop_y, cursor_x, cursor_y)
                || !vpp_process_frame(va_hardware->display, vpp_context, imported_surface, converted_surface)) {
                free(input_frame);
                fclose(stream_output);
                av_packet_free(&packet);
                goto cleanup;
            }
            frame->pts = (int64_t)frame_count++;
            if (!av_ok(avcodec_send_frame(encoder, frame), "avcodec_send_frame(stream)")) {
                free(input_frame);
                fclose(stream_output);
                av_packet_free(&packet);
                goto cleanup;
            }
            if (!write_available_packets(encoder, packet, stream_output, &packets)) {
                free(input_frame);
                fclose(stream_output);
                av_packet_free(&packet);
                goto cleanup;
            }
        }
        free(input_frame);
        if (ferror(stdin) || !frame_count || !av_ok(avcodec_send_frame(encoder, NULL), "avcodec_send_frame(stream flush)")
            || !write_available_packets(encoder, packet, stream_output, &packets)) {
            fclose(stream_output);
            av_packet_free(&packet);
            goto cleanup;
        }
        fclose(stream_output);
        av_packet_free(&packet);
        if (!packets) {
            fprintf(stderr, "GPU stream renderer did not produce H.264 packets.\n");
            goto cleanup;
        }
        fprintf(stderr, "GPU stream renderer wrote %llu frames and %d H.264 packets.\n", (unsigned long long)frame_count, packets);
    } else {
        if (!vpp_process_frame(va_hardware->display, vpp_context, imported_surface, converted_surface)
            || !va_ok(vaSyncSurface(va_hardware->display, converted_surface), "vaSyncSurface(VideoProc target)")
            || !write_encoded_frame(encoder, frame, output)) goto cleanup;
    }
    printf(
        "Vulkan %s -> VAAPI VideoProc NV12 -> VAAPI H.264 succeeded: %s\n",
        raw_cursor_path ? "Recordly stage and Tahoe cursor compositor" : raw_canvas_path ? "Recordly stage compositor" : raw_source_path ? "RGBA source-frame compositor" : "RGBA test compositor",
        output);
    success = 1;

cleanup:
    gpu_compositor_destroy(&compositor);
    av_frame_free(&frame);
    avcodec_free_context(&encoder);
    av_buffer_unref(&frames_ctx);
    if (imported_surface != VA_INVALID_ID && device_ctx) {
        AVHWDeviceContext *hardware = (AVHWDeviceContext *)device_ctx->data;
        AVVAAPIDeviceContext *va_hardware = (AVVAAPIDeviceContext *)hardware->hwctx;
        vaDestroySurfaces(va_hardware->display, &imported_surface, 1);
    }
    if (converted_surface != VA_INVALID_ID && device_ctx) {
        AVHWDeviceContext *hardware = (AVHWDeviceContext *)device_ctx->data;
        AVVAAPIDeviceContext *va_hardware = (AVVAAPIDeviceContext *)hardware->hwctx;
        vaDestroySurfaces(va_hardware->display, &converted_surface, 1);
    }
    if (vpp_context != VA_INVALID_ID && device_ctx) {
        AVHWDeviceContext *hardware = (AVHWDeviceContext *)device_ctx->data;
        AVVAAPIDeviceContext *va_hardware = (AVVAAPIDeviceContext *)hardware->hwctx;
        vaDestroyContext(va_hardware->display, vpp_context);
    }
    if (vpp_config != VA_INVALID_ID && device_ctx) {
        AVHWDeviceContext *hardware = (AVHWDeviceContext *)device_ctx->data;
        AVVAAPIDeviceContext *va_hardware = (AVVAAPIDeviceContext *)hardware->hwctx;
        vaDestroyConfig(va_hardware->display, vpp_config);
    }
    av_buffer_unref(&device_ctx);
    if (exported_fd >= 0) close(exported_fd);
    if (image != VK_NULL_HANDLE) vkDestroyImage(device, image, NULL);
    if (memory != VK_NULL_HANDLE) vkFreeMemory(device, memory, NULL);
    if (device != VK_NULL_HANDLE) vkDestroyDevice(device, NULL);
    if (instance != VK_NULL_HANDLE) vkDestroyInstance(instance, NULL);
    return success ? 0 : 1;
}
