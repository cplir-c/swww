//! All expects in this program must be carefully chosen on purpose. The idea is that if any of
//! them fail there is no point in continuing. All of the initialization code, for example, is full
//! of `expects`, **on purpose**, because we **want** to unwind and exit when they happen

mod animations;
mod cli;
mod wallpaper;
#[allow(dead_code)]
mod wayland;
use log::{debug, error, info, warn, LevelFilter};
use rustix::{
    event::{poll, PollFd, PollFlags},
    fd::OwnedFd,
};

use wallpaper::Wallpaper;
use wayland::{
    globals::{self, Initializer},
    ObjectId,
};

use std::{
    cell::RefCell,
    fs,
    io::{IsTerminal, Write},
    num::{NonZeroI32, NonZeroU32},
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use utils::ipc::{
    connect_to_socket, get_socket_path, read_socket, Answer, BgInfo, ImageReq, MmappedStr,
    RequestRecv, RequestSend, Scale,
};

use animations::{ImageAnimator, TransitionAnimator};

// We need this because this might be set by signals, so we can't keep it in the daemon
static EXIT: AtomicBool = AtomicBool::new(false);

fn exit_daemon() {
    EXIT.store(true, Ordering::Release);
}

fn should_daemon_exit() -> bool {
    EXIT.load(Ordering::Acquire)
}

extern "C" fn signal_handler(_s: libc::c_int) {
    exit_daemon();
}

struct Daemon {
    wallpapers: Vec<Rc<RefCell<Wallpaper>>>,
    transition_animators: Vec<TransitionAnimator>,
    image_animators: Vec<ImageAnimator>,
    use_cache: bool,
    fractional_scale_manager: Option<(ObjectId, NonZeroU32)>,
    poll_time: i32,
}

impl Daemon {
    fn new(initializer: &Initializer, no_cache: bool) -> Self {
        log::info!(
            "Selected wl_shm format: {:?}",
            wayland::globals::pixel_format()
        );
        let fractional_scale_manager = initializer.fractional_scale().cloned();

        let wallpapers = Vec::new();

        Self {
            wallpapers,
            transition_animators: Vec::new(),
            image_animators: Vec::new(),
            use_cache: !no_cache,
            fractional_scale_manager,
            poll_time: -1,
        }
    }

    fn new_output(&mut self, output_name: u32) {
        use wayland::interfaces::*;
        let output = globals::object_create(wayland::WlDynObj::Output);
        wl_registry::req::bind(output_name, output, "wl_output", 4).unwrap();

        let surface = globals::object_create(wayland::WlDynObj::Surface);
        wl_compositor::req::create_surface(surface).unwrap();

        let region = globals::object_create(wayland::WlDynObj::Region);
        wl_compositor::req::create_region(region).unwrap();

        wl_surface::req::set_input_region(surface, Some(region)).unwrap();
        wl_region::req::destroy(region).unwrap();

        let layer_surface = globals::object_create(wayland::WlDynObj::LayerSurface);
        zwlr_layer_shell_v1::req::get_layer_surface(
            layer_surface,
            surface,
            Some(output),
            zwlr_layer_shell_v1::layer::BACKGROUND,
            "swww-daemon",
        )
        .unwrap();

        let viewport = globals::object_create(wayland::WlDynObj::Viewport);
        wp_viewporter::req::get_viewport(viewport, surface).unwrap();

        let wp_fractional = if let Some((id, _)) = self.fractional_scale_manager.as_ref() {
            let fractional = globals::object_create(wayland::WlDynObj::FractionalScale);
            wp_fractional_scale_manager_v1::req::get_fractional_scale(*id, fractional, surface)
                .unwrap();
            Some(fractional)
        } else {
            None
        };

        debug!("New output: {output_name}");
        self.wallpapers.push(Rc::new(RefCell::new(Wallpaper::new(
            output,
            output_name,
            surface,
            viewport,
            wp_fractional,
            layer_surface,
        ))));
    }

    fn recv_socket_msg(&mut self, stream: OwnedFd) {
        let bytes = match utils::ipc::read_socket(&stream) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("FATAL: cannot read socket: {e}. Exiting...");
                exit_daemon();
                return;
            }
        };
        let request = RequestRecv::receive(bytes);
        let answer = match request {
            RequestRecv::Clear(clear) => {
                let wallpapers = self.find_wallpapers_by_names(&clear.outputs);
                self.stop_animations(&wallpapers);
                for wallpaper in &wallpapers {
                    let mut wallpaper = wallpaper.borrow_mut();
                    wallpaper.set_img_info(utils::ipc::BgImg::Color(clear.color));
                    wallpaper.clear(clear.color);
                }
                crate::wallpaper::attach_buffers_and_damage_surfaces(&wallpapers);
                crate::wallpaper::commit_wallpapers(&wallpapers);

                Answer::Ok
            }
            RequestRecv::Ping => Answer::Ping(self.wallpapers.iter().all(|w| {
                w.borrow()
                    .configured
                    .load(std::sync::atomic::Ordering::Acquire)
            })),
            RequestRecv::Kill => {
                exit_daemon();
                Answer::Ok
            }
            RequestRecv::Query => Answer::Info(self.wallpapers_info()),
            RequestRecv::Img(ImageReq {
                transition,
                mut imgs,
                mut outputs,
                mut animations,
            }) => {
                while !imgs.is_empty() && !outputs.is_empty() {
                    let names = outputs.pop().unwrap();
                    let img = imgs.pop().unwrap();
                    let animation = if let Some(ref mut animations) = animations {
                        animations.pop()
                    } else {
                        None
                    };
                    let wallpapers = self.find_wallpapers_by_names(&names);
                    self.stop_animations(&wallpapers);
                    if let Some(mut transition) =
                        TransitionAnimator::new(wallpapers, &transition, img, animation)
                    {
                        transition.frame();
                        self.transition_animators.push(transition);
                    }
                    self.poll_time = 0;
                }
                Answer::Ok
            }
        };
        if let Err(e) = answer.send(&stream) {
            error!("error sending answer to client: {e}");
        }
    }

    fn wallpapers_info(&self) -> Box<[BgInfo]> {
        self.wallpapers
            .iter()
            .map(|wallpaper| wallpaper.borrow().get_bg_info())
            .collect()
    }

    fn find_wallpapers_by_names(&self, names: &[MmappedStr]) -> Vec<Rc<RefCell<Wallpaper>>> {
        self.wallpapers
            .iter()
            .filter_map(|wallpaper| {
                if names.is_empty() || names.iter().any(|n| wallpaper.borrow().has_name(n.str())) {
                    return Some(Rc::clone(wallpaper));
                }
                None
            })
            .collect()
    }

    fn draw(&mut self) {
        self.poll_time = -1;

        let mut i = 0;
        while i < self.transition_animators.len() {
            let animator = &mut self.transition_animators[i];
            if animator
                .wallpapers
                .iter()
                .all(|w| w.borrow().is_draw_ready())
            {
                let time = animator.time_to_draw();
                if time > Duration::from_micros(1200) {
                    self.poll_time = 1;
                    i += 1;
                    continue;
                }

                if !time.is_zero() {
                    spin_sleep(time);
                }

                wallpaper::attach_buffers_and_damage_surfaces(&animator.wallpapers);
                wallpaper::commit_wallpapers(&animator.wallpapers);
                animator.updt_time();
                if animator.frame() {
                    let animator = self.transition_animators.swap_remove(i);
                    if let Some(anim) = animator.into_image_animator() {
                        self.image_animators.push(anim);
                    }
                    continue;
                }
            }
            i += 1;
        }

        self.image_animators.retain(|a| !a.wallpapers.is_empty());
        for animator in &mut self.image_animators {
            if animator
                .wallpapers
                .iter()
                .all(|w| w.borrow().is_draw_ready())
            {
                let time = animator.time_to_draw();
                if time > Duration::from_micros(1200) {
                    self.poll_time = 1;
                    continue;
                }

                if !time.is_zero() {
                    spin_sleep(time);
                }

                wallpaper::attach_buffers_and_damage_surfaces(&animator.wallpapers);
                wallpaper::commit_wallpapers(&animator.wallpapers);
                animator.updt_time();
                animator.frame();
            }
        }
    }

    fn stop_animations(&mut self, wallpapers: &[Rc<RefCell<Wallpaper>>]) {
        for transition in self.transition_animators.iter_mut() {
            transition
                .wallpapers
                .retain(|w1| !wallpapers.iter().any(|w2| w1.borrow().eq(&w2.borrow())));
        }

        for animator in self.image_animators.iter_mut() {
            animator
                .wallpapers
                .retain(|w1| !wallpapers.iter().any(|w2| w1.borrow().eq(&w2.borrow())));
        }

        self.transition_animators
            .retain(|t| !t.wallpapers.is_empty());

        self.image_animators.retain(|a| !a.wallpapers.is_empty());
    }
}

