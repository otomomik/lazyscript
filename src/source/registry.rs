// 複数ディレクトリ × 各ソース を discover してグループ化する。

use std::path::{Path, PathBuf};

use super::ad_hoc::AdHocSource;
use super::package_json::PackageJsonSource;
use super::scripts_json::ScriptsJsonSource;
use super::{AD_HOC, CommandSpec, Script, ScriptSource, SourceId};

/// 1ディレクトリ配下、1ソース（package.json/scripts.json）の見出しと中身。
pub struct SourceGroup {
    pub id: SourceId,
    pub label: String,
    pub scripts: Vec<Script>,
}

/// CLI で渡された1ディレクトリと、その配下のソース群。
pub struct DirGroup {
    /// scripts パネル用の短いラベル（basename）。
    pub label: String,
    /// command パネル用のフルパス（HOME 配下なら `~/...`）。
    pub path: String,
    pub source_groups: Vec<SourceGroup>,
}

pub struct Registry {
    /// (dir_index, source) で実装を引くための一覧。ad-hoc も各ディレクトリ分入れる。
    sources: Vec<(usize, Box<dyn ScriptSource>)>,
    pub directories: Vec<DirGroup>,
}

impl Registry {
    /// 渡された各 root に対して package.json / scripts.json を discover する。
    /// ソース順は固定（package.json → scripts.json）。
    /// ad-hoc は実行用に登録するだけで、一覧グループは持たない（App が動的に保持）。
    pub fn discover(roots: &[PathBuf]) -> Self {
        let mut sources: Vec<(usize, Box<dyn ScriptSource>)> = Vec::new();
        let mut directories: Vec<DirGroup> = Vec::new();

        for (i, root) in roots.iter().enumerate() {
            let mut file_sources: Vec<Box<dyn ScriptSource>> = Vec::new();
            if root.join("package.json").exists() {
                file_sources.push(Box::new(PackageJsonSource::new(root.clone())));
            }
            file_sources.push(Box::new(ScriptsJsonSource::new(root.clone())));

            let source_groups = file_sources
                .iter()
                .map(|source| SourceGroup {
                    id: source.id(),
                    label: source.label().to_string(),
                    scripts: source.discover().unwrap_or_default(),
                })
                .collect();

            directories.push(DirGroup {
                label: dir_basename(root),
                path: dir_full_label(root),
                source_groups,
            });

            for source in file_sources {
                sources.push((i, source));
            }
            sources.push((i, Box::new(AdHocSource::new(root.clone()))));
        }

        Self {
            sources,
            directories,
        }
    }

    /// 全ディレクトリのソースが空（=何も見つからない）かどうか。
    pub fn is_empty(&self) -> bool {
        self.directories.iter().all(|d| d.source_groups.is_empty())
    }

    /// (dir_index, source, name) から登録済みスクリプトを引く。
    pub fn script(&self, dir_index: usize, source: SourceId, name: &str) -> Option<&Script> {
        self.directories
            .get(dir_index)?
            .source_groups
            .iter()
            .find(|g| g.id == source)
            .and_then(|g| g.scripts.iter().find(|s| s.name == name))
    }

    /// 指定 (dir_index, source) の build_command を引く。ad-hoc も同じ経路で引ける。
    pub fn build_command(&self, dir_index: usize, script: &Script) -> Option<CommandSpec> {
        self.sources
            .iter()
            .find(|(i, s)| *i == dir_index && s.id() == script.source)
            .map(|(_, s)| s.build_command(script))
    }

    fn source(&self, dir_index: usize, id: SourceId) -> anyhow::Result<&dyn ScriptSource> {
        self.sources
            .iter()
            .find(|(i, s)| *i == dir_index && s.id() == id)
            .map(|(_, s)| s.as_ref())
            .ok_or_else(|| anyhow::anyhow!("unknown source: {} (#{})", id.0, dir_index))
    }

    pub fn add_script(
        &self,
        dir_index: usize,
        source: SourceId,
        name: &str,
        command: &str,
    ) -> anyhow::Result<()> {
        if source == AD_HOC {
            anyhow::bail!("ad-hoc コマンドは保存されません");
        }
        self.source(dir_index, source)?.add_script(name, command)
    }

    pub fn edit_script(
        &self,
        dir_index: usize,
        source: SourceId,
        old_name: &str,
        new_name: &str,
        command: &str,
    ) -> anyhow::Result<()> {
        if source == AD_HOC {
            anyhow::bail!("ad-hoc コマンドは編集できません");
        }
        self.source(dir_index, source)?
            .edit_script(old_name, new_name, command)
    }

    #[cfg(test)]
    pub fn from_directories(directories: Vec<DirGroup>) -> Self {
        Self {
            sources: Vec::new(),
            directories,
        }
    }
}

/// scripts パネル用のラベル: ディレクトリ名（basename）。
/// `.` などで file_name が取れない場合は canonical の末尾を使う。
fn dir_basename(root: &Path) -> String {
    if let Ok(canon) = root.canonicalize()
        && let Some(name) = canon.file_name().and_then(|s| s.to_str())
    {
        return name.to_string();
    }
    if let Some(name) = root.file_name().and_then(|s| s.to_str()) {
        return name.to_string();
    }
    root.display().to_string()
}

/// command パネル用のラベル: 絶対パス。HOME 配下は `~` に圧縮する。
fn dir_full_label(root: &Path) -> String {
    let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = abs.display().to_string();
    let Some(home) = std::env::var_os("HOME") else {
        return path;
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return path;
    }
    if path == *home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    match path.strip_prefix(&prefix) {
        Some(rest) => format!("~/{rest}"),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::dir_full_label;
    use std::path::Path;

    #[test]
    fn full_label_replaces_home_with_tilde() {
        let Some(home) = std::env::var_os("HOME") else {
            return; // HOME 未設定の環境ではスキップ
        };
        let home = home.to_string_lossy().to_string();
        if home.is_empty() {
            return;
        }
        assert_eq!(dir_full_label(Path::new(&home)), "~");
    }

    #[test]
    fn full_label_keeps_full_path_outside_home() {
        // /tmp は HOME 配下ではないので置換されない。
        let label = dir_full_label(Path::new("/tmp"));
        assert!(label.starts_with('/'));
        assert!(!label.starts_with("~"));
    }
}
