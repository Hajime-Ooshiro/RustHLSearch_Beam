use crate::bitmask::BitMask;
use indicatif::{ProgressBar, ProgressStyle};
use log::debug;
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const BEAM_CANDIDATE_BATCH_SIZE: usize = 500;
pub const TARGET_SCORE: usize = 447;

/// 基底行の生成と補集合シフトテーブルの作成
pub fn build_shift_table(primes: &[usize], cols: usize) -> Vec<Vec<BitMask>> {
    let mut shift_table = Vec::with_capacity(primes.len());

    for &p in primes {
        let mut complement_shifts = Vec::with_capacity(p);
        for k in 0..p {
            let mut mask = BitMask::new_ones(cols);
            for col in 0..cols {
                let idx = col + 1;
                if col >= k {
                    let orig_idx = idx - k;
                    if orig_idx % p == 1 {
                        mask.set(col, false);
                    }
                }
            }
            complement_shifts.push(mask);
        }
        shift_table.push(complement_shifts);
    }

    shift_table
}

#[derive(Clone)]
struct Frame {
    level: usize,
    base_mask: BitMask,
    next_idx: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SearchMode {
    Sequential,
    Parallel,
    Beam,
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub max_score: usize,
    pub shifts: Vec<Vec<usize>>,
}

impl SearchResult {
    pub fn record(&mut self, count: usize, key: Vec<usize>) {
        match count.cmp(&self.max_score) {
            std::cmp::Ordering::Greater => {
                self.max_score = count;
                self.shifts.clear();
                self.shifts.push(key);
            }
            std::cmp::Ordering::Equal => self.shifts.push(key),
            std::cmp::Ordering::Less => {}
        }
    }

    pub fn merge(&mut self, other: SearchResult) {
        if other.max_score > self.max_score {
            self.max_score = other.max_score;
            self.shifts = other.shifts;
            return;
        }

        if other.max_score == self.max_score {
            self.shifts.extend(other.shifts);
        }
    }
}

#[derive(Clone)]
struct BeamState {
    key: Vec<usize>,
    mask: BitMask,
}

pub struct State {
    pub primes: Vec<usize>,
    pub max_depth: usize,
    pub key: Vec<usize>,
    pub zero_mask: BitMask,
    pub max_score: usize,
    target: usize,
    pub shifts: Vec<Vec<usize>>,
    pub node_count: u64,
    pub beam_width: usize,
    shift_table: Vec<Vec<BitMask>>,
}

#[allow(dead_code)]
impl State {
    pub fn new(primes: Vec<usize>, cols: usize, shift_table: Vec<Vec<BitMask>>) -> Self {
        State {
            primes,
            max_depth: 249,
            key: Vec::new(),
            zero_mask: BitMask::new_ones(cols),
            max_score: 0,
            target: TARGET_SCORE,
            shifts: Vec::new(),
            node_count: 0,
            beam_width: usize::MAX,
            shift_table,
        }
    }

    pub fn search_beam(&mut self, depth: usize, beam_width: usize) {
        self.search_beam_recording(depth, beam_width, |_, _| Ok(()))
            .expect("no-op recorder cannot fail");
    }

