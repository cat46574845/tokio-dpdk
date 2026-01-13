/// Debug printing utilities with file:line and timestamp support.
///
/// This module provides macros for debug output that include:
/// - Elapsed time since a start point
/// - File and line number of the call site
/// - Formatted message

/// Debug print macro with timestamp and source location.
///
/// # Usage
/// ```ignore
/// use std::time::Instant;
/// use tokio::util::debug::dbg_print;
///
/// let start = Instant::now();
/// dbg_print!(start, "My message: {}", value);
/// ```
///
/// Output format: `[   0.001s] src/file.rs:42 - My message: value`
#[allow(unused_macros)]
macro_rules! dbg_print {
    ($start:expr, $($arg:tt)*) => {
        {
            let elapsed = $start.elapsed();
            eprintln!("[{:>12.3?}] {}:{} - {}", elapsed, file!(), line!(), format!($($arg)*));
        }
    };
}

/// Debug print macro that is conditionally compiled based on a flag.
///
/// # Usage
/// ```ignore
/// use std::time::Instant;
/// use tokio::util::debug::dbg_print_if;
///
/// const DEBUG: bool = true;
/// let start = Instant::now();
/// dbg_print_if!(DEBUG, start, "My message: {}", value);
/// ```
#[allow(unused_macros)]
macro_rules! dbg_print_if {
    ($flag:expr, $start:expr, $($arg:tt)*) => {
        if $flag {
            let elapsed = $start.elapsed();
            eprintln!("[{:>12.3?}] {}:{} - {}", elapsed, file!(), line!(), format!($($arg)*));
        }
    };
}

#[allow(unused_imports)]
pub(crate) use dbg_print;
#[allow(unused_imports)]
pub(crate) use dbg_print_if;
