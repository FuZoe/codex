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
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicPtr;
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
    static WAITING_FOR_RELEASE: AtomicBool = AtomicBool::new(false);

    /// Which modifier key pair to watch (set once before run-loop starts).
    static TARGET_FLAG: OnceLock<CGEventFlags> = OnceLock::new();
    /// If `true`, fire on the first detected double-press and exit.
    static IMMEDIATE: AtomicBool = AtomicBool::new(false);
    /// The CFMachPortRef for the event tap, stored so the callback can
    /// re-enable it if macOS disables it by timeout.
    static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

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

    // CFMachPort / CFRunLoop helpers from CoreFoundation (C API).
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: CFMachPortRef,
            order: i64,
        ) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
        fn CFRunLoopAddSource(
            rl: core_foundation::runloop::CFRunLoopRef,
            source: *mut c_void,
            mode: core_foundation::string::CFStringRef,
        );
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// The event-tap callback.  Invoked for every `kCGEventFlagsChanged`
    /// event and for tap-disabled notifications.
    unsafe extern "C" fn tap_callback(
        _proxy: CGEventTapProxy,
        event_type: CGEventType,
        event: CGEventRef,
        _user_info: *mut c_void,
    ) -> CGEventRef {
        // macOS may disable the tap after a timeout.  Re-enable it.
        if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT {
            let port = TAP_PORT.load(Ordering::Relaxed);
            if !port.is_null() {
                unsafe { CGEventTapEnable(port, /*enable*/ true) };
            }
            return event;
        }

        if event_type != K_CG_EVENT_FLAGS_CHANGED {
            return event;
        }

        let flags = unsafe { CGEventGetFlags(event) } & MODIFIER_FLAGS_MASK;
        let target = *TARGET_FLAG.get().unwrap_or(&0);

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

                if IMMEDIATE.load(Ordering::Relaxed) {
                    std::process::exit(0);
                }
            } else {
                LAST_PRESS_MS.store(now, Ordering::Relaxed);
            }
            WAITING_FOR_RELEASE.store(true, Ordering::Relaxed);
        } else {
            // Modifier released (or a different modifier is now held).
            // Clear the first-press timestamp when a *different* modifier is
            // pressed so that target → other → target within 400ms is not
            // mis-detected as a double-press of target.
            if flags != 0 {
                LAST_PRESS_MS.store(0, Ordering::Relaxed);
            }
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

        TARGET_FLAG
            .set(target_flag)
            .expect("TARGET_FLAG already set");
        IMMEDIATE.store(immediate, Ordering::Relaxed);

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

        TAP_PORT.store(tap, Ordering::Relaxed);

        let source = unsafe {
            CFMachPortCreateRunLoopSource(std::ptr::null(), tap, /*order*/ 0)
        };
        if source.is_null() {
            // Clean up the tap before exiting.
            unsafe { CFRelease(tap) };
            eprintln!("failed to create run-loop source from event tap");
            std::process::exit(1);
        }

        unsafe {
            let rl = CFRunLoop::get_current();
            CFRunLoopAddSource(rl.as_concrete_TypeRef(), source, kCFRunLoopCommonModes);
            // Release our ownership of the source (the run-loop retains it).
            CFRelease(source);
        }

        // Signal readiness.
        println!("ready");
        let _ = stdout().flush();

        // Run the event loop forever (or until `--immediate` triggers exit).
        // Note: `tap` is intentionally not released here – it must remain
        // alive for the duration of the process.  On exit the OS reclaims it.
        CFRunLoop::run_current();
    }
}
