#[forbid(unsafe_op_in_unsafe_fn)]

use std::cell::UnsafeCell;
use std::mem::size_of;
use std::sync::RwLock;
use std::fs::File;


pub fn init(path: &str) -> Result<bool, std::io::Error> {
    // init timer for non-specialized platforms.
    now();

    let mut state = GLOBAL_STATE.write().unwrap();
    if state.is_some() {
        return Ok(false);
    }

    // init trace file.
    let trace_path = {
        use std::io::Write;

        let (path, new) =
            if path.contains("$") {
                let time = {
                    let time = std::time::SystemTime::now();
                    let unix = time.duration_since(std::time::UNIX_EPOCH)
                        .expect("system time can't be before unix epoch");
                    unix.as_micros().to_string()
                };
                (path.replace("$", &time), true)
            }
            else { (path.to_string(), false) };

        let mut f = std::fs::OpenOptions::new()
            .create(!new)
            .create_new(new)
            .write(true)
            .truncate(true)
            .open(&path)?;

        let hz = timer_frequency();
        let micros = 1_000_000.0 / hz;

        let header = SpallHeader {
            magic_header:   0x0BADF00D,
            version:        1,
            timestamp_unit: micros,
            must_be_0:      0,
        };
        f.write(unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                std::mem::size_of_val(&header))
        })?;

        std::fs::canonicalize(path)?
    };

    *state = Some(GlobalState {
        trace_path,
        buffer_size: 64*1024,
        silent: false,
    });

    return Ok(true);
}



#[macro_export]
macro_rules! trace_scope {
    ($name:expr) => {
        let _trace_scope = $crate::trace_scope_impl($name);
    };

    ($name:expr, $($args:tt)+) => {
        let _trace_scope = $crate::trace_scope_args_impl($name, format_args!($($args)+));
    };
}



#[inline(always)]
pub fn now() -> u64 {
    timer::now()
}

/// in Hz
#[inline(always)]
pub fn timer_frequency() -> f64 {
    timer::timer_frequency()
}




// data structures:

#[repr(C, packed)]
pub struct SpallHeader {
    pub magic_header:   u64, // = 0x0BADF00D
    pub version:        u64, // = 1
    pub timestamp_unit: f64,
    pub must_be_0:      u64, // = 0
}

pub enum EventType {
    Invalid            = 0,
    CustomData         = 1, // Basic readers can skip this.
    StreamOver         = 2,

    Begin              = 3,
    End                = 4,
    Instant            = 5,

    OverwriteTimestamp = 6, // Retroactively change timestamp units - useful for incrementally improving RDTSC frequency.
    PadSkip            = 7,
}

#[repr(C, packed)]
pub struct BeginEvent {
    pub ty:       u8, // = SpallEventType_Begin
    pub category: u8,

    pub pid:  u32,
    pub tid:  u32,
    pub when: f64,

    pub name_len: u8,
    pub args_len: u8,
}

#[repr(C, packed)]
pub struct BeginEventMax {
    pub event: BeginEvent,
    pub name: [u8; 255],
    pub args: [u8; 255],
}

#[repr(C, packed)]
pub struct EndEvent {
    pub ty:   u8, // = SpallEventType_End
    pub pid:  u32,
    pub tid:  u32,
    pub when: f64,
}

#[repr(C, packed)]
pub struct PadSkipEvent {
    pub ty:   u8, // = SpallEventType_Pad_Skip
    pub size: u32,
}



static GLOBAL_STATE: RwLock<Option<GlobalState>> = RwLock::new(None);

struct GlobalState {
    trace_path: std::path::PathBuf,
    buffer_size: usize,
    silent: bool,
}


struct ThreadState {
    pid: u32,
    tid: u32,
    file: File,
    buffer: *mut u8,
    buffer_size: usize,
    write_ptr: *mut u8,
    write_rem: usize,
    silent: bool,
}

impl ThreadState {
    #[inline]
    fn with(f: impl FnOnce(&mut ThreadState)) {
        thread_local! {
            static THIS: UnsafeCell<Option<ThreadState>> = UnsafeCell::new(ThreadState::init());
        }

        THIS.with(|this| {
            if let Some(this) = unsafe { &mut *this.get() } {
                f(this);
            }
        })
    }

    #[cold]
    fn init() -> Option<Self> {
        let global = GLOBAL_STATE.read().ok()?;
        let global = global.as_ref()?;

        let file = match
            std::fs::OpenOptions::new()
                .append(true)
                .open(&global.trace_path)
        {
            Ok(f) => f,

            Err(e) => {
                if !global.silent {
                    eprintln!("spall thread init failed to open file {:?} with error {:?}",
                        global.trace_path, e);
                }
                return None;
            }
        };

        let buffer_size = global.buffer_size;
        let buffer = unsafe {
            let ptr = std::alloc::alloc(
                std::alloc::Layout::from_size_align(buffer_size, 1).unwrap());

            if ptr.is_null() {
                if !global.silent {
                    eprintln!("spall thread init failed allocate buffer");
                }
                return None;
            }

            ptr
        };

        Some(Self {
            // @todo
            pid: 42,
            tid: 69,
            file,
            buffer,
            buffer_size,
            write_ptr: buffer,
            write_rem: buffer_size,
            silent: global.silent,
        })
    }

    #[inline(always)]
    fn reserve(&mut self, size: usize) {
        if size > self.write_rem {
            self.flush();
        }
        debug_assert!(self.write_rem >= size);
    }

