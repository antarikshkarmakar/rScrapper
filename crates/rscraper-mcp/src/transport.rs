//! Bounded, cancellation-safe stdio framing for the MCP server.

use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    io::{Error, ErrorKind},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use rmcp::{
    model::{
        CancelledNotification, CancelledNotificationParam, ClientJsonRpcMessage,
        ClientNotification, ClientRequest, CustomRequest, ErrorData, JsonRpcMessage, RequestId,
        ServerJsonRpcMessage,
    },
    transport::Transport,
    RoleServer,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Mutex, Notify},
};

/// Maximum size of one inbound newline-delimited JSON-RPC frame, excluding
/// its newline. Oversized frames are discarded with bounded memory.
pub const MAX_INBOUND_JSON_LINE_BYTES: usize = 1_048_576;

const INITIAL_LINE_CAPACITY: usize = 8 * 1024;
const UTF8_BOM: &[u8; 3] = b"\xEF\xBB\xBF";
// Terminal close is best-effort: incomplete frames are abandoned immediately,
// while an otherwise safe sink gets only this fixed graceful-shutdown window.
const CLOSE_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

#[derive(Default)]
struct TerminalSignal {
    terminal: AtomicBool,
    changed: Notify,
}

impl TerminalSignal {
    fn is_terminal(&self) -> bool {
        self.terminal.load(Ordering::Acquire)
    }

    fn terminate(&self) {
        if !self.terminal.swap(true, Ordering::AcqRel) {
            self.changed.notify_waiters();
        }
    }

    async fn cancelled(&self) {
        if self.is_terminal() {
            return;
        }
        let notified = self.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_terminal() {
            return;
        }
        notified.await;
    }
}

fn terminal_error() -> Error {
    Error::new(ErrorKind::ConnectionAborted, "MCP transport is terminal")
}

#[derive(Clone)]
struct FrameTicket(Arc<FrameTicketInner>);

struct FrameTicketInner {
    frame: Box<[u8]>,
    completion_id: Option<RequestId>,
    completed: AtomicBool,
}

impl FrameTicket {
    fn new(
        message: &ServerJsonRpcMessage,
        completion_id: Option<RequestId>,
    ) -> Result<Self, Error> {
        let mut frame = serde_json::to_vec(message).map_err(Error::other)?;
        frame.push(b'\n');
        Ok(Self(Arc::new(FrameTicketInner {
            frame: frame.into_boxed_slice(),
            completion_id,
            completed: AtomicBool::new(false),
        })))
    }

    fn completed(&self) -> bool {
        self.0.completed.load(Ordering::Acquire)
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

struct PendingFrame {
    ticket: FrameTicket,
    written: usize,
    flushed: bool,
}

struct WriterState<W> {
    output: Option<W>,
    pending: Option<PendingFrame>,
}

impl<W> WriterState<W>
where
    W: AsyncWrite + Unpin,
{
    async fn finish_pending(
        &mut self,
        active_ids: &Arc<Mutex<HashSet<RequestId>>>,
        terminal: &Arc<TerminalSignal>,
    ) -> Result<(), Error> {
        if terminal.is_terminal() {
            return Err(terminal_error());
        }
        let ticket = {
            let output = self
                .output
                .as_mut()
                .ok_or_else(|| Error::new(ErrorKind::NotConnected, "transport is closed"))?;
            let pending = self
                .pending
                .as_mut()
                .ok_or_else(|| Error::other("missing pending transport frame"))?;
            while pending.written < pending.ticket.0.frame.len() {
                let write = output.write(&pending.ticket.0.frame[pending.written..]);
                tokio::pin!(write);
                let written = tokio::select! {
                    biased;
                    () = terminal.cancelled() => return Err(terminal_error()),
                    result = &mut write => match result {
                        Ok(written) => written,
                        Err(error) => {
                            terminal.terminate();
                            return Err(error);
                        }
                    }
                };
                if written == 0 {
                    terminal.terminate();
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "failed to write MCP frame",
                    ));
                }
                pending.written += written;
            }
            if !pending.flushed {
                let flush = output.flush();
                tokio::pin!(flush);
                tokio::select! {
                    biased;
                    () = terminal.cancelled() => return Err(terminal_error()),
                    result = &mut flush => {
                        if let Err(error) = result {
                            terminal.terminate();
                            return Err(error);
                        }
                    }
                }
                pending.flushed = true;
            }
            pending.ticket.clone()
        };

        if let Some(id) = &ticket.0.completion_id {
            let active_ids_lock = active_ids.lock();
            tokio::pin!(active_ids_lock);
            tokio::select! {
                biased;
                () = terminal.cancelled() => return Err(terminal_error()),
                mut active_ids = &mut active_ids_lock => {
                    active_ids.remove(id);
                }
            }
        }
        ticket.0.completed.store(true, Ordering::Release);
        self.pending = None;
        Ok(())
    }
}

