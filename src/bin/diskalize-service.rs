//! The indexing service.
//!
//! Runs as LocalSystem so raw volume access needs no elevation prompt, and
//! hands finished indexes to the GUI through shared memory.

fn main() {
    diskalize::service::entry();
}
