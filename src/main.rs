mod blit;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use welding::linux_cef::{LinuxCefConfig, LinuxCefProducer};
use welding::{
    CefRuntime, CefRuntimeConfig, CefSandboxMode, CefSurfaceConfig, CefSurfaceProducer,
    EventModifiers, HostWgpuContext, KeyEvent, KeyEventKind, MouseAction, MouseButton, MouseEvent,
    wgpu,
};

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

    let cache_dir = std::path::PathBuf::from("/tmp/rustbrowser_cache");
    let is_subprocess = std::env::args().any(|a| a.starts_with("--type="));
    if !is_subprocess {
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // ── 2. Initialize CEF (this process is confirmed to be the browser/host) ─
    println!("Initializing CEF...");
    let mut runtime_config = CefRuntimeConfig::new(cef_path, sandbox);
    runtime_config.cache_path = Some(cache_dir.clone());
    runtime_config
        .command_line_switches
        .push(("disable-vulkan".into(), None));
    runtime_config
        .command_line_switches
        .push(("no-first-run".into(), None));
    runtime_config
        .command_line_switches
        .push(("no-default-browser-check".into(), None));
    runtime_config
        .command_line_switches
        .push(("disable-gpu-sandbox".into(), None));
    // ── Memory/process controls (how real browsers stay lean) ──
    // Limit the number of renderer processes CEF can spawn.
    runtime_config
        .command_line_switches
        .push(("renderer-process-limit".into(), Some("4".into())));
    // Use one renderer process per site (domain) instead of per tab/iframe.
    runtime_config
        .command_line_switches
        .push(("process-per-site".into(), None));
    // Disable site isolation — without this, every unique ad domain
    // (doubleclick.net, googlesyndication.com, etc.) spawns its own
    // renderer process, easily adding 1-2 GB on ad-heavy pages.
    runtime_config
        .command_line_switches
        .push(("disable-site-isolation-trials".into(), None));
    // Limit V8 JS heap per renderer process (default is ~1.4GB!).
    // We set this to 512MB: 128MB was too aggressive and caused V8 OOM crashes
    // on ad-heavy sites like Britannica, leading to high CPU usage from GC thrashing.
    runtime_config
        .command_line_switches
        .push(("js-flags".into(), Some("--max-old-space-size=512".into())));
    // Disable background networking that pre-fetches resources speculatively.
    runtime_config
        .command_line_switches
        .push(("disable-background-networking".into(), None));

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
    println!("Adapter Info: {:?}", adapter.get_info());

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

    let mut blitter = blit::BgraToRgbaBlitter::new(&device);
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
    let mut last_scale = app.window().scale_factor();
    let mut fps_counter = 0u32;
    let mut last_fps_time = Instant::now();
    let mut readback_buffer: Option<wgpu::Buffer> = None;
    let mut readback_buffer_size: u64 = 0;

    // Async readback state
    let mut is_mapping = false;
    let mut readback_receiver: Option<
        std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    > = None;
    let mut pending_width: u32 = 0;
    let mut pending_height: u32 = 0;
    let mut pending_bytes_per_row: u32 = 0;
    // Hold the CEF frame alive while the GPU is still copying from it.
    // Without this, dropping the frame tells Chromium to recycle the DMA-BUF,
    // which causes a race where Chromium overwrites the buffer mid-copy.
    let mut _pending_frame: Option<welding::ImportedTexture> = None;

    // System stats (lightweight — only refreshes our own process tree)
    let mut sys = sysinfo::System::new();
    let mut last_sys_time = Instant::now();
    let our_pid = sysinfo::get_current_pid().unwrap();

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

                // Follow window resizes
                let win_size = app.window().size();
                let current_scale = app.window().scale_factor();
                let size = (win_size.width, win_size.height);

                if current_scale != last_scale {
                    println!("Scale factor changed to {}", current_scale);
                    let _ = producer.set_scale_factor(current_scale);
                    last_scale = current_scale;
                }

                if size != last_size && size.0 > 0 && size.1 > 0 {
                    println!(
                        "Resizing CEF to {}x{} physical pixels (scale: {})",
                        size.0, size.1, current_scale
                    );
                    let _ = producer.resize(dpi::PhysicalSize::new(size.0, size.1));
                    last_size = size;
                }

                // ── Check if a previous async readback has completed ──
                if is_mapping {
                    let _ = device.poll(wgpu::PollType::Poll);
                    if let Some(ref rx) = readback_receiver {
                        if let Ok(Ok(())) = rx.try_recv() {
                            let buffer = readback_buffer.as_ref().unwrap();
                            let data = buffer
                                .slice(..)
                                .get_mapped_range()
                                .expect("Failed to get mapped range");
                            let mut shared_buffer =
                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(
                                    pending_width,
                                    pending_height,
                                );
                            let dst = shared_buffer.make_mut_bytes();

                            let copy_start = Instant::now();
                            let row_bytes = (pending_width * 4) as usize;
                            if pending_bytes_per_row as usize == row_bytes {
                                dst.copy_from_slice(&data[..(row_bytes * pending_height as usize)]);
                            } else {
                                for (i, chunk) in
                                    data.chunks(pending_bytes_per_row as usize).enumerate()
                                {
                                    let dst_start = i * row_bytes;
                                    let dst_end = dst_start + row_bytes;
                                    dst[dst_start..dst_end].copy_from_slice(&chunk[..row_bytes]);
                                }
                            }
                            drop(data);
                            buffer.unmap();

                            let readback_ms = copy_start.elapsed().as_secs_f64() * 1000.0;
                            app.set_readback_text(slint::SharedString::from(format!(
                                "Readback: {:.1}ms",
                                readback_ms
                            )));

                            let image = slint::Image::from_rgba8_premultiplied(shared_buffer);
                            app.set_browser_frame(image);
                            app.window().request_redraw();

                            fps_counter += 1;
                            let now = Instant::now();
                            if now.duration_since(last_fps_time).as_secs() >= 1 {
                                app.set_fps_text(slint::SharedString::from(format!(
                                    "FPS: {}",
                                    fps_counter
                                )));
                                fps_counter = 0;
                                last_fps_time = now;
                            }

                            // Lightweight system stats every 2s
                            if now.duration_since(last_sys_time).as_secs() >= 2 {
                                sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                                let mut total_mem: u64 = 0;
                                let mut total_cpu: f32 = 0.0;

                                for (p_id, process) in sys.processes() {
                                    if *p_id == our_pid || process.parent() == Some(our_pid) {
                                        total_mem += process.memory();
                                        total_cpu += process.cpu_usage();
                                    }
                                }

                                let mem_mb = total_mem / 1_048_576;
                                app.set_sys_text(slint::SharedString::from(format!(
                                    "CPU: {:.0}% | MEM: {} MB",
                                    total_cpu, mem_mb
                                )));
                                last_sys_time = now;
                            }

                            is_mapping = false;
                            readback_receiver = None;
                            _pending_frame = None;
                            // IMPORTANT: fall through to acquire next frame immediately
                            // instead of returning. This eliminates a wasted tick and
                            // roughly doubles achievable FPS.
                        } else {
                            // Map not ready yet — nothing else to do this tick
                            return;
                        }
                    } else {
                        return;
                    }
                }

                // ── Acquire a new frame from CEF and start async readback ──
                match producer.acquire_frame(&host_ctx) {
                    Ok(Some(imported)) => {
                        let width = imported.size.width;
                        let height = imported.size.height;

                        // Blit BGRA→RGBA on GPU, then copy to CPU buffer.
                        // The blit is needed because the imported DMA-BUF texture
                        // lacks COPY_SRC usage, so we render through a shader to
                        // a COPY_SRC-capable texture first.
                        let (rgba_tex, blit_commands) =
                            blitter.convert(&device, &imported.view, width, height);

                        let bytes_per_row = (width * 4 + 255) & !255;
                        let aligned_buffer_size = (bytes_per_row * height) as u64;

                        if readback_buffer_size != aligned_buffer_size {
                            readback_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                                label: Some("cpu_readback_buffer"),
                                size: aligned_buffer_size,
                                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                                mapped_at_creation: false,
                            }));
                            readback_buffer_size = aligned_buffer_size;
                        }
                        let buffer = readback_buffer.as_ref().unwrap();

                        let mut encoder =
                            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: None,
                            });
                        encoder.copy_texture_to_buffer(
                            wgpu::TexelCopyTextureInfo {
                                texture: rgba_tex,
                                mip_level: 0,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::TexelCopyBufferInfo {
                                buffer,
                                layout: wgpu::TexelCopyBufferLayout {
                                    offset: 0,
                                    bytes_per_row: Some(bytes_per_row),
                                    rows_per_image: None,
                                },
                            },
                            wgpu::Extent3d {
                                width,
                                height,
                                depth_or_array_layers: 1,
                            },
                        );

                        // Single submission: blit + copy in one batch
                        queue.submit([blit_commands, encoder.finish()]);

                        let (sender, receiver) = std::sync::mpsc::channel();
                        buffer
                            .slice(..)
                            .map_async(wgpu::MapMode::Read, move |v| sender.send(v).unwrap());

                        pending_width = width;
                        pending_height = height;
                        pending_bytes_per_row = bytes_per_row;
                        readback_receiver = Some(receiver);
                        is_mapping = true;
                        _pending_frame = Some(imported);
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
