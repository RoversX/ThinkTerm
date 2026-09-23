//! Isolate printable-run allocation and callback costs from terminal rendering.
//! Pass `stats` to count allocating calls separately from timing measurements.
//! The `print-buffer` consumer models reusable text accumulation; `mux-coalesce`
//! uses the actual action coalescer. Neither models screen updates or PTY IO.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;
use wezterm_escape_parser::{
    Action,
    parser::{ParsedAction, Parser},
};
static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static REALLOCS: AtomicUsize = AtomicUsize::new(0);
struct Counter;
// SAFETY: every allocation operation delegates unchanged to System; the
// counters only observe calls and do not alter pointers, layouts, or ownership.
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNT.load(Relaxed) {
            REALLOCS.fetch_add(1, Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
#[global_allocator]
static ALLOC: Counter = Counter;
fn run(data: &[u8], mode: u8, mux: bool, iterations: usize) -> usize {
    let mut parser = Parser::new();
    let mut sum = 0usize;
    for _ in 0..iterations {
        let mut prints = String::new();
        if mux {
            let mut actions = Vec::new();
            match mode {
                0 => parser.parse(black_box(data), |a| a.append_to(&mut actions)),
                1 => parser.parse_print_runs(black_box(data), |a| a.append_to(&mut actions)),
                _ => {
                    parser.parse_with_borrowed_text(black_box(data), |a| a.append_to(&mut actions))
                }
            }
            black_box(&actions);
            sum = sum.wrapping_add(actions.len());
        } else {
            fn apply(a: Action, prints: &mut String, sum: &mut usize) {
                match a {
                    Action::Print(c) => prints.push(c),
                    Action::PrintString(s) => prints.push_str(&s),
                    _ => {
                        *sum = sum.wrapping_add(black_box(prints.len()));
                        black_box(&prints);
                        prints.clear();
                    }
                }
            }
            match mode {
                0 => parser.parse(black_box(data), |a| apply(a, &mut prints, &mut sum)),
                1 => parser.parse_print_runs(black_box(data), |a| apply(a, &mut prints, &mut sum)),
                _ => parser.parse_with_borrowed_text(black_box(data), |a| match a {
                    ParsedAction::Action(a) => apply(a, &mut prints, &mut sum),
                    ParsedAction::Print(s) => prints.push_str(s),
                }),
            }
            sum = sum.wrapping_add(black_box(prints.len()));
            black_box(&prints);
        }
    }
    black_box(sum)
}
fn main() {
    let stats = std::env::args().any(|a| a == "stats");
    println!("case,path,mode,rep,bytes,ns,alloc,realloc");
    for n in [2, 4, 8, 16, 64] {
        for kind in ["sgr", "unicode"] {
            let pattern = if kind == "sgr" {
                format!("\x1b[31m{}\x1b[32m{}", "x".repeat(n), "y".repeat(n))
            } else {
                format!("{}中", "x".repeat(n))
            };
            let data = pattern.repeat((65536 / pattern.len()).max(1)).into_bytes();
            for mux in [false, true] {
                let mut a = Vec::new();
                let mut b = Vec::new();
                Parser::new().parse(&data, |v| v.append_to(&mut a));
                Parser::new().parse_with_borrowed_text(&data, |v| v.append_to(&mut b));
                assert_eq!(a, b);
                for rep in 0..if stats { 1 } else { 5 } {
                    for mode in if rep % 2 == 0 { [0, 1, 2] } else { [2, 1, 0] } {
                        let iters = if stats { 1 } else { 64 };
                        ALLOCS.store(0, Relaxed);
                        REALLOCS.store(0, Relaxed);
                        COUNT.store(stats, Relaxed);
                        let start = Instant::now();
                        run(&data, mode, mux, iters);
                        let ns = start.elapsed().as_nanos();
                        COUNT.store(false, Relaxed);
                        println!(
                            "{kind}-{n},{},{},{rep},{},{ns},{},{}",
                            if mux { "mux-coalesce" } else { "print-buffer" },
                            ["scalar", "owned", "borrowed"][mode as usize],
                            data.len() * iters,
                            ALLOCS.load(Relaxed),
                            REALLOCS.load(Relaxed)
                        );
                    }
                }
            }
        }
    }
}
