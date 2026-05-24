#![cfg(unix)]
#![allow(dead_code)]

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub const DEFAULT_ROWS: u16 = 30;
pub const DEFAULT_COLS: u16 = 100;
pub const DEFAULT_SCREEN_WAIT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub struct ProfileSpec {
    pub name: String,
    pub command: Vec<String>,
    pub timeout_secs: Option<u64>,
}

impl ProfileSpec {
    pub fn new(
        name: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into_iter().map(Into::into).collect(),
            timeout_secs: Some(60),
        }
    }

    pub fn fast(name: impl Into<String>, marker: impl AsRef<str>) -> Self {
        Self::new(
            name,
            [
                "sh".to_string(),
                "-c".to_string(),
                format!("printf '%s\\n' {}", shell_quote(marker.as_ref())),
            ],
        )
    }

    pub fn slow_shell_wrapper(name: impl Into<String>, marker: impl AsRef<str>) -> Self {
        let marker = marker.as_ref().replace('\'', "");
        Self::new(
            name,
            [
                "sh".to_string(),
                "-c".to_string(),
                format!("printf '%s\\n' {marker}; sleep 30 & wait"),
            ],
        )
    }
}

#[derive(Clone, Debug)]
pub struct HarnessConfig {
    pub profiles: Vec<ProfileSpec>,
    pub active_profile: String,
    pub rows: u16,
    pub cols: u16,
    pub extra_env: Vec<(String, String)>,
}

impl HarnessConfig {
    pub fn new(profiles: Vec<ProfileSpec>) -> Self {
        Self {
            active_profile: profiles
                .first()
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| "default".to_string()),
            profiles,
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            extra_env: Vec::new(),
        }
    }

    pub fn with_profile(profile: ProfileSpec) -> Self {
        Self::new(vec![profile])
    }

    pub fn default_fast() -> Self {
        Self::with_profile(ProfileSpec::fast("default", "FORGE_PTY_FAST_MARKER"))
    }

    pub fn size(mut self, rows: u16, cols: u16) -> Self {
        self.rows = rows;
        self.cols = cols;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Key {
    Enter,
    Esc,
    Tab,
    Backspace,
    CtrlC,
    CtrlD,
    CtrlL,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

impl Key {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Enter => b"\r",
            Self::Esc => b"\x1b",
            Self::Tab => b"\t",
            Self::Backspace => b"\x7f",
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::CtrlL => b"\x0c",
            Self::Up => b"\x1b[A",
            Self::Down => b"\x1b[B",
            Self::Right => b"\x1b[C",
            Self::Left => b"\x1b[D",
            Self::PageUp => b"\x1b[5~",
            Self::PageDown => b"\x1b[6~",
            Self::Home => b"\x1b[H",
            Self::End => b"\x1b[F",
        }
    }
}

pub struct PtyHarness {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    child_pid: Option<u32>,
    reader_thread: Option<thread::JoinHandle<()>>,
    rows: u16,
    cols: u16,
    _temp: TempDir,
}

impl PtyHarness {
    pub fn launch(config: HarnessConfig) -> anyhow::Result<Self> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("config.toml");
        write_config(&config_path, &config.profiles)?;

