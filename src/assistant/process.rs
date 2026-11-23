use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Event, Mode, launch};

// Bound both directions, including malformed or unterminated JSONL.
const MAX_FRAME: usize = 1 << 20;
const EXIT_GRACE: Duration = Duration::from_millis(450);

pub(super) struct Process {
    child: Child,
    input: Option<SyncSender<Vec<u8>>>,
    read_next: Option<Sender<()>>,
    reaped: bool,
    backend: &'static str,
    pub(super) generation: u64,
    pub(super) launch: launch::Launch,
}

impl Process {
    pub(super) fn spawn(
        book: &Path,
        mode: Mode,
        generation: u64,
        events: Sender<Event>,
    ) -> Result<Self, String> {
        let mut launch = launch::prepare(book, mode)?;
        let backend = mode.backend();
        let command = launch
            .command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Could not start {backend}: {e}. Is it installed and on PATH?"))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stdin = child.stdin.take().expect("piped stdin");
        let (input, outgoing) = mpsc::sync_channel::<Vec<u8>>(8);
        let (read_next, permits) = mpsc::channel();
        // Own the child before starting threads: even partial startup reaps it.
        let process = Self {
            child,
            input: Some(input),
            read_next: Some(read_next),
            reaped: false,
            backend,
            generation,
            launch,
        };
        let frames = events.clone();
        thread::Builder::new()
            .name("assistant-stdout".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut bytes = Vec::new();
                loop {
                    let frame = read_frame(&mut reader, &mut bytes, backend);
                    let terminal = frame.is_err();
                    if frames.send(Event::Frame(generation, frame)).is_err() || terminal {
                        break;
                    }
                    // Bound the actor mailbox to one frame per reader, but never let
                    // a blocked pipe prevent the actor accepting a cancellation.
                    if permits.recv().is_err() {
                        let _ = io::copy(&mut reader, &mut io::sink());
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        thread::Builder::new()
            .name("assistant-stdin".into())
            .spawn(move || {
                for bytes in outgoing {
                    if stdin.write_all(&bytes).is_err() {
                        let _ = events.send(Event::Frame(
                            generation,
                            Err(format!("{backend} closed its input pipe")),
                        ));
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(process)
    }

    pub(super) fn advance_reader(&self) {
        if let Some(next) = &self.read_next {
            let _ = next.send(());
        }
    }

    pub(super) fn send(&self, frame: &Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(frame).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME {
            return Err("Request exceeds Bookokrat's 1 MiB RPC frame limit".into());
        }
        self.input
            .as_ref()
            .ok_or_else(|| format!("{} input is closed", self.backend))?
            .try_send(bytes)
            .map_err(|_| format!("{} is not accepting input", self.backend))
    }

    // Closing stdin lets OMP flush session records; an unresponsive child
    // cannot hold up a book switch or reader shutdown indefinitely.
    pub(super) fn close(&mut self) -> Result<(), String> {
        self.input.take();
        self.read_next.take();
        let until = Instant::now() + EXIT_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(format!("{} exited with {status}", self.backend))
                    };
                }
                Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(10)),
                _ => break,
            }
        }
        #[cfg(unix)]
        // SAFETY: this unreaped child leads the process group we created at spawn.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        self.reaped = self.child.wait().is_ok();
        Err(format!("{} did not shut down cleanly", self.backend))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.close();
        }
    }
}

fn read_frame(
    reader: &mut impl BufRead,
    bytes: &mut Vec<u8>,
    backend: &str,
) -> Result<Value, String> {
    bytes.clear();
    // take() bounds memory even if a broken executable never writes a newline.
    let count = reader
        .take(MAX_FRAME as u64)
        .read_until(b'\n', bytes)
        .map_err(|e| format!("Could not read from {backend}: {e}"))?;
    if count == 0 {
        return Err(format!(
            "{backend} exited before completing the request. Check your {backend} installation, model and login."
        ));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(format!("Incomplete or oversized {backend} frame"));
    }
    serde_json::from_slice(bytes).map_err(|_| format!("{backend} returned invalid JSON"))
}
