use std::{
    io::{Read, Write},
    sync::mpsc,
};

const SHADER: &str = r#"
struct FrameSize {
    width: u32,
    height: u32,
    content_width: u32,
    content_height: u32,
    crop_x: f32,
    crop_y: f32,
    scale: f32,
    _padding: f32,
}

@group(0) @binding(0) var<storage, read> source: array<u32>;
@group(0) @binding(1) var<storage, read> canvas: array<u32>;
@group(0) @binding(2) var<storage, read_write> destination: array<u32>;
@group(0) @binding(3) var<uniform> frame: FrameSize;

fn unpack(value: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(value & 255u) / 255.0,
        f32((value >> 8u) & 255u) / 255.0,
        f32((value >> 16u) & 255u) / 255.0,
        f32((value >> 24u) & 255u) / 255.0,
    );
}

fn pack(value: vec4<f32>) -> u32 {
    let clamped = vec4<u32>(round(clamp(value, vec4<f32>(0.0), vec4<f32>(1.0)) * 255.0));
    return clamped.x | (clamped.y << 8u) | (clamped.z << 16u) | (clamped.w << 24u);
}

fn source_at(x: u32, y: u32) -> vec4<f32> {
    return unpack(source[y * frame.width + x]);
}

fn sample_source(uv: vec2<f32>) -> vec4<f32> {
    let max_x = frame.width - 1u;
    let max_y = frame.height - 1u;
    let position = clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0))
        * vec2<f32>(f32(max_x), f32(max_y));
    let lower = vec2<u32>(floor(position));
    let upper = min(lower + vec2<u32>(1u), vec2<u32>(max_x, max_y));
    let fraction = fract(position);
    let top = mix(source_at(lower.x, lower.y), source_at(upper.x, lower.y), fraction.x);
    let bottom = mix(source_at(lower.x, upper.y), source_at(upper.x, upper.y), fraction.x);
    return mix(top, bottom, fraction.y);
}

fn squircle_alpha(local: vec2<f32>) -> f32 {
    let radius = 12.5;
    let half = vec2<f32>(f32(frame.content_width), f32(frame.content_height)) * 0.5;
    let centered = abs(local - half);
    let corner = max(centered - (half - vec2<f32>(radius)), vec2<f32>(0.0));
    let curve = pow(corner.x / radius, 4.5) + pow(corner.y / radius, 4.5);
    return 1.0 - smoothstep(0.994, 1.006, curve);
}

@compute @workgroup_size(16, 16)
fn compose(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= frame.width || id.y >= frame.height) {
        return;
    }
    let index = id.y * frame.width + id.x;
    let offset = vec2<u32>(
        (frame.width - frame.content_width) / 2u,
        (frame.height - frame.content_height) / 2u,
    );
    if (id.x < offset.x || id.y < offset.y || id.x >= offset.x + frame.content_width || id.y >= offset.y + frame.content_height) {
        destination[index] = canvas[index];
        return;
    }
    let local = vec2<f32>(id.xy - offset);
    let stage = unpack(canvas[index]);
    let source_uv = vec2<f32>(frame.crop_x, frame.crop_y)
        + local / vec2<f32>(f32(frame.content_width), f32(frame.content_height)) / frame.scale;
    let camera = sample_source(source_uv);
    let alpha = squircle_alpha(local);
    destination[index] = pack(mix(stage, camera, alpha));
}
"#;

const IN_FLIGHT_FRAMES: usize = 8;

