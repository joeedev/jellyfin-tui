use std::process::{Child, ChildStdin, Command, Stdio};

pub(crate) struct SleepInhibitor {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    available: bool,
}

impl SleepInhibitor {
    pub fn new() -> Self {
        let available = Command::new("systemd-inhibit")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !available {
            log::info!("systemd-inhibit not found, sleep inhibition disabled");
        }

        Self { child: None, stdin: None, available }
    }

    pub fn acquire(&mut self) {
        if !self.available || self.child.is_some() {
            return;
        }

        let mut cmd = Command::new("systemd-inhibit");
        cmd.args([
            "--what=sleep",
            "--who=jellyfin-tui",
            "--why=Playing audio",
            "--mode=block",
            "cat",
        ]);
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());

        match cmd.spawn() {
            Ok(mut child) => {
                log::debug!("Acquired sleep inhibitor (pid {})", child.id());
                self.stdin = child.stdin.take();
                self.child = Some(child);
            }
            Err(e) => log::warn!("Failed to acquire sleep inhibitor: {}", e),
        }
    }

    pub fn release(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            log::debug!("Released sleep inhibitor");
        }
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.release();
    }
}