struct SerializedWriter<W> {
    state: Mutex<WriterState<W>>,
    active_ids: Arc<Mutex<HashSet<RequestId>>>,
    terminal: Arc<TerminalSignal>,
}

impl<W> SerializedWriter<W>
where
    W: AsyncWrite + Send + Unpin + 'static,
{
    fn new(
        output: W,
        active_ids: Arc<Mutex<HashSet<RequestId>>>,
        terminal: Arc<TerminalSignal>,
    ) -> Self {
        Self {
            state: Mutex::new(WriterState {
                output: Some(output),
                pending: None,
            }),
            active_ids,
            terminal,
        }
    }

    async fn write(&self, ticket: FrameTicket) -> Result<(), Error> {
        if ticket.completed() {
            return Ok(());
        }
        if self.terminal.is_terminal() {
            return Err(terminal_error());
        }
        let state_lock = self.state.lock();
        tokio::pin!(state_lock);
        let mut state = tokio::select! {
            biased;
            () = self.terminal.cancelled() => return Err(terminal_error()),
            state = &mut state_lock => state,
        };
        if ticket.completed() {
            return Ok(());
        }
        if self.terminal.is_terminal() {
            return Err(terminal_error());
        }
        loop {
            if state.pending.is_none() {
                state.pending = Some(PendingFrame {
                    ticket: ticket.clone(),
                    written: 0,
                    flushed: false,
                });
            }
            let writing_requested_ticket = state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.ticket.same(&ticket));
            if let Err(error) = state.finish_pending(&self.active_ids, &self.terminal).await {
                // Never append another JSON object after an incomplete frame.
                // A failed output is terminal for this transport.
                self.terminal.terminate();
                state.output.take();
                state.pending.take();
                return Err(error);
            }
            if writing_requested_ticket {
                return Ok(());
            }
        }
    }

    async fn close(&self) -> Result<(), Error> {
        self.terminal.terminate();
        let Ok(mut state) = self.state.try_lock() else {
            // The terminal notification interrupts any write/flush future.
            // Never wait here for an arbitrary AsyncWrite implementation.
            return Ok(());
        };
        let safe_to_shutdown = state.pending.as_ref().is_none_or(|pending| {
            pending.written == pending.ticket.0.frame.len() && pending.flushed
        });
        state.pending.take();
        let output = state.output.take();
        drop(state);

        let Some(mut output) = output else {
            return Ok(());
        };
        if !safe_to_shutdown {
            return Ok(());
        }
        match tokio::time::timeout(CLOSE_SHUTDOWN_GRACE, output.shutdown()).await {
            Ok(result) => result,
            Err(_) => Ok(()),
        }
    }
}

enum PendingInbound {
    Message(Box<ClientJsonRpcMessage>),
    BoundaryOutput(FrameTicket),
}

enum BoundedLine {
    Complete(Vec<u8>),
    Oversized,
}

enum BoundedRead {
    Line(BoundedLine),
    Terminated,
}

enum InputObservation {
    Buffered,
    Terminated,
}

enum BoundaryWait {
    Output(Result<(), Error>),
    Input(BoundedRead),
}

