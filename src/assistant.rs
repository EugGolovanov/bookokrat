mod launch;
mod process;
mod protocol;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use process::Process;
use protocol::Dialog;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Chat,
    Agent,
}

pub(crate) struct Request {
    pub book: PathBuf,
    pub mode: Mode,
    pub prompt: String,
    pub quote: Option<String>,
}

#[derive(Debug)]
pub(crate) enum Update {
    Text(String),
    Status(String),
    Approval { description: String },
    Done,
    Failed(String),
}

struct Delivery {
    update: Update,
    dialog: Option<String>,
}

struct Active {
    mode: Mode,
    seq: u64,
    inbox: Receiver<Delivery>,
    displayed_dialog: Option<String>,
}

// Each mode has its own process owner. A chat can neither inherit the agent's
// session nor evict its warm process. Nothing starts just by opening the tab.
#[derive(Default)]
pub(crate) struct Runtime {
    chat: Option<Worker>,
    agent: Option<Worker>,
    active: Option<Active>,
    seq: u64,
    retiring: Vec<JoinHandle<()>>,
}

impl Runtime {
    /// Invalidate the receiver immediately; flushing an old session and reaping
    /// its child run off the reader's book-switch/input path.
    pub(crate) fn leave_book(&mut self) {
        self.active.take();
        self.retiring.retain(|thread| !thread.is_finished());
        for worker in [self.chat.take(), self.agent.take()].into_iter().flatten() {
            let _ = worker.events.send(Event::Stop);
            self.retiring.push(worker.join);
        }
    }

    fn worker(&self, mode: Mode) -> Option<&Worker> {
        match mode {
            Mode::Chat => self.chat.as_ref(),
            Mode::Agent => self.agent.as_ref(),
        }
    }

