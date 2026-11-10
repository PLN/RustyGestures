//! Command line for the M0 spike.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rustygestures_core::{
    load_gesture_file, save_gesture_file, save_stroke_log, GestureName, Point, RawPoint, RawStroke,
    StrokeLog, GESTURE_SCHEMA,
};
use rustygestures_ui::is_implemented;
use rustygestures_win::{run_session, Control};

#[derive(Debug, Parser)]
#[command(
    name = "rustygestures",
    version,
    about = "RustyGestures mouse-gesture engine (M0 spike)"
)]
struct Cli {
    /// Gesture template file.
    #[arg(long, global = true, default_value = "gestures.json")]
    gestures: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Install the hook, draw the trail, and print recognized gestures.
    Run,
    /// Record samples for one gesture and save them.
    Train {
        name: String,
        /// How many strokes to record.
        #[arg(long, default_value_t = 3)]
        count: usize,
    },
    /// Dump raw strokes to a JSON file until Ctrl+C.
    Record {
        #[arg(long, default_value = "strokes.json")]
        output: PathBuf,
    },
    /// List gesture names and sample counts.
    List,
    /// Run the hook and print the callback latency histogram on exit.
    Latency,
}

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rustygestures: {err}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    // Referenced so the placeholder crate stays in the build.
    let _ui = is_implemented();
    match cli.command {
        Commands::List => {
            list(&cli.gestures)?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Run => run_recognize(&cli.gestures),
        Commands::Train { name, count } => train(&cli.gestures, &name, count),
        Commands::Record { output } => record(&output),
        Commands::Latency => run_recognize(&cli.gestures),
    }
}

fn list(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = load_gesture_file(path)?;
    if file.gestures.is_empty() {
        println!("no gestures");
        return Ok(());
    }
    for gesture in &file.gestures {
        println!("{}  samples={}", gesture.name, gesture.samples.len());
    }
    Ok(())
}

fn run_recognize(path: &std::path::Path) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let set = load_gesture_file(path)?.to_set();
    println!(
        "hook installed (right button, 10 px, 500 ms cancel). Ctrl+C to exit. templates={}",
        set.len()
    );
    let stats = run_session(|stroke| {
        match set.recognize(stroke.as_slice()) {
            Some(found) => {
                println!(
                    "{}  score={:.3} margin={:.3}",
                    found.name, found.score, found.margin
                );
            }
            None => println!("(no match)"),
        }
        Control::Continue
    })?;
    println!("{}", stats.latency);
    if stats.dropped_strokes > 0 {
        println!("dropped strokes={}", stats.dropped_strokes);
    }
    Ok(ExitCode::SUCCESS)
}

fn train(
    path: &std::path::Path,
    name: &str,
    count: usize,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let gesture_name = GestureName::parse(name)?;
    if count == 0 {
        return Err("train count must be at least 1".into());
    }
    let mut file = load_gesture_file(path)?;
    println!(
        "draw {count} stroke(s) for '{}' with the right button. Ctrl+C cancels.",
        gesture_name.as_str()
    );
    let mut kept = 0usize;
    let stats = run_session(|stroke| {
        kept += 1;
        file.add_sample(gesture_name.as_str(), stroke.as_slice());
        println!("saved sample {kept}/{count} ({} points)", stroke.len);
        if kept >= count {
            Control::Stop
        } else {
            Control::Continue
        }
    })?;
    if kept == 0 {
        println!("{}", stats.latency);
        return Err("no strokes recorded".into());
    }
    save_gesture_file(path, &file)?;
    println!("wrote {} ({kept} new sample(s))", path.display());
    println!("{}", stats.latency);
    Ok(ExitCode::SUCCESS)
}

fn record(output: &std::path::Path) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut strokes = Vec::new();
    println!("recording strokes to {}. Ctrl+C to stop.", output.display());
    let stats = run_session(|stroke| {
        strokes.push(points_of(stroke.as_slice()));
        println!("recorded stroke {} ({} points)", strokes.len(), stroke.len);
        Control::Continue
    })?;
    let log = StrokeLog {
        schema: GESTURE_SCHEMA,
        strokes: strokes
            .into_iter()
            .map(|points| RawStroke { points })
            .collect(),
    };
    save_stroke_log(output, &log)?;
    println!("wrote {}", output.display());
    println!("{}", stats.latency);
    Ok(ExitCode::SUCCESS)
}

fn points_of(points: &[Point]) -> Vec<RawPoint> {
    points
        .iter()
        .map(|point| RawPoint {
            x: point.x,
            y: point.y,
        })
        .collect()
}