enum PrefetchedBoundaryWait {
    Output(Result<(), Error>),
    Input(InputObservation),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EofPhase {
    Open,
    SnapshotPending,
    Delivering,
    Done,
}

/// A server-side rmcp transport that guards the newline framing boundary.
///
/// Parsing, protocol state, request dispatch, and cancellation remain owned by
/// rmcp. This adapter only bounds frames, protects in-flight IDs, and converts
/// EOF or terminal output failure into official cancellation notifications
/// before abandoning or closing the transport.
#[doc(hidden)]
/// Bounded rmcp transport over newline-delimited input and output.
///
/// It retains at most one complete prefetched inbound frame beyond current SDK
/// work and applies backpressure after that slot is occupied.
pub struct GuardedStdioTransport<R, W> {
    read: BufReader<R>,
    writer: Arc<SerializedWriter<W>>,
    active_ids: Arc<Mutex<HashSet<RequestId>>>,
    terminal: Arc<TerminalSignal>,
    line: Vec<u8>,
    discarding_oversized_line: bool,
    pending_inbound: Option<PendingInbound>,
    // A blocked boundary ticket may race one reader observation. A complete
    // later line is already capped by `read_bounded_line` and occupies this
    // sole slot; no further input is consumed until the ticket completes.
    prefetched_line: Option<BoundedLine>,
    eof_phase: EofPhase,
    eof_cancellations: VecDeque<ClientJsonRpcMessage>,
    cancelled_ids_to_release: Vec<RequestId>,
}

impl<R, W> GuardedStdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    /// Wrap an async reader/writer pair with bounded framing and cancellation.
    pub fn new(read: R, write: W) -> Self {
        let active_ids = Arc::new(Mutex::new(HashSet::new()));
        let terminal = Arc::new(TerminalSignal::default());
        Self {
            read: BufReader::with_capacity(INITIAL_LINE_CAPACITY, read),
            writer: Arc::new(SerializedWriter::new(
                write,
                Arc::clone(&active_ids),
                Arc::clone(&terminal),
            )),
            active_ids,
            terminal,
            line: Vec::with_capacity(INITIAL_LINE_CAPACITY),
            discarding_oversized_line: false,
            pending_inbound: None,
            prefetched_line: None,
            eof_phase: EofPhase::Open,
            eof_cancellations: VecDeque::new(),
            cancelled_ids_to_release: Vec::new(),
        }
    }

    fn begin_shutdown(&mut self) {
        self.terminal.terminate();
        if self.eof_phase == EofPhase::Open {
            self.pending_inbound.take();
            self.prefetched_line.take();
            self.line = Vec::with_capacity(INITIAL_LINE_CAPACITY);
            self.discarding_oversized_line = false;
            self.eof_phase = EofPhase::SnapshotPending;
        }
    }

    fn queue_boundary_error(&mut self, error: ErrorData, id: Option<RequestId>) {
        if self.terminal.is_terminal() {
            self.begin_shutdown();
            return;
        }
        let message = ServerJsonRpcMessage::error(error, id);
        match FrameTicket::new(&message, None) {
            Ok(ticket) => self.pending_inbound = Some(PendingInbound::BoundaryOutput(ticket)),
            Err(_) => self.begin_shutdown(),
        }
    }

    fn queue_invalid_request(&mut self, message: &'static str) {
        self.queue_boundary_error(ErrorData::invalid_request(message, None), None);
    }

