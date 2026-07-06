//! Segfault hunter: prints every input before resolving so a process-killing
//! crash (which proptest cannot persist) identifies its input in the log.
//! Gated behind DOOVER_FUZZ_ITERS; used by the fuzz-hunt CI workflow.

use doover_core::registry::Registry;
use doover_core::resolver::{Ctx, resolve};
use std::io::Write;

/// xorshift64* — deterministic per seed, no extra dependencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const SHELL_CHARS: &[char] = &[
    '$', '(', ')', '{', '}', '<', '>', '|', '&', ';', '`', '"', '\'', '\\', '*', '?', '[', ']',
    '~', '!', '#', '=', ' ', '-', '.', '/', 'a', 'r', 'm', 'c', 'd', 'e', 'v', 'f', 'i', 'o',
];

fn gen_input(rng: &mut Rng) -> String {
    let len = rng.below(161) as usize;
    let mut s = String::with_capacity(len * 2);
    for _ in 0..len {
        let ch = match rng.below(100) {
            0..=54 => SHELL_CHARS[rng.below(SHELL_CHARS.len() as u64) as usize],
            55..=84 => (0x20 + rng.below(0x5f)) as u8 as char,
            85..=89 => '\n',
            _ => loop {
                let cp = rng.below(0x11_0000) as u32;
                if let Some(c) = char::from_u32(cp) {
                    break c;
                }
            },
        };
        s.push(ch);
    }
    s
}

#[test]
fn hunt() {
    let Ok(iters) = std::env::var("DOOVER_FUZZ_ITERS") else {
        eprintln!("DOOVER_FUZZ_ITERS not set; skipping hunt");
        return;
    };
    let iters: u64 = iters.parse().expect("DOOVER_FUZZ_ITERS must be a number");
    let seed: u64 = std::env::var("DOOVER_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let registry = Registry::builtin().unwrap();
    let jail = tempfile::tempdir().unwrap();
    let home = jail.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ctx = Ctx {
        cwd: jail.path(),
        home: &home,
    };

    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let stdout = std::io::stdout();
    for i in 0..iters {
        let input = gen_input(&mut rng);
        {
            let mut lock = stdout.lock();
            writeln!(lock, "INPUT {i}: {input:?}").unwrap();
            lock.flush().unwrap();
        }
        let _ = resolve(&input, &registry, &ctx);
    }
    println!("hunt finished cleanly: {iters} iterations, seed {seed}");
}
