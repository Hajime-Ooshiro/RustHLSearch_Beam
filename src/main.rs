mod bitmask;
mod output;
mod primes;
mod search;

use clap::Parser;
use log::{info, LevelFilter};
use output::dated_output_path;
use primes::generate_primes;
use search::{build_shift_table, SearchMode, State};
use serde::Serialize;
use simple_logger::SimpleLogger;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Serialize)]
struct OutputFile<'a> {
    config: OutputConfig<'a>,
    result: OutputResult,
}

#[derive(Serialize)]
struct OutputConfig<'a> {
    mode: &'a str,
    depth: usize,
    max_depth: usize,
    cols: usize,
    beam_width: usize,
    elapsed: String,
}

#[derive(Serialize)]
struct OutputResult {
    max_count: usize,
}

#[derive(Parser, Debug)]
#[command(author, version, about = "HLSearch: 素数シフト探索プログラム (Rust版)", long_about = None)]
pub struct Cli {
    #[arg(short, long, default_value_t = 8, help = "探索する階層数")]
    pub depth: usize,

    #[arg(
        short,
        long,
        value_enum,
        default_value_t = SearchMode::Beam,
        help = "探索モード (sequential | parallel | beam)"
    )]
    pub mode: SearchMode,

    #[arg(long, default_value_t = 32, help = "ビーム幅 (beam モードで使用)")]
    pub beam_width: usize,

    #[arg(long, default_value_t = 249, help = "打ち切り判定用 max-depth")]
    pub max_depth: usize,

    #[arg(long, default_value_t = 3159, help = "列数 (長さ)")]
    pub cols: usize,

    #[arg(short, long, default_value = ".", help = "出力ディレクトリ")]
    pub output: PathBuf,
}

impl Cli {
    fn validate(&self, available_primes: usize) -> Result<(), String> {
        if self.depth == 0 {
            return Err("depth must be at least 1".to_string());
        }
        if self.cols == 0 {
            return Err("cols must be at least 1".to_string());
        }
        if self.beam_width == 0 {
            return Err("beam_width must be at least 1".to_string());
        }
        if self.max_depth == 0 {
            return Err("max_depth must be at least 1".to_string());
        }
        if self.depth > self.max_depth {
            return Err(format!(
                "depth ({}) cannot exceed max_depth ({})",
                self.depth, self.max_depth
            ));
        }
        if self.depth > available_primes {
            return Err(format!(
                "depth ({}) cannot exceed available primes ({})",
                self.depth, available_primes
            ));
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new().with_level(LevelFilter::Info).init()?;
    let cli = Cli::parse();
    let all_primes = generate_primes(1579);

    if let Err(message) = cli.validate(all_primes.len()) {
        eprintln!("エラー: {}", message);
        std::process::exit(1);
    }

    let primes = all_primes;

    info!("HLSearch (Rust) 開始");
    info!(
        "設定: mode={:?} depth={} max_depth={} primes=all",
        cli.mode, cli.depth, cli.max_depth
    );

    let start_time = Instant::now();
    let shift_table = build_shift_table(&primes[..cli.depth], cli.cols);
    let mut state = State::new(primes, cli.cols, shift_table);
    state.max_depth = cli.max_depth;

    match cli.mode {
        SearchMode::Sequential => state.search(cli.depth),
        SearchMode::Parallel => {
            let result = state.search_parallel(cli.depth);
            state.max_count = result.max_count;
            state.shifts = result.shifts;
        }
        SearchMode::Beam => state.beam_search(cli.depth, cli.beam_width),
    }

    let elapsed = start_time.elapsed();
    info!("探索時間: {:?}", elapsed);
    info!("最大値: {}", state.max_count);

    std::fs::create_dir_all(&cli.output)?;
    let shift_path = dated_output_path(&cli.output, "shift_path", cli.depth, "txt");
    let result_path = dated_output_path(&cli.output, "result", cli.depth, "json");

    let shift_file = File::create(&shift_path)?;
    let mut shift_writer = BufWriter::new(shift_file);
    for shifts in &state.shifts {
        writeln!(shift_writer, "{shifts:?}")?;
    }
    info!("シフトパス出力ファイル: {}", shift_path.display());

    let output = OutputFile {
        config: OutputConfig {
            mode: match cli.mode {
                SearchMode::Sequential => "sequential",
                SearchMode::Parallel => "parallel",
                SearchMode::Beam => "beam",
            },
            depth: cli.depth,
            max_depth: cli.max_depth,
            cols: cli.cols,
            beam_width: cli.beam_width,
            elapsed: format!("{elapsed:?}"),
        },
        result: OutputResult {
            max_count: state.max_count,
        },
    };
    let result_file = File::create(&result_path)?;
    let mut result_writer = BufWriter::new(result_file);
    serde_json::to_writer_pretty(&mut result_writer, &output)?;
    writeln!(result_writer)?;
    info!("探索結果出力ファイル: {}", result_path.display());

    info!("HLSearch 終了");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cli() -> Cli {
        Cli {
            depth: 1,
            mode: SearchMode::Sequential,
            beam_width: 32,
            max_depth: 249,
            cols: 4,
            output: PathBuf::from("."),
        }
    }

    #[test]
    fn cli_validation_accepts_valid_configuration() {
        assert!(test_cli().validate(3).is_ok());
    }

    #[test]
    fn cli_validation_rejects_invalid_configuration() {
        let mut cli = test_cli();
        cli.depth = 0;
        assert!(cli.validate(3).is_err());
        cli = test_cli();
        cli.cols = 0;
        assert!(cli.validate(3).is_err());
        cli = test_cli();
        cli.depth = 4;
        assert!(cli.validate(3).is_err());
    }

    #[test]
    fn cli_validation_rejects_unexpected_zero_or_empty_inputs() {
        let mut cli = test_cli();
        cli.beam_width = 0;
        assert!(cli.validate(3).is_err());

        let mut beam_cli = test_cli();
        beam_cli.max_depth = 0;
        assert!(beam_cli.validate(3).is_err());

        let zero_depth_cli = Cli {
            depth: 0,
            mode: SearchMode::Parallel,
            beam_width: 1,
            max_depth: 2,
            cols: 1,
            output: PathBuf::from("."),
        };
        assert!(zero_depth_cli.validate(0).is_err());

        let depth_exceeds_max_depth = Cli {
            depth: 5,
            mode: SearchMode::Beam,
            beam_width: 1,
            max_depth: 3,
            cols: 1,
            output: PathBuf::from("."),
        };
        assert!(depth_exceeds_max_depth.validate(10).is_err());

        let empty_primes_cli = test_cli();
        assert!(empty_primes_cli.validate(0).is_err());
    }
}
