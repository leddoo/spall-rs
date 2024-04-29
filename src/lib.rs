
pub fn init(path: &str) -> Result<bool, std::io::Error> {
    // init timer for non-specialized platforms.
    now();

    let mut state = GLOBAL_STATE.lock().unwrap();
    if state.is_some() {
        return Ok(false);
    }

    let time = {
        let time = std::time::SystemTime::now();
        let unix = time.duration_since(std::time::UNIX_EPOCH)
            .expect("system time can't be before unix epoch");
        unix.as_micros().to_string()
    };

    let trace_path = {
        let (path, new) = match path.contains("$") {
            true  => (path.replace("$", &time), true),
            false => (path.to_string(), false),
        };

        let mut f = std::fs::OpenOptions::new()
            .create(!new)
            .create_new(new)
            .write(true)
            .truncate(true)
            .open(&path)?;


        use std::io::Write;

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
    });

    return Ok(true);
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



use std::sync::Mutex;


static GLOBAL_STATE: Mutex<Option<GlobalState>> = Mutex::new(None);

struct GlobalState {
    trace_path: std::path::PathBuf,
}


struct ThreadState {
}


#[repr(C, packed)]
pub struct SpallHeader {
    pub magic_header:   u64, // = 0x0BADF00D
    pub version:        u64, // = 1
    pub timestamp_unit: f64,
    pub must_be_0:      u64, // = 0
}


