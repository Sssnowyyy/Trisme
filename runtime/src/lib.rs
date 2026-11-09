//! Zakuro as the runtime of a 3DS title recompiled for the Wii U.
//!
//! The C platform layer in `platform/` owns the main thread: SDL2's video
//! (TV and GamePad), sound, the controllers and the HOME menu. This crate
//! runs the console on a thread of its own, the recompiled code linked into
//! the program, and trades input for frames and sound through the
//! functions in `platform/zakuro_wiiu.h`.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use zakuro_common::Screen;
use zakuro_core::services::hid::{InputState, PadState};
use zakuro_core::recompiled::Linked;
use zakuro_core::{loader, Config, FrameOutcome};

pub const TOP_WIDTH: usize = 400;
pub const BOTTOM_WIDTH: usize = 320;
pub const SCREEN_HEIGHT: usize = 240;
const TOP_BYTES: usize = TOP_WIDTH * SCREEN_HEIGHT * 4;
const BOTTOM_BYTES: usize = BOTTOM_WIDTH * SCREEN_HEIGHT * 4;

/// one 3DS frame, about 16.71 ms.
const FRAME_TIME: Duration =
    Duration::from_nanos(zakuro_core::CYCLES_PER_FRAME * 1_000_000_000 / zakuro_core::kernel::thread::CPU_CLOCK_HZ);
/// how far behind the schedule the console may fall before it starts over
/// rather than run flat out to catch up.
const CATCH_UP_LIMIT: Duration = Duration::from_millis(200);
/// the main thread went quiet this long, it is in the HOME menu or gone.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(250);
/// the most sound held for the main thread, a quarter second; past that the
/// oldest goes, the output fell behind.
const MOST_AUDIO: usize = 8192;
/// the console's thread runs deep recompiled call chains and big frames.
const EMU_STACK: usize = 16 << 20;

pub const STATE_LOADING: i32 = 0;
pub const STATE_RUNNING: i32 = 1;
pub const STATE_EXITED: i32 = 2;
pub const STATE_FAULTED: i32 = 3;
pub const STATE_FAILED: i32 = 4;

/// platform/zakuro_wiiu.h's ZwInput.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ZwInput {
    pub buttons: u32,
    pub circle_x: f32,
    pub circle_y: f32,
    pub c_stick_x: f32,
    pub c_stick_y: f32,
    pub touching: u8,
    pub pad_: [u8; 1],
    pub touch_x: u16,
    pub touch_y: u16,
    pub pad2_: u16,
}

/// platform/zakuro_wiiu.h's ZwCode.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ZwCode {
    pub program_id: u64,
    pub abi: u32,
    pub generation: u32,
    pub entries: *const c_void,
    pub entry_count: u32,
    pub modules: *const c_void,
    pub module_count: u32,
    pub origins: *const c_void,
    pub present: u8,
    pub pad_: [u8; 3],
}

/// platform/zakuro_wiiu.h's ZwParams.
#[repr(C)]
pub struct ZwParams {
    pub game_path: *const c_char,
    pub data_dir: *const c_char,
    pub code: ZwCode,
    pub new3ds: u8,
    pub language: u8,
    pub region: u8,
    /// 0 draws the 3D on the CPU, 1 on the GPU through GX2 when the
    /// platform layer offers it.
    pub renderer: u8,
    pub emu_core: i32,
}

// the layouts powerpc-eabi-gcc -meabi gives platform/zakuro_wiiu.h's structs
#[cfg(target_vendor = "wiiu")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<ZwInput>() == 28 && offset_of!(ZwInput, touch_x) == 22);
    assert!(size_of::<ZwCode>() == 40 && offset_of!(ZwCode, entries) == 16 && offset_of!(ZwCode, present) == 36);
    assert!(size_of::<ZwParams>() == 56 && offset_of!(ZwParams, code) == 8);
    assert!(offset_of!(ZwParams, new3ds) == 48 && offset_of!(ZwParams, renderer) == 51 && offset_of!(ZwParams, emu_core) == 52);
};