impl wayland::interfaces::wl_display::EvHandler for Daemon {
    fn delete_id(&mut self, id: u32) {
        if let Some(id) = NonZeroU32::new(id) {
            globals::object_remove(ObjectId::new(id));
        }
    }
}
impl wayland::interfaces::wl_registry::EvHandler for Daemon {
    fn global(&mut self, name: u32, interface: &str, version: u32) {
        if interface == "wl_output" {
            if version < 4 {
                error!("your compositor must support at least version 4 of wl_output");
            } else {
                self.new_output(name);
            }
        }
    }

    fn global_remove(&mut self, name: u32) {
        if let Some(i) = self
            .wallpapers
            .iter()
            .position(|w| w.borrow().has_output_name(name))
        {
            let w = self.wallpapers.remove(i);
            self.stop_animations(&[w]);
        }
    }
}

impl wayland::interfaces::wl_shm::EvHandler for Daemon {
    fn format(&mut self, format: u32) {
        warn!(
            "received a wl_shm format after initialization: {format}. This shouldn't be possible"
        );
    }
}

impl wayland::interfaces::wl_output::EvHandler for Daemon {
    fn geometry(
        &mut self,
        sender_id: ObjectId,
        _x: i32,
        _y: i32,
        _physical_width: i32,
        _physical_height: i32,
        _subpixel: i32,
        _make: &str,
        _model: &str,
        transform: i32,
    ) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                if transform as u32 > wayland::interfaces::wl_output::transform::FLIPPED_270 {
                    error!("received invalid transform value from compositor: {transform}")
                } else {
                    wallpaper.set_transform(transform as u32);
                }
                break;
            }
        }
    }

    fn mode(&mut self, sender_id: ObjectId, _flags: u32, width: i32, height: i32, _refresh: i32) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                wallpaper.set_dimensions(width, height);
                break;
            }
        }
    }

    fn done(&mut self, sender_id: ObjectId) {
        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_output(sender_id) {
                wallpaper
                    .borrow_mut()
                    .commit_surface_changes(self.use_cache);
                self.stop_animations(&[wallpaper.clone()]);
                break;
            }
        }
    }

    fn scale(&mut self, sender_id: ObjectId, factor: i32) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                match NonZeroI32::new(factor) {
                    Some(factor) => wallpaper.set_scale(Scale::Whole(factor)),
                    None => error!("received scale factor of 0 from compositor"),
                }
                break;
            }
        }
    }

    fn name(&mut self, sender_id: ObjectId, name: &str) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                wallpaper.set_name(name.to_string());
                break;
            }
        }
    }

    fn description(&mut self, sender_id: ObjectId, description: &str) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_output(sender_id) {
                wallpaper.set_desc(description.to_string());
                break;
            }
        }
    }
}
impl wayland::interfaces::wl_surface::EvHandler for Daemon {
    fn enter(&mut self, _sender_id: ObjectId, output: ObjectId) {
        debug!("Output {}: Surface Enter", output.get());
    }

