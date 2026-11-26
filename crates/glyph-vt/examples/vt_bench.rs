//! Microbenchmark suite for `glyph_vt`:
//! 1. `Vt::feed` throughput (MB/s and MiB/s) on plain text, SGR-heavy output,
//!    and full-screen `120x40` TUI redraw streams.
//! 2. `Vt::fill` latency (ns / us per call) on an unchanged, one-row-changed,
//!    and fully-changed `120x40` screen.
//!
//! Run with:
//!   cargo run --release -p glyph-vt --example vt_bench

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

use glyph_vt::{Theme, Vt};
use term_cells::Screen;

const COLS: u16 = 120;
const ROWS: u16 = 40;

fn build_plain_stream() -> Vec<u8> {
    let mut buf = String::with_capacity(256 * 1024);
    for line_no in 0..2_500u32 {
        let _ = writeln!(
            buf,
            "{line_no:06}  crates/glyph-vt/src/lib.rs: resolved terminal row cells into Screen without heap allocation\r"
        );
    }
    buf.into_bytes()
}

fn build_sgr_heavy_stream() -> Vec<u8> {
    let mut buf = String::with_capacity(256 * 1024);
    for line_no in 0..1_200u32 {
        let r = ((line_no * 37) & 0xff) as u8;
        let g = ((line_no * 73) & 0xff) as u8;
        let b = ((line_no * 19) & 0xff) as u8;
        let pal = (16 + (line_no % 216)) as u8;
        let _ = write!(
            buf,
            "\x1b[38;5;244m{line_no:05}\x1b[0m \
             \x1b[1;34mfn\x1b[0m \x1b[38;2;{r};{g};{b}mrender_row\x1b[0m\
             (\x1b[3;33mscreen\x1b[0m: \x1b[32m&mut Screen\x1b[0m) -> \
             \x1b[4:3;58;5;{pal};38;5;{pal}mbool\x1b[0m \
             \x1b[7;36m OK \x1b[0m\r\n"
        );
    }
    buf.into_bytes()
}

fn build_fullscreen_redraw_stream() -> Vec<u8> {
    let mut buf = String::with_capacity(256 * 1024);
    for frame in 0..24u32 {
        buf.push_str("\x1b[?2026h\x1b[H");
        for row in 1..=ROWS {
            let _ = write!(buf, "\x1b[{row};1H");
            if row == 1 || row == ROWS {
                let bg = 235 + ((frame as u8 + row as u8) % 6);
                let _ = write!(
                    buf,
                    "\x1b[1;38;5;255;48;5;{bg}m WALL TUI FRAME {frame:04} | ROW {row:02}/{ROWS:02} | MODE=NORMAL | 120x40 {:<51}\x1b[0m",
                    ""
                );
            } else {
                let gutter_fg = 240 + (row as u8 % 10);
                let token_r = ((frame * 29 + u32::from(row) * 17) & 0xff) as u8;
                let token_g = ((frame * 53 + u32::from(row) * 31) & 0xff) as u8;
                let token_b = ((frame * 11 + u32::from(row) * 43) & 0xff) as u8;
                let _ = write!(
                    buf,
                    "\x1b[38;5;{gutter_fg}m{:4} │\x1b[0m \x1b[1;35mpub fn\x1b[0m \
                     \x1b[38;2;{token_r};{token_g};{token_b}mstep_frame_{frame:02}_{row:02}\x1b[0m\
                     (ctx: &Context, dt: f32) {{ ctx.draw_cells(0..120); }}\x1b[K",
                    u32::from(row) + frame
                );
            }
        }
        buf.push_str("\x1b[?2026l");
    }
    buf.into_bytes()
}

/// Time `trials` short quantum-sized batches and take the minimum (unpreempted)
/// batch duration so OS runqueue preemption under heavy host load does not
/// masquerade as instruction cost.
fn bench_feed(label: &str, payload: &[u8], trials: usize) {
    let theme = Theme::default();
    let mut vt = Vt::new(COLS, ROWS, &theme);

    // Warmup.
    for _ in 0..2 {
        vt.feed(payload);
        black_box(&vt);
    }

    let mut best = Duration::MAX;
    for _ in 0..trials {
        let start = Instant::now();
        vt.feed(black_box(payload));
        let elapsed = start.elapsed();
        black_box(&vt);
        if elapsed < best {
            best = elapsed;
        }
    }

    let secs = best.as_secs_f64();
    let mb_per_sec = (payload.len() as f64 / 1_000_000.0) / secs;
    let mib_per_sec = (payload.len() as f64 / (1024.0 * 1024.0)) / secs;
    let ns_per_byte = best.as_nanos() as f64 / payload.len() as f64;

    println!(
        "  {label:<26} {:>8.1} MB/s  ({:>7.1} MiB/s, {:>5.2} ns/B, best of {trials} x {:.0} KiB)",
        mb_per_sec,
        mib_per_sec,
        ns_per_byte,
        payload.len() as f64 / 1024.0,
    );
}