extern "C" {
    fn zw_platform_log(level: i32, message: *const c_char);
    fn zw_platform_thread_setup(core: i32);
    /// platform/zw_gx2.h: 1 when the GX2 rasterizer's shaders loaded and the
    /// main thread runs its batches.
    fn zw_gx2_available() -> i32;
}

/// ZwParams::renderer's values.
pub const RENDERER_CPU: u8 = 0;
pub const RENDERER_GX2: u8 = 1;

/// the newest frame, waiting for the main thread.
struct Frame {
    top: Vec<u8>,
    bottom: Vec<u8>,
    fresh: bool,
}

struct Shared {
    input: Mutex<InputState>,
    frame: Mutex<Frame>,
    audio: Mutex<VecDeque<[i16; 2]>>,
    heartbeat: Mutex<Instant>,
    message: Mutex<String>,
    /// the last thing the log said while loading, shown under "Loading..."
    status: Mutex<String>,
    state: AtomicI32,
    stop: AtomicBool,
    /// the platform holds the console still, in the HOME menu.
    paused: AtomicBool,
    /// frames the console ran, for the log.
    frames: AtomicU32,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        input: Mutex::new(InputState::default()),
        frame: Mutex::new(Frame { top: vec![0; TOP_BYTES], bottom: vec![0; BOTTOM_BYTES], fresh: false }),
        audio: Mutex::new(VecDeque::with_capacity(MOST_AUDIO)),
        heartbeat: Mutex::new(Instant::now()),
        message: Mutex::new(String::new()),
        status: Mutex::new(String::new()),
        state: AtomicI32::new(STATE_LOADING),
        stop: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        frames: AtomicU32::new(0),
    })
}

static THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// what the console's thread is handed.
struct Start {
    game: PathBuf,
    data_dir: Option<PathBuf>,
    linked: Option<Linked>,
    new3ds: bool,
    language: Option<u8>,
    region: Option<u8>,
    core: i32,
    renderer: u8,
}

// SAFETY: the Linked holds pointers to the recompiled code's tables, which
// are static and only read, so moving it to the console's thread is fine.
unsafe impl Send for Start {}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            log::Level::Error => 1,
            log::Level::Warn => 2,
            log::Level::Info => 3,
            log::Level::Debug => 4,
            log::Level::Trace => 5,
        };
        let shared = shared();
        if record.level() <= log::Level::Info && shared.state.load(Ordering::SeqCst) == STATE_LOADING {
            if let Ok(mut status) = shared.status.try_lock() {
                *status = record.args().to_string();
            }
        }
        let text = format!("[{}] {}: {}", record.level(), record.target(), record.args());
        let text = CString::new(text.replace('\0', " ")).unwrap_or_default();
        // SAFETY: the platform's logger takes a NUL-terminated string it
        // copies before returning
        unsafe { zw_platform_log(level, text.as_ptr()) };
    }

    fn flush(&self) {}
}

