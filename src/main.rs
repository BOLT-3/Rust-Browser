mod blit;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use welding::{
    wgpu, CefRuntime, CefRuntimeConfig, CefSandboxMode, CefSurfaceConfig, CefSurfaceProducer,
    EventModifiers, HostWgpuContext, KeyEvent, KeyEventKind, MouseAction, MouseButton, MouseEvent,
};
#[cfg(target_os = "linux")]
use welding::linux_cef::{LinuxCefConfig, LinuxCefProducer};

#[cfg(target_os = "windows")]
use welding::windows_cef::{WindowsCefConfig, WindowsCefProducer};

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    // ── 1. CEF subprocess check — MUST be the first thing in main() ──────────
    // CEF re-launches this same binary as its renderer/GPU/utility processes.
    // Every one of those re-launches must hit this line and exit before doing
    // anything else (creating a window, touching wgpu, etc).
    let cef_path = std::env::var("CEF_PATH").unwrap_or_else(|_| {
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    });

    // NOTE ON SECURITY: this is welding's only currently implemented sandbox
    // mode, and it disables Chromium's OS-level process sandbox entirely
    // (welding's own doc comment: "Do not use it for arbitrary untrusted web
    // content"). Fine for getting this running and iterating; treat wiring up
    // a real sandboxed mode as a hard requirement before this ever loads a
    // page you didn't choose yourself.
    let sandbox = CefSandboxMode::UnsandboxedTrustedContent;

    if let Some(code) = CefRuntime::execute_process_from(cef_path.as_ref(), sandbox)? {
        std::process::exit(code);
    }

    // ── 2. Initialize CEF (this process is confirmed to be the browser/host) ─
    println!("Initializing CEF...");
    let mut runtime_config = CefRuntimeConfig::new(cef_path, sandbox);
    runtime_config.command_line_switches.push(("disable-vulkan".into(), None));
    let runtime = CefRuntime::initialize(runtime_config)?;
    println!("CEF Initialized.");

    // ── 3. Build a Vulkan wgpu Instance + Adapter + DMA-BUF-capable Device ───
    // welding's Linux import path requires the Vulkan backend specifically
    // (see InteropBackend::detect in welding's source), so the instance is
    // pinned to Vulkan rather than letting wgpu auto-pick a backend.
    let instance = wgpu::Instance::default();

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        ..Default::default()
    }))?;

    // build_dmabuf_capable_device (not a plain adapter.request_device) is
    // what actually turns on the Vulkan external-memory / DRM-format-modifier
    // extensions CEF's DMA-BUF frames need to import successfully on Linux.
    #[cfg(target_os = "linux")]
    let (device, queue) = welding::build_dmabuf_capable_device(
        &adapter,
        &wgpu::DeviceDescriptor {
            label: Some("shared_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        },
    )?;

    #[cfg(target_os = "windows")]
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("shared_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        },
        None,
    ))?;

    // ── 4. Slint uses its default backend (OpenGL), bypassing wgpu presentation bugs ──

    // ── 5. Build the Slint window ─────────────────────────────────────────
    println!("Creating Slint window...");
    let app = AppWindow::new()?;
    println!("Slint window created!");

    let win_size = app.window().size();
    
    #[cfg(target_os = "linux")]
    let producer = LinuxCefProducer::new(
        &runtime,
        LinuxCefConfig {
            surface: CefSurfaceConfig {
                initial_url: "https://google.com".into(),
                initial_size: dpi::PhysicalSize::new(win_size.width.max(1), win_size.height.max(1)),
                scale_factor: app.window().scale_factor(),
                ..Default::default()
            },
        },
    )?;

    #[cfg(target_os = "windows")]
    let producer = WindowsCefProducer::new(
        &runtime,
        WindowsCefConfig {
            surface: CefSurfaceConfig {
                initial_url: "https://google.com".into(),
                initial_size: dpi::PhysicalSize::new(win_size.width.max(1), win_size.height.max(1)),
                scale_factor: app.window().scale_factor(),
                ..Default::default()
            },
        },
    )?;

    let blitter = blit::BgraToRgbaBlitter::new(&device);
    let host_ctx = HostWgpuContext::new(device.clone(), queue.clone());

    // ── 7. Wire mouse/keyboard input from the .slint UI into CEF ────────────
    // These callbacks are plain closures called synchronously from Slint's
    // event handling, so a plain Mutex (not a channel) is fine here.
    let producer = Arc::new(Mutex::new(producer));

    {
        let producer = producer.clone();
        app.on_mouse_moved(move |x, y| {
            let _ = producer.lock().unwrap().send_mouse_input(MouseEvent {
                x: x as i32,
                y: y as i32,
                button: MouseButton::Left,
                action: MouseAction::Moved,
                modifiers: EventModifiers::default(),
            });
        });
    }
    {
        let producer = producer.clone();
        app.on_mouse_pressed(move |x, y| {
            let _ = producer.lock().unwrap().send_mouse_input(MouseEvent {
                x: x as i32,
                y: y as i32,
                button: MouseButton::Left,
                action: MouseAction::Pressed,
                modifiers: EventModifiers::default(),
            });
        });
    }
    {
        let producer = producer.clone();
        app.on_mouse_released(move |x, y| {
            let _ = producer.lock().unwrap().send_mouse_input(MouseEvent {
                x: x as i32,
                y: y as i32,
                button: MouseButton::Left,
                action: MouseAction::Released,
                modifiers: EventModifiers::default(),
            });
        });
    }
    {
        let producer = producer.clone();
        app.on_mouse_wheel(move |x, y, dx, dy| {
            let _ = producer.lock().unwrap().send_mouse_input(MouseEvent {
                x: x as i32,
                y: y as i32,
                button: MouseButton::Left,
                action: MouseAction::WheelScrolled {
                    delta_x: dx as i32,
                    delta_y: dy as i32,
                },
                modifiers: EventModifiers::default(),
            });
        });
    }
    {
        let producer = producer.clone();
        app.on_key_pressed(move |text, shift, ctrl, alt, meta| {
            // TODO: this only forwards a printable character (fine for typing
            // into a text field). Arrow keys, Backspace, Tab, function keys
            // etc. need real windows_key_code / native_key_code values, which
            // means mapping Slint's key event onto CEF's key-code space -
            // check welding's docs/examples for its recommended mapping table
            // before relying on non-character keys.
            let character = text.chars().next();
            let _ = producer.lock().unwrap().send_keyboard_input(KeyEvent {
                kind: KeyEventKind::Char,
                windows_key_code: 0,
                native_key_code: 0,
                character,
                modifiers: EventModifiers {
                    shift,
                    ctrl,
                    alt,
                    meta,
                    ..Default::default()
                },
            });
        });
    }
    {
        let producer = producer.clone();
        app.on_key_released(move |text, shift, ctrl, alt, meta| {
            let character = text.chars().next();
            let _ = producer.lock().unwrap().send_keyboard_input(KeyEvent {
                kind: KeyEventKind::KeyUp,
                windows_key_code: 0,
                native_key_code: 0,
                character,
                modifiers: EventModifiers {
                    shift,
                    ctrl,
                    alt,
                    meta,
                    ..Default::default()
                },
            });
        });
    }

    // ── 8. Heartbeat: pump CEF, pull frames, blit, push into the scene ──────
    // Independent of whether Slint itself has a reason to repaint - Chromium
    // needs this tick to keep running JS timers, network, its own compositor,
    // etc, regardless of what the rest of the UI is doing.
    let app_weak = app.as_weak();
    let timer = slint::Timer::default();
    let mut last_size = (win_size.width, win_size.height);

    {
        let producer = producer.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(8),
            move || {
                runtime.do_message_loop_work();

                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                let mut producer = producer.lock().unwrap();

                // Follow window resizes: Slint's own PhysicalSize (u32 width/
                // height) is a different type from dpi::PhysicalSize<u32>
                // that welding wants, so this converts between the two.
                let win_size = app.window().size();
                let size = (win_size.width, win_size.height);
                if size != last_size && size.0 > 0 && size.1 > 0 {
                    let _ = producer.resize(dpi::PhysicalSize::new(size.0, size.1));
                    last_size = size;
                }

                match producer.acquire_frame(&host_ctx) {
                    Ok(Some(imported)) => {
                        let rgba = blitter.convert(
                            &device,
                            &queue,
                            &imported.view,
                            imported.size.width,
                            imported.size.height,
                        );
                        
                        let width = imported.size.width;
                        let height = imported.size.height;
                        let bytes_per_row = (width * 4 + 255) & !255;
                        let aligned_buffer_size = (bytes_per_row * height) as u64;
                        
                        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("cpu_readback_buffer"),
                            size: aligned_buffer_size,
                            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                            mapped_at_creation: false,
                        });
                        
                        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                        encoder.copy_texture_to_buffer(
                            wgpu::TexelCopyTextureInfo {
                                texture: &rgba,
                                mip_level: 0,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::TexelCopyBufferInfo {
                                buffer: &buffer,
                                layout: wgpu::TexelCopyBufferLayout {
                                    offset: 0,
                                    bytes_per_row: Some(bytes_per_row),
                                    rows_per_image: None,
                                },
                            },
                            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                        );
                        let submission_index = queue.submit(Some(encoder.finish()));
                        
                        let buffer_slice = buffer.slice(..);
                        let (sender, receiver) = std::sync::mpsc::channel();
                        buffer_slice.map_async(wgpu::MapMode::Read, move |v| sender.send(v).unwrap());
                        device.poll(wgpu::PollType::Wait { submission_index: Some(submission_index), timeout: None }).unwrap();
                        if receiver.recv().is_ok() {
                            let data = buffer_slice.get_mapped_range().expect("Failed to get mapped range");
                            let mut shared_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
                            let dst = shared_buffer.make_mut_bytes();
                            for (i, chunk) in data.chunks(bytes_per_row as usize).enumerate() {
                                let dst_start = i * (width * 4) as usize;
                                let dst_end = dst_start + (width * 4) as usize;
                                dst[dst_start..dst_end].copy_from_slice(&chunk[..(width * 4) as usize]);
                            }
                            drop(data);
                            buffer.unmap();
                            
                            let image = slint::Image::from_rgba8_premultiplied(shared_buffer);
                            app.set_browser_frame(image);
                            app.window().request_redraw();
                        }
                    }
                    Ok(None) => {}
                    Err(err) => eprintln!("CEF frame acquire failed: {err}"),
                }
            },
        );
    }

    println!("Running Slint app loop...");
    app.run()?;
    println!("Slint loop ended.");
    Ok(())
}
