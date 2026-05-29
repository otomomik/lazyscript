// package.json ソース。scripts を順序保持で列挙し、npm/pnpm/yarn のいずれで
// 実行するかをロックファイルと packageManager フィールドから判定する。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{CommandSpec, Script, ScriptSource, SourceId, json_store};

pub const ID: SourceId = SourceId("package.json");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
}

pub struct PackageJsonSource {
    root: PathBuf,
}

impl PackageJsonSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self) -> PathBuf {
        self.root.join("package.json")
    }
}

impl ScriptSource for PackageJsonSource {
    fn id(&self) -> SourceId {
        ID
    }

    fn label(&self) -> &str {
        ID.0
    }

    fn discover(&self) -> Result<Vec<Script>> {
        let path = self.path();
        let text =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(parse_scripts(&text))
    }

    fn build_command(&self, script: &Script) -> CommandSpec {
        let text = fs::read_to_string(self.path()).unwrap_or_default();
        let pm = detect_pm(&self.root, &text);
        let (program, mut args) = match pm {
            PackageManager::Pnpm => ("pnpm", vec!["run".to_string()]),
            PackageManager::Npm => ("npm", vec!["run".to_string()]),
            PackageManager::Yarn => ("yarn", vec![]),
        };
        args.push(script.name.clone());
        CommandSpec {
            program: program.to_string(),
            args,
            cwd: self.root.clone(),
            env: Vec::new(),
        }
    }

    fn add_script(&self, name: &str, command: &str) -> Result<()> {
        let path = self.path();
        let mut value = json_store::read(&path);
        json_store::set_script(&mut value, Some("scripts"), name, command);
        json_store::write(&path, &value)
    }

    fn edit_script(&self, old_name: &str, new_name: &str, command: &str) -> Result<()> {
        let path = self.path();
        let mut value = json_store::read(&path);
        json_store::rename_script(&mut value, Some("scripts"), old_name, new_name, command);
        json_store::write(&path, &value)
    }
}

/// package.json のテキストから scripts を順序通りに取り出す。
fn parse_scripts(text: &str) -> Vec<Script> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(scripts) = json.get("scripts").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    scripts
        .iter()
        .map(|(name, value)| Script {
            name: name.clone(),
            source: ID,
            command: value.as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// packageManager フィールド優先、無ければロックファイルで判定。既定は npm。
fn detect_pm(root: &Path, package_json_text: &str) -> PackageManager {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(package_json_text)
        && let Some(field) = json.get("packageManager").and_then(|v| v.as_str())
    {
        if field.starts_with("pnpm") {
            return PackageManager::Pnpm;
        }
        if field.starts_with("yarn") {
            return PackageManager::Yarn;
        }
        if field.starts_with("npm") {
            return PackageManager::Npm;
        }
    }
    if root.join("pnpm-lock.yaml").exists() {
        return PackageManager::Pnpm;
    }
    if root.join("yarn.lock").exists() {
        return PackageManager::Yarn;
    }
    PackageManager::Npm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scripts_in_file_order() {
        let text = r#"{ "scripts": { "dev": "x", "build": "y", "test": "z" } }"#;
        let names: Vec<_> = parse_scripts(text)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["dev", "build", "test"]);
    }

    #[test]
    fn no_scripts_field_yields_empty() {
        assert!(parse_scripts(r#"{ "name": "x" }"#).is_empty());
        assert!(parse_scripts("not json").is_empty());
    }

    #[test]
    fn package_manager_field_wins_over_lockfile() {
        let dir = tempdir();
        std::fs::write(dir.join("yarn.lock"), "").unwrap();
        let pm = detect_pm(&dir, r#"{ "packageManager": "pnpm@9.0.0" }"#);
        assert_eq!(pm, PackageManager::Pnpm);
        cleanup(&dir);
    }

    #[test]
    fn lockfile_detects_manager() {
        let dir = tempdir();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_pm(&dir, "{}"), PackageManager::Pnpm);
        cleanup(&dir);
    }

    #[test]
    fn defaults_to_npm() {
        let dir = tempdir();
        assert_eq!(detect_pm(&dir, "{}"), PackageManager::Npm);
        cleanup(&dir);
    }

    #[test]
    fn build_command_uses_run_for_npm() {
        let dir = tempdir();
        std::fs::write(dir.join("package.json"), r#"{ "scripts": { "dev": "x" } }"#).unwrap();
        let source = PackageJsonSource::new(&dir);
        let script = Script {
            name: "dev".to_string(),
            source: ID,
            command: "x".to_string(),
        };
        let spec = source.build_command(&script);
        assert_eq!(spec.program, "npm");
        assert_eq!(spec.args, ["run", "dev"]);
        assert_eq!(spec.cwd, dir);
        cleanup(&dir);
    }

    #[test]
    fn add_and_edit_roundtrip_preserves_other_fields() {
        let dir = tempdir();
        std::fs::write(
            dir.join("package.json"),
            r#"{ "name": "x", "scripts": { "dev": "vite" } }"#,
        )
        .unwrap();
        let source = PackageJsonSource::new(&dir);

        source.add_script("build", "vite build").unwrap();
        source.edit_script("dev", "start", "vite serve").unwrap();

        let names: Vec<_> = source
            .discover()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["start", "build"]); // 名前変更は位置維持、追加は末尾

        let text = std::fs::read_to_string(dir.join("package.json")).unwrap();
        assert!(text.contains("\"name\"")); // 他フィールドは残る
        assert!(text.contains("vite serve"));
        cleanup(&dir);
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lazyscript-test-{}-{}",
            std::process::id(),
            fastcount()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    // 衝突回避用の単調増加カウンタ。
    fn fastcount() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        C.fetch_add(1, Ordering::Relaxed)
    }
}