fn init_logging() {
    static LOGGER: Logger = Logger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// a C string the platform passed, as a path.
///
/// # Safety
/// pointer is null or a NUL-terminated string.
unsafe fn path_from(pointer: *const c_char) -> Option<PathBuf> {
    if pointer.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// starts the console on the game, on a thread of its own. 0 when it
/// started (loading goes on there, see zakuro_wiiu_state), negative when
/// the parameters were bad or it runs already.
///
/// # Safety
/// params points to a ZwParams whose strings are NUL-terminated and whose
/// code tables, when present, are 3dsrecomp's recomp_ symbols linked in.
#[no_mangle]
pub unsafe extern "C" fn zakuro_wiiu_start(params: *const ZwParams) -> i32 {
    init_logging();
    install_panic_hook();
    let Some(params) = (unsafe { params.as_ref() }) else { return -1 };
    let Some(game) = (unsafe { path_from(params.game_path) }) else { return -2 };
    let data_dir = unsafe { path_from(params.data_dir) };
    let code = params.code;
    let linked = (code.present != 0 && !code.entries.is_null()).then(|| {
        // SAFETY: the caller promises these are the recomp_ symbols
        let linked = unsafe { Linked::new(code.program_id, code.abi, code.entries, code.entry_count, code.modules, code.module_count) };
        if code.origins.is_null() {
            linked
        } else {
            unsafe { linked.with_origins(code.generation, code.origins) }
        }
    });
    let start = Start {
        game,
        data_dir,
        linked,
        new3ds: params.new3ds != 0,
        language: (params.language != 0xFF).then_some(params.language),
        region: (params.region != 0xFF).then_some(params.region),
        core: params.emu_core,
        renderer: params.renderer,
    };

    let mut thread = THREAD.lock().unwrap();
    if thread.is_some() {
        return -3;
    }
    let shared = shared();
    shared.stop.store(false, Ordering::SeqCst);
    shared.state.store(STATE_LOADING, Ordering::SeqCst);
    *shared.heartbeat.lock().unwrap() = Instant::now();
    let spawned = std::thread::Builder::new().name("zakuro".into()).stack_size(EMU_STACK).spawn(move || console(start));
    match spawned {
        Ok(handle) => {
            *thread = Some(handle);
            0
        }
        Err(error) => {
            fail(format!("could not start the console's thread, {error}"));
            -4
        }
    }
}

/// a panic, built with panic=abort, would end the program with its message
/// unseen. this logs it and puts it on the screen instead; on any thread
/// but the main one (the platform's, which calls in through the FFI) the
/// panicking thread then sleeps for good rather than aborting, so the
/// message stays up until the player closes the game.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    static MAIN: OnceLock<std::thread::ThreadId> = OnceLock::new();
    let _ = MAIN.set(std::thread::current().id());
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let thread = std::thread::current();
            let message = format!("the console stopped: {info} (thread {})", thread.name().unwrap_or("unnamed"));
            log::error!("{message}");
            let shared = shared();
            if let Ok(mut slot) = shared.message.try_lock() {
                *slot = message;
            }
            shared.state.store(STATE_FAULTED, Ordering::SeqCst);
            if MAIN.get() != Some(&thread.id()) {
                loop {
                    std::thread::park();
                }
            }
        }));
    });
}

fn fail(message: String) {
    log::error!("{message}");
    let shared = shared();
    *shared.message.lock().unwrap() = message;
    shared.state.store(STATE_FAILED, Ordering::SeqCst);
}