struct FrameSlot {
    source: wgpu::Buffer,
    destination: wgpu::Buffer,
    readback: wgpu::Buffer,
    _parameters: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

fn main() -> Result<(), String> {
    let arguments = arguments_from_args()?;
    let width = arguments.width;
    let height = arguments.height;
    let frame_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "Frame dimensions are too large.".to_owned())?
        as usize;
    let canvas = image::open(&arguments.canvas_path)
        .map_err(|error| format!("Could not open the static Recordly canvas: {error}"))?
        .to_rgba8();
    if canvas.width() != width || canvas.height() != height {
        return Err(format!(
            "Static Recordly canvas size is {}x{}, expected {width}x{height}.",
            canvas.width(),
            canvas.height()
        ));
    }

    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .map_err(|error| format!("Could not open a Vulkan GPU adapter: {error}"))?;
    let info = adapter.get_info();
    if info.device_type != wgpu::DeviceType::DiscreteGpu {
        return Err(format!(
            "Refusing non-discrete GPU adapter for the renderer probe: {} ({:?})",
            info.name, info.device_type
        ));
    }
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("PikScreen GPU frame renderer probe"),
        ..Default::default()
    }))
    .map_err(|error| format!("Could not create the GPU rendering device: {error}"))?;

    let byte_size = frame_bytes as u64;
    let canvas_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("PikScreen static Recordly canvas"),
        size: byte_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&canvas_buffer, 0, canvas.as_raw());

    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("PikScreen GPU compositor shader"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("PikScreen GPU compositor"),
        layout: None,
        module: &module,
        entry_point: Some("compose"),
        compilation_options: Default::default(),
        cache: None,
    });
    let layout = pipeline.get_bind_group_layout(0);
    let parameter_bytes = frame_parameters_as_bytes(
        width,
        height,
        content_size(width, height).0,
        content_size(width, height).1,
        arguments.crop_x,
        arguments.crop_y,
        arguments.scale,
    );
    let slots = (0..IN_FLIGHT_FRAMES)
        .map(|_| {
            let source = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("PikScreen source frame"),
                size: byte_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let destination = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("PikScreen composed frame"),
                size: byte_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("PikScreen frame readback"),
                size: byte_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let parameters = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("PikScreen frame parameters"),
                size: 32,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&parameters, 0, &parameter_bytes);
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("PikScreen GPU frame bindings"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: source.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: canvas_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: destination.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: parameters.as_entire_binding(),
                    },
                ],
            });
            FrameSlot {
                source,
                destination,
                readback,
                _parameters: parameters,
                bind_group,
            }
        })
        .collect::<Vec<_>>();

    let mut input = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut frame = vec![0_u8; frame_bytes];
    let mut frames = 0_u64;
    let mut input_finished = false;
    while !input_finished {
        let mut batch_size = 0_usize;
        for slot in &slots {
            match input.read_exact(&mut frame) {
                Ok(()) => {
                    queue.write_buffer(&slot.source, 0, &frame);
                    batch_size += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    input_finished = true;
                    break;
                }
                Err(error) => return Err(format!("Could not read RGBA frame from stdin: {error}")),
            }
        }
        if batch_size == 0 {
            break;
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("PikScreen GPU frame command encoder"),
        });
        for slot in slots.iter().take(batch_size) {
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("PikScreen GPU composition pass"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.dispatch_workgroups(width.div_ceil(16), height.div_ceil(16), 1);
            }
            encoder.copy_buffer_to_buffer(&slot.destination, 0, &slot.readback, 0, byte_size);
        }
        queue.submit(Some(encoder.finish()));
        let receivers = slots
            .iter()
            .take(batch_size)
            .map(|slot| {
                let (sender, receiver) = mpsc::channel();
                slot.readback
                    .slice(..)
                    .map_async(wgpu::MapMode::Read, move |result| {
                        let _ = sender.send(result);
                    });
                receiver
            })
            .collect::<Vec<_>>();
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| {
                format!("GPU frame renderer stopped while waiting for work: {error}")
            })?;
        for (slot, receiver) in slots.iter().zip(receivers) {
            receiver
                .recv()
                .map_err(|error| format!("GPU frame renderer did not return a frame: {error}"))?
                .map_err(|error| format!("Could not map GPU compositor output: {error}"))?;
            let output = slot.readback.slice(..).get_mapped_range();
            stdout
                .write_all(&output)
                .map_err(|error| format!("Could not write composed RGBA frame: {error}"))?;
            drop(output);
            slot.readback.unmap();
            frames += 1;
        }
    }
    stdout
        .flush()
        .map_err(|error| format!("Could not flush GPU compositor output: {error}"))?;
    eprintln!(
        "PikScreen GPU frame renderer composed {frames} {}x{} frames on {} ({:?}); {} frame slots stayed in flight.",
        width, height, info.name, info.backend
        , IN_FLIGHT_FRAMES
    );
    Ok(())
}

struct Arguments {
    width: u32,
    height: u32,
    canvas_path: String,
    scale: f32,
    crop_x: f32,
    crop_y: f32,
}

fn arguments_from_args() -> Result<Arguments, String> {
    let mut arguments = std::env::args().skip(1);
    let width = arguments
        .next()
        .ok_or_else(|| usage().to_owned())?
        .parse::<u32>()
        .map_err(|_| "Width must be a positive integer.".to_owned())?;
    let height = arguments
        .next()
        .ok_or_else(|| usage().to_owned())?
        .parse::<u32>()
        .map_err(|_| "Height must be a positive integer.".to_owned())?;
    let canvas_path = arguments.next().ok_or_else(|| usage().to_owned())?;
    let scale = arguments
        .next()
        .map(|value| value.parse::<f32>())
        .transpose()
        .map_err(|_| "Scale must be a number.".to_owned())?
        .unwrap_or(1.0);
    let crop_x = arguments
        .next()
        .map(|value| value.parse::<f32>())
        .transpose()
        .map_err(|_| "Crop x must be a number.".to_owned())?
        .unwrap_or(0.0);
    let crop_y = arguments
        .next()
        .map(|value| value.parse::<f32>())
        .transpose()
        .map_err(|_| "Crop y must be a number.".to_owned())?
        .unwrap_or(0.0);
    if width == 0
        || height == 0
        || !scale.is_finite()
        || scale < 1.0
        || !crop_x.is_finite()
        || !crop_y.is_finite()
        || arguments.next().is_some()
    {
        return Err(usage().to_owned());
    }
    Ok(Arguments {
        width,
        height,
        canvas_path,
        scale,
        crop_x,
        crop_y,
    })
}

fn usage() -> &'static str {
    "Usage: pikscreen_gpu_probe <width> <height> <canvas.png> [scale crop_x crop_y]"
}

fn content_size(width: u32, height: u32) -> (u32, u32) {
    (
        (width as f64 * 0.92).round() as u32,
        (height as f64 * 0.92).round() as u32,
    )
}

fn frame_parameters_as_bytes(
    width: u32,
    height: u32,
    content_width: u32,
    content_height: u32,
    crop_x: f32,
    crop_y: f32,
    scale: f32,
) -> Vec<u8> {
    let mut values = Vec::with_capacity(32);
    values.extend_from_slice(&width.to_ne_bytes());
    values.extend_from_slice(&height.to_ne_bytes());
    values.extend_from_slice(&content_width.to_ne_bytes());
    values.extend_from_slice(&content_height.to_ne_bytes());
    values.extend_from_slice(&crop_x.to_ne_bytes());
    values.extend_from_slice(&crop_y.to_ne_bytes());
    values.extend_from_slice(&scale.to_ne_bytes());
    values.extend_from_slice(&0_f32.to_ne_bytes());
    values
}
