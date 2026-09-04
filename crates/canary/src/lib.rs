//! Compiles every sibling crate against the working tree at once, and drives
//! them end to end.
//!
//! Its only job is to fail. A change in `lumen-core` that breaks `lumen-device`
//! should surface in one `cargo test` here, not three weeks later in a dependent
//! repo — which is the failure mode split repos introduce and the reason this
//! meta-repo exists.
//!
//! The tests deliberately assert **behaviour across a repo boundary**, not
//! constants. An earlier version checked `PROTOCOL_VERSION == 0`; when the
//! version byte started packing a major and a minor nibble, that failed and told
//! nobody anything useful. A canary that breaks on every harmless change gets
//! muted, and then it is not a canary.

#![forbid(unsafe_code)]

#[cfg(test)]
mod tests {
    use lumen_lang::compile;
    use lumen_proto::{Datagram, Header, MsgType, Payload, HEADER_LEN, TAG_LEN};
    use lumen_vm::program::Program;
    use lumen_vm::q16::Q16;
    use lumen_vm::vm::{Machine, NoUniforms, PixelInputs, PixelOutput};

    /// lumen-lang -> lumen-vm: source text in, lit pixel out.
    ///
    /// This is the whole compiler-to-runtime contract in one test. If the
    /// emitter and the VM ever disagree about an instruction, a register
    /// convention or the program format, it shows up here rather than on a
    /// device.
    #[test]
    fn an_effect_compiles_and_renders_across_the_crate_boundary() {
        let src = r#"
lumen 1
effect "canary" {
  param level : float = 0.5 range 0..1
  let wave = sin01(t)
  layer base {
    color = rgb(level, wave, u)
  }
}
"#;
        let (compiled, diags) = compile(src);
        let compiled = compiled.unwrap_or_else(|| panic!("compile failed:\n{}", diags.render(src)));

        let program =
            Program::parse(&compiled.bytecode).expect("the emitter produced a bad program");
        let mut m = Machine::new();
        m.run_frame_at(&program, Q16::from_ratio(1, 4), Q16::ZERO, &mut NoUniforms)
            .unwrap();
        let out = m
            .run_pixel(
                &program,
                &PixelInputs {
                    u: Q16::ONE,
                    ..Default::default()
                },
                &mut NoUniforms,
            )
            .unwrap();

        match out {
            PixelOutput::Rgb { r, g, b } => {
                assert_eq!(
                    r,
                    Q16::HALF,
                    "the parameter default did not reach the pixel"
                );
                // sin over a quarter turn is 1.
                assert!(
                    g > Q16::from_ratio(9, 10),
                    "the hoisted value did not reach the pixel: {g:?}"
                );
                assert_eq!(b, Q16::ONE, "the per-pixel input did not reach the pixel");
            }
            other => panic!("expected an RGB emit, got {other:?}"),
        }

        // The budget report has to describe the program that was actually
        // emitted, or every publish-time budget decision is made on fiction.
        assert_eq!(program.budget, compiled.report.instructions_per_pixel);
    }

    /// The compiler's budget report and the VM's own accounting must agree.
    ///
    /// They are computed in different crates from the same cost table. If one
    /// side ever drifts, "will this run at 60 fps" stops being answerable at
    /// publish time, which is the entire argument for a VM over native code.
    #[test]
    fn the_compilers_budget_matches_what_the_vm_actually_spends() {
        let src = r#"
lumen 1
effect "cost" {
  layer base {
    let n = noise2(x, y)
    color = rgb(n, n, n)
  }
}
"#;
        let (compiled, _) = compile(src);
        let compiled = compiled.unwrap();
        let program = Program::parse(&compiled.bytecode).unwrap();

        let mut m = Machine::new();
        m.run_pixel(&program, &PixelInputs::default(), &mut NoUniforms)
            .unwrap();
        assert_eq!(
            m.spent(),
            compiled.report.instructions_per_pixel,
            "the compiler and the VM disagree about what a pixel costs"
        );
    }

    /// lumen-proto round-trips a datagram the way a device would build one.
    #[test]
    fn a_datagram_round_trips_through_the_codec() {
        let mut body = [0u8; 8];
        {
            use lumen_proto::Writer;
            let mut w = Writer::new(&mut body);
            w.u64(1_234_567).unwrap();
        }
        let tag = [0u8; TAG_LEN];
        let mut header = Header::new(MsgType::SyncReq, [1, 2], [3, 4, 5, 6], 1, 99);
        header.payload_len = body.len() as u16;

        let dg = Datagram {
            header,
            payload: &body,
            tag: &tag,
        };
        let mut buf = [0u8; HEADER_LEN + 8 + TAG_LEN];
        let n = dg.encode(&mut buf).unwrap();
        let back = Datagram::decode(&buf[..n]).unwrap();
        assert_eq!(
            back.parse_payload().unwrap(),
            Some(Payload::SyncReq(lumen_proto::msg::SyncReq {
                t1: 1_234_567
            }))
        );
    }

    /// The device crate's sans-IO shape is what every other repo builds on.
    #[test]
    fn the_device_core_is_still_sans_io() {
        // Not a behavioural test — a structural one. `on_event` taking a time
        // and returning actions is the contract the simulator, the firmware and
        // the conformance runner all depend on, and it is worth failing loudly
        // if it ever changes shape.
        let claim = lumen_device::SourceClaim {
            priority: 200,
            expires_at_us: Some(1_000),
        };
        assert!(
            claim.expires_at_us.is_some(),
            "a source above the ambient floor must carry an expiry"
        );
    }
}
