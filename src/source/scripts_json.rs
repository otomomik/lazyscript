// scripts.json ソース。package.json の scripts と同形式（名前→コマンド文字列）。
// build_command は生コマンドをシェル経由で直接実行する（npm run は挟まない）。

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use serde_json::Value;

use super::{CommandSpec, Script, ScriptSource, SourceId, json_store};

pub const ID: SourceId = SourceId("scripts.json");

pub struct ScriptsJsonSource {
    root: PathBuf,
}

impl ScriptsJsonSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self) -> PathBuf {
        self.root.join("scripts.json")
    }
}

impl ScriptSource for ScriptsJsonSource {
    fn id(&self) -> SourceId {
        ID
    }

    fn label(&self) -> &str {
        ID.0
    }

    fn discover(&self) -> Result<Vec<Script>> {
        let path = self.path();
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(parse_scripts(&text))
    }

    fn build_command(&self, script: &Script) -> CommandSpec {
        // 生コマンドを現在のシェル経由で実行する。
        super::shell_command(&script.command, self.root.clone())
    }

    fn add_script(&self, name: &str, command: &str) -> Result<()> {
        let path = self.path();
        let mut value = json_store::read(&path);
        let key = scripts_key(&value);
        json_store::set_script(&mut value, key, name, command);
        json_store::write(&path, &value)
    }

    fn edit_script(&self, old_name: &str, new_name: &str, command: &str) -> Result<()> {
        let path = self.path();
        let mut value = json_store::read(&path);
        let key = scripts_key(&value);
        json_store::rename_script(&mut value, key, old_name, new_name, command);
        json_store::write(&path, &value)
    }
}

/// 既存ファイルの形に合わせて書き込み先を選ぶ。
/// `scripts` キーがあればその配下、トップレベルに項目があればトップレベル、
/// 空/新規なら正規形の `scripts` キー配下。
fn scripts_key(value: &Value) -> Option<&'static str> {
    if value.get("scripts").is_some_and(Value::is_object) {
        Some("scripts")
    } else if value.as_object().is_some_and(|o| !o.is_empty()) {
        None
    } else {
        Some("scripts")
    }
}

/// `scripts` キーがあればその配下、無ければトップレベルの map を scripts とみなす。
fn parse_scripts(text: &str) -> Vec<Script> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let map = match json.get("scripts").and_then(|v| v.as_object()) {
        Some(scripts) => Some(scripts),
        None => json.as_object(),
    };
    let Some(map) = map else {
        return Vec::new();
    };
    map.iter()
        .map(|(name, value)| Script {
            name: name.clone(),
            source: ID,
            command: value.as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scripts_key() {
        let text = r#"{ "scripts": { "deploy": "./deploy.sh", "gen": "cargo run" } }"#;
        let scripts = parse_scripts(text);
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0].name, "deploy");
        assert_eq!(scripts[0].command, "./deploy.sh");
    }

    #[test]
    fn parses_top_level_when_no_scripts_key() {
        let text = r#"{ "deploy": "./deploy.sh" }"#;
        let scripts = parse_scripts(text);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "deploy");
        assert_eq!(scripts[0].command, "./deploy.sh");
    }

    #[test]
    fn add_creates_file_when_missing() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lazyscript-sj-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let source = ScriptsJsonSource::new(&dir);
        source.add_script("deploy", "./deploy.sh").unwrap();

        assert!(dir.join("scripts.json").exists());
        let names: Vec<_> = source
            .discover()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["deploy"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_command_runs_raw_via_shell() {
        let source = ScriptsJsonSource::new("/tmp");
        let script = Script {
            name: "deploy".to_string(),
            source: ID,
            command: "echo hi && ls".to_string(),
        };
        let spec = source.build_command(&script);
        #[cfg(not(windows))]
        {
            let expected = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
            assert_eq!(spec.program, expected);
            assert_eq!(spec.args, ["-i", "-c", "echo hi && ls"]);
        }
    }
}
