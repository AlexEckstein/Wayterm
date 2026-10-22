//! Parse throughput, VTE vs libghostty-vt, on one corpus (doc 02
//! §Benchmark). Built only with the `backend-ghostty` feature:
//!
//! ```sh
//! env -u WAYLAND_DISPLAY GDK_BACKEND=x11 xvfb-run -a \
//!     cargo bench --features backend-ghostty
//! ```
//!
//! Each corpus (plain text, SGR-colored text, cursor-addressed redraws,
//! multi-byte UTF-8) is fed in transport-sized chunks to an 80×24
//! terminal; the best of several runs counts. Neither side draws:
//! libghostty-vt is timed around `vt_write`; VTE is fed through
//! `VteBackend::write` and timed until a final OSC 2 title shows it has
//! processed everything, so its time includes the main-loop scheduling of
//! its input processing — what a tab sees. Without a display the VTE half
//! is skipped.
//!
//! Without `--bench` (`cargo test --benches`), a small corpus and one run
//! make it a smoke test.

use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use vte4::prelude::*;
use wayterm::terminal::ghostty::engine::Engine;
use wayterm::terminal::{TerminalBackend, VteBackend};

/// A typical transport read.
const CHUNK: usize = 16 * 1024;
/// The grid both backends parse into.
const GRID: (u16, u16) = (80, 24);

/// One named input stream.
struct Corpus {
    /// Shown in the results table.
    name: &'static str,
    /// The bytes fed to each backend.
    bytes: Vec<u8>,
}

/// A deterministic xorshift, so every run feeds the same bytes.
struct Rng(u64);