    fn leave(&mut self, _sender_id: ObjectId, output: ObjectId) {
        debug!("Output {}: Surface Leave", output.get());
    }

    fn preferred_buffer_scale(&mut self, sender_id: ObjectId, factor: i32) {
        for wallpaper in self.wallpapers.iter() {
            let mut wallpaper = wallpaper.borrow_mut();
            if wallpaper.has_surface(sender_id) {
                match NonZeroI32::new(factor) {
                    Some(factor) => wallpaper.set_scale(Scale::Whole(factor)),
                    None => error!("received scale factor of 0 from compositor"),
                }
                break;
            }
        }
    }

    fn preferred_buffer_transform(&mut self, _sender_id: ObjectId, _transform: u32) {
        warn!("Received PreferredBufferTransform. We currently ignore those")
    }
}

impl wayland::interfaces::wl_buffer::EvHandler for Daemon {
    fn release(&mut self, sender_id: ObjectId) {
        for wallpaper in self.wallpapers.iter() {
            let strong_count = Rc::strong_count(wallpaper);
            if wallpaper
                .borrow_mut()
                .try_set_buffer_release_flag(sender_id, strong_count)
            {
                break;
            }
        }
    }
}

impl wayland::interfaces::wl_callback::EvHandler for Daemon {
    fn done(&mut self, sender_id: ObjectId, _callback_data: u32) {
        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_callback(sender_id) {
                wallpaper.borrow_mut().frame_callback_completed();
                self.draw();
                break;
            }
        }
    }
}

