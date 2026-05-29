// CommandSpec を PTY 上で起動し、出力/終了をイベントチャネルへ流す。

use std::io::Read;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;
use crate::source::CommandSpec;

pub type ProcId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcStatus {
    Running,
    Exited(i32),
    Failed,
}

/// メインループへ届くプロセス由来のイベント。
/// 出力の反映は reader スレッドが parser に直接行い、描画は Tick 側で拾う。
pub enum ProcEvent {
    Exited { id: ProcId, status: ProcStatus },
}

pub struct Process {
    pub script: String,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub status: ProcStatus,
    master: Box<dyn MasterPty + Send>,
    /// プロセスグループ ID（setsid によりリーダー = 子の pid）。グループごと kill するのに使う。
    pgid: Option<i32>,
}

impl Process {
    /// CommandSpec を PTY で起動し、reader/wait スレッドを配線する。
    pub fn spawn(
        id: ProcId,
        script: String,
        spec: &CommandSpec,
        rows: u16,
        cols: u16,
        scrollback: usize,
        tx: UnboundedSender<Event>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let master = pair.master;
        let slave = pair.slave;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        cmd.env("TERM", "xterm-256color");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = slave.spawn_command(cmd)?;
        drop(slave);

        // setsid 済みなので pgid == 子の pid。グループ kill 用に控えておく。
        let pgid = child.process_id().map(|p| p as i32);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, scrollback)));

        // 出力読み取りスレッド。受信バイトは parser へ直接反映する。
        {
            let mut reader = master.try_clone_reader()?;
            let parser = Arc::clone(&parser);
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => parser.lock().unwrap().process(&buf[..n]),
                    }
                }
            });
        }

        // 終了監視スレッド。
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let status = match child.wait() {
                    Ok(s) if s.success() => ProcStatus::Exited(0),
                    // シグナル終了（Ctrl+C 等）は失敗扱い。終了コード 0 と区別する。
                    Ok(s) if s.signal().is_some() => ProcStatus::Failed,
                    Ok(s) => ProcStatus::Exited(s.exit_code() as i32),
                    Err(_) => ProcStatus::Failed,
                };
                let _ = tx.send(Event::Proc(ProcEvent::Exited { id, status }));
            });
        }

        Ok(Self {
            script,
            parser,
            status: ProcStatus::Running,
            master,
            pgid,
        })
    }

    /// PTY と vt100 スクリーンを同じサイズへ追従させる。
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.lock().unwrap().screen_mut().set_size(rows, cols);
    }

    /// プロセスグループごと強制停止する（npm が fork する sh/node 等まで止める）。
    /// 段階4で SIGTERM→猶予→SIGKILL の段階的停止に拡張する予定。
    pub fn kill(&self) {
        #[cfg(unix)]
        {
            self.signal(nix::sys::signal::Signal::SIGTERM);
            self.signal(nix::sys::signal::Signal::SIGKILL);
        }
    }

    /// Ctrl+C 相当。プロセスグループに SIGINT を送って割り込む（TUI は閉じない）。
    pub fn interrupt(&self) {
        #[cfg(unix)]
        self.signal(nix::sys::signal::Signal::SIGINT);
    }

    #[cfg(unix)]
    fn signal(&self, sig: nix::sys::signal::Signal) {
        if let Some(pid) = self.pgid {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), sig);
        }
    }

    #[cfg(test)]
    fn pgid(&self) -> Option<i32> {
        self.pgid
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::source::CommandSpec;
    use std::time::Duration;

    /// kill が「直接の子」だけでなくプロセスグループ全体を止めることを確認する。
    #[test]
    fn kill_terminates_whole_process_group() {
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let spec = CommandSpec {
            program: "sh".to_string(),
            // リーダー(sh)が2つの子(sleep)を fork する小さなツリー。
            args: vec!["-c".to_string(), "sleep 30 & sleep 30 & wait".to_string()],
            cwd: std::env::current_dir().unwrap(),
            env: Vec::new(),
        };
        let proc = Process::spawn(1, "tree".to_string(), &spec, 24, 80, 100, tx).unwrap();
        let pgid = Pid::from_raw(proc.pgid().expect("pgid"));

        std::thread::sleep(Duration::from_millis(200));
        // 起動直後はグループが生きている。
        assert!(killpg(pgid, None).is_ok(), "group should be alive before kill");

        proc.kill();
        std::thread::sleep(Duration::from_millis(400));
        // kill 後はグループにメンバーが居ない（ESRCH）。
        assert!(
            killpg(pgid, None).is_err(),
            "group should be gone after kill"
        );
    }
}
