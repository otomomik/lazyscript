// スクリプトソースの抽象。将来 Makefile/Taskfile を足せるよう、
// 実行方法を知るのは各ソースの build_command だけにする。

mod ad_hoc;
mod json_store;
mod package_json;
mod registry;
mod scripts_json;

pub use registry::Registry;
#[cfg(test)]
pub use registry::SourceGroup;

use std::path::PathBuf;

/// ソース種別の識別子（"package.json" 等）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SourceId(pub &'static str);

/// その場限りのコマンド実行（ファイルに紐づかないメモリ上のグループ）。
pub const AD_HOC: SourceId = SourceId("commands");

/// 一覧に並ぶ1スクリプト。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Script {
    pub name: String,
    pub source: SourceId,
    /// スクリプトの中身（package.json なら scripts の値の文字列）。表示用。
    pub command: String,
}

/// PTY 起動側が受け取る実行仕様。ソース種別に依存しない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// 生コマンドを「現在のシェル」で実行する CommandSpec を作る。
/// Unix は $SHELL（無ければ sh）、Windows は cmd。
/// Unix では `-i`（対話モード）を付け、rc（.zshrc/.bashrc 等）をソースさせて
/// ユーザー定義のエイリアスを効かせる。
pub fn shell_command(command: &str, cwd: PathBuf) -> CommandSpec {
    #[cfg(windows)]
    let (program, args) = (
        "cmd".to_string(),
        vec!["/C".to_string(), command.to_string()],
    );
    #[cfg(not(windows))]
    let (program, args) = (
        std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()),
        vec!["-i".to_string(), "-c".to_string(), command.to_string()],
    );
    CommandSpec {
        program,
        args,
        cwd,
        env: Vec::new(),
    }
}

pub trait ScriptSource: Send + Sync {
    fn id(&self) -> SourceId;
    /// 一覧見出しに使うラベル。
    fn label(&self) -> &str;
    fn discover(&self) -> anyhow::Result<Vec<Script>>;
    /// このスクリプトの実行方法を CommandSpec 化する。
    fn build_command(&self, script: &Script) -> CommandSpec;
    /// スクリプトを新規追加する（ソースが入り口を持つ。書き込み先のファイル/形式は実装側が決める）。
    fn add_script(&self, name: &str, command: &str) -> anyhow::Result<()>;
    /// 既存スクリプトの名前と中身を更新する。
    fn edit_script(&self, old_name: &str, new_name: &str, command: &str) -> anyhow::Result<()>;
}