    pub fn search_beam_recording<F>(
        &mut self,
        depth: usize,
        beam_width: usize,
        record: F,
    ) -> Result<(), String>
    where
        F: Fn(&[usize], usize) -> Result<(), String>,
    {
        let width = beam_width.max(1);
        self.beam_width = width;
        let pb = progress_bar();
        let mut beam = vec![BeamState {
            key: Vec::new(),
            mask: self.zero_mask.clone(),
        }];

        self.key.clear();
        let mut result = SearchResult::default();

        for level in 0..depth {
            let mut next_beam = Vec::new();
            let mut candidate_batch = Vec::with_capacity(BEAM_CANDIDATE_BATCH_SIZE);
            for candidate in &beam {
                let prime = self.primes[level];
                for i in (0..prime).rev() {
                    let mut key = candidate.key.clone();
                    key.push(i);
                    let node_mask = candidate.mask.bitand(&self.shift_table[level][i]);
                    let count = node_mask.count_ones();

                    // if count + (depth - level) < result.max_score {
                    //     continue;
                    // }
                    // if count < result.max_score {
                    //     continue;
                    // }
                    if count < self.target {
                        continue;
                    }

                    if let Err(error) = record(&key, count) {
                        pb.finish_with_message("探索失敗");
                        return Err(error);
                    }

                    if level + 1 >= depth {
                        result.record(count, key.clone());
                        if count >= result.max_score {
                            debug!("best level={} key={:?} count={}", level + 1, key, count);
                        }
                    }

                    candidate_batch.push(BeamState {
                        key,
                        mask: node_mask,
                    });
                    if candidate_batch.len() == BEAM_CANDIDATE_BATCH_SIZE {
                        retain_best_candidates(
                            &mut next_beam,
                            &mut candidate_batch,
                            beam_width,
                            width,
                        );
                    }
                }
            }

            if !candidate_batch.is_empty() {
                retain_best_candidates(&mut next_beam, &mut candidate_batch, beam_width, width);
            }

            if next_beam.is_empty() {
                break;
            }

            beam = next_beam;
        }

        self.max_score = result.max_score;
        self.shifts = result.shifts;
        pb.finish_with_message("探索完了");
        Ok(())
    }

    pub fn beam_search(&mut self, depth: usize, beam_width: usize) {
        self.search_beam(depth, beam_width);
    }

    pub fn search(&mut self, depth: usize) {
        self.search_recording(depth, |_, _| Ok(()))
            .expect("no-op recorder cannot fail");
    }

    pub fn search_recording<F>(&mut self, depth: usize, record: F) -> Result<(), String>
    where
        F: Fn(&[usize], usize) -> Result<(), String>,
    {
        let pb = progress_bar();
        let mut stack = vec![Frame {
            level: 0,
            base_mask: self.zero_mask.clone(),
            next_idx: self.primes[0],
        }];
        let mut result = SearchResult::default();

        while let Some(frame) = stack.last_mut() {
            if frame.next_idx == 0 {
                stack.pop();
                if let Some(parent) = stack.last() {
                    self.key.pop();
                    self.zero_mask = parent.base_mask.clone();
                }
                continue;
            }

            frame.next_idx -= 1;
            let i = frame.next_idx;
            let level = frame.level;
            let base_mask = frame.base_mask.clone();
            self.key.push(i);
            self.node_count += 1;

            let node_mask = base_mask.bitand(&self.shift_table[level][i]);
            let count = node_mask.count_ones();

            if self.node_count.is_multiple_of(10_000) {
                pb.set_position(self.node_count);
                pb.set_message(format!(
                    "best: {} | depth: {}",
                    result.max_score,
                    self.key.len()
                ));
            }

            // if count + (depth - level) < result.max_score {
            //     self.key.pop();
            //     continue;
            // }

            // if count < result.max_score {
            //     self.key.pop();
            //     continue;
            // }

            if count < self.target {
                self.key.pop();
                continue;
            }

            if let Err(error) = record(&self.key, count) {
                self.key.pop();
                pb.finish_with_message("探索失敗");
                return Err(error);
            }

            if level + 1 >= depth {
                result.record(count, self.key.clone());
                debug!(
                    "best level={} key={:?} count={}",
                    level + 1,
                    self.key,
                    count
                );
                self.key.pop();
                continue;
            }

            self.zero_mask = node_mask.clone();
            stack.push(Frame {
                level: level + 1,
                base_mask: node_mask,
                next_idx: self.primes[level + 1],
            });
        }

        self.max_score = result.max_score;
        self.shifts = result.shifts;
        pb.finish_with_message("探索完了");
        Ok(())
    }