/// the console's thread: loads the game and runs it, a frame at a time, at
/// the console's pace.
fn console(start: Start) {
    // SAFETY: a plain call into the platform layer
    unsafe { zw_platform_thread_setup(start.core) };
    let shared = shared();
    let interpreted = start.linked.is_none();
    let mut config = Config {
        new3ds: start.new3ds,
        data_dir: start.data_dir.clone(),
        linked: start.linked,
        // the program carries its code; there is no library to look for
        find_recompiled: false,
        recompiled: None,
        hardware_renderer: false,
        gx2_renderer: false,
        texture_packs: false,
        interpolate: false,
        resolution: 1,
        username: "Wii U".into(),
        ..Config::default()
    };
    if let Some(language) = start.language {
        config.language = language;
    }
    // the loader turns GX2 on, and says so, or why it draws on the CPU
    config.gx2_renderer = match start.renderer {
        RENDERER_GX2 => {
            // SAFETY: a plain query of the platform layer
            let available = unsafe { zw_gx2_available() } == 1;
            if !available {
                log::warn!("the GX2 renderer was asked for, but the platform layer has none, drawing the 3D on the CPU");
            }
            available
        }
        RENDERER_CPU => false,
        other => {
            log::warn!("unknown renderer {other}, drawing the 3D on the CPU");
            false
        }
    };
    if let Some(region) = start.region {
        config.region = region;
    }
    if let Some(dir) = &start.data_dir {
        if let Err(error) = std::fs::create_dir_all(dir) {
            log::warn!("could not make {}, {error}", dir.display());
        }
    }
    log::info!(
        "loading {}{}",
        start.game.display(),
        if interpreted { ", interpreting everything" } else { ", on the recompiled code" }
    );
    let mut system = match loader::load(&start.game, config) {
        Ok(system) => system,
        Err(error) => return fail(format!("could not open {}, {error}", start.game.display())),
    };
    log::info!("the 3D draws {}", if system.gpu.draws_on_gpu() { "on the GPU through GX2" } else { "on the CPU" });
    if let Some(code) = &system.recompiled {
        if code.outdated() {
            log::warn!("the recompiled code is older than this runtime expects; parts may run in the interpreter");
        }
    }
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    shared.state.store(STATE_RUNNING, Ordering::SeqCst);
    log::info!("running");

    let mut next_frame = Instant::now();
    let mut unshown = 0u32;
    let mut last_report = Instant::now();
    let mut frames_since = 0u32;
    while !shared.stop.load(Ordering::Relaxed) {
        // the main thread stopped coming by: the HOME menu has the console,
        // so this one waits, and starts its schedule over after
        let quiet = shared.paused.load(Ordering::Relaxed) || shared.heartbeat.lock().unwrap().elapsed() > HEARTBEAT_TIMEOUT;
        if quiet {
            std::thread::sleep(Duration::from_millis(10));
            next_frame = Instant::now();
            continue;
        }

        let input = *shared.input.lock().unwrap();
        system.set_input(input);
        let outcome = system.run_frame();
        frames_since += 1;
        shared.frames.fetch_add(1, Ordering::Relaxed);

        let sound = system.take_audio();
        if !sound.is_empty() {
            let mut audio = shared.audio.lock().unwrap();
            let excess = (audio.len() + sound.len()).saturating_sub(MOST_AUDIO);
            let excess = excess.min(audio.len());
            audio.drain(..excess);
            audio.extend(sound.iter().copied());
        }

        // the screens are read out when the main thread took the last ones,
        // or every few frames anyway, so falling behind costs no readouts
        let taken = !shared.frame.lock().unwrap().fresh;
        if taken || unshown >= 3 {
            unshown = 0;
            let top = system.read_screen(Screen::Top);
            let bottom = system.read_screen(Screen::Bottom);
            let mut frame = shared.frame.lock().unwrap();
            if top.len() == TOP_BYTES {
                frame.top.copy_from_slice(&top);
            }
            if bottom.len() == BOTTOM_BYTES {
                frame.bottom.copy_from_slice(&bottom);
            }
            frame.fresh = true;
        } else {
            unshown += 1;
        }

        match outcome {
            FrameOutcome::Completed => {}
            FrameOutcome::Exited => {
                log::info!("the title exited");
                *shared.message.lock().unwrap() = "The game closed.".into();
                shared.state.store(STATE_EXITED, Ordering::SeqCst);
                break;
            }
            FrameOutcome::Faulted => {
                let errors = system.fatal_errors.join("; ");
                log::error!("the title stopped on a fault: {errors}");
                *shared.message.lock().unwrap() = format!("The game stopped on a fault. {errors}");
                shared.state.store(STATE_FAULTED, Ordering::SeqCst);
                break;
            }
        }

        if last_report.elapsed() >= Duration::from_secs(5) {
            let rate = frames_since as f32 / last_report.elapsed().as_secs_f32();
            log::info!("{rate:.1} frames a second, {}", system.status_line());
            frames_since = 0;
            last_report = Instant::now();
        }

        // the console's pace: ahead of it, wait; far behind, start over
        next_frame += FRAME_TIME;
        let now = Instant::now();
        if now > next_frame + CATCH_UP_LIMIT {
            next_frame = now;
        } else if now < next_frame {
            std::thread::sleep(next_frame - now);
        }
    }
    // the system goes here, on its own thread, and with it anything it
    // still had to write
    drop(system);
    log::info!("the console stopped");
}

/// what the controllers say now.
///
/// # Safety
/// input points to a ZwInput.
#[no_mangle]
pub unsafe extern "C" fn zakuro_wiiu_set_input(input: *const ZwInput) {
    let Some(input) = (unsafe { input.as_ref() }) else { return };
    let state = InputState {
        buttons: PadState::from_bits_truncate(input.buttons),
        circle_x: input.circle_x.clamp(-1.0, 1.0),
        circle_y: input.circle_y.clamp(-1.0, 1.0),
        c_stick_x: input.c_stick_x.clamp(-1.0, 1.0),
        c_stick_y: input.c_stick_y.clamp(-1.0, 1.0),
        touch: (input.touching != 0).then(|| (input.touch_x.min(319), input.touch_y.min(239))),
        tilt: [0.0; 2],
    };
    *shared().input.lock().unwrap() = state;
}

