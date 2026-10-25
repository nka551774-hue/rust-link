//! What the node does about the C library it runs on.
//!
//! Nothing here changes how the node behaves; it changes what the allocator
//! behind it costs. glibc gives every thread an arena of its own, up to eight
//! per core, and an arena holds on to the memory that was freed through it
//! instead of handing it back. A node that has a runtime's worth of threads and
//! a few hundred megabytes to live in pays for that in resident memory, so the
//! count is capped while the process is still single threaded, which is the
//! same thing a game server does with `MALLOC_ARENA_MAX`.
//!
//! The variable is honoured rather than overridden: an operator who named a
//! count knows what they want, and the node only applies its own value when
//! there is none.

use std::ffi::OsStr;

/// The number of arenas the node runs with. Two is enough for the threads that
/// allocate at the same time (the runtime's workers, one per playing player and
/// the helper readers); the point of the cap is to stop the allocator from
/// keeping an arena for every thread that has ever allocated.
const ARENAS: i32 = 2;

/// Caps the malloc arenas, unless the environment already named a count.
pub fn tune_memory() {
    let configured = std::env::var_os("MALLOC_ARENA_MAX");
    let Some(arenas) = arenas(configured.as_deref()) else {
        return;
    };
    // SAFETY: the call takes two numbers and touches nothing this process
    // owns. It runs before any thread exists, so no allocation can be racing
    // it, and the value is one the allocator accepts.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, arenas);
    }
}

/// The arena count to apply, or none when the environment names one itself.
///
/// An empty value counts as nothing: a unit file that writes
/// `MALLOC_ARENA_MAX=` means "not set", not the "no limit at all" that glibc
/// would read into an empty string.
fn arenas(configured: Option<&OsStr>) -> Option<i32> {
    match configured.and_then(OsStr::to_str).map(str::trim) {
        None | Some("") => Some(ARENAS),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variable_nobody_set_gets_the_nodes_own_cap() {
        assert_eq!(arenas(None), Some(ARENAS));
        assert_eq!(arenas(Some(OsStr::new(""))), Some(ARENAS));
        assert_eq!(arenas(Some(OsStr::new("   "))), Some(ARENAS));
    }

    #[test]
    fn a_variable_the_operator_set_is_left_alone() {
        assert_eq!(arenas(Some(OsStr::new("8"))), None);
        assert_eq!(arenas(Some(OsStr::new("0"))), None);
    }
}
