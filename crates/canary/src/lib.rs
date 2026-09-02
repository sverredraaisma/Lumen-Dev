//! Compiles every sibling crate against the working tree at once.
//!
//! Its only job is to fail. A change in `lumen-core` that breaks `lumen-device`
//! should be caught here, in one `cargo test`, rather than three weeks later in
//! a dependent repo — which is the failure mode split repos introduce and the
//! reason this meta-repo exists.

#[cfg(test)]
mod tests {
    #[test]
    fn siblings_link_together() {
        assert_eq!(lumen_proto::PROTOCOL_VERSION, 0);
        assert_eq!(lumen_vm::Q16::from_int(1), lumen_vm::Q16::ONE);
    }
}
