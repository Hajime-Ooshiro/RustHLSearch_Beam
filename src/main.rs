mod bitmask;
mod output;
mod primes;
mod rocksdb_output;
mod search;

use clap::Parser;
use log::{info, LevelFilter};
use output::dated_output_path;
use primes::generate_primes;
use rocksdb_output::ShiftPathStore;
use search::{build_shift_table, SearchMode, SearchResult, State, TARGET_SCORE};
use simple_logger::SimpleLogger;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

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

    #[arg(
        long,
        default_value_t = 500,
        help = "ビーム幅 (beam モードで使用。候補全体を保持するには usize::MAX を指定)"
    )]
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
        "設定: mode={:?} depth={} target={} max_depth={} primes=all",
        cli.mode, cli.depth, TARGET_SCORE, cli.max_depth
    );

    let start_time = Instant::now();
    let shift_table = build_shift_table(&primes[..cli.depth], cli.cols);
    let mut state = State::new(primes, cli.cols, shift_table);
    state.max_depth = cli.max_depth;

    let database_path = cli.output.join("hlsearch.rocksdb");
    std::fs::create_dir_all(&cli.output)?;
    let config_hash = {
        let config = format!(
            "mode={:?};depth={};cols={};beam_width={};target={}",
            cli.mode, cli.depth, cli.cols, cli.beam_width, TARGET_SCORE
        );
        config.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
    };
    let store = ShiftPathStore::create(&database_path, cli.depth, true, config_hash)
        .map_err(std::io::Error::other)?;
    store
        .set_search_complete(false)
        .map_err(std::io::Error::other)?;

    let existing_records = store
        .load_top_scored_paths(cli.depth, 100)
        .map_err(std::io::Error::other)?;
    if !existing_records.is_empty() {
        info!(
            "深さ{}の既存レコード (スコア上位{}件):",
            cli.depth,
            existing_records.len()
        );
        for (index, (score, path)) in existing_records.iter().enumerate() {
            info!("  {}: score={} path={:?}", index + 1, score, path);
        }
    }

    let previous_depth = cli.depth - 1;
    let mut resumed_result = SearchResult::default();
    let mut resumed_candidates = 0usize;

    if previous_depth > 0 {
        match cli.mode {
            SearchMode::Beam => {
                let paths = store
                    .load_top_scored_paths(previous_depth, cli.beam_width)
                    .map_err(std::io::Error::other)?
                    .into_iter()
                    .map(|(_, path)| path)
                    .collect::<Vec<_>>();
                resumed_candidates = paths.len();
                if !paths.is_empty() {
                    resumed_result = state
                        .search_last_level_from_paths(cli.depth, &paths, |path, score| {
                            store.append(score, path)
                        })
                        .map_err(std::io::Error::other)?;
                }
            }
            SearchMode::Sequential | SearchMode::Parallel => {
                let mut after_node_id = None;
                loop {
                    let batch = store
                        .load_full_depth_paths_batch(previous_depth, after_node_id, 500)
                        .map_err(std::io::Error::other)?;
                    if batch.is_empty() {
                        break;
                    }
                    resumed_candidates += batch.len();
                    after_node_id = batch.last().map(|(node_id, _)| *node_id);
                    let paths = batch.into_iter().map(|(_, path)| path).collect::<Vec<_>>();
                    let result = if cli.mode == SearchMode::Parallel {
                        state.search_last_level_from_paths_parallel(
                            cli.depth,
                            &paths,
                            |path, score| store.append(score, path),
                        )
                    } else {
                        state.search_last_level_from_paths(cli.depth, &paths, |path, score| {
                            store.append(score, path)
                        })
                    }
                    .map_err(std::io::Error::other)?;
                    resumed_result.merge(result);
                }
            }
        }
    }

    if resumed_candidates > 0 {
        info!(
            "深さ{}の保存済み候補{}件から探索を再開",
            previous_depth, resumed_candidates
        );
        state.max_score = resumed_result.max_score;
        state.shifts = resumed_result.shifts;
    } else {
        match cli.mode {
            SearchMode::Sequential => state
                .search_recording(cli.depth, |path, score| store.append(score, path))
                .map_err(std::io::Error::other)?,
            SearchMode::Parallel => {
                let result = state
                    .search_parallel_recording(cli.depth, |path, score| store.append(score, path))
                    .map_err(std::io::Error::other)?;
                state.max_score = result.max_score;
                state.shifts = result.shifts;
            }
            SearchMode::Beam => state
                .search_beam_recording(cli.depth, cli.beam_width, |path, score| {
                    store.append(score, path)
                })
                .map_err(std::io::Error::other)?,
        }
    }

    store
        .flush()
        .and_then(|()| store.set_search_complete(true))
        .map_err(std::io::Error::other)?;

    let elapsed = start_time.elapsed();
    info!("探索時間: {:?}", elapsed);
    info!("最大値: {}", state.max_score);
    info!("RocksDB 出力: {}", database_path.display());

    let result_path = dated_output_path(&cli.output, "result", cli.depth, "txt");
    let result_file = File::create(&result_path)?;
    let mut result_writer = BufWriter::new(result_file);
    writeln!(
        result_writer,
        "mode: {}",
        match cli.mode {
            SearchMode::Sequential => "sequential",
            SearchMode::Parallel => "parallel",
            SearchMode::Beam => "beam",
        }
    )?;
    writeln!(result_writer, "depth: {}", cli.depth)?;
    writeln!(result_writer, "max_depth: {}", cli.max_depth)?;
    writeln!(result_writer, "cols: {}", cli.cols)?;
    writeln!(result_writer, "beam_width: {}", cli.beam_width)?;
    writeln!(result_writer, "target: {TARGET_SCORE}")?;
    writeln!(result_writer, "elapsed: {elapsed:?}")?;
    writeln!(result_writer, "max_count: {}", state.max_score)?;
    writeln!(result_writer, "shift_paths:")?;
    for shifts in &state.shifts {
        writeln!(result_writer, "{shifts:?}")?;
    }
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
        assert_eq!(cli.validate(3), Err("depth must be at least 1".to_string()));
        cli = test_cli();
        cli.cols = 0;
        assert_eq!(cli.validate(3), Err("cols must be at least 1".to_string()));
        cli = test_cli();
        cli.depth = 4;
        assert_eq!(
            cli.validate(3),
            Err("depth (4) cannot exceed available primes (3)".to_string())
        );
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
        assert_eq!(
            depth_exceeds_max_depth.validate(10),
            Err("depth (5) cannot exceed max_depth (3)".to_string())
        );

        let empty_primes_cli = test_cli();
        assert_eq!(
            empty_primes_cli.validate(0),
            Err("depth (1) cannot exceed available primes (0)".to_string())
        );
    }

    #[test]
    fn cli_rejects_removed_target_option() {
        assert!(Cli::try_parse_from(["hlsearch", "--target", "0"]).is_err());
    }
}
