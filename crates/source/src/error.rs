use std::fmt;
use std::process::ExitStatus;

#[derive(Debug)]
pub struct NixCommandFailure {
    pub command: &'static str,
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl fmt::Display for NixCommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            self.command,
            self.status,
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr)
        )
    }
}

impl std::error::Error for NixCommandFailure {}