    fn malformed_tools_call_id(value: &Value) -> Option<RequestId> {
        let object = value.as_object()?;
        if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned()))
            || object.get("method") != Some(&Value::String("tools/call".to_owned()))
            || object.contains_key("result")
            || object.contains_key("error")
        {
            return None;
        }
        let params = object.get("params")?;
        if params.is_object() || params.is_null() {
            return None;
        }
        serde_json::from_value(object.get("id")?.clone()).ok()
    }

    fn is_standard_client_request_method(method: &str) -> bool {
        matches!(
            method,
            "ping"
                | "initialize"
                | "server/discover"
                | "completion/complete"
                | "logging/setLevel"
                | "prompts/get"
                | "prompts/list"
                | "resources/list"
                | "resources/templates/list"
                | "resources/read"
                | "subscriptions/listen"
                | "resources/subscribe"
                | "resources/unsubscribe"
                | "tools/call"
                | "tools/list"
                | "tasks/get"
                | "tasks/update"
                | "tasks/cancel"
        )
    }

    /// rmcp 3.1.4's custom-request deserializer uses a metadata-flattening
    /// helper that requires `params` to be an object. Preserve JSON-RPC's
    /// broader custom-method semantics without bypassing rmcp dispatch: only
    /// an otherwise valid request for a non-standard method is constructed as
    /// rmcp's official `CustomRequest` variant.
    fn valid_unknown_request(value: &Value) -> Option<ClientJsonRpcMessage> {
        let object = value.as_object()?;
        if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned()))
            || object.contains_key("result")
            || object.contains_key("error")
        {
            return None;
        }
        let id = serde_json::from_value(object.get("id")?.clone()).ok()?;
        let method = object.get("method")?.as_str()?;
        if Self::is_standard_client_request_method(method) {
            return None;
        }
        let params = object.get("params").cloned();
        Some(ClientJsonRpcMessage::request(
            ClientRequest::CustomRequest(CustomRequest::new(method, params)),
            id,
        ))
    }

    /// Read one complete bounded line. Stream termination is distinct from a
    /// newline-terminated oversized line, including when EOF ends an
    /// oversized unterminated line.
    async fn read_bounded_line(&mut self) -> BoundedRead {
        loop {
            let available = match self.read.fill_buf().await {
                Ok(available) => available,
                Err(_) => return BoundedRead::Terminated,
            };
            if available.is_empty() {
                // An unterminated trailing frame is never dispatched. EOF is a
                // terminal observation even if the discarded prefix exceeded
                // the line bound; there is no newline-terminated frame to
                // recover before shutdown.
                self.line = Vec::with_capacity(INITIAL_LINE_CAPACITY);
                self.discarding_oversized_line = false;
                return BoundedRead::Terminated;
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |offset| offset + 1);

            if self.discarding_oversized_line {
                self.read.consume(consumed);
                if newline.is_some() {
                    self.discarding_oversized_line = false;
                    return BoundedRead::Line(BoundedLine::Oversized);
                }
                continue;
            }

            let payload_bytes = newline.unwrap_or(available.len());
            if self.line.len().saturating_add(payload_bytes) > MAX_INBOUND_JSON_LINE_BYTES {
                // Release the capped accumulation before draining the rest of
                // the attacker-controlled line through BufReader's fixed buffer.
                self.line = Vec::with_capacity(INITIAL_LINE_CAPACITY);
                self.discarding_oversized_line = newline.is_none();
                self.read.consume(consumed);
                if newline.is_some() {
                    return BoundedRead::Line(BoundedLine::Oversized);
                }
                continue;
            }

            self.line.extend_from_slice(&available[..payload_bytes]);
            self.read.consume(consumed);
            if newline.is_some() {
                let line =
                    std::mem::replace(&mut self.line, Vec::with_capacity(INITIAL_LINE_CAPACITY));
                return BoundedRead::Line(BoundedLine::Complete(line));
            }
        }
    }

    /// Observe only whether the byte immediately following the occupied
    /// prefetch slot is EOF/read failure. `fill_buf` may move at most the fixed
    /// reader capacity into `BufReader`, but does not logically consume it.
    async fn observe_input_termination(&mut self) -> InputObservation {
        match self.read.fill_buf().await {
            Ok([]) => InputObservation::Terminated,
            Ok(_) => InputObservation::Buffered,
            Err(_) => InputObservation::Terminated,
        }
    }

    async fn release_cancelled_ids(&mut self) {
        if self.cancelled_ids_to_release.is_empty() {
            return;
        }
        let active_ids = Arc::clone(&self.active_ids);
        let active_ids_lock = active_ids.lock();
        tokio::pin!(active_ids_lock);
        let mut active_ids = tokio::select! {
            biased;
            () = self.terminal.cancelled() => return,
            active_ids = &mut active_ids_lock => active_ids,
        };
        for id in self.cancelled_ids_to_release.drain(..) {
            active_ids.remove(&id);
        }
    }

    async fn advance_boundary_output(
        &mut self,
        ticket: FrameTicket,
    ) -> Option<Option<ClientJsonRpcMessage>> {
        let outcome = if self.prefetched_line.is_some() {
            let observation = {
                let writer = Arc::clone(&self.writer);
                let output = writer.write(ticket.clone());
                let input = self.observe_input_termination();
                tokio::pin!(output, input);
                tokio::select! {
                    biased;
                    input = &mut input => PrefetchedBoundaryWait::Input(input),
                    output = &mut output => PrefetchedBoundaryWait::Output(output),
                }
            };
            match observation {
                PrefetchedBoundaryWait::Input(InputObservation::Terminated) => {
                    BoundaryWait::Input(BoundedRead::Terminated)
                }
                PrefetchedBoundaryWait::Input(InputObservation::Buffered) => {
                    // A buffered byte belongs to a second logical line. Leave
                    // it untouched and await only the earlier boundary ticket;
                    // repeatedly polling `fill_buf` here would busy-loop.
                    BoundaryWait::Output(self.writer.write(ticket).await)
                }
                PrefetchedBoundaryWait::Output(output) => BoundaryWait::Output(output),
            }
        } else {
            let writer = Arc::clone(&self.writer);
            let output = writer.write(ticket);
            let input = self.read_bounded_line();
            tokio::pin!(output, input);
            tokio::select! {
                biased;
                input = &mut input => BoundaryWait::Input(input),
                output = &mut output => BoundaryWait::Output(output),
            }
        };

        match outcome {
            BoundaryWait::Output(Ok(())) => {
                self.pending_inbound.take();
                Some(None)
            }
            BoundaryWait::Output(Err(_)) | BoundaryWait::Input(BoundedRead::Terminated) => {
                self.pending_inbound.take();
                self.begin_shutdown();
                Some(None)
            }
            BoundaryWait::Input(BoundedRead::Line(line)) => {
                debug_assert!(self.prefetched_line.is_none());
                self.prefetched_line = Some(line);
                Some(None)
            }
        }
    }

    async fn advance_pending_inbound(&mut self) -> Option<Option<ClientJsonRpcMessage>> {
        match self.pending_inbound.as_ref()? {
            PendingInbound::BoundaryOutput(ticket) => {
                let ticket = ticket.clone();
                self.advance_boundary_output(ticket).await
            }
            PendingInbound::Message(message)
                if matches!(message.as_ref(), JsonRpcMessage::Request(_)) =>
            {
                let JsonRpcMessage::Request(request) = message.as_ref() else {
                    unreachable!("guarded request variant changed")
                };
                let id = request.id.clone();
                let active_ids = Arc::clone(&self.active_ids);
                let active_ids_lock = active_ids.lock();
                tokio::pin!(active_ids_lock);
                let active_ids = tokio::select! {
                    biased;
                    () = self.terminal.cancelled() => None,
                    active_ids = &mut active_ids_lock => Some(active_ids),
                };
                let Some(mut active_ids) = active_ids else {
                    self.pending_inbound.take();
                    self.begin_shutdown();
                    return Some(None);
                };
                if self.terminal.is_terminal() {
                    drop(active_ids);
                    self.pending_inbound.take();
                    self.begin_shutdown();
                    return Some(None);
                }
                if active_ids.contains(&id) {
                    drop(active_ids);
                    self.pending_inbound.take();
                    self.queue_boundary_error(
                        ErrorData::invalid_request("duplicate request id", None),
                        Some(id),
                    );
                    Some(None)
                } else {
                    active_ids.insert(id);
                    drop(active_ids);
                    let Some(PendingInbound::Message(message)) = self.pending_inbound.take() else {
                        unreachable!("pending message changed while exclusively borrowed")
                    };
                    Some(Some(*message))
                }
            }
            PendingInbound::Message(message)
                if matches!(message.as_ref(), JsonRpcMessage::Notification(_)) =>
            {
                let JsonRpcMessage::Notification(notification) = message.as_ref() else {
                    unreachable!("guarded notification variant changed")
                };
                if let ClientNotification::CancelledNotification(cancelled) =
                    &notification.notification
                {
                    if let Some(id) = &cancelled.params.request_id {
                        self.cancelled_ids_to_release.push(id.clone());
                    }
                }
                let Some(PendingInbound::Message(message)) = self.pending_inbound.take() else {
                    unreachable!("pending message changed while exclusively borrowed")
                };
                Some(Some(*message))
            }
            PendingInbound::Message(_) => {
                let Some(PendingInbound::Message(message)) = self.pending_inbound.take() else {
                    unreachable!("pending message changed while exclusively borrowed")
                };
                Some(Some(*message))
            }
        }
    }

    async fn advance_eof(&mut self) -> Option<Option<ClientJsonRpcMessage>> {
        match self.eof_phase {
            EofPhase::Open => None,
            EofPhase::SnapshotPending => {
                let ids = self.active_ids.lock().await.drain().collect::<Vec<_>>();
                self.cancelled_ids_to_release.clear();
                self.eof_cancellations.extend(ids.into_iter().map(|id| {
                    let cancellation =
                        CancelledNotification::new(CancelledNotificationParam::new(Some(id), None));
                    ClientJsonRpcMessage::notification(ClientNotification::from(cancellation))
                }));
                self.eof_phase = if self.eof_cancellations.is_empty() {
                    EofPhase::Done
                } else {
                    EofPhase::Delivering
                };
                Some(None)
            }
            EofPhase::Delivering => {
                let cancellation = self.eof_cancellations.pop_front();
                if self.eof_cancellations.is_empty() {
                    self.eof_phase = EofPhase::Done;
                }
                Some(cancellation)
            }
            EofPhase::Done => Some(None),
        }
    }
}

