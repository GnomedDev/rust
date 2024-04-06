/// Helper for 'no-allocation-before-main'.
///
/// This also contains a meta-test to make sure that the AbortingAllocator does indeed abort.
//@ run-fail

pub struct AbortingAllocator;

unsafe impl std::alloc::GlobalAlloc for AbortingAllocator {
    unsafe fn alloc(&self, _: std::alloc::Layout) -> *mut u8 {
        std::process::abort()
    }

    unsafe fn dealloc(&self, _: *mut u8, _: std::alloc::Layout) {
        std::process::abort()
    }
}

#[global_allocator]
static ALLOCATOR: AbortingAllocator = AbortingAllocator;

fn main() {
    std::hint::black_box(Box::new(false));
}
