use chrono::Local;
use std::path::{Path, PathBuf};

/// 指定したディレクトリに、深さと日付を含む出力ファイル名を作る。
pub fn dated_output_path(directory: &Path, prefix: &str, depth: usize, extension: &str) -> PathBuf {
    let date = Local::now().format("%Y%m%d").to_string();
    directory.join(format!("{prefix}_depth{depth}_{date}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::dated_output_path;
    use std::path::Path;

    #[test]
    fn dated_path_contains_prefix_depth_and_date() {
        let path = dated_output_path(Path::new("results"), "shift_path", 13, "txt");
        let file_name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(path.parent(), Some(Path::new("results")));
        assert!(file_name.starts_with("shift_path_"));
        assert!(file_name.contains("depth13_"));
        assert!(file_name.ends_with(".txt"));
    }

    #[test]
    fn result_path_uses_depth_date_and_text_extension() {
        let path = dated_output_path(Path::new("results"), "result", 8, "txt");
        let file_name = path.file_name().unwrap().to_str().unwrap();
        assert!(file_name.starts_with("result_depth8_"));
        assert!(file_name.ends_with(".txt"));
    }
}
