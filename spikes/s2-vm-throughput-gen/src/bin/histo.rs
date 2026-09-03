//! Opcode histogram for a compiled program's pixel section.

use std::collections::BTreeMap;
use std::fs;

use lumen_vm::program::{Program, Section};

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = fs::read(&path).expect("read");
        let p = Program::parse(&bytes).expect("parse");
        let mut histo: BTreeMap<String, usize> = BTreeMap::new();
        let mut budget = 0u32;
        for i in 0..p.section_len(Section::Pixel) {
            let ins = p.instruction(Section::Pixel, i).expect("instruction");
            *histo.entry(format!("{:?}", ins.op)).or_default() += 1;
            budget += ins.op.cost();
        }
        let name = path
            .rsplit(|c| c == '/' || c == '\\')
            .next()
            .unwrap_or(&path)
            .to_string();
        let n: usize = histo.values().sum();
        println!("{name}: {n} instructions, {budget} units");
        let mut v: Vec<_> = histo.into_iter().collect();
        v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        let line: Vec<String> = v.iter().map(|(o, c)| format!("{o}x{c}")).collect();
        println!("   {}", line.join(" "));
    }
}
