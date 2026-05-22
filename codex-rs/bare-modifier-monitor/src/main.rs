//! Monitors for double-press of a bare modifier key (Shift or Command) on macOS.
//!
//! Prints `ready` to stdout once the event tap is installed, then prints `fired`
//! each time the configured double-press is detected.
//!
//! Usage:
//!     bare-modifier-monitor --key <DoubleShift|DoubleCommand> [--immediate]

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("bare-modifier-monitor is only supported on macOS");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::runloop::CFRunLoop;
    use core_foundation::runloop::kCFRunLoopCommonModes;
    use std::ffi::c_void;
    use std::io::Write;
    use std::io::stdout;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    // ---------------------------------------------------------------
    // Core Graphics FFI declarations
    // ---------------------------------------------------------------

    type CGEventRef = *mut c_void;
    type CGEventTapProxy = *mut c_void;
    type CGEventType = u32;
    type CGEventMask = u64;
    type CGEventFlags = u64;
    type CFMachPortRef = *mut c_void;

    // Event tap placement – use session-level tap for broader compatibility
    // with macOS 15+ (Sequoia) and later.  The older `kCGHIDEventTap` (0)
    // can silently fail to deliver events on recent macOS versions.
    const K_CG_SESSION_EVENT_TAP: u32 = 1;

    // Passive listener – we never modify events.
    const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
    const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;

    // Event types
    const K_CG_EVENT_FLAGS_CHANGED: CGEventType = 12;
    const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: CGEventType = 0xFFFFFFFE;

    // Modifier flag masks
    const K_CG_EVENT_FLAG_MASK_SHIFT: CGEventFlags = 0x00020000;
    const K_CG_EVENT_FLAG_MASK_COMMAND: CGEventFlags = 0x00100000;
    // Mask covering device-independent modifier bits only.
    const MODIFIER_FLAGS_MASK: CGEventFlags = 0x00FF0000;

    const DOUBLE_TAP_MAX_INTERVAL_MS: u64 = 400;

    // Global state for the callback.  The callback is invoked on the same
    // thread that runs the CFRunLoop so no cross-thread synchronisation is
    // required beyond atomic stores/loads.
    static LAST_PRESS_MS: AtomicU64 = AtomicU64::new(0);
    static WAITING_FOR_RELEASE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Which modifier key pair to watch.
    static mut TARGET_FLAG: CGEventFlags = 0;
    /// If `true`, fire on the first detected double-press and exit.
    static mut IMMEDIATE: bool = false;
    /// The CFMachPortRef for the event tap, stored so the callback can
    /// re-enable it if macOS disables it by timeout.
    static mut TAP_PORT: CFMachPortRef = std::ptr::null_mut();

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: CGEventMask,
            callback: unsafe extern "C" fn(
                CGEventTapProxy,
                CGEventType,
                CGEventRef,
                *mut c_void,
            ) -> CGEventRef,
            user_info: *mut c_void,
        ) -> CFMachPortRef;

        fn CGEventGetFlags(event: CGEventRef) -> CGEventFlags;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    }

    // CFMachPort helpers from CoreFoundation (C API).
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: CFMachPortRef,
            order: i64,
        ) -> *mut c_void;
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// The event-tap callback.  Invoked for every `kCGEventFlagsChanged`
    /// event and for tap-disabled notifications.
    ///
    /// SAFETY: called from the CFRunLoop on the main thread; the globals it
    /// touches (`TARGET_FLAG`, `IMMEDIATE`, `TAP_PORT`) are only written
    /// before the run-loop starts and read here.
    unsafe extern "C" fn tap_callback(
        _proxy: CGEventTapProxy,
        event_type: CGEventType,
        event: CGEventRef,
        _user_info: *mut c_void,
    ) -> CGEventRef {
        // macOS may disable the tap after a timeout.  Re-enable it.
        if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT {
            unsafe {
                if !TAP_PORT.is_null() {
                    CGEventTapEnable(TAP_PORT, true);
                }
            }
            return event;
        }

        if event_type != K_CG_EVENT_FLAGS_CHANGED {
            return event;
        }

        let flags = unsafe { CGEventGetFlags(event) } & MODIFIER_FLAGS_MASK;
        let target = unsafe { TARGET_FLAG };

        let target_pressed = (flags & target) == target;
        // Make sure *only* our target modifier is held (no other modifiers).
        let only_target = target_pressed && (flags & !target) == 0;

        if only_target {
            if WAITING_FOR_RELEASE.load(Ordering::Relaxed) {
                // Still held from the first press – ignore.
                return event;
            }

            let now = now_ms();
            let prev = LAST_PRESS_MS.load(Ordering::Relaxed);
            let delta = now.saturating_sub(prev);

            if prev != 0 && delta <= DOUBLE_TAP_MAX_INTERVAL_MS {
                // Double-press detected.
                let _ = writeln!(stdout(), "fired");
                let _ = stdout().flush();
                LAST_PRESS_MS.store(0, Ordering::Relaxed);

                if unsafe { IMMEDIATE } {
                    std::process::exit(0);
                }
            } else {
                LAST_PRESS_MS.store(now, Ordering::Relaxed);
            }
            WAITING_FOR_RELEASE.store(true, Ordering::Relaxed);
        } else {
            // Modifier released (or a different modifier is now held).
            WAITING_FOR_RELEASE.store(false, Ordering::Relaxed);
        }

        event
    }

    pub(super) fn run() {
        let args: Vec<String> = std::env::args().collect();

        let mut key: Option<&str> = None;
        let mut immediate = false;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--key" => {
                    i += 1;
                    if i < args.len() {
                        key = Some(&args[i]);
                    }
                }
                "--immediate" => {
                    immediate = true;
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    std::process::exit(1);
                }
            }
            i += 1;
        }

        let target_flag = match key {
            Some("DoubleShift") => K_CG_EVENT_FLAG_MASK_SHIFT,
            Some("DoubleCommand") => K_CG_EVENT_FLAG_MASK_COMMAND,
            Some(other) => {
                eprintln!("unsupported key: {other}");
                std::process::exit(1);
            }
            None => {
                eprintln!(
                    "usage: bare-modifier-monitor --key <DoubleShift|DoubleCommand> [--immediate]"
                );
                std::process::exit(1);
            }
        };

        // SAFETY: these globals are written once before the run-loop starts
        // and only read from the callback afterwards.
        unsafe {
            TARGET_FLAG = target_flag;
            IMMEDIATE = immediate;
        }

        let event_mask: CGEventMask = 1 << K_CG_EVENT_FLAGS_CHANGED;

        let tap = unsafe {
            CGEventTapCreate(
                K_CG_SESSION_EVENT_TAP,
                K_CG_HEAD_INSERT_EVENT_TAP,
                K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                event_mask,
                tap_callback,
                std::ptr::null_mut(),
            )
        };

        if tap.is_null() {
            eprintln!(
                "failed to create event tap – check Accessibility / Input Monitoring permissions"
            );
            std::process::exit(1);
        }

        unsafe {
            TAP_PORT = tap;
        }

        let source = unsafe {
            CFMachPortCreateRunLoopSource(std::ptr::null(), tap, /*order*/ 0)
        };
        if source.is_null() {
            eprintln!("failed to create run-loop source from event tap");
            std::process::exit(1);
        }

        unsafe {
            let rl = CFRunLoop::get_current();
            // `CFRunLoopAddSource` is not wrapped by the `core-foundation`
            // crate, so call through the raw C API.
            extern "C" {
                fn CFRunLoopAddSource(
                    rl: core_foundation::runloop::CFRunLoopRef,
                    source: *mut c_void,
                    mode: core_foundation::string::CFStringRef,
                );
            }
            CFRunLoopAddSource(rl.as_concrete_TypeRef(), source, kCFRunLoopCommonModes);
        }

        // Signal readiness.
        println!("ready");
        let _ = stdout().flush();

        // Run the event loop forever (or until `--immediate` triggers exit).
        CFRunLoop::run_current();
    }
}
