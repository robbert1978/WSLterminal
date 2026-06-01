//! Throughput micro-bench for the VT core alone (no GUI), to separate parser
//! cost from render cost. Run: `cargo run --release -p wslterm-core --example bench`.

use std::time::Instant;
use wslterm_core::Terminal;

fn bench(name: &str, data: &[u8], iters: usize) {
    let mut t = Terminal::new(96, 27);
    let start = Instant::now();
    for _ in 0..iters {
        t.feed(data);
    }
    let secs = start.elapsed().as_secs_f64();
    let bytes = (data.len() * iters) as f64;
    println!(
        "{name:14} {secs:8.3}s  {:7.4} GB/s  ({:.0} MB)",
        bytes / secs / 1e9,
        bytes / 1e6
    );
}

fn main() {
    // LongLine: plain ascii, lots of wrapping/scrolling.
    let mut longline = Vec::new();
    for _ in 0..20000 {
        longline.extend_from_slice(b"the quick brown fox jumps over the lazy dog ");
    }
    bench("LongLine", &longline, 50);

    // FGPerChar: an SGR fg change before each visible char.
    let mut fg = Vec::new();
    for i in 0..200000u32 {
        let color = (i % 216) + 16;
        let ch = (b'a' + (i % 26) as u8) as char;
        fg.extend_from_slice(format!("\x1b[38;5;{color}m{ch}").as_bytes());
    }
    bench("FGPerChar", &fg, 50);

    // FGBGPerChar: fg+bg per char.
    let mut fgbg = Vec::new();
    for i in 0..200000u32 {
        let c = (i % 216) + 16;
        let ch = (b'a' + (i % 26) as u8) as char;
        fgbg.extend_from_slice(format!("\x1b[38;5;{c};48;5;{}m{ch}", (c + 7) % 216 + 16).as_bytes());
    }
    bench("FGBGPerChar", &fgbg, 50);
}
