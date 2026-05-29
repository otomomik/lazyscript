// JSON ベースのソース（package.json / scripts.json）共通の読み書きヘルパ。
// 各ソースの「入り口」（どのファイル・どのキー配下か）は呼び出し側が決め、
// ここでは map への upsert / rename と順序保持の整形書き込みだけを担う。

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

/// ファイルを読む。無い/壊れている場合は空オブジェクト。
pub fn read(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// 2スペース整形＋末尾改行で書き出す（親ディレクトリが無ければ作る）。
pub fn write(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// スクリプトを追加/更新する（既存キーは位置を保ったまま値を更新）。
pub fn set_script(value: &mut Value, key: Option<&str>, name: &str, command: &str) {
    scripts_map(value, key).insert(name.to_string(), Value::String(command.to_string()));
}

/// 名前を変更しつつ値も更新する（順序を保つ）。
pub fn rename_script(value: &mut Value, key: Option<&str>, old: &str, new: &str, command: &str) {
    let map = scripts_map(value, key);
    if old == new {
        map.insert(new.to_string(), Value::String(command.to_string()));
        return;
    }
    let mut rebuilt = Map::new();
    let mut replaced = false;
    for (k, v) in map.iter() {
        if k == old {
            rebuilt.insert(new.to_string(), Value::String(command.to_string()));
            replaced = true;
        } else {
            rebuilt.insert(k.clone(), v.clone());
        }
    }
    if !replaced {
        rebuilt.insert(new.to_string(), Value::String(command.to_string()));
    }
    *map = rebuilt;
}

/// key 配下（None ならトップレベル）の map を可変で取り出す。無ければ作る。
fn scripts_map<'a>(value: &'a mut Value, key: Option<&str>) -> &'a mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    match key {
        None => value.as_object_mut().unwrap(),
        Some(k) => {
            let obj = value.as_object_mut().unwrap();
            if !obj.get(k).is_some_and(Value::is_object) {
                obj.insert(k.to_string(), Value::Object(Map::new()));
            }
            obj.get_mut(k).unwrap().as_object_mut().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_appends_and_updates_in_place() {
        let mut v: Value = serde_json::from_str(r#"{"scripts":{"a":"1","b":"2"}}"#).unwrap();
        set_script(&mut v, Some("scripts"), "c", "3"); // 追加は末尾
        set_script(&mut v, Some("scripts"), "a", "9"); // 既存は位置維持で更新
        let s = serde_json::to_string(&v["scripts"]).unwrap();
        assert_eq!(s, r#"{"a":"9","b":"2","c":"3"}"#);
    }

    #[test]
    fn rename_keeps_position() {
        let mut v: Value =
            serde_json::from_str(r#"{"scripts":{"a":"1","b":"2","c":"3"}}"#).unwrap();
        rename_script(&mut v, Some("scripts"), "b", "bb", "22");
        let s = serde_json::to_string(&v["scripts"]).unwrap();
        assert_eq!(s, r#"{"a":"1","bb":"22","c":"3"}"#);
    }

    #[test]
    fn set_creates_object_and_key_when_missing() {
        let mut v = Value::Object(Map::new());
        set_script(&mut v, Some("scripts"), "x", "y");
        assert_eq!(v["scripts"]["x"], Value::String("y".into()));
    }

    #[test]
    fn top_level_layout() {
        let mut v: Value = serde_json::from_str(r#"{"deploy":"./d.sh"}"#).unwrap();
        set_script(&mut v, None, "gen", "cargo run");
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, r#"{"deploy":"./d.sh","gen":"cargo run"}"#);
    }
}
