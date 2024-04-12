#![doc = include_str!("../../stdarch/crates/core_arch/src/core_arch_docs.md")]

#[allow(unused_imports)]
#[stable(feature = "simd_arch", since = "1.27.0")]
pub use crate::core_arch::arch::*;
use crate::{
    std_detect,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Inline assembly.
///
/// Refer to [Rust By Example] for a usage guide and the [reference] for
/// detailed information about the syntax and available options.
///
/// [Rust By Example]: https://doc.rust-lang.org/nightly/rust-by-example/unsafe/asm.html
/// [reference]: https://doc.rust-lang.org/nightly/reference/inline-assembly.html
#[stable(feature = "asm", since = "1.59.0")]
#[rustc_builtin_macro]
pub macro asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}

/// Module-level inline assembly.
///
/// Refer to [Rust By Example] for a usage guide and the [reference] for
/// detailed information about the syntax and available options.
///
/// [Rust By Example]: https://doc.rust-lang.org/nightly/rust-by-example/unsafe/asm.html
/// [reference]: https://doc.rust-lang.org/nightly/reference/inline-assembly.html
#[stable(feature = "global_asm", since = "1.59.0")]
#[rustc_builtin_macro]
pub macro global_asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}

static FEATURE_HOOK: AtomicUsize = AtomicUsize::new(0);
type FeatureHook = fn(std_detect::Feature) -> bool;

/// Detects if a CPU feature is enabled.
///
/// This is an internal implementation detail and `is_*_feature_detected` should be used instead.
#[unstable(feature = "stdarch_internal", issue = "none")]
pub fn detect_cpu_feature(feature: std_detect::Feature) -> bool {
    let hook = FEATURE_HOOK.load(Ordering::Relaxed);
    if hook == 0 {
        return false;
    }

    let hook = unsafe { crate::mem::transmute::<usize, FeatureHook>(hook) };
    hook(feature)
}

/// Sets the hook to be called by [`detect_cpu_feature`].
///
/// This should be set by the runtime if there is one, otherwise always returns false.
#[unstable(feature = "stdarch_internal", issue = "none")]
pub fn set_cpu_feature_hook(hook: FeatureHook) {
    FEATURE_HOOK.store(hook as usize, Ordering::Relaxed);
}