    pub(crate) fn submit(&mut self, req: Request) -> Result<(), String> {
        if self.active.is_some() {
            return Err("Another assistant request is active".into());
        }
        if req.prompt.trim().is_empty() {
            return Err("Enter a question first".into());
        }
        let mode = req.mode;
        let slot = match mode {
            Mode::Chat => &mut self.chat,
            Mode::Agent => &mut self.agent,
        };
        if slot.is_none() {
            *slot = Some(Worker::start()?);
        }
        self.seq += 1;
        // Dropping this receiver invalidates all events from this submission.
        let (reply, inbox) = mpsc::sync_channel(128);
        slot.as_ref()
            .unwrap()
            .events
            .send(Event::Run(self.seq, req, reply))
            .map_err(|_| "Assistant worker stopped".to_owned())?;
        self.active = Some(Active {
            mode,
            seq: self.seq,
            inbox,
            displayed_dialog: None,
        });
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> Option<Update> {
        let active = self.active.as_mut()?;
        let delivery = match active.inbox.try_recv() {
            Ok(delivery) => delivery,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Delivery {
                update: Update::Failed("Assistant worker disconnected".into()),
                dialog: None,
            },
        };
        if let Some(id) = delivery.dialog {
            active.displayed_dialog = Some(id);
        }
        if matches!(&delivery.update, Update::Status(s) if s == "Approval withdrawn") {
            active.displayed_dialog = None;
        }
        if matches!(delivery.update, Update::Done | Update::Failed(_)) {
            self.active = None;
        }
        Some(delivery.update)
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(active) = &self.active
            && let Some(w) = self.worker(active.mode)
        {
            let _ = w.events.send(Event::Cancel(active.seq));
        }
    }

    pub(crate) fn approve(&mut self, yes: bool) {
        let Some(active) = &mut self.active else {
            return;
        };
        let Some(id) = active.displayed_dialog.take() else {
            return;
        };
        let (mode, seq) = (active.mode, active.seq);
        if let Some(w) = self.worker(mode) {
            let _ = w.events.send(Event::Answer(seq, id, yes));
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.leave_book();
        for thread in self.retiring.drain(..) {
            let _ = thread.join();
        }
    }
}

struct Worker {
    events: Sender<Event>,
    join: JoinHandle<()>,
}

impl Worker {
    fn start() -> Result<Self, String> {
        let (events, rx) = mpsc::channel();
        let tx = events.clone();
        let join = thread::Builder::new()
            .name("bookokrat-omp".into())
            .spawn(move || Session::default().run(rx, tx))
            .map_err(|e| format!("Cannot start assistant worker: {e}"))?;
        Ok(Self { events, join })
    }
}

enum Event {
    Run(u64, Request, SyncSender<Delivery>),
    Cancel(u64),
    Answer(u64, String, bool),
    Frame(u64, Result<Value, String>),
    Stop,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Ready,
    Filter,
    Preflight,
    Running,
    Settling,
}

struct Turn {
    seq: u64,
    id: String,
    mode: Mode,
    reply: SyncSender<Delivery>,
    pending_prompt: Option<Value>,
    phase: Phase,
    deadline: Instant,
    dialog: Option<Dialog>,
    streamed: bool,
    prior_text: bool,
    background_error: Option<String>,
}

impl Turn {
    fn new(seq: u64, req: &Request, reply: SyncSender<Delivery>) -> Self {
        let id = format!("reader-{seq}");
        let pending_prompt = Some(protocol::prompt(&id, req));
        Self {
            seq,
            id,
            mode: req.mode,
            reply,
            pending_prompt,
            phase: Phase::Ready,
            deadline: Instant::now() + Duration::from_secs(60),
            dialog: None,
            streamed: false,
            prior_text: false,
            background_error: None,
        }
    }

    fn update(&self, update: Update) {
        let _ = self.reply.send(Delivery {
            update,
            dialog: None,
        });
    }
    fn status(&self, text: impl Into<String>) {
        self.update(Update::Status(text.into()));
    }

    fn preflight(&mut self, send: impl FnOnce(&Value) -> Result<(), String>) -> Result<(), String> {
        send(&json!({"type":"get_state", "id":"state"}))?;
        self.phase = Phase::Preflight;
        Ok(())
    }

    fn send_prompt(
        &mut self,
        send: impl FnOnce(&Value) -> Result<(), String>,
    ) -> Result<(), String> {
        let prompt = self
            .pending_prompt
            .take()
            .ok_or("Prompt was already sent")?;
        send(&prompt)?;
        self.phase = Phase::Running;
        self.status("Waiting for OMP");
        Ok(())
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.streamed && self.prior_text {
            self.update(Update::Text("\n\n".into()));
        }
        self.streamed = true;
        self.prior_text = true;
        self.update(Update::Text(text.into()));
    }
}

#[derive(Default)]
struct Session {
    process: Option<Process>,
    book: PathBuf,
    generation: u64,
    turn: Option<Turn>,
}

impl Session {
    fn run(&mut self, inbox: Receiver<Event>, events: Sender<Event>) {
        loop {
            let event = if let Some(turn) = &self.turn
                && matches!(turn.phase, Phase::Ready | Phase::Filter | Phase::Preflight)
            {
                let left = turn.deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    self.fail("OMP request timed out".into());
                    continue;
                }
                match inbox.recv_timeout(left) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.fail("OMP request timed out".into());
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else if let Ok(event) = inbox.recv() {
                event
            } else {
                break;
            };
            let result = match event {
                Event::Stop => break,
                Event::Run(seq, req, reply) => self.begin(seq, req, reply, &events),
                Event::Frame(generation, frame) => {
                    let Some(process) = &self.process else {
                        continue;
                    };
                    if process.generation != generation {
                        continue;
                    }
                    process.advance_reader();
                    frame.and_then(|frame| self.frame(&frame))
                }
                Event::Cancel(seq) if self.turn.as_ref().is_some_and(|t| t.seq == seq) => {
                    self.stop_process();
                    if let Some(turn) = self.turn.take() {
                        turn.update(Update::Done);
                    }
                    Ok(())
                }
                Event::Answer(seq, id, yes) => {
                    let Some(t) = &mut self.turn else {
                        continue;
                    };
                    if t.seq != seq || !t.dialog.as_ref().is_some_and(|d| d.id == id) {
                        continue;
                    }
                    let answer = t.dialog.take().unwrap().reply(yes);
                    t.status(if yes { "Approved once" } else { "Denied" });
                    self.process.as_ref().unwrap().send(&answer)
                }
                Event::Cancel(_) => continue,
            };
            if let Err(error) = result {
                self.fail(error);
            }
        }
        self.stop_process();
    }

    fn begin(
        &mut self,
        seq: u64,
        req: Request,
        reply: SyncSender<Delivery>,
        events: &Sender<Event>,
    ) -> Result<(), String> {
        if self.process.is_some() && (req.mode == Mode::Chat || req.book != self.book) {
            self.stop_process();
        }
        let turn = self.turn.insert(Turn::new(seq, &req, reply));
        let reuse = self.process.is_some();
        let process = match &mut self.process {
            Some(process) => process,
            slot @ None => {
                turn.status("Starting OMP");
                self.generation += 1;
                let process = Process::spawn(&req.book, req.mode, self.generation, events.clone())?;
                self.book = req.book;
                slot.insert(process)
            }
        };
        if req.mode == Mode::Agent {
            turn.status(format!("Workspace: {}", process.launch.workspace.display()));
        }
        if reuse {
            turn.preflight(|frame| process.send(frame))?;
        }
        Ok(())
    }

    fn stop_process(&mut self) {
        if let Some(process) = self.process.take() {
            if let Some(turn) = &self.turn {
                if let Some(dialog) = &turn.dialog {
                    let _ = process.send(&protocol::cancel_dialog(&dialog.id));
                }
                if matches!(turn.phase, Phase::Running | Phase::Settling) {
                    let _ = process.send(&json!({"type":"abort"}));
                }
            }
            drop(process);
        }
    }

    fn fail(&mut self, message: String) {
        self.stop_process();
        if let Some(turn) = self.turn.take() {
            turn.update(Update::Failed(message));
        }
    }

    fn done(&mut self) -> Result<(), String> {
        if self.turn.as_ref().is_some_and(|t| t.mode == Mode::Chat)
            && let Some(mut process) = self.process.take()
        {
            process.close()?;
        }
        if let Some(turn) = self.turn.take() {
            turn.update(Update::Done);
        }
        Ok(())
    }

    fn frame(&mut self, f: &Value) -> Result<(), String> {
        let p = self.process.as_ref().unwrap();
        let kind = f["type"].as_str().ok_or("Missing OMP frame type")?;
        match kind {
            "extension_ui_request" => return dialog(p, self.turn.as_mut(), f),
            "host_tool_call" => return p.send(&json!({"type":"host_tool_result", "id":f["id"], "isError":true, "result":{"content":[{"type":"text","text":"Bookokrat does not offer host tools"}]}})),
            "host_uri_request" => return p.send(&json!({"type":"host_uri_result", "id":f["id"], "isError":true, "error":"Bookokrat does not offer host URIs"})),
            "rpc_frame_error" => return Err("OMP output exceeded its transport limit".into()),
            "notice" if f["level"] == "error" => return Err(f["message"].as_str().unwrap_or("OMP reported an error").into()),
            "extension_error" => return Err(f["error"].as_str().unwrap_or("OMP extension failed").into()),
            _ => (),
        }
        let Some(t) = &mut self.turn else {
            return Ok(());
        };
        if advance(t, kind, f, |frame| p.send(frame))? {
            self.done()?;
        }
        Ok(())
    }
}

// Completion means quiescence, not a command acknowledgement, agent_end,
// or a yield while some background work can still wake the agent.
fn advance(
    t: &mut Turn,
    kind: &str,
    f: &Value,
    mut send: impl FnMut(&Value) -> Result<(), String>,
) -> Result<bool, String> {
    match kind {
        "ready" if t.phase == Phase::Ready => {
            if f["protocolVersion"] != 1 {
                return Err("Unsupported OMP RPC protocol (expected v1)".into());
            }
            send(&protocol::filter())?;
            t.phase = Phase::Filter;
        }
        "response" => {
            let id = f["id"].as_str();
            let filter = id == Some("filter") && t.phase == Phase::Filter;
            let state = id == Some("state") && t.phase == Phase::Preflight;
            let prompt = id == Some(t.id.as_str()) && t.phase == Phase::Running;
            if !(filter || state || prompt || f["command"] == "parse") {
                return Ok(false);
            }
            if f["success"] != true {
                return Err(f["error"]
                    .as_str()
                    .unwrap_or("OMP rejected the request")
                    .into());
            }
            if filter {
                t.preflight(send)?;
            } else if state {
                match t.mode {
                    Mode::Chat => launch::check_chat_state(&f["data"])?,
                    Mode::Agent => launch::check_agent_tools(&f["data"])?,
                }
                t.send_prompt(send)?;
            } else if prompt && f["data"]["agentInvoked"] == false {
                return Err("OMP handled this question locally instead of invoking a model".into());
            }
        }
        "prompt_result" if f["id"].as_str() == Some(t.id.as_str()) && t.phase == Phase::Running => {
            match f["status"].as_str() {
                Some("completed") if f["agentInvoked"] == false => {
                    return Err("OMP handled this question without invoking a model".into());
                }
                Some("completed") if f["sessionSettled"] == true => return Ok(true),
                Some("completed") if f["sessionSettled"] == false => {
                    t.phase = Phase::Settling;
                    t.status("Waiting for background work");
                }
                Some("aborted") => return Err("OMP aborted the request".into()),
                Some("error") => {
                    return Err(f["error"]["message"]
                        .as_str()
                        .unwrap_or("OMP model request failed")
                        .into());
                }
                _ => return Err("Unrecognized OMP prompt result".into()),
            }
        }
        "session_settled" if t.phase == Phase::Settling => {
            return t.background_error.take().map_or(Ok(true), Err);
        }
        _ if !matches!(t.phase, Phase::Running | Phase::Settling) => (),
        "message_update" if f["assistantMessageEvent"]["type"] == "text_delta" => {
            if let Some(text) = f["assistantMessageEvent"]["delta"].as_str() {
                t.text(text);
            }
        }
        "message_end" if f["message"]["role"] == "assistant" => {
            let msg = &f["message"];
            if !t.streamed
                && let Some(blocks) = msg["content"].as_array()
            {
                for block in blocks.iter().filter(|b| b["type"] == "text") {
                    if let Some(text) = block["text"].as_str() {
                        t.text(text);
                    }
                }
            }
            t.streamed = false;
            if let Some(n) = msg["usage"]["cacheRead"].as_u64() {
                t.status(format!("Cache read: {n} tokens"));
            }
            if t.phase == Phase::Settling {
                // A retry can succeed after a failed assistant message;
                // only the final attempt determines the settled result.
                t.background_error = (msg["stopReason"] == "error").then(|| {
                    msg["errorMessage"]
                        .as_str()
                        .unwrap_or("Background OMP request failed")
                        .into()
                });
            }
        }
        "tool_execution_start" => t.status(format!(
            "Running {}",
            f["toolName"].as_str().unwrap_or("tool")
        )),
        "tool_execution_end" => t.status(if f["isError"] == true {
            "Tool returned an error"
        } else {
            "Tool finished"
        }),
        "auto_retry_start" => t.status("OMP is retrying"),
        "auto_compaction_start" => t.status("Compacting agent history"),
        "notice" => {
            if let Some(text) = f["message"].as_str() {
                t.status(text);
            }
        }
        _ => (),
    }
    Ok(false)
}

fn dialog(p: &Process, turn: Option<&mut Turn>, f: &Value) -> Result<(), String> {
    match f["method"].as_str() {
        Some("cancel") => {
            if let Some(t) = turn
                && let Some(d) = &t.dialog
                && Some(d.id.as_str()) == f["targetId"].as_str()
            {
                t.dialog = None;
                t.status("Approval withdrawn");
            }
            return Ok(());
        }
        Some("notify" | "setStatus") => {
            if let Some(t) = turn
                && let Some(text) = f["message"].as_str().or_else(|| f["statusText"].as_str())
            {
                t.status(text);
            }
            return Ok(());
        }
        Some("setTitle" | "set_editor_text" | "setWidget" | "open_url") => return Ok(()),
        _ => (),
    }
    let id = f["id"].as_str().ok_or("OMP UI request missing its id")?;
    if let Some(t) = turn {
        if t.mode == Mode::Agent
            && matches!(t.phase, Phase::Running | Phase::Settling)
            && t.dialog.is_none()
            && let Some(mut dialog) = Dialog::from_request(f)
        {
            let description = std::mem::take(&mut dialog.description);
            let _ = t.reply.send(Delivery {
                update: Update::Approval { description },
                dialog: Some(dialog.id.clone()),
            });
            t.dialog = Some(dialog);
            return Ok(());
        }
        t.status("Declined unsupported or overlapping OMP dialog");
    }
    p.send(&protocol::cancel_dialog(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(mode: Mode) -> (Turn, Receiver<Delivery>) {
        let (tx, rx) = mpsc::sync_channel(32);
        let req = Request {
            book: "book.epub".into(),
            mode,
            prompt: "Explain".into(),
            quote: Some("private passage".into()),
        };
        (Turn::new(1, &req, tx), rx)
    }

    fn receive(t: &mut Turn, frame: Value, writes: &mut Vec<Value>) -> Result<bool, String> {
        advance(t, frame["type"].as_str().unwrap(), &frame, |command| {
            writes.push(command.clone());
            Ok(())
        })
    }

    #[test]
    fn missing_configured_model_never_receives_the_passage() {
        let (mut t, _rx) = turn(Mode::Chat);
        let mut out = Vec::new();
        receive(
            &mut t,
            json!({"type":"ready","protocolVersion":1}),
            &mut out,
        )
        .unwrap();
        receive(
            &mut t,
            json!({"type":"response","command":"set_event_filter","id":"filter","success":true}),
            &mut out,
        )
        .unwrap();
        let err = receive(
            &mut t,
            json!({"type":"response","command":"get_state","id":"state","success":true,"data":{"model":null,"dumpTools":[],"messageCount":0}}),
            &mut out,
        );
        assert!(err.is_err());
        assert_eq!(
            out,
            vec![protocol::filter(), json!({"type":"get_state","id":"state"})]
        );
    }

    #[test]
    fn streamed_agent_yield_is_not_session_completion() {
        let (mut t, rx) = turn(Mode::Agent);
        let mut out = Vec::new();
        for frame in [
            json!({"type":"ready","protocolVersion":1}),
            json!({"type":"response","command":"set_event_filter","id":"filter","success":true}),
            json!({"type":"response","command":"get_state","id":"state","success":true,"data":{"dumpTools":[{"name":"read"},{"name":"write"}]}}),
            json!({"type":"response","command":"prompt","id":"reader-1","success":true}),
            json!({"type":"session_settled"}),
            json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"First"}}),
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"First"}],"usage":{"cacheRead":528}}}),
            json!({"type":"agent_end","yielded":true}),
            json!({"type":"prompt_result","id":"old","status":"completed","sessionSettled":true}),
            json!({"type":"prompt_result","id":"reader-1","status":"completed","sessionSettled":false}),
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Second"}]}}),
        ] {
            assert!(!receive(&mut t, frame, &mut out).unwrap());
        }
        assert!(receive(&mut t, json!({"type":"session_settled"}), &mut out).unwrap());
        assert_eq!(out.len(), 3);
        assert_eq!(out[2]["type"], "prompt");
        let mut transcript = String::new();
        let mut cache_reported = false;
        for delivery in rx.try_iter() {
            match delivery.update {
                Update::Text(s) => transcript.push_str(&s),
                Update::Status(s) => cache_reported |= s == "Cache read: 528 tokens",
                _ => panic!("unexpected update"),
            }
        }
        assert_eq!(transcript, "First\n\nSecond");
        assert!(cache_reported);
    }
}
