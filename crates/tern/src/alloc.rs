//! The allocator. On macOS tern uses mimalloc and returns freed memory to the OS at once.
//!
//! A vault unlock runs scrypt at cost 2^18, which needs 256 MiB of scratch. libmalloc keeps that
//! freed block resident, and mimalloc's default 10 ms purge delay leaves it counted in the
//! footprint until the kernel reclaims it (349 MB at launch). With the delay at 0 it is 91 MB.
//! mimalloc's memory carries VM tag 240 ("App-Specific Tag 1"); its default tag is the one macOS
//! names IOAccelerator, which made tools count tern's heap as GPU memory.
#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `mi_option_purge_delay` in mimalloc 2.1+ (libmimalloc-sys 0.1.49 has no constant for it).
#[cfg(target_os = "macos")]
const PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

/// `mi_option_os_tag`: the VM tag on memory mimalloc maps. Its default, 100, is
/// `VM_MEMORY_IOACCELERATOR`, so Activity Monitor and `vmmap` counted tern's heap as GPU memory.
#[cfg(target_os = "macos")]
const OS_TAG: libmimalloc_sys::mi_option_t = 18;

/// `VM_MEMORY_APPLICATION_SPECIFIC_1`, shown as "App-Specific Tag 1".
#[cfg(target_os = "macos")]
const APP_SPECIFIC_TAG: std::ffi::c_long = 240;

/// Labels mimalloc's memory as tern's own. A region keeps the tag it was mapped with, and the
/// Rust runtime allocates before `main` (mimalloc reserves its first 1 GiB arena then), so this runs
/// as an image initializer, at load time.
#[cfg(target_os = "macos")]
extern "C" fn tag_heap() {
    // SAFETY: as in `init`.
    unsafe {
        libmimalloc_sys::mi_option_set(OS_TAG, APP_SPECIFIC_TAG);
    }
}

#[cfg(target_os = "macos")]
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static TAG_HEAP: extern "C" fn() = tag_heap;

/// Makes mimalloc purge freed pages immediately. Call before the first large allocation.
pub fn init() {
    #[cfg(target_os = "macos")]
    // SAFETY: `mi_option_set` only stores an integer in mimalloc's option table, and any thread
    // may call it at any time.
    unsafe {
        libmimalloc_sys::mi_option_set(PURGE_DELAY, 0);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    // A test thread may block on `vmmap`.
    #![allow(clippy::expect_used, clippy::disallowed_methods)]

    /// The label `vmmap` gives the region holding `addr` in this process.
    fn region_label(addr: usize) -> Option<String> {
        let out = std::process::Command::new("vmmap")
            .arg(std::process::id().to_string())
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|line| {
                let range = line.split_whitespace().find(|w| {
                    w.contains('-') && w.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
                })?;
                let (start, end) = range.split_once('-')?;
                let (start, end) = (
                    usize::from_str_radix(start, 16).ok()?,
                    usize::from_str_radix(end, 16).ok()?,
                );
                let label = line[..line.find(range)?].trim().to_owned();
                (start..end).contains(&addr).then_some(label)
            })
    }

    #[test]
    fn heap_memory_is_labelled_app_specific_not_gpu() {
        super::init();
        let block = vec![1u8; 64 << 20];
        let label = region_label(block.as_ptr() as usize).expect("vmmap lists the block's region");
        assert!(
            label.starts_with("App-Specific Tag 1"),
            "a 64 MiB heap block is labelled {label:?}; Activity Monitor would count tern's heap as GPU memory"
        );
    }
}