    pub fn search_last_level_from_paths<F>(
        &self,
        depth: usize,
        paths: &[Vec<usize>],
        record: F,
    ) -> Result<SearchResult, String>
    where
        F: Fn(&[usize], usize) -> Result<(), String>,
    {
        if depth < 2 {
            return Err("depth must be at least 2 to search from previous-level paths".to_string());
        }

        let mut result = SearchResult::default();
        for path in paths {
            result.merge(extend_saved_path(self, depth, path, &record)?);
        }

        Ok(result)
    }

    pub fn search_last_level_from_paths_parallel<F>(
        &self,
        depth: usize,
        paths: &[Vec<usize>],
        record: F,
    ) -> Result<SearchResult, String>
    where
        F: Fn(&[usize], usize) -> Result<(), String> + Sync,
    {
        if depth < 2 {
            return Err("depth must be at least 2 to search from previous-level paths".to_string());
        }

        let result: Result<SearchResult, String> = paths
            .par_iter()
            .try_fold(SearchResult::default, |mut result, path| {
                result.merge(extend_saved_path(self, depth, path, &record)?);
                Ok(result)
            })
            .try_reduce(SearchResult::default, |mut left, right| {
                left.merge(right);
                Ok(left)
            });
        result
    }

    pub fn search_parallel(&self, depth: usize) -> SearchResult {
        self.search_parallel_recording(depth, |_, _| Ok(()))
            .expect("no-op recorder cannot fail")
    }

