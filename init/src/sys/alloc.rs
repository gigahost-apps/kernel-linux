//! The tests' allocator: the system's, counting what each thread holds,
//! so a test can bound the memory one step costs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// The bytes this thread has allocated and not freed.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    /// The most [`LIVE`] has been since [`peak_during`] started.
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn bytes(size: usize) -> isize {
    isize::try_from(size).unwrap_or(isize::MAX)
}

/// Adds `change` to this thread's count. Plain thread-local cells
/// without destructors: nothing here allocates or can be gone.
fn count(change: isize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get().saturating_add(change);
        live.set(now);
        let _ = PEAK.try_with(|peak| {
            if now > peak.get() {
                peak.set(now);
            }
        });
    });
}

pub struct Counting;

// SAFETY: every call goes to the system allocator unchanged, with the
// caller's layout and pointer; the counting never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps `alloc`'s contract, the system's too.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            count(bytes(layout.size()));
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            count(bytes(layout.size()));
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a pointer this allocator (so the
        // system's) returned for `layout`.
        unsafe { System.dealloc(pointer, layout) };
        count(-bytes(layout.size()));
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, with a valid new size.
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            count(bytes(new_size).saturating_sub(bytes(layout.size())));
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `step` and returns its result and the most bytes the calling
/// thread held at once during it, beyond what it held before.
pub fn peak_during<T>(step: impl FnOnce() -> T) -> (T, usize) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let result = step();
    let peak = PEAK.with(Cell::get);
    (result, usize::try_from(peak - start).unwrap_or(0))
}