impl wayland::interfaces::zwlr_layer_surface_v1::EvHandler for Daemon {
    fn configure(&mut self, sender_id: ObjectId, serial: u32, _width: u32, _height: u32) {
        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_layer_surface(sender_id) {
                wayland::interfaces::zwlr_layer_surface_v1::req::ack_configure(sender_id, serial)
                    .unwrap();
                break;
            }
        }
    }

    fn closed(&mut self, sender_id: ObjectId) {
        self.wallpapers
            .retain(|w| !w.borrow().has_layer_surface(sender_id));
    }
}

impl wayland::interfaces::wp_fractional_scale_v1::EvHandler for Daemon {
    fn preferred_scale(&mut self, sender_id: ObjectId, scale: u32) {
        for wallpaper in self.wallpapers.iter() {
            if wallpaper.borrow().has_fractional_scale(sender_id) {
                match NonZeroI32::new(scale as i32) {
                    Some(factor) => {
                        wallpaper.borrow_mut().set_scale(Scale::Fractional(factor));
                        if wallpaper
                            .borrow_mut()
                            .commit_surface_changes(self.use_cache)
                        {
                            self.stop_animations(&[wallpaper.clone()]);
                        }
                    }
                    None => error!("received scale factor of 0 from compositor"),
                }
                break;
            }
        }
    }
}