impl<R, W> Transport<RoleServer> for GuardedStdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = Error;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let completion_id = match &item {
            JsonRpcMessage::Response(response) => Some(response.id.clone()),
            JsonRpcMessage::Error(response) => response.id.clone(),
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
        };
        let terminal = Arc::clone(&self.terminal);
        let ticket = if terminal.is_terminal() {
            Err(terminal_error())
        } else {
            FrameTicket::new(&item, completion_id)
        };
        if ticket.is_err() {
            terminal.terminate();
        }
        let writer = Arc::clone(&self.writer);
        async move { writer.write(ticket?).await }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        loop {
            if self.terminal.is_terminal() {
                self.begin_shutdown();
            }

            if self.eof_phase != EofPhase::Open {
                if let Some(result) = self.advance_eof().await {
                    if let Some(message) = result {
                        return Some(message);
                    }
                    if self.eof_phase == EofPhase::Done {
                        return None;
                    }
                    continue;
                }
            }

            self.release_cancelled_ids().await;
            if self.terminal.is_terminal() {
                self.begin_shutdown();
                continue;
            }

            if let Some(result) = self.advance_pending_inbound().await {
                if let Some(message) = result {
                    return Some(message);
                }
                continue;
            }

            let read_result = if let Some(prefetched_line) = self.prefetched_line.take() {
                BoundedRead::Line(prefetched_line)
            } else {
                let terminal = Arc::clone(&self.terminal);
                tokio::select! {
                    biased;
                    () = terminal.cancelled() => BoundedRead::Terminated,
                    line = self.read_bounded_line() => line,
                }
            };
            let line = match read_result {
                BoundedRead::Line(BoundedLine::Complete(line)) => line,
                BoundedRead::Line(BoundedLine::Oversized) => {
                    self.queue_invalid_request("request frame exceeds 1048576-byte limit");
                    continue;
                }
                BoundedRead::Terminated => {
                    self.begin_shutdown();
                    continue;
                }
            };

            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            let line = line.strip_prefix(UTF8_BOM.as_slice()).unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            let value = match serde_json::from_slice::<Value>(line) {
                Ok(value) => value,
                Err(error) => {
                    match error.classify() {
                        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {}
                        serde_json::error::Category::Data | serde_json::error::Category::Io => {
                            self.queue_invalid_request("Invalid request");
                        }
                    }
                    continue;
                }
            };

            if let Some(id) = Self::malformed_tools_call_id(&value) {
                self.queue_boundary_error(
                    ErrorData::invalid_params("invalid tools/call parameters", None),
                    Some(id),
                );
                continue;
            }

            match serde_json::from_value::<ClientJsonRpcMessage>(value.clone()) {
                Ok(message) => {
                    self.pending_inbound = Some(PendingInbound::Message(Box::new(message)));
                }
                Err(_) => match Self::valid_unknown_request(&value) {
                    Some(message) => {
                        self.pending_inbound = Some(PendingInbound::Message(Box::new(message)));
                    }
                    None => self.queue_invalid_request("Invalid request"),
                },
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.begin_shutdown();
        self.writer.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };

    struct AlwaysFailWriter;

    impl AsyncWrite for AlwaysFailWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<Result<usize, Error>> {
            Poll::Ready(Err(Error::new(
                ErrorKind::BrokenPipe,
                "deterministic output failure",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Error>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn write_test_request(input: &mut tokio::io::DuplexStream, id: i64) {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": format!("request-{id}"),
            "params": {}
        });
        input
            .write_all(
                serde_json::to_string(&frame)
                    .expect("JSON frame")
                    .as_bytes(),
            )
            .await
            .expect("write request");
        input.write_all(b"\n").await.expect("write newline");
    }

    #[tokio::test]
    async fn cancelled_eof_snapshot_cannot_expose_none_before_all_active_cancellations() {
        let (server_io, mut client_io) = tokio::io::duplex(4 * 1024);
        let (read, write) = tokio::io::split(server_io);
        let mut transport = GuardedStdioTransport::new(read, write);
        for (id, method) in [(71, "first"), (72, "second")] {
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": {}
            });
            client_io
                .write_all(
                    serde_json::to_string(&frame)
                        .expect("JSON frame")
                        .as_bytes(),
                )
                .await
                .expect("write request");
            client_io.write_all(b"\n").await.expect("write newline");
        }
        client_io.shutdown().await.expect("close input");

        for expected in [71, 72] {
            let message = transport.receive().await.expect("accepted request");
            let ClientJsonRpcMessage::Request(request) = message else {
                panic!("expected request {expected}");
            };
            assert_eq!(request.id, RequestId::Number(expected));
        }

        let active_ids = Arc::clone(&transport.active_ids);
        let active_guard = active_ids.lock().await;
        tokio::select! {
            biased;
            message = transport.receive() => {
                panic!("EOF snapshot completed while the active-ID lock was held: {message:?}");
            }
            () = tokio::task::yield_now() => {}
        }
        drop(active_guard);

        let mut cancelled = HashSet::new();
        for _ in 0..2 {
            let message = transport
                .receive()
                .await
                .expect("active request cancellation was lost");
            let ClientJsonRpcMessage::Notification(notification) = message else {
                panic!("expected EOF cancellation notification");
            };
            let ClientNotification::CancelledNotification(notification) = notification.notification
            else {
                panic!("expected official cancellation notification");
            };
            cancelled.insert(
                notification
                    .params
                    .request_id
                    .expect("cancelled request ID"),
            );
        }
        assert_eq!(
            cancelled,
            HashSet::from([RequestId::Number(71), RequestId::Number(72)])
        );
        assert!(transport.receive().await.is_none());
    }

    #[tokio::test]
    async fn output_failure_is_terminal_clears_active_ids_and_drops_later_input() {
        let (read, mut input) = tokio::io::duplex(4 * 1024);
        let mut transport = GuardedStdioTransport::new(read, AlwaysFailWriter);
        for id in [81, 82] {
            write_test_request(&mut input, id).await;
            let message = transport.receive().await.expect("accepted request");
            let ClientJsonRpcMessage::Request(request) = message else {
                panic!("expected request {id}");
            };
            assert_eq!(request.id, RequestId::Number(id));
        }

        let response = ServerJsonRpcMessage::error(
            ErrorData::internal_error("fixed output failure", None),
            Some(RequestId::Number(81)),
        );
        transport
            .send(response)
            .await
            .expect_err("writer must fail");
        write_test_request(&mut input, 83).await;

        let mut cancelled = HashSet::new();
        for _ in 0..2 {
            let message = tokio::time::timeout(Duration::from_millis(250), transport.receive())
                .await
                .expect("terminal output did not wake receive")
                .expect("active cancellation was lost");
            let ClientJsonRpcMessage::Notification(notification) = message else {
                panic!("terminal transport admitted later input: {message:?}");
            };
            let ClientNotification::CancelledNotification(notification) = notification.notification
            else {
                panic!("expected official cancellation notification");
            };
            cancelled.insert(
                notification
                    .params
                    .request_id
                    .expect("cancelled request ID"),
            );
        }
        assert_eq!(
            cancelled,
            HashSet::from([RequestId::Number(81), RequestId::Number(82)])
        );
        assert!(transport.active_ids.lock().await.is_empty());
        assert!(transport.receive().await.is_none());
    }
}
