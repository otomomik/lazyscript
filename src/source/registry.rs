// 複数ソースを discover してソース順にグループ化する。

use std::path::Path;

use super::ad_hoc::AdHocSource;
use super::package_json::PackageJsonSource;
use super::scripts_json::ScriptsJsonSource;
use super::{CommandSpec, Script, ScriptSource, SourceId};

/// 一覧の1見出し（ソース）と、その配下のスクリプト群。
pub struct SourceGroup {
    pub id: SourceId,
    pub label: String,
    pub scripts: Vec<Script>,
}

pub struct Registry {
    sources: Vec<Box<dyn ScriptSource>>,
    pub groups: Vec<SourceGroup>,
}

impl Registry {
    /// cwd 配下の各ソースを discover する。ソース順は固定（package.json → scripts.json）。
    /// package.json はファイルがある時だけ、scripts.json は常に見出しとして並べる。
    /// ad-hoc は実行用にのみ登録し、一覧グループは持たない（動的リストは App が保持）。
    pub fn discover(root: &Path) -> Self {
        let mut file_sources: Vec<Box<dyn ScriptSource>> = Vec::new();
        if root.join("package.json").exists() {
            file_sources.push(Box::new(PackageJsonSource::new(root)));
        }
        file_sources.push(Box::new(ScriptsJsonSource::new(root)));

        let groups = file_sources
            .iter()
            .map(|source| SourceGroup {
                id: source.id(),
                label: source.label().to_string(),
                scripts: source.discover().unwrap_or_default(),
            })
            .collect();

        let mut sources = file_sources;
        sources.push(Box::new(AdHocSource::new(root)));

        Self { sources, groups }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// ソースと名前から登録済みスクリプトを引く。
    pub fn script(&self, source: SourceId, name: &str) -> Option<&Script> {
        self.groups
            .iter()
            .find(|g| g.id == source)
            .and_then(|g| g.scripts.iter().find(|s| s.name == name))
    }

    /// 指定ソースの build_command を引く。
    pub fn build_command(&self, script: &Script) -> Option<CommandSpec> {
        self.sources
            .iter()
            .find(|s| s.id() == script.source)
            .map(|s| s.build_command(script))
    }

    fn source(&self, id: SourceId) -> anyhow::Result<&dyn ScriptSource> {
        self.sources
            .iter()
            .find(|s| s.id() == id)
            .map(|s| s.as_ref())
            .ok_or_else(|| anyhow::anyhow!("unknown source: {}", id.0))
    }

    pub fn add_script(&self, source: SourceId, name: &str, command: &str) -> anyhow::Result<()> {
        self.source(source)?.add_script(name, command)
    }

    pub fn edit_script(
        &self,
        source: SourceId,
        old_name: &str,
        new_name: &str,
        command: &str,
    ) -> anyhow::Result<()> {
        self.source(source)?
            .edit_script(old_name, new_name, command)
    }

    #[cfg(test)]
    pub fn from_groups(groups: Vec<SourceGroup>) -> Self {
        Self {
            sources: Vec::new(),
            groups,
        }
    }
}