fn main() -> Result<(), String> {
    // first, get the command line arguments and make the logger
    let cli = cli::Cli::new();
    make_logger(cli.quiet);

    // initialize the wayland connection, getting all the necessary globals
    let initializer = wayland::globals::init(cli.format);

    // create the socket listener and setup the signal handlers
    // this will also return an error if there is an `swww-daemon` instance already
    // running
    let listener = SocketWrapper::new()?;
    setup_signals();

    // use the initializer to create the Daemon, then drop it to free up the memory
    let mut daemon = Daemon::new(&initializer, cli.no_cache);
    for &output_name in initializer.output_names() {
        daemon.new_output(output_name);
    }
    drop(initializer);

    if let Ok(true) = sd_notify::booted() {
        if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
            error!("Error sending status update to systemd: {e}");
        }
    }

    let wayland_fd = wayland::globals::wayland_fd();
    let mut fds = [
        PollFd::new(&wayland_fd, PollFlags::IN),
        PollFd::new(&listener.0, PollFlags::IN),
    ];

    // main loop
    while !should_daemon_exit() {
        use wayland::{interfaces::*, wire, WlDynObj};

        if let Err(e) = poll(&mut fds, daemon.poll_time) {
            match e {
                rustix::io::Errno::INTR => continue,
                _ => return Err(format!("failed to poll file descriptors: {e:?}")),
            }
        }

        if !fds[0].revents().is_empty() {
            let (msg, payload) = match wire::WireMsg::recv() {
                Ok((msg, payload)) => (msg, payload),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(format!("failed to receive wire message: {e:?}")),
            };

            match msg.sender_id() {
                globals::WL_DISPLAY => wl_display::event(&mut daemon, msg, payload),
                globals::WL_REGISTRY => wl_registry::event(&mut daemon, msg, payload),
                globals::WL_COMPOSITOR => error!("wl_compositor has no events"),
                globals::WL_SHM => wl_shm::event(&mut daemon, msg, payload),
                globals::WP_VIEWPORTER => error!("wp_viewporter has no events"),
                globals::ZWLR_LAYER_SHELL_V1 => error!("zwlr_layer_shell_v1 has no events"),
                other => {
                    let obj_id = globals::object_type_get(other);
                    match obj_id {
                        Some(WlDynObj::Output) => wl_output::event(&mut daemon, msg, payload),
                        Some(WlDynObj::Surface) => wl_surface::event(&mut daemon, msg, payload),
                        Some(WlDynObj::Region) => error!("wl_region has no events"),
                        Some(WlDynObj::LayerSurface) => {
                            zwlr_layer_surface_v1::event(&mut daemon, msg, payload)
                        }
                        Some(WlDynObj::Buffer) => wl_buffer::event(&mut daemon, msg, payload),
                        Some(WlDynObj::ShmPool) => error!("wl_shm_pool has no events"),
                        Some(WlDynObj::Callback) => wl_callback::event(&mut daemon, msg, payload),
                        Some(WlDynObj::Viewport) => error!("wp_viewport has no events"),
                        Some(WlDynObj::FractionalScale) => {
                            wp_fractional_scale_v1::event(&mut daemon, msg, payload)
                        }
                        None => error!("Received event for deleted object ({other:?})"),
                    }
                }
            }
        }

        if !fds[1].revents().is_empty() {
            match rustix::net::accept(&listener.0) {
                Ok(stream) => daemon.recv_socket_msg(stream),
                Err(rustix::io::Errno::INTR | rustix::io::Errno::WOULDBLOCK) => continue,
                Err(e) => return Err(format!("failed to accept incoming connection: {e}")),
            }
        }

        if daemon.poll_time > -1 {
            daemon.draw();
        }
    }

    drop(daemon);
    drop(listener);
    info!("Goodbye!");
    Ok(())
}

fn setup_signals() {
    // C data structure, expected to be zeroed out.
    let mut sigaction: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe { libc::sigemptyset(std::ptr::addr_of_mut!(sigaction.sa_mask)) };

    #[cfg(not(target_os = "aix"))]
    {
        sigaction.sa_sigaction = signal_handler as usize;
    }
    #[cfg(target_os = "aix")]
    {
        sigaction.sa_union.__su_sigaction = handler;
    }

    for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
        let ret =
            unsafe { libc::sigaction(signal, std::ptr::addr_of!(sigaction), std::ptr::null_mut()) };
        if ret != 0 {
            error!("Failed to install signal handler!")
        }
    }
    debug!("Finished setting up signal handlers")
}