    pub fn search_parallel_recording<F>(
        &self,
        depth: usize,
        record: F,
    ) -> Result<SearchResult, String>
    where
        F: Fn(&[usize], usize) -> Result<(), String> + Sync,
    {
        let max_score = Arc::new(AtomicUsize::new(0));
        let shifts = Arc::new(Mutex::new(Vec::<Vec<usize>>::new()));
        let node_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pb = progress_bar();

        let p0 = self.primes[0];
        let search_result: Result<(), String> = (0..p0).into_par_iter().rev().try_for_each(|i| {
            let mut key = vec![i];
            let base_mask = self.zero_mask.bitand(&self.shift_table[0][i]);
            let count = base_mask.count_ones();
            if depth == 1 {
                if count >= self.target {
                    record(&key, count)?;
                    let previous_max = max_score.load(Ordering::Relaxed);
                    if count > previous_max {
                        max_score.store(count, Ordering::Relaxed);
                        let mut found_shifts = shifts.lock().unwrap();
                        found_shifts.clear();
                        found_shifts.push(key.clone());
                    } else if count == previous_max {
                        shifts.lock().unwrap().push(key.clone());
                    }
                }
                return Ok(());
            }
            if count < self.target {
                return Ok(());
            }
            record(&key, count)?;

            let mut stack = vec![Frame {
                level: 1,
                base_mask,
                next_idx: self.primes[1],
            }];

            while let Some(frame) = stack.last_mut() {
                if frame.next_idx == 0 {
                    stack.pop();
                    if stack.last().is_some() {
                        key.pop();
                    }
                    continue;
                }

                frame.next_idx -= 1;
                let idx = frame.next_idx;
                let level = frame.level;
                let current_base = frame.base_mask.clone();
                key.push(idx);
                let n = node_count.fetch_add(1, Ordering::Relaxed) + 1;
                let node_mask = current_base.bitand(&self.shift_table[level][idx]);
                let c_count = node_mask.count_ones();

                if n.is_multiple_of(10_000) {
                    pb.set_position(n);
                    pb.set_message(format!(
                        "best: {} | depth: {}",
                        max_score.load(Ordering::Relaxed),
                        key.len()
                    ));
                }

                // if c_count + (depth - level) < max_score.load(Ordering::Relaxed) {
                //     key.pop();
                //     continue;
                // }

                // if c_count < max_score.load(Ordering::Relaxed) {
                //     key.pop();
                //     continue;
                // }
                if c_count < self.target {
                    key.pop();
                    continue;
                }

                record(&key, c_count)?;

                if level + 1 >= depth {
                    let previous_max = max_score.load(Ordering::Relaxed);
                    if c_count > previous_max {
                        max_score.store(c_count, Ordering::Relaxed);
                        let mut found_shifts = shifts.lock().unwrap();
                        found_shifts.clear();
                        found_shifts.push(key.clone());
                        debug!("best level={} key={:?} count={}", level + 1, key, c_count);
                    } else if c_count == previous_max {
                        shifts.lock().unwrap().push(key.clone());
                        // info!("best level={} key={:?} count={}", level+1, key, c_count);
                    }
                    key.pop();
                    continue;
                }

                stack.push(Frame {
                    level: level + 1,
                    base_mask: node_mask,
                    next_idx: self.primes[level + 1],
                });
            }
            Ok(())
        });

        pb.finish_with_message(if search_result.is_ok() {
            "探索完了"
        } else {
            "探索失敗"
        });
        search_result?;
        let final_shifts = shifts.lock().unwrap();
        let mut result = SearchResult::default();
        result.merge(SearchResult {
            max_score: max_score.load(Ordering::Relaxed),
            shifts: final_shifts.clone(),
        });
        Ok(result)
    }
}

fn extend_saved_path<F>(
    state: &State,
    depth: usize,
    path: &[usize],
    record: &F,
) -> Result<SearchResult, String>
where
    F: Fn(&[usize], usize) -> Result<(), String>,
{
    let level = depth - 1;
    if path.len() != level {
        return Err(format!(
            "保存済みパスの長さ {} が期待値 {} と一致しません",
            path.len(),
            level
        ));
    }

    let mut base_mask = state.zero_mask.clone();
    for (path_level, &shift) in path.iter().enumerate() {
        if shift >= state.primes[path_level] {
            return Err(format!(
                "深さ {} のシフト値 {} が範囲外です",
                path_level + 1,
                shift
            ));
        }
        base_mask = base_mask.bitand(&state.shift_table[path_level][shift]);
    }

    let mut result = SearchResult::default();
    if base_mask.count_ones() < state.target {
        return Ok(result);
    }

    let mut key = path.to_vec();
    for shift in (0..state.primes[level]).rev() {
        key.push(shift);
        let count = base_mask
            .bitand(&state.shift_table[level][shift])
            .count_ones();
        if count >= state.target {
            record(&key, count)?;
            result.record(count, key.clone());
        }
        key.pop();
    }
    Ok(result)
}

fn retain_best_candidates(
    retained: &mut Vec<BeamState>,
    batch: &mut Vec<BeamState>,
    beam_width: usize,
    width: usize,
) {
    retained.append(batch);
    retained.sort_by(|lhs, rhs| rhs.mask.count_ones().cmp(&lhs.mask.count_ones()));
    if beam_width != usize::MAX {
        retained.truncate(width);
    }
}

fn progress_bar() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} [{elapsed_precise}] nodes: {human_pos} ({per_sec}) {msg}")
            .unwrap(),
    );
    pb
}

#[cfg(test)]
mod tests {
    use super::{build_shift_table, SearchResult, State, TARGET_SCORE};

