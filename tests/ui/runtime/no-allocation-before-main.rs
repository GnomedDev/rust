//! Tests that a program with no body does not allocate.
//!
//! The initial runtime should not allocate for performance/binary size reasons.
//@ run-pass

#[allow(dead_code)]
#[path = "aborting-alloc.rs"]
mod aux;

fn main() {}