impl Rng {
    /// Next xorshift64 value.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Words the generated lines are made of.
const WORDS: [&str; 12] = [
    "connect",
    "session",
    "drwxr-xr-x",
    "root",
    "4096",
    "Oct",
    "15",
    "build.rs",
    "target",
    "warning:",
    "src/main.rs",
    "ok",
];

/// Lines of words until `size` bytes, each word wrapped by `decorate`.
fn lines(
    size: usize,
    rng: &mut Rng,
    mut decorate: impl FnMut(&mut Rng, &str) -> String,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 256);
    while out.len() < size {
        let mut width = 0;
        while width < 70 {
            let word = WORDS[rng.below(WORDS.len() as u64) as usize];
            out.extend_from_slice(decorate(rng, word).as_bytes());
            out.push(b' ');
            width += word.len() + 1;
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Every corpus, `size` bytes each, from a fixed seed.
fn corpora(size: usize) -> Vec<Corpus> {
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    let plain = lines(size, &mut rng, |_, word| word.to_string());
    let sgr = lines(size, &mut rng, |rng, word| match rng.below(4) {
        0 => format!("\x1b[1;31m{word}\x1b[0m"),
        1 => format!("\x1b[38;5;{}m{word}\x1b[39m", rng.below(256)),
        2 => format!(
            "\x1b[38;2;{};{};{}m\x1b[48;5;{}m{word}\x1b[0m",
            rng.below(256),
            rng.below(256),
            rng.below(256),
            rng.below(256)
        ),
        _ => word.to_string(),
    });
    let mut cursor = Vec::with_capacity(size + 4096);
    while cursor.len() < size {
        // One `top`-style frame: home, then every row rewritten in place.
        cursor.extend_from_slice(b"\x1b[H");
        for row in 1..=GRID.1 {
            let pid = rng.below(99_999);
            let cpu = rng.below(1000);
            cursor.extend_from_slice(
                format!(
                    "\x1b[{row};1H\x1b[K{}{pid:>7} root  20 0 {cpu:>3}.{} S  wayterm\x1b[0m",
                    if row == 1 { "\x1b[7m" } else { "" },
                    cpu % 10
                )
                .as_bytes(),
            );
        }
    }
    let utf8 = lines(size, &mut rng, |rng, word| match rng.below(4) {
        0 => "漢字かな交じり".to_string(),
        1 => "héllo wörld".to_string(),
        2 => "🙂👍".to_string(),
        _ => word.to_string(),
    });
    vec![
        Corpus {
            name: "plain",
            bytes: plain,
        },
        Corpus {
            name: "sgr",
            bytes: sgr,
        },
        Corpus {
            name: "cursor",
            bytes: cursor,
        },
        Corpus {
            name: "utf8",
            bytes: utf8,
        },
    ]
}

/// One libghostty-vt run over `corpus`.
fn ghostty_once(corpus: &Corpus) -> Duration {
    let engine = Engine::new(GRID.0, GRID.1).expect("libghostty-vt terminal");
    let start = Instant::now();
    for chunk in corpus.bytes.chunks(CHUNK) {
        engine.write(chunk);
    }
    start.elapsed()
}

/// A presented VTE terminal, or `None` without a display.
fn vte_setup() -> Option<(
    gtk4::Window,
    VteBackend,
    vte4::Terminal,
    mpsc::Receiver<wayterm::terminal::TerminalEvent>,
)> {
    gtk4::init().ok()?;
    let (tx, rx) = mpsc::channel(1024);
    let backend = VteBackend::new(tx);
    let terminal = backend.widget().downcast::<vte4::Terminal>().ok()?;
    backend.set_size(GRID.1.into(), GRID.0.into());
    let window = gtk4::Window::new();
    window.set_child(Some(&terminal));
    window.present();
    Some((window, backend, terminal, rx))
}

/// One VTE run over `corpus`, until its closing title shows it parsed everything.
fn vte_once(
    backend: &VteBackend,
    terminal: &vte4::Terminal,
    corpus: &Corpus,
    run: usize,
) -> Duration {
    let context = glib::MainContext::default();
    backend.reset();
    while context.iteration(false) {}
    let marker = format!("wayterm-bench-{}-{run}", corpus.name);
    let start = Instant::now();
    for chunk in corpus.bytes.chunks(CHUNK) {
        backend.write(chunk);
    }
    backend.write(format!("\x1b]2;{marker}\x07").as_bytes());
    while terminal.window_title().as_deref() != Some(marker.as_str()) {
        context.iteration(true);
    }
    start.elapsed()
}

/// The fastest of `runs` runs.
fn best(runs: usize, mut once: impl FnMut(usize) -> Duration) -> Duration {
    (0..runs).map(&mut once).min().unwrap_or_default()
}

/// Throughput in MiB/s.
fn mib_per_s(bytes: usize, time: Duration) -> f64 {
    bytes as f64 / (1024.0 * 1024.0) / time.as_secs_f64().max(f64::EPSILON)
}

fn main() {
    let bench = std::env::args().any(|arg| arg == "--bench");
    let (size, runs) = if bench { (8 << 20, 5) } else { (64 << 10, 1) };
    let corpora = corpora(size);
    let vte = vte_setup();
    if vte.is_none() {
        println!("no display: VTE skipped (run under xvfb-run -a)");
    }

    println!(
        "{:<8} {:>9} {:>14} {:>14} {:>8}",
        "corpus", "MiB", "ghostty MiB/s", "VTE MiB/s", "ratio"
    );
    for corpus in &corpora {
        let ghostty = best(runs, |_| ghostty_once(corpus));
        let mib = corpus.bytes.len() as f64 / (1024.0 * 1024.0);
        let ghostty_rate = mib_per_s(corpus.bytes.len(), ghostty);
        match &vte {
            Some((_, backend, terminal, _)) => {
                let vte = best(runs, |run| vte_once(backend, terminal, corpus, run));
                let vte_rate = mib_per_s(corpus.bytes.len(), vte);
                println!(
                    "{:<8} {mib:>9.2} {ghostty_rate:>14.1} {vte_rate:>14.1} {:>7.1}x",
                    corpus.name,
                    ghostty_rate / vte_rate
                );
            }
            None => println!(
                "{:<8} {mib:>9.2} {ghostty_rate:>14.1} {:>14} {:>8}",
                corpus.name, "-", "-"
            ),
        }
    }
    if let Some((window, ..)) = vte {
        window.close();
    }
}