    #[test]
    fn build_shift_table_creates_expected_complement_masks() {
        let table = build_shift_table(&[2], 6);
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].len(), 2);
        assert_eq!(table[0][0].count_ones(), 3);
        assert_eq!(table[0][1].count_ones(), 3);
    }

    #[test]
    fn new_search_state_uses_fixed_target_score() {
        let state = State::new(vec![2], 4, build_shift_table(&[2], 4));
        assert_eq!(state.target, TARGET_SCORE);
        assert_eq!(state.target, 447);
    }

    #[test]
    fn build_shift_table_applies_each_shift_at_the_expected_columns() {
        let table = build_shift_table(&[3], 7);

        assert_eq!(table[0][0].count_ones(), 4);
        assert_eq!(table[0][1].count_ones(), 5);
        assert_eq!(table[0][2].count_ones(), 5);

        let shifted_once = table[0][1].clone();
        assert_eq!(
            shifted_once.bitand(&table[0][0]).count_ones(),
            2,
            "different shifts must exclude different residue classes"
        );
    }

    #[test]
    fn sequential_and_parallel_search_find_best_results() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.target = 0;
        sequential.max_depth = 2;
        sequential.search(2);
        let mut parallel = State::new(primes.clone(), cols, table);
        parallel.target = 0;
        parallel.max_depth = 2;
        let result = parallel.search_parallel(2);

        assert_eq!(sequential.max_score, 2);
        assert_eq!(result.max_score, 2);
        assert!(!result.shifts.is_empty());
        for shift_path in &result.shifts {
            assert_eq!(shift_path.len(), 2);
            for (level, &shift) in shift_path.iter().enumerate() {
                assert!(shift < primes[level]);
            }
        }
    }

    #[test]
    fn beam_search_keeps_best_partial_states() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut beam = State::new(primes, cols, table);
        beam.target = 0;
        beam.max_depth = 2;

        beam.search_beam(2, 2);

        assert_eq!(beam.max_score, 2);
        assert!(!beam.shifts.is_empty());
        assert_eq!(beam.shifts[0].len(), 2);
    }

    #[test]
    fn searches_final_level_from_saved_previous_level_paths() {
        let primes = vec![2, 3];
        let table = build_shift_table(&primes, 4);
        let mut state = State::new(primes.clone(), 4, table.clone());
        state.target = 0;
        let saved_paths = vec![vec![0], vec![1]];
        let resumed = state
            .search_last_level_from_paths(2, &saved_paths, |_, _| Ok(()))
            .unwrap();
        let resumed_parallel = state
            .search_last_level_from_paths_parallel(2, &saved_paths, |_, _| Ok(()))
            .unwrap();

        let mut full_search = State::new(primes, 4, table);
        full_search.target = 0;
        full_search.search(2);
        let mut resumed_paths = resumed.shifts;
        resumed_paths.sort();
        let mut full_paths = full_search.shifts;
        full_paths.sort();
        let mut resumed_parallel_paths = resumed_parallel.shifts;
        resumed_parallel_paths.sort();

        assert_eq!(resumed.max_score, full_search.max_score);
        assert_eq!(resumed_paths, full_paths);
        assert_eq!(resumed_parallel.max_score, full_search.max_score);
        assert_eq!(resumed_parallel_paths, full_paths);
    }

    #[test]
    fn rejects_saved_previous_level_paths_with_invalid_shifts() {
        let primes = vec![2, 3];
        let state = State::new(primes.clone(), 4, build_shift_table(&primes, 4));

        assert!(state
            .search_last_level_from_paths(2, &[vec![2]], |_, _| Ok(()))
            .is_err());
    }

    #[test]
    fn beam_search_processes_candidates_in_batches_and_keeps_only_the_beam() {
        let primes = vec![23, 23, 2];
        let table = build_shift_table(&primes, 32);
        let mut beam = State::new(primes, 32, table);
        beam.target = 0;
        let recorded_depths = std::sync::Mutex::new(Vec::new());

        beam.search_beam_recording(3, 23, |path, _| {
            recorded_depths.lock().unwrap().push(path.len());
            Ok(())
        })
        .unwrap();

        let recorded_depths = recorded_depths.into_inner().unwrap();
        assert_eq!(
            recorded_depths.iter().filter(|&&depth| depth == 1).count(),
            23
        );
        assert_eq!(
            recorded_depths.iter().filter(|&&depth| depth == 2).count(),
            23 * 23
        );
        assert_eq!(
            recorded_depths.iter().filter(|&&depth| depth == 3).count(),
            23 * 2
        );
    }

    #[test]
    fn search_recorders_keep_target_passing_nodes_at_every_depth() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let target = 2;

        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.target = target;
        let sequential_nodes = std::sync::Mutex::new(Vec::new());
        sequential
            .search_recording(2, |path, score| {
                sequential_nodes.lock().unwrap().push((path.len(), score));
                Ok(())
            })
            .unwrap();

        let parallel = State::new(primes.clone(), cols, table.clone());
        let parallel = {
            let mut parallel = parallel;
            parallel.target = target;
            let parallel_nodes = std::sync::Mutex::new(Vec::new());
            parallel
                .search_parallel_recording(2, |path, score| {
                    parallel_nodes.lock().unwrap().push((path.len(), score));
                    Ok(())
                })
                .unwrap();
            parallel_nodes.into_inner().unwrap()
        };

        let mut beam = State::new(primes, cols, table);
        beam.target = target;
        let beam_nodes = std::sync::Mutex::new(Vec::new());
        beam.search_beam_recording(2, usize::MAX, |path, score| {
            beam_nodes.lock().unwrap().push((path.len(), score));
            Ok(())
        })
        .unwrap();

        for nodes in [
            sequential_nodes.into_inner().unwrap(),
            parallel,
            beam_nodes.into_inner().unwrap(),
        ] {
            assert!(nodes.iter().any(|(depth, _)| *depth == 1));
            assert!(nodes.iter().any(|(depth, _)| *depth == 2));
            assert!(nodes.iter().all(|(_, score)| *score >= target));
        }
    }

    #[test]
    fn all_search_modes_agree_when_beam_retains_every_candidate() {
        let primes = vec![2, 3];
        let cols = 8;
        let table = build_shift_table(&primes, cols);
        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.target = 0;
        sequential.search(2);

        let mut parallel_state = State::new(primes.clone(), cols, table.clone());
        parallel_state.target = 0;
        let parallel = parallel_state.search_parallel(2);

        let mut beam = State::new(primes, cols, table);
        beam.target = 0;
        beam.search_beam(2, usize::MAX);

        assert_eq!(parallel.max_score, sequential.max_score);
        assert_eq!(beam.max_score, sequential.max_score);
        assert_eq!(beam.shifts, sequential.shifts);
    }

    #[test]
    fn beam_search_keeps_all_candidates_when_width_is_unbounded() {
        let primes = vec![2, 3];
        let cols = 6;
        let table = build_shift_table(&primes, cols);
        let mut beam = State::new(primes, cols, table);
        beam.target = 0;

        beam.search_beam(2, usize::MAX);

        assert_eq!(beam.max_score, 2);
        assert!(!beam.shifts.is_empty());
        assert_eq!(beam.shifts[0].len(), 2);
    }

    #[test]
    fn search_result_tracks_best_count_and_ties() {
        let mut result = SearchResult::default();

        result.record(2, vec![1]);
        result.record(2, vec![0]);
        result.record(3, vec![1, 0]);
        result.record(3, vec![0, 1]);

        assert_eq!(result.max_score, 3);
        assert_eq!(result.shifts.len(), 2);
    }

    #[test]
    fn search_result_merge_replaces_lower_best_and_combines_ties() {
        let mut result = SearchResult {
            max_score: 2,
            shifts: vec![vec![0]],
        };

        result.merge(SearchResult {
            max_score: 3,
            shifts: vec![vec![1, 0]],
        });
        result.merge(SearchResult {
            max_score: 3,
            shifts: vec![vec![0, 1]],
        });
        result.merge(SearchResult {
            max_score: 1,
            shifts: vec![vec![1]],
        });

        assert_eq!(result.max_score, 3);
        assert_eq!(result.shifts, vec![vec![1, 0], vec![0, 1]]);
    }
}
