# Generating the spike's programs

```bash
cargo run --bin opcost   -- ../s2-vm-throughput/opcost    # one program per opcode, plus a manifest
cargo run --bin dispatch -- ../s2-vm-throughput/opcost    # NOP programs at 16, 64 and 256, for the fit
cargo run --bin histo    -- ../s2-vm-throughput/programs/*.lfxb   # opcode histogram of a compiled effect
```

`opcost` and `dispatch` write the files the spike `include_bytes!`s, so a change
to either means rebuilding the spike. `histo` is the analysis tool that showed
`03-drift` to be the only corpus effect carrying a `MASKTEST`, which is why it
measures at half its budget while the others land within 4%.

The weights `opcost` prints in its manifest are read from `OpCode::cost()`, so
the manifest is a snapshot of the table under test rather than a second copy of
it.
