//! シフトパス木の RocksDB 永続化。
//!
//! - `nodes` CF:  key = node_id (u64 big-endian)
//!                value = `{"depth":..,"parent_node_id":..,"shift":..,"score":..}` (JSON)
//! - `node_index` CF: (parent_node_id, shift) -> node_id (重複ノード検出用の補助インデックス)
//! - `target_paths` CF: (depth, target_shifts JSON) -> target_count
//! - `meta` CF: 設定ハッシュ
//!
//! node_id は全深さで一意で、上位 16 bit に深さ、下位 48 bit に連番を持つ。
//! そのためビッグエンディアンのキー順 = (深さ, 連番) 順となり、
//! 特定の深さのノードを範囲スキャンで列挙できる。

use rocksdb::{ColumnFamilyDescriptor, Direction, IteratorMode, Options, WriteBatch, DB};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

const MAX_DEPTH: usize = 249;
const APPEND_BATCH_SIZE: usize = 4096;
const DEPTH_SHIFT: u32 = 48;
const SEQ_MASK: u64 = (1u64 << DEPTH_SHIFT) - 1;

const CF_NODES: &str = "nodes";
const CF_INDEX: &str = "node_index";
const CF_TARGETS: &str = "target_paths";
const CF_META: &str = "meta";

type PendingAppends = Vec<(usize, Vec<usize>)>;

/// `nodes` CF に JSON で保存されるノード情報
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub depth: usize,
    pub parent_node_id: Option<u64>,
    pub shift: usize,
    pub score: usize,
}

fn err<E: ToString>(error: E) -> String {
    error.to_string()
}

fn make_node_id(depth: usize, seq: u64) -> u64 {
    ((depth as u64) << DEPTH_SHIFT) | (seq & SEQ_MASK)
}

fn node_depth(node_id: u64) -> usize {
    (node_id >> DEPTH_SHIFT) as usize
}

fn decode_u64(bytes: &[u8]) -> Result<u64, String> {
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| format!("不正な u64 バイト列です (len={})", bytes.len()))?;
    Ok(u64::from_be_bytes(array))
}

/// 親 (ルートは 0) と shift から補助インデックスのキーを作る
fn index_key(parent_node_id: Option<u64>, shift: usize) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&parent_node_id.unwrap_or(0).to_be_bytes());
    key[8..].copy_from_slice(&(shift as u64).to_be_bytes());
    key
}

fn target_prefix(depth: usize) -> [u8; 2] {
    (depth as u16).to_be_bytes()
}

fn target_key(depth: usize, target_shifts: &str) -> Vec<u8> {
    let mut key = target_prefix(depth).to_vec();
    key.extend_from_slice(target_shifts.as_bytes());
    key
}

fn config_key(depth: usize) -> String {
    format!("config_hash:{depth}")
}

fn completion_key(depth: usize) -> String {
    format!("search_complete:{depth}")
}

fn cf<'a>(db: &'a DB, name: &str) -> Result<&'a rocksdb::ColumnFamily, String> {
    db.cf_handle(name)
        .ok_or_else(|| format!("カラムファミリー {name} が見つかりません"))
}

#[derive(Clone)]
pub struct ShiftPathStore {
    db: Arc<DB>,
    depth: usize,
    pending_appends: Arc<Mutex<PendingAppends>>,
    /// 深さごとの次に払い出す連番 (index = depth)
    next_seq: Arc<Mutex<Vec<u64>>>,
}

