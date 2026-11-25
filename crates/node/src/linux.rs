//! Caps glibc malloc arenas at startup (before threads spawn) to bound resident
//! memory, while honouring any explicit `MALLOC_ARENA_MAX` from the environment.

use std::ffi::OsStr;

const ARENAS: i32 = 2;

/// Caps malloc arenas unless the environment already specifies a count.
pub fn tune_memory() {
    let configured = std::env::var_os("MALLOC_ARENA_MAX");
    if let Some(arenas) = arenas(configured.as_deref()) {
        // SAFETY: runs single-threaded before runtime creation with a valid arena count.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, arenas);
        }
    }
}

fn arenas(configured: Option<&OsStr>) -> Option<i32> {
    configured.is_none_or(|v| v.as_encoded_bytes().trim_ascii().is_empty()).then_some(ARENAS)
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