/// copies the newest frame out, 1 when it had not been taken yet.
///
/// # Safety
/// top has room for 400x240x4 bytes and bottom for 320x240x4.
#[no_mangle]
pub unsafe extern "C" fn zakuro_wiiu_take_frame(top: *mut u8, bottom: *mut u8) -> i32 {
    if top.is_null() || bottom.is_null() {
        return 0;
    }
    let mut frame = shared().frame.lock().unwrap();
    if !frame.fresh {
        return 0;
    }
    // SAFETY: the caller's buffers have room, see above
    unsafe {
        std::ptr::copy_nonoverlapping(frame.top.as_ptr(), top, TOP_BYTES);
        std::ptr::copy_nonoverlapping(frame.bottom.as_ptr(), bottom, BOTTOM_BYTES);
    }
    frame.fresh = false;
    1
}

/// takes up to max_frames of sound, interleaved stereo.
///
/// # Safety
/// out has room for max_frames * 2 samples.
#[no_mangle]
pub unsafe extern "C" fn zakuro_wiiu_take_audio(out: *mut i16, max_frames: u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    let mut audio = shared().audio.lock().unwrap();
    let count = audio.len().min(max_frames as usize);
    for (index, [left, right]) in audio.drain(..count).enumerate() {
        // SAFETY: index < max_frames
        unsafe {
            *out.add(index * 2) = left;
            *out.add(index * 2 + 1) = right;
        }
    }
    count as u32
}

/// the sound's sample rate, rounded, about 32728 Hz.
#[no_mangle]
pub extern "C" fn zakuro_wiiu_audio_rate() -> u32 {
    zakuro_core::AUDIO_SAMPLE_RATE.round() as u32
}

#[no_mangle]
pub extern "C" fn zakuro_wiiu_state() -> i32 {
    shared().state.load(Ordering::SeqCst)
}

/// the message for the state into buffer, NUL-terminated, cut to fit; its
/// full length.
///
/// # Safety
/// buffer has room for size bytes, or is null.
#[no_mangle]
pub unsafe extern "C" fn zakuro_wiiu_message(buffer: *mut c_char, size: u32) -> u32 {
    let shared = shared();
    // while loading, what it's doing; afterwards, why it stopped
    let message = if shared.state.load(Ordering::SeqCst) == STATE_LOADING { shared.status.lock().unwrap() } else { shared.message.lock().unwrap() };
    let bytes = message.as_bytes();
    if !buffer.is_null() && size > 0 {
        let count = bytes.len().min(size as usize - 1);
        // SAFETY: count < size
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), buffer, count);
            *buffer.add(count) = 0;
        }
    }
    bytes.len() as u32
}

#[no_mangle]
pub extern "C" fn zakuro_wiiu_heartbeat() {
    *shared().heartbeat.lock().unwrap() = Instant::now();
}

/// holds the console still, or lets it go on.
#[no_mangle]
pub extern "C" fn zakuro_wiiu_pause(paused: i32) {
    shared().paused.store(paused != 0, Ordering::SeqCst);
    if paused == 0 {
        zakuro_wiiu_heartbeat();
    }
}

/// stops the console and waits for its thread, which writes out what it
/// still has to. a game still loading has nothing to write and isn't waited
/// for, so closing from the HOME menu never hangs on a long read.
#[no_mangle]
pub extern "C" fn zakuro_wiiu_stop() {
    let shared = shared();
    shared.stop.store(true, Ordering::SeqCst);
    // the console waits out a pause or a quiet main thread; it must see the stop
    shared.paused.store(false, Ordering::SeqCst);
    zakuro_wiiu_heartbeat();
    if let Some(handle) = THREAD.lock().unwrap().take() {
        if shared.state.load(Ordering::SeqCst) == STATE_LOADING {
            log::info!("closing while the game loads, not waiting for it");
            drop(handle);
        } else {
            let _ = handle.join();
        }
    }
}