impl ShiftPathStore {
    pub fn create(
        path: &Path,
        depth: usize,
        clear_existing: bool,
        config_hash: u64,
    ) -> Result<Self, String> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let descriptors = [CF_NODES, CF_INDEX, CF_TARGETS, CF_META]
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default()));
        let db = DB::open_cf_descriptors(&options, path, descriptors).map_err(err)?;

        check_config_hash(&db, depth, config_hash)?;
        if clear_existing {
            clear_targets(&db, depth)?;
        }
        store_config_hash(&db, depth, config_hash)?;
        let next_seq = load_next_seq(&db)?;

        Ok(Self {
            db: Arc::new(db),
            depth,
            pending_appends: Arc::new(Mutex::new(Vec::new())),
            next_seq: Arc::new(Mutex::new(next_seq)),
        })
    }

    #[allow(dead_code)] // Convenience API; some callers use append_batch directly.
    pub fn append(&self, max_count: usize, path: &[usize]) -> Result<(), String> {
        if path.is_empty() {
            return Ok(());
        }
        self.append_batch(vec![(max_count, path.to_vec())])
    }

    pub fn append_batch(&self, mut appends: Vec<(usize, Vec<usize>)>) -> Result<(), String> {
        if appends.is_empty() {
            return Ok(());
        }
        let mut buffer = self
            .pending_appends
            .lock()
            .map_err(|_| "バッファのロック取得に失敗しました".to_string())?;
        buffer.append(&mut appends);
        let should_flush = buffer.len() >= APPEND_BATCH_SIZE;
        drop(buffer);
        if should_flush {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&self) -> Result<(), String> {
        let mut buffer = self
            .pending_appends
            .lock()
            .map_err(|_| "バッファのロック取得に失敗しました".to_string())?;
        if buffer.is_empty() {
            return Ok(());
        }
        let mut counters = self
            .next_seq
            .lock()
            .map_err(|_| "RocksDB 出力のロック取得に失敗しました".to_string())?;

        let nodes_cf = cf(&self.db, CF_NODES)?;
        let index_cf = cf(&self.db, CF_INDEX)?;

        // バッチ内の書き込みは未コミットなので、オーバーレイで管理する
        let mut next_seq = counters.clone();
        let mut touched: HashMap<u64, NodeRecord> = HashMap::new();
        let mut new_index: HashMap<[u8; 16], u64> = HashMap::new();

        for (max_count, path) in buffer.iter() {
            if path.len() > MAX_DEPTH {
                return Err(format!(
                    "パス長 {} が上限 {MAX_DEPTH} を超えています",
                    path.len()
                ));
            }
            let mut parent_node_id: Option<u64> = None;
            for (depth, &shift) in (1..).zip(path.iter()) {
                let key = index_key(parent_node_id, shift);
                let node_id = if let Some(&id) = new_index.get(&key) {
                    id
                } else if let Some(value) = self.db.get_cf(index_cf, key).map_err(err)? {
                    decode_u64(&value)?
                } else {
                    let id = make_node_id(depth, next_seq[depth]);
                    next_seq[depth] += 1;
                    touched.insert(
                        id,
                        NodeRecord {
                            depth,
                            parent_node_id,
                            shift,
                            score: *max_count,
                        },
                    );
                    new_index.insert(key, id);
                    id
                };
                parent_node_id = Some(node_id);
            }

            // 葉ノードのスコアを更新する
            let leaf_id = parent_node_id.expect("path is non-empty");
            if let Some(record) = touched.get_mut(&leaf_id) {
                record.score = *max_count;
            } else {
                let mut record = read_node(&self.db, leaf_id)?
                    .ok_or_else(|| format!("node_id {leaf_id} がありません"))?;
                record.score = *max_count;
                touched.insert(leaf_id, record);
            }
        }

        let mut batch = WriteBatch::default();
        for (node_id, record) in &touched {
            batch.put_cf(
                nodes_cf,
                node_id.to_be_bytes(),
                serde_json::to_vec(record).map_err(err)?,
            );
        }
        for (key, node_id) in &new_index {
            batch.put_cf(index_cf, key, node_id.to_be_bytes());
        }
        self.db.write(batch).map_err(err)?;

        // コミット成功後にだけ状態を進める
        *counters = next_seq;
        buffer.clear();
        Ok(())
    }

    pub fn append_target(&self, target_count: usize, path: &[usize]) -> Result<(), String> {
        let target_shifts = serde_json::to_string(path).map_err(err)?;
        let targets_cf = cf(&self.db, CF_TARGETS)?;
        let key = target_key(self.depth, &target_shifts);
        // INSERT OR IGNORE 相当: 既にあれば何もしない
        if self.db.get_cf(targets_cf, &key).map_err(err)?.is_none() {
            self.db
                .put_cf(targets_cf, &key, (target_count as u64).to_be_bytes())
                .map_err(err)?;
        }
        Ok(())
    }

    pub fn set_search_complete(&self, complete: bool) -> Result<(), String> {
        let meta_cf = cf(&self.db, CF_META)?;
        self.db
            .put_cf(meta_cf, completion_key(self.depth), [u8::from(complete)])
            .map_err(err)
    }

    pub fn is_search_complete(&self) -> Result<bool, String> {
        let meta_cf = cf(&self.db, CF_META)?;
        match self
            .db
            .get_cf(meta_cf, completion_key(self.depth))
            .map_err(err)?
        {
            Some(value) if value.as_ref() == [1] => Ok(true),
            Some(value) if value.as_ref() == [0] => Ok(false),
            Some(_) => Err("不正な探索完了フラグです".to_string()),
            None => Ok(false),
        }
    }

    #[allow(dead_code)] // Optional result-loading API.
    pub fn load_result_paths(&self, max_count: usize) -> Result<Vec<Vec<usize>>, String> {
        self.load_leaf_paths(self.depth, Some(max_count))
    }

    pub fn load_full_depth_paths(&self, depth: usize) -> Result<Vec<Vec<usize>>, String> {
        self.load_leaf_paths(depth, None)
    }

    /// 指定深さの葉を node_id 順に最大 `limit` 件返す。
    ///
    /// `after_node_id` を指定した場合は、その直後のノードから読み始める。
    pub fn load_full_depth_paths_batch(
        &self,
        depth: usize,
        after_node_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<(u64, Vec<usize>)>, String> {
        self.flush()?;
        if depth == 0 || depth > MAX_DEPTH || limit == 0 {
            return Ok(Vec::new());
        }

        let nodes_cf = cf(&self.db, CF_NODES)?;
        let start_node_id = after_node_id.unwrap_or_else(|| make_node_id(depth, 0));
        let start = start_node_id.to_be_bytes();
        let mut leaf_ids = Vec::with_capacity(limit);
        let mut leaves = Vec::with_capacity(limit);
        for item in self
            .db
            .iterator_cf(nodes_cf, IteratorMode::From(&start, Direction::Forward))
        {
            let (key, value) = item.map_err(err)?;
            let node_id = decode_u64(&key)?;
            if node_depth(node_id) != depth {
                break;
            }
            if after_node_id.is_some_and(|after| node_id <= after) {
                continue;
            }

            leaf_ids.push(node_id);
            leaves.push(serde_json::from_slice(&value).map_err(err)?);
            if leaves.len() == limit {
                break;
            }
        }

        let paths = self.reconstruct_leaf_paths(depth, leaves)?;
        Ok(leaf_ids.into_iter().zip(paths).collect())
    }

    /// 指定深さの葉をスコア降順、同スコアでは node_id 昇順で最大 `limit` 件返す。
    pub fn load_top_scored_paths(
        &self,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<(usize, Vec<usize>)>, String> {
        self.flush()?;
        if depth == 0 || depth > MAX_DEPTH || limit == 0 {
            return Ok(Vec::new());
        }

        let nodes_cf = cf(&self.db, CF_NODES)?;
        let start = make_node_id(depth, 0).to_be_bytes();
        let mut top = BinaryHeap::new();
        for item in self
            .db
            .iterator_cf(nodes_cf, IteratorMode::From(&start, Direction::Forward))
        {
            let (key, value) = item.map_err(err)?;
            let node_id = decode_u64(&key)?;
            if node_depth(node_id) != depth {
                break;
            }
            let record: NodeRecord = serde_json::from_slice(&value).map_err(err)?;
            let candidate = (Reverse(record.score), node_id);
            if top.len() < limit {
                top.push(candidate);
            } else if top.peek().is_some_and(|worst| candidate < *worst) {
                top.pop();
                top.push(candidate);
            }
        }

        let mut leaves: Vec<_> = top
            .into_iter()
            .map(|(Reverse(score), node_id)| (score, node_id))
            .collect();
        leaves.sort_unstable_by(|(left_score, left_id), (right_score, right_id)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_id.cmp(right_id))
        });
        let records = leaves
            .iter()
            .map(|(_, node_id)| {
                read_node(&self.db, *node_id)?
                    .ok_or_else(|| format!("node_id {node_id} がありません"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let scores = leaves
            .into_iter()
            .map(|(score, _)| score)
            .collect::<Vec<_>>();
        let paths = self.reconstruct_leaf_paths(depth, records)?;
        Ok(scores.into_iter().zip(paths).collect())
    }

    fn load_leaf_paths(
        &self,
        depth: usize,
        max_count: Option<usize>,
    ) -> Result<Vec<Vec<usize>>, String> {
        self.flush()?;
        if depth == 0 || depth > MAX_DEPTH {
            return Ok(Vec::new());
        }
        let leaves: Vec<NodeRecord> = self
            .load_nodes(depth)?
            .into_iter()
            .map(|(_, record)| record)
            .filter(|record| max_count.is_none_or(|score| record.score == score))
            .collect();
        self.reconstruct_leaf_paths(depth, leaves)
    }

    fn reconstruct_leaf_paths(
        &self,
        depth: usize,
        leaves: Vec<NodeRecord>,
    ) -> Result<Vec<Vec<usize>>, String> {
        let mut paths: Vec<Vec<usize>> = leaves.iter().map(|leaf| vec![leaf.shift]).collect();
        let mut parent_ids: Vec<Option<u64>> =
            leaves.iter().map(|leaf| leaf.parent_node_id).collect();
        let mut cache: HashMap<u64, NodeRecord> = HashMap::new();

        for _ in 1..depth {
            for (path, parent_id) in paths.iter_mut().zip(parent_ids.iter_mut()) {
                let node_id =
                    parent_id.ok_or_else(|| format!("depth {depth} の親ノードが欠けています"))?;
                if !cache.contains_key(&node_id) {
                    let record = read_node(&self.db, node_id)?
                        .ok_or_else(|| format!("node_id {node_id} がありません"))?;
                    cache.insert(node_id, record);
                }
                let record = &cache[&node_id];
                path.push(record.shift);
                *parent_id = record.parent_node_id;
            }
        }
        for path in &mut paths {
            path.reverse();
        }
        Ok(paths)
    }

    /// 指定した深さのノードを node_id 昇順で返す
    pub(crate) fn load_nodes(&self, depth: usize) -> Result<Vec<(u64, NodeRecord)>, String> {
        self.flush()?;
        if depth == 0 || depth > MAX_DEPTH {
            return Ok(Vec::new());
        }
        let nodes_cf = cf(&self.db, CF_NODES)?;
        let start = make_node_id(depth, 0).to_be_bytes();
        let mut nodes = Vec::new();
        for item in self
            .db
            .iterator_cf(nodes_cf, IteratorMode::From(&start, Direction::Forward))
        {
            let (key, value) = item.map_err(err)?;
            let node_id = decode_u64(&key)?;
            if node_depth(node_id) != depth {
                break;
            }
            nodes.push((node_id, serde_json::from_slice(&value).map_err(err)?));
        }
        Ok(nodes)
    }

    #[allow(dead_code)] // Optional target-loading API.
    pub fn load_target_paths(&self) -> Result<Vec<Vec<usize>>, String> {
        self.flush()?;
        let targets_cf = cf(&self.db, CF_TARGETS)?;
        let prefix = target_prefix(self.depth);
        let mut targets: Vec<(u64, Vec<usize>)> = Vec::new();
        for item in self
            .db
            .iterator_cf(targets_cf, IteratorMode::From(&prefix, Direction::Forward))
        {
            let (key, value) = item.map_err(err)?;
            if !key.starts_with(&prefix) {
                break;
            }
            let shifts: Vec<usize> = serde_json::from_slice(&key[prefix.len()..]).map_err(err)?;
            targets.push((decode_u64(&value)?, shifts));
        }
        targets.sort_by_key(|(count, _)| *count); // 安定ソート
        Ok(targets.into_iter().map(|(_, shifts)| shifts).collect())
    }
}

impl Drop for ShiftPathStore {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn read_node(db: &DB, node_id: u64) -> Result<Option<NodeRecord>, String> {
    let nodes_cf = cf(db, CF_NODES)?;
    match db.get_cf(nodes_cf, node_id.to_be_bytes()).map_err(err)? {
        Some(value) => Ok(Some(serde_json::from_slice(&value).map_err(err)?)),
        None => Ok(None),
    }
}

/// 各深さの最後の node_id を逆方向シークで調べ、次の連番を求める
fn load_next_seq(db: &DB) -> Result<Vec<u64>, String> {
    let nodes_cf = cf(db, CF_NODES)?;
    let mut next_seq = vec![1u64; MAX_DEPTH + 1];
    for (depth, slot) in next_seq.iter_mut().enumerate().skip(1) {
        let upper = make_node_id(depth, SEQ_MASK).to_be_bytes();
        let mut iter = db.iterator_cf(nodes_cf, IteratorMode::From(&upper, Direction::Reverse));
        if let Some(item) = iter.next() {
            let (key, _) = item.map_err(err)?;
            let node_id = decode_u64(&key)?;
            if node_depth(node_id) == depth {
                *slot = (node_id & SEQ_MASK) + 1;
            }
        }
    }
    Ok(next_seq)
}

fn clear_targets(db: &DB, depth: usize) -> Result<(), String> {
    let targets_cf = cf(db, CF_TARGETS)?;
    let prefix = target_prefix(depth);
    let mut batch = WriteBatch::default();
    for item in db.iterator_cf(targets_cf, IteratorMode::From(&prefix, Direction::Forward)) {
        let (key, _) = item.map_err(err)?;
        if !key.starts_with(&prefix) {
            break;
        }
        batch.delete_cf(targets_cf, key);
    }
    db.write(batch).map_err(err)
}

fn check_config_hash(db: &DB, depth: usize, config_hash: u64) -> Result<(), String> {
    let meta_cf = cf(db, CF_META)?;
    if let Some(value) = db.get_cf(meta_cf, config_key(depth)).map_err(err)? {
        let stored_hash = decode_u64(&value)?;
        if stored_hash != config_hash {
            return Err(format!(
                "設定ハッシュが異なります。別の --output-dir を使用してください (stored_hash={}, current_hash={})",
                stored_hash as i64, config_hash as i64
            ));
        }
    }
    Ok(())
}

fn store_config_hash(db: &DB, depth: usize, config_hash: u64) -> Result<(), String> {
    let meta_cf = cf(db, CF_META)?;
    db.put_cf(meta_cf, config_key(depth), config_hash.to_be_bytes())
        .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::{NodeRecord, ShiftPathStore};

    fn database_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hlsearch-shift-paths-{}-{}.rocksdb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn appends_reusable_parent_nodes_and_reconstructs_leaves_in_order() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store
            .append_batch(vec![
                (10, vec![1]),
                (10, vec![1, 2]),
                (11, vec![1, 3]),
                (11, vec![1, 3]),
            ])
            .unwrap();

        assert_eq!(
            store.load_full_depth_paths(2).unwrap(),
            vec![vec![1, 2], vec![1, 3]]
        );
        assert_eq!(store.load_result_paths(11).unwrap(), vec![vec![1, 3]]);

        let roots = store.load_nodes(1).unwrap();
        assert_eq!(roots.len(), 1);
        let root_id = roots[0].0;
        let nodes: Vec<NodeRecord> = store
            .load_nodes(2)
            .unwrap()
            .into_iter()
            .map(|(_, record)| record)
            .collect();
        let record = |shift, score| NodeRecord {
            depth: 2,
            parent_node_id: Some(root_id),
            shift,
            score,
        };
        assert_eq!(nodes, vec![record(2, 10), record(3, 11)]);

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn prefixes_are_stored_in_their_own_depth_ranges() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 3, true, 12345).unwrap();
        store.append(4, &[1, 2, 3]).unwrap();

        assert_eq!(store.load_full_depth_paths(1).unwrap(), vec![vec![1]]);
        assert_eq!(store.load_full_depth_paths(2).unwrap(), vec![vec![1, 2]]);
        assert_eq!(store.load_full_depth_paths(3).unwrap(), vec![vec![1, 2, 3]]);
        assert_eq!(store.load_result_paths(4).unwrap(), vec![vec![1, 2, 3]]);

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn top_scored_paths_are_score_sorted_and_stable_for_ties() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store
            .append_batch(vec![
                (7, vec![1, 2]),
                (9, vec![0, 1]),
                (9, vec![0, 2]),
                (8, vec![1, 0]),
            ])
            .unwrap();

        assert_eq!(
            store.load_top_scored_paths(2, 3).unwrap(),
            vec![(9, vec![0, 1]), (9, vec![0, 2]), (8, vec![1, 0])]
        );

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn loads_full_depth_paths_in_node_id_batches() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 1, true, 12345).unwrap();
        store
            .append_batch((0..501).map(|shift| (1, vec![shift])).collect())
            .unwrap();

        let first = store.load_full_depth_paths_batch(1, None, 500).unwrap();
        assert_eq!(first.len(), 500);
        assert_eq!(first[0].1, vec![0]);
        assert_eq!(first[499].1, vec![499]);

        let second = store
            .load_full_depth_paths_batch(1, Some(first[499].0), 500)
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1, vec![500]);
        assert!(store
            .load_full_depth_paths_batch(1, Some(second[0].0), 500)
            .unwrap()
            .is_empty());

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn clearing_a_depth_keeps_stored_paths_and_removes_its_targets() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append(3, &[1, 2]).unwrap();
        store.append_target(3, &[1, 2]).unwrap();
        drop(store);

        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        assert_eq!(store.load_full_depth_paths(2).unwrap(), vec![vec![1, 2]]);
        assert!(store.load_target_paths().unwrap().is_empty());

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn node_ids_continue_after_reopen_without_duplicating_nodes() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append(5, &[1, 2]).unwrap();
        drop(store);

        let store = ShiftPathStore::create(&path, 2, false, 12345).unwrap();
        store.append(6, &[1, 3]).unwrap();
        store.append(5, &[1, 2]).unwrap();
        assert_eq!(
            store.load_full_depth_paths(2).unwrap(),
            vec![vec![1, 2], vec![1, 3]]
        );
        assert_eq!(store.load_nodes(1).unwrap().len(), 1);

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn targets_are_ordered_by_count_and_deduplicated() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        store.append_target(2, &[1, 3]).unwrap();
        store.append_target(1, &[1, 2]).unwrap();
        store.append_target(9, &[1, 2]).unwrap();
        assert_eq!(
            store.load_target_paths().unwrap(),
            vec![vec![1, 2], vec![1, 3]]
        );

        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn rejects_mismatched_config_hash() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 1).unwrap();
        drop(store);
        assert!(ShiftPathStore::create(&path, 2, false, 2).is_err());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn persists_search_completion_state() {
        let path = database_path();
        let store = ShiftPathStore::create(&path, 2, true, 12345).unwrap();
        assert!(!store.is_search_complete().unwrap());
        store.set_search_complete(true).unwrap();
        assert!(store.is_search_complete().unwrap());
        drop(store);

        let store = ShiftPathStore::create(&path, 2, false, 12345).unwrap();
        assert!(store.is_search_complete().unwrap());
        store.set_search_complete(false).unwrap();
        assert!(!store.is_search_complete().unwrap());
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
}
