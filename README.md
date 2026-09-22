# RustHLSearch

HLSearch（素数シフト探索）の Rust 実装です。指定した深さまでの素数シフト列を探索し、葉のビット数（popcount）が最大となるパスを記録します。探索手法として、通常の深さ優先探索（DFS）だけでなく、各深さで候補を絞るビームサーチにも対応しています。

## 機能・特徴

- **高速なビット並列処理**: 64-bit 単位の独自 `BitMask` 構造体による高速 bitwise AND および popcount。
- **逐次 DFS / 並列 DFS / ビームサーチ**: `sequential`, `parallel`, `beam` の3つの探索モードを提供。
- **最大値探索**: 指定した深さの全候補を探索し、最大 popcount のパスを記録。
- **ビーム幅制御**: `beam` モードでは `--beam-width` により候補の上位のみを保持して探索を高速化。
- **降順探索**: 各素数のシフト候補を降順（$p-1 \dots 0$）に探索。
- **リアルタイム進捗表示**: `indicatif` による探索ノード数・処理速度・最良 popcount のライブ表示。

## ビルド

### Cargo によるビルド

```bash
cargo build --release
```

バイナリは `target/release/hlsearch`（Windows では `target/release/hlsearch.exe`）に出力されます。

### Windows 用バッチファイル

Windows 環境向けに `build.bat` も用意されています（デバッグビルドおよびリリースビルドを順に実行）。

```cmd
build.bat
```

## テスト・静的解析

```bash
# 単体テストをすべて実行
cargo test

# テスト名を指定して実行
cargo test beam_search_keeps_best_partial_states

# フォーマット確認
cargo fmt -- --check

# Clippy
cargo clippy --all-targets --all-features -- -D warnings
```

## ソース構成

```text
src/
  main.rs      # CLI解析、入力検証、探索実行、結果出力
  bitmask.rs   # BitMaskによる64-bit単位のビット演算
  primes.rs    # エラトステネスの篩による素数生成
  search.rs    # シフトテーブル生成と DFS / 並列DFS / ビームサーチ
  output.rs    # タイムスタンプ付き出力パス生成
```

## 実行

ヘルプの表示:

```bash
cargo run --release -- --help
```

### 実行例

デフォルト（ビームサーチ、深さ 8、cols 3159）:

```bash
cargo run --release
```

逐次モード（単一スレッド）で実行:

```bash
cargo run --release -- --mode sequential --depth 8
```

ビームサーチ（候補を幅優先で絞り込む）:

```bash
cargo run --release -- --mode beam --beam-width 32 --depth 8
```

素数の個数や出力先を指定して実行:

```bash
cargo run --release -- --depth 10 -o output
```

> **Note**: 並列モード時のスレッド数は Rayon の既定値（論理コア数）となります。環境変数 `RAYON_NUM_THREADS` でスレッド数を指定可能です。

### 入力値の検証

実行開始前に次の条件を検証します。条件に違反した場合はエラーを表示して終了します。

- `depth` は 1 以上
- `cols` は 1 以上
- `depth` は使用する素数数以下

## 探索アルゴリズムの概要

1. **素数生成**:
  - 1579 以下の素数（最大 249 個）をエラトステネスの篩で生成し、先頭から `depth` 個を探索階層に使用します。
2. **補集合シフトテーブル作成**:
   - 各素数 $p$ とシフト $k \in [0, p)$ について、長さ `cols` の補集合ビットマスクを事前構築します。
3. **検索モード**:
   - `sequential`: 非再帰 DFS で決定論的に探索。
  - `parallel`: 第1素数の候補を Rayon で並列化し、最大 popcount を集計。
   - `beam`: 各深さで popcount を基準に候補をソートし、`--beam-width` の上位だけを保持して探索を続行。
4. **葉ノード（深さ `depth`）の判定**:
   - popcount がこれまでの最大値を超えた場合、`max_count` を更新します。
  - 最大 popcount に到達したシフトパスを記録します。

### 探索モード

- `--mode sequential`: 単一スレッドで決定論的に非再帰 DFS を実行します。
- `--mode parallel`（デフォルト）: 第1素数のシフト候補を逆順（降順）で Rayon の並列イテレータに分配し、複数スレッドで並列 DFS します。いずれかのスレッドが解を見つけた時点で全スレッドを停止します。
- `--mode beam`: 各階層で候補の popcount を比較し、`--beam-width` の上位候補のみを残します。探索空間を削減して高速化を狙います。

並列モードではスレッドの実行順序により、記録される解のシフト列が逐次モードと異なる場合があります。

## コマンドラインオプション

| フラグ | 短縮 | 既定値 | 説明 |
| --- | --- | --- | --- |
| `--depth` | `-d` | `8` | 探索する階層数（使用する素数の個数） |
| `--mode` | `-m` | `beam` | 探索モード（`sequential` / `parallel` / `beam`） |
| `--beam-width` |  | `32` | `beam` モードで保持する候補の最大数 |
| `--cols` | | `3159` | ビット列の長さ |
| `--output` | `-o` | `.` | 出力ディレクトリ |
| `--max-depth` | | `249` | 出力設定に記録される予約パラメータ |

> **Note**: `--max-depth` は現在の探索条件には影響せず、実行設定として出力ファイルに記録されます。

## 出力ファイル形式

実行時の日付と探索深度を付けて、次の2ファイルを出力します。

- `shift_path_depth8_YYYYMMDD.txt`: 見つかったシフト列を1行に1列、空白区切りで出力
- `result_depth8_YYYYMMDD.json`: 実行設定と最大 popcount を含む探索結果

```json
{
  "config": {
    "mode": "parallel",
    "depth": 8,
    "max_depth": 249,
    "cols": 3159,
    "beam_width": 32,
    "elapsed": "1.234567s"
  },
  "result": {
    "max_count": 447
  }
}
```

- `config`: 実行時設定と経過時間
- `result.max_count`: 早期終了までに到達した葉ノードの最大 popcount

## ライセンス

[MIT License](LICENSE)