        let runs = temp.path().join("runs");
        let sessions = temp.path().join("sessions");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home)?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let forge_bin = PathBuf::from(env!("CARGO_BIN_EXE_forge"));
        let mut builder = CommandBuilder::new(&forge_bin);
        builder.arg("--config");
        builder.arg(&config_path);
        builder.arg("--runs-dir");
        builder.arg(&runs);
        builder.arg("--sessions-dir");
        builder.arg(&sessions);
        builder.arg("--profile");
        builder.arg(&config.active_profile);
        builder.env_clear();
        if let Ok(path_env) = std::env::var("PATH") {
            builder.env("PATH", path_env);
        }
        builder.env("HOME", &home);
        builder.env("TERM", "xterm-256color");
        builder.env("LANG", "C.UTF-8");
        for (key, value) in config.extra_env {
            builder.env(key, value);
        }
        builder.cwd(temp.path());

        let child = pair.slave.spawn_command(builder)?;
        let child_pid = child.process_id();

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            config.rows,
            config.cols,
            1000,
        )));
        let parser_for_thread = Arc::clone(&parser);
        let mut reader = pair.master.try_clone_reader()?;
        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => parser_for_thread.lock().unwrap().process(&buf[..n]),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                }
            }
        });

        let writer = pair.master.take_writer()?;

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            child_pid,
            reader_thread: Some(reader_thread),
            rows: config.rows,
            cols: config.cols,
            _temp: temp,
        })
    }

    pub fn launch_with_profile(profile: ProfileSpec) -> anyhow::Result<Self> {
        Self::launch(HarnessConfig::with_profile(profile))
    }

    pub fn launch_default() -> anyhow::Result<Self> {
        Self::launch(HarnessConfig::default_fast())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child_pid
    }

    pub fn screen(&self) -> String {
        normalize_screen(&self.parser.lock().unwrap().screen().contents())
    }

    pub fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn send_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.send(text.as_bytes())
    }

    pub fn send_line(&mut self, text: &str) -> anyhow::Result<()> {
        self.send_text(text)?;
        self.send_key(Key::Enter)
    }

    pub fn send_key(&mut self, key: Key) -> anyhow::Result<()> {
        self.send(key.bytes())
    }

    pub fn wait_for<F>(
        &self,
        label: &str,
        timeout: Duration,
        mut predicate: F,
    ) -> anyhow::Result<String>
    where
        F: FnMut(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.screen();
            if predicate(&snap) {
                return Ok(snap);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for `{label}`. Last screen:\n---\n{snap}\n---");
            }
            thread::sleep(Duration::from_millis(40));
        }
    }

    pub fn wait_for_contains(&self, needle: &str) -> anyhow::Result<String> {
        self.wait_for(
            &format!("screen contains {needle:?}"),
            DEFAULT_SCREEN_WAIT,
            |screen| screen.contains(needle),
        )
    }

    pub fn wait_for_not_contains(&self, needle: &str) -> anyhow::Result<String> {
        self.wait_for(
            &format!("screen does not contain {needle:?}"),
            DEFAULT_SCREEN_WAIT,
            |screen| !screen.contains(needle),
        )
    }

    pub fn wait_for_screen_change(
        &self,
        baseline: &str,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        self.wait_for("screen changed", timeout, |screen| screen != baseline)
    }

    pub fn try_wait(&mut self) -> anyhow::Result<Option<portable_pty::ExitStatus>> {
        self.child.try_wait().map_err(Into::into)
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) -> anyhow::Result<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "process did not exit within {:?}. Last screen:\n---\n{}\n---",
                    timeout,
                    self.screen()
                );
            }
            thread::sleep(Duration::from_millis(40));
        }
    }

    pub fn exit_cleanly(&mut self) -> anyhow::Result<portable_pty::ExitStatus> {
        self.send_line("/exit")?;
        self.wait_for_exit(Duration::from_secs(5))
    }

    pub fn shutdown(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            self.join_reader();
            return;
        }

        let _ = self.send_line("/exit");
        if self.wait_for_exit(Duration::from_secs(2)).is_ok() {
            self.join_reader();
            return;
        }

        self.terminate_process_group(libc::SIGTERM);
        if self.wait_for_exit(Duration::from_millis(500)).is_ok() {
            self.join_reader();
            return;
        }

        self.terminate_process_group(libc::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_reader();
    }

    pub fn pid_alive(pid: u32) -> bool {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn terminate_process_group(&mut self, signal: libc::c_int) {
        if let Some(pid) = self.child_pid {
            unsafe {
                libc::kill(-(pid as libc::pid_t), signal);
                libc::kill(pid as libc::pid_t, signal);
            }
        }
    }

    fn join_reader(&mut self) {
        let _ = std::mem::replace(&mut self.writer, Box::new(std::io::sink()));
        let _ = self.master.resize(PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn write_config(path: &Path, profiles: &[ProfileSpec]) -> anyhow::Result<()> {
    let mut body = String::new();
    for profile in profiles {
        writeln!(&mut body, "[profiles.{}]", profile.name)?;
        write!(&mut body, "command = [")?;
        for (index, arg) in profile.command.iter().enumerate() {
            if index > 0 {
                write!(&mut body, ", ")?;
            }
            write!(&mut body, "{arg:?}")?;
        }
        writeln!(&mut body, "]")?;
        if let Some(timeout_secs) = profile.timeout_secs {
            writeln!(&mut body, "timeout_secs = {timeout_secs}")?;
        }
        writeln!(&mut body)?;
    }
    std::fs::write(path, body)?;
    Ok(())
}

fn normalize_screen(screen: &str) -> String {
    screen
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
