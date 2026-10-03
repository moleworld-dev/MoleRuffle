//! 探针:Metal 驱动的 8MB 资源池块(IOGPUMetalPooledResource)是被什么带出来的。
//! 分阶段创建设备 / 着色器 / 渲染管线 / 命令缓冲,每阶段后用 vmmap 数一次 8192K 的显存块。
//! 运行:cargo run --release -p moleruffle-desktop --example metal_probe
use std::process::Command;

fn blocks(stage: &str) {
    std::thread::sleep(std::time::Duration::from_millis(400));
    let pid = std::process::id().to_string();
    let out = Command::new("vmmap").arg(&pid).output().expect("vmmap");
    let text = String::from_utf8_lossy(&out.stdout);
    let graphics: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("owned unmapped (graphics)"))
        .collect();
    let b8 = graphics.iter().filter(|l| l.contains("[ 8192K")).count();
    println!("{stage:<44} 8MB块={b8:<4} 显存区域={}", graphics.len());
}

const SHADER: &str = r#"
@group(0) @binding(0) var<uniform> u: vec4<f32>;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    return vec4<f32>(f32(i) * 0.5 + u.x, 0.0, 0.0, 1.0);
}
@fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(u.y, VARIANT, 0.0, 1.0); }
"#;

fn main() {
    blocks("进程启动");
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let adapter = futures::executor::block_on(instance.request_adapter(&Default::default()))
        .expect("adapter");
    let (device, queue) =
        futures::executor::block_on(adapter.request_device(&Default::default())).expect("device");
    blocks("创建设备后");

    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let make = |variant: usize, samples: u32, blend: Option<wgpu::BlendState>| {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(
                SHADER.replace("VARIANT", &format!("{}.0", variant)).into(),
            ),
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: samples,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        })
    };

    let mut keep = Vec::new();
    keep.push(make(0, 1, None));
    blocks("1 条管线");
    for i in 1..10 {
        keep.push(make(i, 1, None));
    }
    blocks("10 条管线(不同着色器,同状态)");
    for i in 10..90 {
        keep.push(make(i, 1, None));
    }
    blocks("90 条管线(不同着色器,同状态)");
    for i in 0..40 {
        keep.push(make(i, 4, Some(wgpu::BlendState::ALPHA_BLENDING)));
    }
    blocks("再加 40 条(4x 多重采样 + 混合)");

    // 画几帧:每帧一个通道,用第一条管线
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: 256,
            height: 256,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    let mut draw_with = |range: std::ops::Range<usize>, label: &str| {
        for _ in 0..4 {
            let mut encoder = device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_bind_group(0, &bind, &[]);
                for p in &keep[range.clone()] {
                    pass.set_pipeline(p);
                    pass.draw(0..3, 0..1);
                }
            }
            queue.submit([encoder.finish()]);
            let _ = device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            });
        }
        blocks(label);
    };
    draw_with(0..1, "用 1 条管线画 4 帧");
    draw_with(0..10, "用 10 条管线画 4 帧");
    draw_with(0..90, "用 90 条管线画 4 帧");
}
