// その場限りのコマンド実行ソース。ファイルに紐づかない。
// 一覧（動的リスト）は App 側が保持し、ここは「実行方法」だけを担う。

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::{AD_HOC, CommandSpec, Script, ScriptSource, SourceId, shell_command};

pub struct AdHocSource {
    root: PathBuf,
}

impl AdHocSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl ScriptSource for AdHocSource {
    fn id(&self) -> SourceId {
        AD_HOC
    }

    fn label(&self) -> &str {
        AD_HOC.0
    }

    fn discover(&self) -> Result<Vec<Script>> {
        // 動的リストは App が保持するため、ファイル由来の項目は無い。
        Ok(Vec::new())
    }

    fn build_command(&self, script: &Script) -> CommandSpec {
        // 名前＝コマンド。現在のシェルで実行する。
        shell_command(&script.command, self.root.clone())
    }

    fn add_script(&self, _name: &str, _command: &str) -> Result<()> {
        bail!("ad-hoc コマンドは保存されません")
    }

    fn edit_script(&self, _old_name: &str, _new_name: &str, _command: &str) -> Result<()> {
        bail!("ad-hoc コマンドは編集できません")
    }
}