/// This is a wrapper that makes sure to delete the socket when it is dropped
struct SocketWrapper(OwnedFd);
impl SocketWrapper {
    fn new() -> Result<Self, String> {
        let socket_addr = get_socket_path();

        if socket_addr.exists() {
            if is_daemon_running(&socket_addr)? {
                return Err(
                    "There is an swww-daemon instance already running on this socket!".to_string(),
                );
            } else {
                warn!(
                    "socket file {} was not deleted when the previous daemon exited",
                    socket_addr.to_string_lossy()
                );
                if let Err(e) = std::fs::remove_file(&socket_addr) {
                    return Err(format!("failed to delete previous socket: {e}"));
                }
            }
        }

        let runtime_dir = match socket_addr.parent() {
            Some(path) => path,
            None => return Err("couldn't find a valid runtime directory".to_owned()),
        };

        if !runtime_dir.exists() {
            match fs::create_dir(runtime_dir) {
                Ok(()) => (),
                Err(e) => return Err(format!("failed to create runtime dir: {e}")),
            }
        }

        let socket = rustix::net::socket_with(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC.union(rustix::net::SocketFlags::NONBLOCK),
            None,
        )
        .expect("failed to create socket file descriptor");

        rustix::net::bind_unix(
            &socket,
            &rustix::net::SocketAddrUnix::new(&socket_addr).unwrap(),
        )
        .unwrap();

        rustix::net::listen(&socket, 0).unwrap();

        debug!("Created socket in {:?}", socket_addr);
        Ok(Self(socket))
    }
}

impl Drop for SocketWrapper {
    fn drop(&mut self) {
        let socket_addr = get_socket_path();
        if let Err(e) = fs::remove_file(&socket_addr) {
            error!("Failed to remove socket at {socket_addr:?}: {e}");
        }
        info!("Removed socket at {:?}", socket_addr);
    }
}

struct Logger {
    level_filter: LevelFilter,
    start: std::time::Instant,
    is_term: bool,
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level_filter
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let time = self.start.elapsed().as_millis();

            let level = if self.is_term {
                match record.level() {
                    log::Level::Error => "\x1b[31m[ERROR]\x1b[0m",
                    log::Level::Warn => "\x1b[33m[WARN]\x1b[0m ",
                    log::Level::Info => "\x1b[32m[INFO]\x1b[0m ",
                    log::Level::Debug | log::Level::Trace => "\x1b[36m[DEBUG]\x1b[0m",
                }
            } else {
                match record.level() {
                    log::Level::Error => "[ERROR]",
                    log::Level::Warn => "[WARN] ",
                    log::Level::Info => "[INFO] ",
                    log::Level::Debug | log::Level::Trace => "[DEBUG]",
                }
            };

            let msg = record.args();
            let _ = std::io::stderr()
                .lock()
                .write_fmt(format_args!("{time:>10}ms {level} {msg}\n"));
        }
    }

    fn flush(&self) {
        //no op (we do not buffer anything)
    }
}

fn make_logger(quiet: bool) {
    let level_filter = if quiet {
        LevelFilter::Error
    } else {
        LevelFilter::Debug
    };

    log::set_boxed_logger(Box::new(Logger {
        level_filter,
        start: std::time::Instant::now(),
        is_term: std::io::stderr().is_terminal(),
    }))
    .map(|()| log::set_max_level(level_filter))
    .unwrap();
}

pub fn is_daemon_running(addr: &PathBuf) -> Result<bool, String> {
    let sock = match connect_to_socket(addr, 5, 100) {
        Ok(s) => s,
        // likely a connection refused; either way, this is a reliable signal there's no surviving
        // daemon.
        Err(_) => return Ok(false),
    };

    RequestSend::Ping.send(&sock)?;
    let answer = Answer::receive(read_socket(&sock)?);
    match answer {
        Answer::Ping(_) => Ok(true),
        _ => Err("Daemon did not return Answer::Ping, as expected".to_string()),
    }
}

/// copy-pasted from the `spin_sleep` crate on crates.io
///
/// This will sleep for an amount of time we can roughly expected the OS to still be precise enough
/// for frame timing (125 us, currently).
fn spin_sleep(duration: std::time::Duration) {
    const ACCURACY: std::time::Duration = std::time::Duration::new(0, 125_000);
    let start = std::time::Instant::now();
    if duration > ACCURACY {
        std::thread::sleep(duration - ACCURACY);
    }

    while start.elapsed() < duration {
        std::thread::yield_now();
    }
}