    #[inline(always)]
    unsafe fn push_bytes(&mut self, bytes: &[u8]) { unsafe {
        let len = bytes.len();
        debug_assert!(self.write_rem >= len);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.write_ptr, len);
        self.write_ptr = self.write_ptr.add(len);
        self.write_rem -= len;
    }}

    #[inline(always)]
    unsafe fn push_as_bytes<T>(&mut self, v: T) { unsafe {
        let len = size_of::<T>();
        debug_assert!(self.write_rem >= len);
        self.write_ptr.cast::<T>().write_unaligned(v);
        self.write_ptr = self.write_ptr.add(len);
        self.write_rem -= len;
    }}

    #[inline]
    fn push_args(&mut self, max_len: usize, args: std::fmt::Arguments) -> usize {
        use std::fmt::Write;

        struct Writer {
            ptr: *mut u8,
            rem: usize,
        }

        impl std::fmt::Write for Writer {
            #[inline]
            fn write_str(&mut self, s: &str) -> std::fmt::Result { unsafe {
                let bytes = s.as_bytes();

                let len = bytes.len().min(self.rem);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr, len);
                self.ptr = self.ptr.add(len);
                self.rem -= len;

                Ok(())
            }}
        }

        let mut writer = Writer {
            ptr: self.write_ptr,
            rem: self.write_rem.min(max_len),
        };
        _ = writer.write_fmt(args);

        let len = writer.ptr as usize - self.write_ptr as usize;
        self.write_ptr = writer.ptr;
        self.write_rem -= len;

        return len;
    }

    #[inline]
    unsafe fn push_begin_event(&mut self, when: u64, name_len: u8, args_len: u8) -> *mut u8 { unsafe {
        let ptr = self.write_ptr;
        self.push_as_bytes(BeginEvent {
            ty: EventType::Begin as u8,
            category: 0,
            pid: self.pid,
            tid: self.tid,
            when: when as f64,
            name_len,
            args_len,
        });
        return ptr;
    }}

    #[inline]
    unsafe fn patch_begin_args_len(&mut self, begin: *mut u8, args_len: u8) {
        let offset = std::mem::offset_of!(BeginEvent, args_len);
        unsafe { begin.add(offset).write(args_len) }
    }

    #[inline]
    unsafe fn push_end_event(&mut self, when: u64) { unsafe {
        self.push_as_bytes(EndEvent {
            ty: EventType::End as u8,
            pid: self.pid,
            tid: self.tid,
            when: when as f64,
        });
    }}

    #[cold]
    fn flush(&mut self) {
        use std::io::Write;

        let t0 = now();

        let len = self.write_ptr as usize - self.buffer as usize;
        let res = self.file.write(unsafe { core::slice::from_raw_parts(self.buffer, len) });
        if let Err(e) = res {
            if !self.silent {
                eprintln!("spall file write failed {:?}", e);
            }
        }

        self.write_ptr = self.buffer;
        self.write_rem = self.buffer_size;

        unsafe {
            let name = "spall/flush";
            self.push_begin_event(t0, name.len() as u8, 0);
            self.push_bytes(name.as_bytes());

            let t1 = now();
            self.push_end_event(t1);
        }
    }
}

impl Drop for ThreadState {
    fn drop(&mut self) {
        self.flush();
    }
}



pub struct TraceScope;

impl Drop for TraceScope {
    #[inline]
    fn drop(&mut self) {
        ThreadState::with(|s| unsafe {
            s.reserve(size_of::<EndEvent>());
            s.push_end_event(now());
        });
    }
}

#[inline]
pub fn trace_scope_impl(name: &str) -> TraceScope {
    ThreadState::with(|s| unsafe {
        let name_len = name.len().min(255);
        s.reserve(size_of::<BeginEvent>() + name_len);

        s.push_begin_event(now(), name_len as u8, 0);
        s.push_bytes(&name.as_bytes()[..name_len]);
    });
    TraceScope
}

#[inline]
pub fn trace_scope_args_impl(name: &str, args: std::fmt::Arguments) -> TraceScope {
    ThreadState::with(|s| unsafe {
        let name_len = name.len().min(255);
        s.reserve(size_of::<BeginEvent>() + name_len + 255);

        let begin = s.push_begin_event(now(), name_len as u8, 0);
        s.push_bytes(&name.as_bytes()[..name_len]);

        let args_len = s.push_args(255, args);
        s.patch_begin_args_len(begin, args_len as u8);
    });
    TraceScope
}



#[cfg(target_arch = "aarch64")]
mod timer {
    #[inline(always)]
    pub fn now() -> u64 {
        let tsc: u64;
        unsafe {
            std::arch::asm!(
                "mrs {tsc}, cntvct_el0",
                tsc = out(reg) tsc,
            );
        }
        tsc
    }

    #[inline(always)]
    pub fn timer_frequency() -> f64 {
        let freq: u64;
        unsafe {
            std::arch::asm!(
                "mrs {freq}, cntfrq_el0",
                freq = out(reg) freq,
            );
        }
        freq as f64
    }
}

#[cfg(not(target_arch = "aarch64"))]
mod timer {
    use std::sync::OnceLock;
    use std::time::Instant;

    static T0: OnceLock<Instant> = OnceLock::new();

    #[inline(always)]
    pub fn now() -> u64 {
        let t0 = T0.get_or_init(|| Instant::now());
        t0.elapsed().as_nanos() as u64
    }

    #[inline(always)]
    pub fn timer_frequency() -> f64 {
        1_000_000_000.0
    }
}

