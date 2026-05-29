// クリップボードへのコピー。OS クリップボード（arboard）と、SSH 越しでも効く OSC52 の両方へ書く。

use std::io::Write;

use anyhow::Result;
use base64::Engine;

use crate::config::ClipboardSink;

pub fn copy(text: &str, sink: ClipboardSink) -> Result<()> {
    match sink {
        ClipboardSink::System => copy_system(text),
        ClipboardSink::Osc52 => {
            copy_osc52(text);
            Ok(())
        }
        ClipboardSink::Both => {
            // OSC52 はベストエフォート。OS 側の成否を返す。
            copy_osc52(text);
            copy_system(text)
        }
    }
}

fn copy_system(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.to_string())?;
    Ok(())
}

/// 端末経由でローカルのクリップボードへコピーする OSC52 シーケンスを出力する。
fn copy_osc52(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
    let _ = stdout.flush();
}