fn populate_styled_screen(vt: &mut Vt, variant: u32) {
    let mut buf = String::with_capacity(32 * 1024);
    buf.push_str("\x1b[H");
    for row in 1..=ROWS {
        let fg = 16 + ((u32::from(row) * 7 + variant * 13) % 216);
        let bg = 232 + ((u32::from(row) + variant) % 12);
        let ch = char::from_u32(u32::from(b'a') + ((u32::from(row) + variant) % 26)).unwrap_or('a');
        let _ = write!(
            buf,
            "\x1b[{row};1H\x1b[1;38;5;{fg};48;5;{bg}m[{row:02}:{variant:04}]\x1b[0m "
        );
        for col in 0..108 {
            let c = if col % 12 == 0 { ch.to_ascii_uppercase() } else { ch };
            buf.push(c);
        }
    }
    vt.feed(buf.as_bytes());
}

fn print_fill_stat(label: &str, best_batch: Duration, batch_iters: u32, trials: u32) {
    let ns_per_call = best_batch.as_nanos() as f64 / f64::from(batch_iters);
    let us_per_call = ns_per_call / 1_000.0;
    println!(
        "  {label:<26} {:>8.2} us/call  ({:>8.0} ns/call, best of {trials} x {batch_iters})",
        us_per_call, ns_per_call
    );
}

fn bench_fill() {
    let theme = Theme::default();
    let mut vt = Vt::new(COLS, ROWS, &theme);
    let mut screen = Screen::new(COLS, ROWS);
    populate_styled_screen(&mut vt, 0);
    vt.fill(&mut screen);

    // 1. Unchanged 120x40 screen.
    let unchanged_batch: u32 = 10_000;
    let unchanged_trials: u32 = 20;
    for _ in 0..1_000 {
        black_box(vt.fill(black_box(&mut screen)));
    }
    let mut best_unchanged = Duration::MAX;
    for _ in 0..unchanged_trials {
        let start = Instant::now();
        for _ in 0..unchanged_batch {
            black_box(vt.fill(black_box(&mut screen)));
        }
        best_unchanged = best_unchanged.min(start.elapsed());
    }
    print_fill_stat(
        "fill/unchanged_120x40",
        best_unchanged,
        unchanged_batch,
        unchanged_trials,
    );

    // 2. One-row-changed 120x40 screen (time only `fill`, excluding `feed`).
    let one_row_batch: u32 = 200;
    let one_row_trials: u32 = 25;
    let row_a = b"\x1b[20;1H\x1b[1;38;2;240;180;80m[ROW-20-A]\x1b[0m updated single terminal line with fresh text AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let row_b = b"\x1b[20;1H\x1b[1;38;2;80;200;240m[ROW-20-B]\x1b[0m updated single terminal line with fresh text BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    for i in 0..200u32 {
        vt.feed(if i % 2 == 0 { row_a } else { row_b });
        black_box(vt.fill(&mut screen));
    }
    let mut best_one_row = Duration::MAX;
    for t in 0..one_row_trials {
        let mut batch_dur = Duration::ZERO;
        for i in 0..one_row_batch {
            let seq = t * one_row_batch + i;
            vt.feed(if seq.is_multiple_of(2) { row_a } else { row_b });
            let t0 = Instant::now();
            let changed = vt.fill(black_box(&mut screen));
            batch_dur += t0.elapsed();
            debug_assert!(changed);
        }
        best_one_row = best_one_row.min(batch_dur);
    }
    print_fill_stat(
        "fill/one_row_120x40",
        best_one_row,
        one_row_batch,
        one_row_trials,
    );

    // 3. Fully-changed 120x40 screen (alternate two pre-fed Vt states into `screen`).
    let mut vt_even = Vt::new(COLS, ROWS, &theme);
    let mut vt_odd = Vt::new(COLS, ROWS, &theme);
    populate_styled_screen(&mut vt_even, 1);
    populate_styled_screen(&mut vt_odd, 2);
    for i in 0..40u32 {
        if i % 2 == 0 {
            black_box(vt_even.fill(&mut screen));
        } else {
            black_box(vt_odd.fill(&mut screen));
        }
    }
    let full_batch: u32 = 40;
    let full_trials: u32 = 25;
    let mut best_full = Duration::MAX;
    for t in 0..full_trials {
        let start = Instant::now();
        for i in 0..full_batch {
            let seq = t * full_batch + i;
            let changed = if seq.is_multiple_of(2) {
                vt_even.fill(black_box(&mut screen))
            } else {
                vt_odd.fill(black_box(&mut screen))
            };
            black_box(changed);
        }
        best_full = best_full.min(start.elapsed());
    }
    print_fill_stat("fill/full_120x40", best_full, full_batch, full_trials);
}

fn main() {
    let plain = build_plain_stream();
    let sgr = build_sgr_heavy_stream();
    let redraw = build_fullscreen_redraw_stream();

    println!("glyph-vt benchmark ({COLS}x{ROWS} grid)");
    println!("feed() throughput:");
    bench_feed("feed/plain_ascii", &plain, 25);
    bench_feed("feed/sgr_heavy", &sgr, 25);
    bench_feed("feed/fullscreen_redraw", &redraw, 25);
    println!(
        "fill() latency ({COLS}x{ROWS} = {} cells):",
        u32::from(COLS) * u32::from(ROWS)
    );
    bench_fill();
}
