//! Durable SFP acceptance and the ACP v1 dispatch boundary.
use super::*;
use sill_core::sfp::{Journal, Source};
use sill_proto::{rpc::Error, sfp as p};

/// Show a person's unconfirmed picks over the agent's reported controls, where it offers them.
fn overlay(controls: &mut p::SessionControls, picked: &sill_core::store::AgentSettings) {
    if let Some(mode) = &picked.mode
        && controls.modes.iter().any(|m| m.value == *mode)
    {
        controls.current_mode = Some(mode.clone());
    }
    for option in &mut controls.options {
        let Some(want) = picked.config.get(&option.option_id) else { continue };
        match &mut option.value {
            p::ControlValue::Id { value } => {
                if option.choices.iter().any(|c| c.value == *want) {
                    *value = want.clone();
                }
            }
            p::ControlValue::Boolean { value } => {
                if let Ok(want) = want.parse() {
                    *value = want;
                }
            }
        }
    }
}

/// The one place the controls' published state is decided: a person's unconfirmed picks over the
/// agent's values, then the status — restoring saved settings, agent not ready, a setting that
/// failed, or ready.
fn stamp(controls: &mut p::SessionControls, st: &State, ready: bool) {
    if let Some(picked) = st.context.optimistic() {
        overlay(controls, picked);
    }
    controls.status = if st.context.locking() {
        "restoring"
    } else if !ready {
        "loading"
    } else if st.context.controls_error.is_some() {
        "failed"
    } else {
        "ready"
    }
    .into();
    controls.error = st.context.controls_error.clone();
}

impl Runner {
    /// Publish pending state without changing the last confirmed controls. On timeout show the
    /// latest reported values, even if restoration never succeeded.
    pub(super) fn sfp_controls_state(&self, store: &Store, st: &State) -> Result<Vec<p::StoredEvent>> {
        let journal = Journal::new(store)?;
        let mut session = journal.ensure(&self.id, &st.generation_id)?;
        let before = session.clone();
        if !st.context.restoring() && st.context.controls_error.is_some() {
            session.controls = st.controls.as_ref().map(super::super::sfp_adapter::session_controls);
        }
        if let Some(controls) = &mut session.controls {
            stamp(controls, st, st.ready.is_some());
        }
        if before == session {
            return Ok(vec![]);
        }
        journal.append(&self.id, None, &[p::Event::SessionUpdated { session }])
    }
    pub(in crate::daemon) fn sfp_control(&self, hub: &Hub, control: p::Control) -> Result<()> {
        if matches!(control, p::Control::RetrySession) {
            self.command(hub, wire::Command::RetrySession)?;
            return self.sfp_login_done(hub);
        }
        let command = match control {
            p::Control::Authenticate { method_id, api_key: Some(key) } => return self.sfp_authenticate_key(hub, method_id, key),
            p::Control::Authenticate { method_id, api_key: None } => wire::Command::Authenticate(method_id),
            p::Control::RetrySession => wire::Command::RetrySession,
            p::Control::SetMode { mode_id } => wire::Command::SetMode(mode_id),
            p::Control::SetConfig { option_id, value } => wire::Command::SetConfig(
                option_id,
                match value {
                    p::ConfigValue::Id { value } => wire::ConfigValue::Id(value),
                    p::ConfigValue::Bool { value } => wire::ConfigValue::Bool(value),
                },
            ),
        };
        self.command(hub, command)
    }
    /// A login with an API key the person typed: kept where the agent's jail cannot read it
    /// (`agents::api_key_dir`), then the relay is told to send it from there — the command and
    /// the runner's saved outbox carry the file, never the key. The agent's next login requests
    /// are answered with it too, without the card (`Runner::saved_key_for`).
    fn sfp_authenticate_key(&self, hub: &Hub, method_id: String, key: wire::Secret) -> Result<()> {
        let key = key.0.trim().to_string();
        if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_control) {
            return Err(Error::invalid_params("an API key is 1 to 4096 characters on one line").into());
        }
        hub.with_store(|store| {
            let agent =
                store.sessions()?.into_iter().find(|s| s.id == self.id).map(|s| s.agent).ok_or_else(|| Error::domain("not_found", "session not found"))?;
            let key_file = sill_core::agents::save_api_key(&hub.home, &agent, &method_id, &key)?;
            let mut st = self.st.lock().unwrap();
            if st.ended || st.retired {
                return Err(Error::domain("invalid_state", "the session is not running").into());
            }
            st.key_tried = true;
            Self::queue(&mut st, relay::Command::AuthenticateKey { method: method_id, key_file });
            self.save(store, &st)?;
            self.drain(store, &mut st)
        })
    }
    /// A login ran outside the protocol (the agent's own terminal command) while its session was
    /// open: the session asked for it at a prompt, not at its start, so there is nothing to open
    /// again. The login is taken as done — the next prompt tells otherwise — and the queue moves.
    fn sfp_login_done(&self, hub: &Hub) -> Result<()> {
        hub.sync_credentials_for(Some(&self.id))?;
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            let tx = store.conn().unchecked_transaction()?;
            let Some(events) = self.sfp_login_events(store, &mut st)? else { return Ok(()) };
            self.save(store, &st)?;
            tx.commit()?;
            hub.sfp_publish(&self.id, &events);
            self.sfp_dispatch(store, hub, &mut st)
        })
    }
    fn sfp_login_events(&self, store: &Store, st: &mut State) -> Result<Option<Vec<p::StoredEvent>>> {
        if st.ready.is_none() || st.auth.is_none() || st.ended || st.retired {
            return Ok(None);
        }
        Ok(Some(self.sfp_project(store, st, &relay::Event::Other("_sill_authenticated".into()), 0)?))
    }
    pub(in crate::daemon) fn sfp_cancel_turn(&self, hub: &Hub, turn_id: &str) -> Result<()> {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            if st.adapter.turn_id() != Some(turn_id) || !st.busy {
                return Err(Error::domain("invalid_state", "target turn is no longer active").into());
            }
            self.commit_command(store, &hub.home, &mut st, wire::Command::Cancel, &[])?;
            self.drain(store, &mut st)
        })
    }
    pub(in crate::daemon) fn sfp_initialize(&self, store: &Store, hub: &Hub) -> Result<()> {
        let st = self.st.lock().unwrap();
        self.sfp_initialize_locked(store, hub, &st)
    }
    fn sfp_initialize_locked(&self, store: &Store, hub: &Hub, st: &State) -> Result<()> {
        let tx = store.conn().unchecked_transaction()?;
        let events = self.sfp_initial_events(store, st)?;
        // Persist the generation before any submission can become accepted under it.
        self.save(store, st)?;
        tx.commit()?;
        hub.sfp_publish(&self.id, &events);
        Ok(())
    }
    fn sfp_initial_events(&self, store: &Store, st: &State) -> Result<Vec<p::StoredEvent>> {
        let journal = Journal::new(store)?;
        let state = journal.ensure(&self.id, &st.generation_id)?;
        let mut events = if state.agent_generation_id != st.generation_id { journal.replace_generation(&self.id, &st.generation_id)? } else { vec![] };
        // A journal created after Ready needs the capabilities already held by the runner.
        let session = journal.session(&self.id)?;
        let mut seeded = session.clone();
        if seeded.controls.is_none()
            && let Some(c) = &st.controls
        {
            seeded.controls = Some(super::super::sfp_adapter::session_controls(c));
        }
        if !seeded.capabilities.contains_key("images")
            && let Some(images) = st.images
        {
            seeded.capabilities.insert("images".into(), images);
        }
        if let Some(controls) = &mut seeded.controls {
            stamp(controls, st, st.ready.is_some());
        }
        // the runner's summary turn is the truth: a daemon that stopped mid-change says it right
        seeded.summarizing = Self::summarizing_wire(st);
        if seeded != session {
            events.extend(journal.append(&self.id, None, &[p::Event::SessionUpdated { session: seeded }])?);
        }
        Ok(events)
    }
    /// As `Runner::summarizing`: the summary turn running, or the one that stopped and is not
    /// yet applied (`completed`). A closing summary therefore says `end` until the session's end
    /// clears it in the same update, never open and not summarizing in between.
    fn summarizing_wire(st: &State) -> Option<String> {
        st.summary.as_ref().map(|(kind, _, _)| *kind).or_else(|| st.completed.as_ref().map(|done| done.kind)).map(|kind| kind.wire().to_string())
    }
    /// The session state's `summarizing` brought to the runner's summary turn: one
    /// `session.updated` when they differ, none when they agree. Called wherever the summary
    /// turn starts, stops, changes kind, is applied or is dropped, in the same transaction.
    pub(super) fn sfp_summarizing(&self, store: &Store, st: &State) -> Result<Vec<p::StoredEvent>> {
        let want = Self::summarizing_wire(st);
        let journal = Journal::new(store)?;
        let session = journal.ensure(&self.id, &st.generation_id)?;
        if session.summarizing == want {
            return Ok(vec![]);
        }
        let mut next = session;
        next.summarizing = want;
        journal.append(&self.id, None, &[p::Event::SessionUpdated { session: next }])
    }
    pub(super) fn sfp_recover(&self, store: &Store, hub: &Hub) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        self.sfp_initialize_locked(store, hub, &st)?;
        self.drain(store, &mut st)?;
        // A crash may have happened after durable acceptance but before dispatch, with no
        // future relay event available to wake an idle queue.
        self.sfp_dispatch(store, hub, &mut st)
    }
    pub(super) fn sfp_project(&self, store: &Store, st: &mut State, event: &relay::Event, seq: u64) -> Result<Vec<p::StoredEvent>> {
        let journal = Journal::new(store)?;
        let mut session = journal.ensure(&self.id, &st.generation_id)?;
        let mut out = vec![];
        if session.agent_generation_id != st.generation_id {
            out.extend(journal.replace_generation(&self.id, &st.generation_id)?);
            session = journal.session(&self.id)?;
        }
        let source = (seq > 0).then(|| Source { generation_id: st.generation_id.clone(), seq });
        if let Some(source) = &source {
            if journal.source_seen(&self.id, source)? {
                return Ok(out);
            }
        }
        // ACP v1 session/load replay has no durable source identities. Existing history stays;
        // do not invent mappings from matching text or create duplicate history.
        if st.replaying
            && matches!(
                event,
                relay::Event::Text(_)
                    | relay::Event::Thought(_)
                    | relay::Event::UserText(_)
                    | relay::Event::ToolCall { .. }
                    | relay::Event::ToolCallUpdate { .. }
                    | relay::Event::SubagentText { .. }
                    | relay::Event::Image { .. }
            )
        {
            out.extend(journal.append(&self.id, source, &[])?);
            return Ok(out);
        }
        if matches!(event, relay::Event::Ended(_)) && st.context.restoring() {
            st.context.expire_controls(u64::MAX);
        }
        match event {
            relay::Event::Ready { resumed, controls, .. } | relay::Event::V2Ready { resumed, controls, .. } => {
                st.context.ready(*resumed);
                self.restore(store, st, controls, true)?;
            }
            relay::Event::Controls(controls) => {
                self.restore(store, st, controls, false)?;
            }
            relay::Event::ControlFailed { setting, reason } => {
                st.context.fail_control(setting, reason);
                // what waited for a refused model goes out against the model the agent kept
                if let Some(reported) = st.controls.clone() {
                    self.restore(store, st, &reported, false)?;
                }
            }
            _ => {}
        }
        let question = match event {
            relay::Event::Resolved { request } => st.adapter.question_id(*request).map(|id| journal.question(&self.id, id)).transpose()?,
            _ => None,
        };
        let authenticated = matches!(event,relay::Event::Other(text) if text=="_sill_authenticated");
        let mut events = if authenticated {
            st.auth = None;
            let mut next = session.clone();
            next.authentication = None;
            if next.question_ids.is_empty() && next.turn_id.is_none() {
                next.activity = "idle".into();
            }
            vec![p::Event::SessionUpdated { session: next }]
        } else {
            st.adapter.project(&session, event, question.as_ref())
        };
        if matches!(event, relay::Event::ControlFailed { .. }) {
            let mut next = session.clone();
            next.controls = st.controls.as_ref().map(super::super::sfp_adapter::session_controls);
            events.push(p::Event::SessionUpdated { session: next });
        }
        for projected in &mut events {
            if let p::Event::SessionUpdated { session: next } = projected {
                if matches!(event, relay::Event::Ended(_)) && st.context.controls_error.is_some() {
                    next.controls = st.controls.as_ref().map(super::super::sfp_adapter::session_controls);
                }
                if st.context.locking() {
                    next.controls = session.controls.clone().or_else(|| Some(super::super::sfp_adapter::session_controls(&Controls::default())));
                }
                // this event is projected before the runner records it: a Ready makes the agent ready
                let ready = st.ready.is_some() || matches!(event, relay::Event::Ready { .. } | relay::Event::V2Ready { .. });
                if let Some(controls) = &mut next.controls {
                    stamp(controls, st, ready);
                }
            }
        }
        // Agent image bytes commit with the events that reference them, in this transaction.
        let refused = super::super::uploads::keep_agent_media(store, &self.id, st.adapter.take_media())?;
        super::super::sfp_adapter::mark_omitted(&mut events, &refused);
        let interrupted = matches!(event,relay::Event::Stopped(acp::StopReason::Other(reason)) if reason=="_sill_prompt_error")
            || matches!(event, relay::Event::AuthRequired { .. }) && (st.ready.is_some() || st.active_submission.is_some())
            || matches!(event, relay::Event::Ended(_) | relay::Event::V2PromptFailed { .. });
        if interrupted {
            let reason = if matches!(event, relay::Event::AuthRequired { .. }) {
                "authentication_required"
            } else if matches!(event, relay::Event::Ended(_)) {
                "agent_lost"
            } else {
                "adapter_error"
            };
            let records = journal.active_records(&self.id)?;
            for record in records {
                match record {
                    p::Record::Submission { mut submission }
                        if submission.delivery == "queued" && submission.insertion == "pending" && submission.outcome == "pending" =>
                    {
                        submission.held = true;
                        submission.hold_reason = Some(p::Reason { code: reason.into(), message: "Resume explicitly after the agent is ready".into() });
                        events.push(p::Event::SubmissionUpdated { submission });
                    }
                    p::Record::Question { mut question } if matches!(event, relay::Event::Ended(_)) => {
                        if question.state == "open" {
                            question.state = "cancelled".into();
                            question.reason = Some(p::Reason { code: "agent_lost".into(), message: "The agent connection ended".into() });
                            events.push(p::Event::QuestionUpdated { question });
                        } else if question.state == "answered" && question.delivery.as_deref() == Some("pending") {
                            question.delivery = Some("unknown".into());
                            events.push(p::Event::QuestionUpdated { question });
                        }
                    }
                    _ => {}
                }
            }
        }
        if matches!(event, relay::Event::Ended(_)) {
            st.answer_outbox.clear();
            st.outbox.clear();
            // A terminal relay will not send Resolved for its remaining questions.
            let mut next = events
                .iter()
                .rev()
                .find_map(|e| match e {
                    p::Event::SessionUpdated { session } => Some(session.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| session.clone());
            next.question_ids.clear();
            next.activity = "unknown".into();
            events.retain(|e| !matches!(e, p::Event::SessionUpdated { .. }));
            events.push(p::Event::SessionUpdated { session: next });
        }
        if matches!(event, relay::Event::V2PermissionsCancelled) {
            let records = journal.active_records(&self.id)?;
            let mut next = events
                .iter()
                .rev()
                .find_map(|e| match e {
                    p::Event::SessionUpdated { session } => Some(session.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| session.clone());
            for record in records {
                if let p::Record::Question { mut question } = record {
                    if question.agent_generation_id != st.generation_id {
                        continue;
                    }
                    if let Some(request) = st.adapter.request_for_question(&question.question_id) {
                        st.answer_outbox.remove(&request);
                    }
                    if question.state == "open" {
                        question.state = "cancelled".into();
                        question.reason =
                            Some(p::Reason { code: "cancelled".into(), message: "Session cancellation cancelled the native permission handler".into() });
                        next.question_ids.retain(|id| id != &question.question_id);
                        events.push(p::Event::QuestionUpdated { question });
                    } else if question.state == "answered" && question.delivery.as_deref() == Some("pending") {
                        question.delivery = Some("unknown".into());
                        events.push(p::Event::QuestionUpdated { question });
                    }
                }
            }
            events.retain(|e| !matches!(e, p::Event::SessionUpdated { .. }));
            events.push(p::Event::SessionUpdated { session: next });
        }
        if let Some(id) = st.active_submission.clone() {
            let mut submission = journal.submission(&self.id, &id)?;
            match event {
                relay::Event::PromptAccepted { .. } => {
                    submission.insertion = "confirmed".into();
                    events.push(p::Event::SubmissionUpdated { submission });
                }
                relay::Event::V2Update { .. } => {
                    if let Some(turn) = events.iter().find_map(|e| match e {
                        p::Event::TurnUpdated { turn } if turn.outcome != "pending" => Some(turn),
                        _ => None,
                    }) {
                        submission.outcome = if submission.insertion == "confirmed" { turn.outcome.clone() } else { "unknown".into() };
                        submission.reason = turn.reason.clone();
                        events.push(p::Event::SubmissionUpdated { submission });
                        st.active_submission = None;
                    }
                }
                relay::Event::V2PromptFailed { code, message } => {
                    if submission.insertion != "confirmed" {
                        submission.insertion = if code == "not_dispatched" { "rejected" } else { "unknown" }.into();
                    }
                    submission.outcome = if code == "not_dispatched" { "failed" } else { "unknown" }.into();
                    submission.reason = Some(p::Reason { code: code.clone(), message: message.clone() });
                    events.push(p::Event::SubmissionUpdated { submission });
                    st.active_submission = None;
                }
                // ACP v1 run loop emits TurnStarted when it begins this command's prompt call;
                // it does not prove remote insertion. Completion is stronger evidence below.
                relay::Event::Stopped(reason) => {
                    let known = !matches!(reason, acp::StopReason::Other(_));
                    submission.insertion = if known { "confirmed" } else { "unknown" }.into();
                    submission.outcome = if !known {
                        "unknown"
                    } else if *reason == acp::StopReason::Cancelled {
                        "cancelled"
                    } else if *reason == acp::StopReason::Refusal {
                        "failed"
                    } else {
                        "completed"
                    }
                    .into();
                    events.push(p::Event::SubmissionUpdated { submission });
                    st.active_submission = None;
                }
                relay::Event::Ended(_) | relay::Event::AuthRequired { .. } => {
                    if submission.insertion != "confirmed" {
                        submission.insertion = "unknown".into();
                    }
                    submission.outcome = "unknown".into();
                    submission.reason = Some(p::Reason { code: "adapter_interrupted".into(), message: "No conclusive prompt result from the agent".into() });
                    events.push(p::Event::SubmissionUpdated { submission });
                    st.active_submission = None;
                }
                _ => {}
            }
        }
        out.extend(journal.append(&self.id, source, &events)?);
        Ok(out)
    }
    pub(in crate::daemon) fn sfp_submit(&self, hub: &Hub, input: p::Submit) -> Result<p::Submission> {
        hub.sync_credentials_for(Some(&self.id))?;
        if input.session_id != self.id {
            return Err(Error::domain("not_found", "session not found").into());
        }
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            self.sfp_initialize_locked(store, hub, &st)?;
            if st.ended || st.retired {
                return Err(Error::domain("invalid_state", "the agent connection ended; explicitly restart the session before submitting new work").into());
            }
            let tx = store.conn().unchecked_transaction()?;
            validate_attachments(store, &self.id, &input.attachments)?;
            let input = inline_as_uploads(store, input)?;
            let journal = Journal::new(store)?;
            // Accept records even while the adapter is starting/busy; dispatch owns readiness.
            let (submission, events) = journal.accept(&input, "queued")?;
            tx.commit()?;
            hub.sfp_publish(&self.id, &events);
            // Once accepted, delivery failure cannot turn success into an ambiguous rejection.
            if let Err(e) = self.sfp_dispatch(store, hub, &mut st) {
                eprintln!("sill daemon: SFP dispatch {}: {e:#}", self.id);
            }
            Ok(journal.submission(&self.id, &submission.submission_id)?)
        })
    }
    pub(super) fn sfp_dispatch(&self, store: &Store, hub: &Hub, st: &mut State) -> Result<()> {
        if !dispatch_ready(st) {
            return Ok(());
        }
        let journal = Journal::new(store)?;
        let records = journal.active_records(&self.id)?;
        let queued: Vec<_> = records
            .into_iter()
            .filter_map(|r| match r {
                p::Record::Submission { submission } if submission.delivery == "queued" && submission.outcome == "pending" => Some(submission),
                _ => None,
            })
            .collect();
        let Some(submission) = next_queued(queued, st.send_next.take()) else {
            return Ok(());
        };
        if submission.held {
            return Ok(());
        }
        let events = self.sfp_commit_submission(store, &hub.home, st, submission, &hub.running_session_ids())?;
        hub.sfp_publish(&self.id, &events);
        self.drain(store, st)
    }
    /// Durable delivery intent and exact prepared payload, before any relay send.
    fn sfp_commit_submission(
        &self,
        store: &Store,
        home: &std::path::Path,
        st: &mut State,
        mut submission: p::Submission,
        live: &[String],
    ) -> Result<Vec<p::StoredEvent>> {
        let journal = Journal::new(store)?;
        let before = st.clone();
        let result = (|| -> Result<Vec<p::StoredEvent>> {
            let tx = store.conn().unchecked_transaction()?;
            let prepared = st.context.prepare(store, &self.id, home, &submission.text, live)?;
            let attachments = attachments(store, &self.id, &submission.attachments, st.images.unwrap_or(true))?;
            Self::queue(st, relay::Command::Prompt { text: prepared.text.clone(), attachments });
            st.adapter.prompt_purpose(submission.purpose.clone());
            st.context.commit(prepared);
            st.busy = true;
            st.active_submission = Some(submission.submission_id.clone());
            submission.delivery = "direct".into();
            let events = journal.dispatched(&submission)?;
            self.save(store, st)?;
            tx.commit()?;
            Ok(events)
        })();
        match result {
            Ok(events) => Ok(events),
            Err(e) => {
                *st = before;
                Err(e)
            }
        }
    }
    pub(in crate::daemon) fn sfp_queue_action(&self, hub: &Hub, key: &p::SubmissionKey, resume: bool) -> Result<p::Submission> {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            self.sfp_initialize_locked(store, hub, &st)?;
            let journal = Journal::new(store)?;
            let (_, events) = if resume { journal.resume(key)? } else { journal.cancel(key)? };
            hub.sfp_publish(&self.id, &events);
            if let Err(e) = self.sfp_dispatch(store, hub, &mut st) {
                eprintln!("sill daemon: queued dispatch: {e:#}");
            }
            journal.submission(&self.id, &key.submission_id)
        })
    }
    /// Send a queued prompt now (`sill.send_now`): resumed if held, first in the queue, and the
    /// turn in progress stopped so it goes as soon as the agent has stopped.
    pub(in crate::daemon) fn sfp_send_now(&self, hub: &Hub, key: &p::SubmissionKey) -> Result<p::Submission> {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            self.sfp_initialize_locked(store, hub, &st)?;
            let journal = Journal::new(store)?;
            let s = journal.submission(&key.session_id, &key.submission_id)?;
            if s.delivery != "queued" || s.insertion != "pending" || s.outcome != "pending" {
                return Err(Error::domain("invalid_state", "submission is not an unsent queued intent").into());
            }
            let (_, events) = journal.resume(key)?;
            hub.sfp_publish(&self.id, &events);
            st.send_next = Some(key.submission_id.clone());
            self.save(store, &st)?;
            if st.busy {
                self.commit_command(store, &hub.home, &mut st, wire::Command::Cancel, &[])?;
                self.drain(store, &mut st)?;
            } else if let Err(e) = self.sfp_dispatch(store, hub, &mut st) {
                eprintln!("sill daemon: send now: {e:#}");
            }
            journal.submission(&self.id, &key.submission_id)
        })
    }
    pub(in crate::daemon) fn sfp_answer(&self, hub: &Hub, input: p::AnswerQuestion) -> Result<p::Question> {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            self.sfp_initialize_locked(store, hub, &st)?;
            self.sfp_answer_locked(store, hub, &mut st, input)
        })
    }
    pub(super) fn sfp_answer_locked(&self, store: &Store, hub: &Hub, st: &mut State, input: p::AnswerQuestion) -> Result<p::Question> {
        let (question, events) = self.sfp_commit_answer(store, st, input)?;
        hub.sfp_publish(&self.id, &events);
        if let Err(e) = self.drain(store, st) {
            eprintln!("sill daemon: answer awaiting delivery: {e:#}");
        }
        Ok(question)
    }
    fn sfp_commit_answer(&self, store: &Store, st: &mut State, input: p::AnswerQuestion) -> Result<(p::Question, Vec<p::StoredEvent>)> {
        let journal = Journal::new(store)?;
        let before = st.clone();
        let result = (|| -> Result<(p::Question, Vec<p::StoredEvent>)> {
            let tx = store.conn().unchecked_transaction()?;
            let (question, events) = journal.answer(&input)?;
            if !events.is_empty() {
                let request = st
                    .adapter
                    .request_for_question(&question.question_id)
                    .ok_or_else(|| Error::domain("invalid_state", "question belongs to an unavailable agent"))?;
                let answer = match &input.answer {
                    p::Answer::Permission { option_id } => relay::Answer::Permission(Some(option_id.clone())),
                    p::Answer::Form { values } => relay::Answer::Form(Some(values.clone())),
                    p::Answer::Cancel if question.kind == "permission" => relay::Answer::Permission(None),
                    p::Answer::Cancel => relay::Answer::Form(None),
                };
                st.answer_outbox.insert(request, answer);
                self.save(store, st)?;
            }
            tx.commit()?;
            Ok((question, events))
        })();
        match result {
            Ok(x) => Ok(x),
            Err(e) => {
                *st = before;
                Err(e)
            }
        }
    }
    pub(super) fn sfp_auto_answer(&self, store: &Store, hub: &Hub, st: &mut State, event: &relay::Event) -> Result<()> {
        if let relay::Event::PermissionRequest { request, options, .. } | relay::Event::V2PermissionRequest { request, options, .. } = event {
            let Some(question_id) = st.adapter.question_id(*request).map(str::to_owned) else { return Ok(()) };
            let journal = Journal::new(store)?;
            if journal.question(&self.id, &question_id)?.state != "open" {
                return Ok(());
            }
            let option = options
                .iter()
                .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
                .or_else(|| options.iter().find(|o| o.kind == acp::PermissionOptionKind::AllowAlways));
            let answer = option.map(|o| p::Answer::Permission { option_id: o.id.clone() }).unwrap_or(p::Answer::Cancel);
            self.sfp_answer_locked(
                store,
                hub,
                st,
                p::AnswerQuestion { session_id: self.id.clone(), question_id, answer_id: format!("policy-{}-{request}", st.generation_id), answer },
            )?;
        }
        Ok(())
    }
}
fn dispatch_ready(st: &State) -> bool {
    !st.busy
        && st.ready.is_some()
        && !st.ended
        && !st.retired
        && !st.context.restoring()
        && st.active_submission.is_none()
        && st.auth.is_none()
        && st.summary.is_none()
        && st.completed.is_none()
}
fn validate_attachments(store: &Store, session_id: &str, input: &[p::Attachment]) -> Result<()> {
    let mut size = 0usize;
    if input.len() > 32 {
        return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Request, Some(32), "a submission supports at most 32 attachments").into());
    }
    for a in input {
        match a {
            p::Attachment::Inline { mime, data_base64, .. } => {
                if !mime.starts_with("image/") {
                    return Err(Error::domain("unsupported", "inline attachments support images; upload other files first").into());
                }
                size = size.saturating_add(super::super::uploads::decode(data_base64)?.len());
            }
            p::Attachment::Uploaded { upload_id, name, mime } => {
                size = size.saturating_add(super::super::uploads::validate(store, session_id, upload_id, name, mime)?);
            }
        }
    }
    if size > sill_proto::initialize::MAX_INLINE_BYTES {
        return Err(sill_proto::rpc::Error::limit(
            sill_proto::rpc::LimitScope::Request,
            Some(sill_proto::initialize::MAX_INLINE_BYTES as u64),
            "image attachments exceed 10 MiB per submission",
        )
        .into());
    }
    Ok(())
}
/// Journal records reference attachment bytes and never carry them: every attach would otherwise
/// download every screenshot ever sent, and enough of them exceed the snapshot limit.
pub(in crate::daemon) fn inline_as_uploads(store: &Store, mut input: p::Submit) -> Result<p::Submit> {
    for (i, a) in input.attachments.iter_mut().enumerate() {
        if let p::Attachment::Inline { name, mime, data_base64 } = a {
            let (upload_id, name) = super::super::uploads::from_inline(store, &input.session_id, &input.submission_id, i, name, mime, data_base64)?;
            *a = p::Attachment::Uploaded { upload_id, name, mime: mime.clone() };
        }
    }
    Ok(input)
}
fn attachments(store: &Store, session_id: &str, input: &[p::Attachment], images: bool) -> Result<Vec<acp::Attachment>> {
    validate_attachments(store, session_id, input)?;
    input
        .iter()
        .map(|a| match a {
            p::Attachment::Inline { name, mime, data_base64 } => {
                Ok(acp::Attachment { name: name.clone(), mime: mime.clone(), kind: acp::AttachmentKind::Image { data_base64: data_base64.clone() } })
            }
            p::Attachment::Uploaded { upload_id, .. } => super::super::uploads::resolve(store, session_id, upload_id, images),
        })
        .collect()
}

/// The queued submission to send next: the one the person asked to send now, else the oldest.
fn next_queued(mut queued: Vec<p::Submission>, send_now: Option<String>) -> Option<p::Submission> {
    queued.sort_by_key(|s| s.accepted_seq.parse::<u64>().unwrap_or(u64::MAX));
    let at = send_now.and_then(|id| queued.iter().position(|s| s.submission_id == id)).unwrap_or(0);
    (at < queued.len()).then(|| queued.swap_remove(at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    #[test]
    fn restoration_publishes_confirmed_snapshot_until_final_acknowledgement() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = f.runner.st.lock().unwrap();
        let selected = acp::Controls { modes: Some(("auto".into(), vec![("auto".into(), "Auto".into()), ("ask".into(), "Ask".into())])), options: vec![] };
        let mut session = j.session(&f.runner.id).unwrap();
        session.controls = Some(super::super::super::sfp_adapter::session_controls(&selected));
        j.append(&f.runner.id, None, &[p::Event::SessionUpdated { session }]).unwrap();
        f.store.set_session_settings(&f.runner.id, &sill_core::store::AgentSettings { mode: Some("auto".into()), config: Default::default() }).unwrap();
        let mut defaults = selected.clone();
        defaults.modes.as_mut().unwrap().0 = "ask".into();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Ready { session_id: "agent".into(), resumed: true, controls: defaults.clone() }, 1).unwrap();
        let c = j.session(&f.runner.id).unwrap().controls.unwrap();
        assert_eq!(c.current_mode.as_deref(), Some("auto"));
        assert_eq!(c.status, "restoring");
        assert!(st.context.restoring());
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Controls(selected), 2).unwrap();
        let c = j.session(&f.runner.id).unwrap().controls.unwrap();
        assert_eq!(c.current_mode.as_deref(), Some("auto"));
        assert_eq!(c.status, "ready");
        assert!(!st.context.restoring());
        st.ready = Some(("agent".into(), true));
        st.controls = Some(defaults.clone());
        st.context.chosen(Some(&defaults), &acp::Command::SetMode("auto".into()));
        assert!(st.context.expire_controls(u64::MAX));
        f.runner.sfp_controls_state(&f.store, &st).unwrap();
        let c = j.session(&f.runner.id).unwrap().controls.unwrap();
        assert_eq!(c.status, "failed");
        assert_eq!(c.current_mode.as_deref(), Some("ask"), "failure shows the agent's actual values");
        assert!(c.error.unwrap().contains("mode"));
        st.context.chosen(Some(&defaults), &acp::Command::SetMode("auto".into()));
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::ControlFailed { setting: "mode".into(), reason: "mode rejected".into() }, 3).unwrap();
        let c = j.session(&f.runner.id).unwrap().controls.unwrap();
        assert_eq!(c.status, "failed");
        assert!(c.error.unwrap().contains("mode rejected"));
        assert!(!st.context.restoring(), "explicit rejection must not wait for the timeout");
    }

    #[test]
    fn a_pick_is_published_at_once_and_reverts_when_the_agent_refuses_it() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = f.runner.st.lock().unwrap();
        let reported = acp::Controls { modes: Some(("ask".into(), vec![("auto".into(), "Auto".into()), ("ask".into(), "Ask".into())])), options: vec![] };
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Ready { session_id: "agent".into(), resumed: false, controls: reported.clone() }, 1).unwrap();
        st.ready = Some(("agent".into(), false));
        st.controls = Some(reported.clone());
        st.context.chosen(Some(&reported), &acp::Command::SetMode("auto".into()));
        f.runner.sfp_controls_state(&f.store, &st).unwrap();
        let c = j.session(&f.runner.id).unwrap().controls.unwrap();
        assert_eq!((c.current_mode.as_deref(), c.status.as_str()), (Some("auto"), "ready"), "shown before the agent answers, controls usable");
        // an update that is not the acknowledgement does not put the old value back
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Controls(reported.clone()), 2).unwrap();
        let c = j.session(&f.runner.id).unwrap().controls.unwrap();
        assert_eq!((c.current_mode.as_deref(), c.status.as_str()), (Some("auto"), "ready"));
        assert!(st.context.restoring());
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::ControlFailed { setting: "mode".into(), reason: "set mode failed".into() }, 3).unwrap();
        let c = j.session(&f.runner.id).unwrap().controls.unwrap();
        assert_eq!((c.current_mode.as_deref(), c.status.as_str()), (Some("ask"), "failed"), "a refused pick shows the agent's value again");
    }

    #[test]
    fn image_budget_covers_inline_uploaded_and_mixed_submissions() {
        let root = std::env::temp_dir().join(format!("sill-image-budget-{}", sill_core::sfp::new_id()));
        let store = Store::open(&root.join("store.db")).unwrap();
        let session = store.create("test", "default").unwrap();
        let limit = sill_proto::initialize::MAX_INLINE_BYTES;
        let data = acp::base64(&vec![0; limit]);
        let inline = p::Attachment::Inline { name: "large.png".into(), mime: "image/png".into(), data_base64: data.clone() };
        assert!(validate_attachments(&store, &session.id, &[inline.clone()]).is_ok());
        let extra = p::Attachment::Inline { name: "extra.png".into(), mime: "image/png".into(), data_base64: "AA==".into() };
        assert!(validate_attachments(&store, &session.id, &[inline.clone(), extra.clone()]).is_err());
        // A maximum image plus a maximum text prompt fits the control transport.
        let request = json!({"jsonrpc":"2.0", "id":1, "method":"session.submit", "params": {
            "session_id":session.id, "submission_id":"large", "text":"x".repeat(1024*1024), "attachments":[inline]
        }});
        assert!(serde_json::to_vec(&request).unwrap().len() + 1 < sill_proto::initialize::MAX_MESSAGE_BYTES);
        crate::daemon::uploads::create(
            &store,
            p::UploadCreate {
                session_id: session.id.clone(),
                upload_id: "large".into(),
                name: "large.png".into(),
                mime: "image/png".into(),
                data_base64: data,
            },
        )
        .unwrap();
        let uploaded = p::Attachment::Uploaded { upload_id: "large".into(), name: "large.png".into(), mime: "image/png".into() };
        assert!(validate_attachments(&store, &session.id, &[uploaded.clone()]).is_ok());
        assert!(validate_attachments(&store, &session.id, &[uploaded.clone(), extra]).is_err());
        assert!(validate_attachments(&store, &session.id, &[uploaded.clone(), uploaded]).is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    struct Fixture {
        dir: std::path::PathBuf,
        store: Store,
        runner: Arc<Runner>,
        received: std::sync::mpsc::Receiver<serde_json::Value>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sill-sfp-runner-{}", sill_core::sfp::new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let socket = dir.join("relay.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let (tx, received) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut hello = String::new();
                reader.read_line(&mut hello).unwrap();
                writeln!(stream, "{}", serde_json::json!({"Hello":{"acknowledged":0,"last":0,"version":relay::RELAY_PROTOCOL}})).unwrap();
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let response = if let Some(c) = message.get("Command") {
                        serde_json::json!({"Accepted":{"id":c["id"],"error":null}})
                    } else if let Some(a) = message.get("Answer") {
                        serde_json::json!({"Answered":{"request":a["request"],"error":null}})
                    } else {
                        continue;
                    };
                    tx.send(message).ok();
                    if writeln!(stream, "{response}").is_err() {
                        break;
                    }
                }
            });
            let (client, _) = relay::connect(&socket, 0).unwrap();
            let store = Store::open(&dir.join("s.db")).unwrap();
            let id = store.create("test", "default").unwrap().id;
            Runner::load(&id, &store).unwrap();
            let runner = Runner::new(&id, client, serde_json::Value::Null).unwrap();
            {
                let mut st = runner.st.lock().unwrap();
                st.ready = Some(("agent".into(), false));
                Journal::new(&store).unwrap().ensure(&id, &st.generation_id).unwrap();
                runner.save(&store, &st).unwrap();
            }
            Self { dir, store, runner, received }
        }
        fn input(&self) -> p::Submit {
            p::Submit { session_id: self.runner.id.clone(), submission_id: "submission".into(), text: "work".into(), attachments: vec![], purpose: None }
        }
        fn reopened(&self) -> (Store, Arc<Runner>) {
            let store = Store::open(&self.dir.join("s.db")).unwrap();
            let runner = Runner::new(&self.runner.id, self.runner.relay.clone(), Runner::load(&self.runner.id, &store).unwrap()).unwrap();
            (store, runner)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn both_prompt_paths_repeat_live_peer_notice_for_existing_conversations() {
        let f = Fixture::new();
        let peer = f.store.create("reviewer", "default").unwrap();
        f.store.set_name(&peer.id, "rapid-fire").unwrap();
        for id in [&f.runner.id, &peer.id] {
            f.store.pin(id, &f.dir).unwrap();
        }
        f.store
            .conn()
            .execute("INSERT INTO daemon_runners(id,state) VALUES(?1,?2)", rusqlite::params![peer.id, json!({"ready":["peer",false],"busy":true}).to_string()])
            .unwrap();
        let live = vec![peer.id];
        let mut st = f.runner.st.lock().unwrap();
        for n in 0..4 {
            st.busy = false;
            st.active_submission = None;
            if n < 2 {
                let mut input = f.input();
                input.submission_id = format!("peer-test-{n}");
                let (submission, _) = Journal::new(&f.store).unwrap().accept(&input, "queued").unwrap();
                f.runner.sfp_commit_submission(&f.store, &f.dir, &mut st, submission, &live).unwrap();
            } else {
                f.runner
                    .commit_command(&f.store, &f.dir, &mut st, wire::Command::Prompt { text: "work".into(), attachments: vec![], entry: None }, &live)
                    .unwrap();
            }
            let relay::Command::Prompt { text, .. } = st.outbox.values().last().unwrap() else { panic!("prompt expected") };
            assert!(text.contains("rapid-fire") && text.contains("working"), "every prompt has live peer status: {text}");
        }
        st.busy = false;
        f.runner
            .commit_command(&f.store, &f.dir, &mut st, wire::Command::Prompt { text: "after peer stopped".into(), attachments: vec![], entry: None }, &[])
            .unwrap();
        let relay::Command::Prompt { text, .. } = st.outbox.values().last().unwrap() else { panic!("prompt expected") };
        assert!(!text.contains("rapid-fire"));
    }

    #[test]
    fn a_submissions_purpose_marks_the_turn_it_starts_and_only_that_one() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = f.runner.st.lock().unwrap();
        let turns = |events: Vec<p::StoredEvent>| {
            events
                .into_iter()
                .filter_map(|e| match e.event {
                    p::Event::TurnUpdated { turn } => Some(turn),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let mut commit = f.input();
        commit.submission_id = "commit".into();
        commit.purpose = Some("commit".into());
        let (submission, _) = j.accept(&commit, "queued").unwrap();
        assert_eq!(submission.purpose.as_deref(), Some("commit"));
        f.runner.sfp_commit_submission(&f.store, &f.dir, &mut st, submission, &[]).unwrap();
        let started = turns(f.runner.sfp_project(&f.store, &mut st, &relay::Event::TurnStarted, 1).unwrap());
        assert_eq!(started[0].purpose.as_deref(), Some("commit"));
        let ended = turns(f.runner.sfp_project(&f.store, &mut st, &relay::Event::Stopped(acp::StopReason::EndTurn), 2).unwrap());
        assert!(ended.iter().all(|t| t.purpose.as_deref() == Some("commit")), "the turn keeps its purpose to its end");
        st.busy = false;
        let (plain, _) = j.accept(&f.input(), "queued").unwrap();
        f.runner.sfp_commit_submission(&f.store, &f.dir, &mut st, plain, &[]).unwrap();
        let started = turns(f.runner.sfp_project(&f.store, &mut st, &relay::Event::TurnStarted, 3).unwrap());
        assert_eq!(started[0].purpose, None, "the person's own prompt");
        let mut odd = f.input();
        odd.submission_id = "odd".into();
        odd.purpose = Some("anything".into());
        assert!(j.accept(&odd, "queued").is_err(), "only known purposes");
    }

    #[test]
    fn send_now_goes_before_older_queued_prompts() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut ids = vec![];
        for id in ["first", "second", "third"] {
            let mut input = f.input();
            input.submission_id = id.into();
            ids.push(j.accept(&input, "queued").unwrap().0);
        }
        let pick = |send_now: Option<&str>| super::next_queued(ids.clone(), send_now.map(str::to_owned)).map(|s| s.submission_id);
        assert_eq!(pick(None).as_deref(), Some("first"), "the oldest by default");
        assert_eq!(pick(Some("third")).as_deref(), Some("third"));
        assert_eq!(pick(Some("gone")).as_deref(), Some("first"), "a sent-now prompt removed meanwhile changes nothing");
        assert!(super::next_queued(vec![], Some("third".into())).is_none());
    }

    #[test]
    fn dispatch_commit_keeps_exact_payload_and_identity_after_reopen() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let (accepted, _) = j.accept(&f.input(), "queued").unwrap();
        let payload;
        {
            let mut st = f.runner.st.lock().unwrap();
            f.runner.sfp_commit_submission(&f.store, &f.dir, &mut st, accepted, &[]).unwrap();
            payload = serde_json::to_value(&st.outbox).unwrap();
        }
        assert!(f.received.try_recv().is_err(), "commit must precede any relay delivery");
        let (store, runner) = f.reopened();
        {
            let mut st = runner.st.lock().unwrap();
            assert_eq!(serde_json::to_value(&st.outbox).unwrap(), payload);
            assert_eq!(st.active_submission.as_deref(), Some("submission"));
            let (retry, events) = Journal::new(&store).unwrap().accept(&f.input(), "queued").unwrap();
            assert!(events.is_empty());
            assert_eq!(retry.delivery, "direct");
            runner.drain(&store, &mut st).unwrap();
            assert!(st.outbox.is_empty());
        }
        let delivered = f.received.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(delivered["Command"]["id"], 1);
        assert_eq!(delivered["Command"]["command"], payload["1"]);
        assert_eq!(store.entry_count(&runner.id).unwrap(), 1);
    }

    #[test]
    fn failed_dispatch_commit_rolls_back_delivery_and_preserves_accepted_queue() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let (accepted, _) = j.accept(&f.input(), "queued").unwrap();
        f.store.conn().execute_batch("CREATE TRIGGER fail_dispatch BEFORE INSERT ON daemon_runners BEGIN SELECT RAISE(ABORT,'test failure'); END").unwrap();
        let mut st = f.runner.st.lock().unwrap();
        let before = serde_json::to_value(&*st).unwrap();
        assert!(f.runner.sfp_commit_submission(&f.store, &f.dir, &mut st, accepted, &[]).is_err());
        assert_eq!(serde_json::to_value(&*st).unwrap(), before);
        assert_eq!(j.submission(&f.runner.id, "submission").unwrap().delivery, "queued");
        assert_eq!(
            j.snapshot(&f.runner.id).unwrap().1.iter().filter(|r| matches!(r, p::Record::Item { .. })).count(),
            0,
            "no user item until a dispatch commits: the failed one made none"
        );
        assert!(f.received.try_recv().is_err());
    }

    #[test]
    fn winning_answer_and_delivery_intent_survive_restart_before_send() {
        let f = Fixture::new();
        let input;
        {
            let mut st = f.runner.st.lock().unwrap();
            f.runner
                .sfp_project(
                    &f.store,
                    &mut st,
                    &relay::Event::PermissionRequest {
                        request: 7,
                        tool_call_id: String::new(),
                        title: "Run?".into(),
                        options: vec![acp::PermissionOption { id: "yes".into(), name: "Yes".into(), kind: acp::PermissionOptionKind::AllowOnce }],
                    },
                    1,
                )
                .unwrap();
            input = p::AnswerQuestion {
                session_id: f.runner.id.clone(),
                question_id: st.adapter.question_id(7).unwrap().into(),
                answer_id: "answer".into(),
                answer: p::Answer::Permission { option_id: "yes".into() },
            };
            f.runner.sfp_commit_answer(&f.store, &mut st, input.clone()).unwrap();
        }
        assert!(f.received.try_recv().is_err());
        let (store, runner) = f.reopened();
        {
            let mut st = runner.st.lock().unwrap();
            assert!(st.answer_outbox.contains_key(&7));
            let (question, events) = runner.sfp_commit_answer(&store, &mut st, input).unwrap();
            assert!(events.is_empty());
            assert_eq!(question.answer_id.as_deref(), Some("answer"));
            runner.drain(&store, &mut st).unwrap();
            assert!(st.answer_outbox.is_empty());
        }
        let delivered = f.received.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(delivered["Answer"]["request"], 7);
        assert_eq!(delivered["Answer"]["answer"], serde_json::json!({"Permission":"yes"}));
    }

    #[test]
    fn prompt_error_does_not_claim_insertion_or_success() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        j.accept(&f.input(), "direct").unwrap();
        let mut st = f.runner.st.lock().unwrap();
        st.active_submission = Some("submission".into());
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Stopped(acp::StopReason::Other("_sill_prompt_error".into())), 1).unwrap();
        let submission = j.submission(&f.runner.id, "submission").unwrap();
        assert_eq!(submission.insertion, "unknown");
        assert_eq!(submission.outcome, "unknown");
        assert!(st.active_submission.is_none());
    }
    #[test]
    fn a_stopped_summary_is_summarizing_until_it_is_applied() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = State::default();
        let summarizing = |st: &State| {
            f.runner.sfp_summarizing(&f.store, st).unwrap();
            j.session(&f.runner.id).unwrap().summarizing
        };
        st.summary = Some((SummaryKind::Checkpoint, String::new(), 1));
        assert_eq!(summarizing(&st).as_deref(), Some("checkpoint"));
        // the turn stopped: its answer waits for the hub, the session still summarizes
        st.summary = None;
        st.completed = Some(SummaryDone { kind: SummaryKind::Checkpoint, text: Some("summary".into()) });
        assert!(f.runner.sfp_summarizing(&f.store, &st).unwrap().is_empty(), "no update while it is applied");
        assert_eq!(summarizing(&st).as_deref(), Some("checkpoint"));
        // applied (`summary_applied`)
        st.completed = None;
        assert_eq!(summarizing(&st), None);
    }
    #[test]
    fn ending_summary_and_authentication_block_queue_dispatch() {
        let mut st = State::default();
        st.ready = Some(("agent".into(), false));
        assert!(dispatch_ready(&st));
        st.completed = Some(SummaryDone { kind: SummaryKind::End, text: Some("summary".into()) });
        assert!(!dispatch_ready(&st), "end completion must retire before another prompt");
        st.completed = None;
        st.summary = Some((SummaryKind::Checkpoint, String::new(), 1));
        assert!(!dispatch_ready(&st));
        st.summary = None;
        st.auth = Some(relay::Event::AuthRequired { message: "login".into(), methods: vec![] });
        assert!(!dispatch_ready(&st));
    }
    #[test]
    fn prompt_error_holds_queue_and_successful_auth_does_not_resume_it_implicitly() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut input = f.input();
        input.submission_id = "queued".into();
        j.accept(&input, "queued").unwrap();
        let mut st = f.runner.st.lock().unwrap();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Stopped(acp::StopReason::Other("_sill_prompt_error".into())), 1).unwrap();
        assert!(j.submission(&f.runner.id, "queued").unwrap().held);
        let auth = relay::Event::AuthRequired { message: "login".into(), methods: vec![] };
        f.runner.sfp_project(&f.store, &mut st, &auth, 2).unwrap();
        st.auth = Some(auth);
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Other("_sill_authenticated".into()), 3).unwrap();
        assert!(st.auth.is_none());
        assert!(j.session(&f.runner.id).unwrap().authentication.is_none());
        assert!(j.submission(&f.runner.id, "queued").unwrap().held, "login success alone does not authorize queued delivery after interruption");
    }
    #[test]
    fn terminal_relay_cancels_open_questions_and_marks_unconfirmed_answer_unknown() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = f.runner.st.lock().unwrap();
        for request in [1, 2] {
            f.runner
                .sfp_project(
                    &f.store,
                    &mut st,
                    &relay::Event::PermissionRequest {
                        request,
                        tool_call_id: String::new(),
                        title: "Run?".into(),
                        options: vec![acp::PermissionOption { id: "yes".into(), name: "Yes".into(), kind: acp::PermissionOptionKind::AllowOnce }],
                    },
                    request,
                )
                .unwrap();
        }
        let open = st.adapter.question_id(1).unwrap().to_string();
        let answered = st.adapter.question_id(2).unwrap().to_string();
        let input = p::AnswerQuestion {
            session_id: f.runner.id.clone(),
            question_id: answered.clone(),
            answer_id: "a".into(),
            answer: p::Answer::Permission { option_id: "yes".into() },
        };
        f.runner.sfp_commit_answer(&f.store, &mut st, input).unwrap();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Ended(Some("lost".into())), 3).unwrap();
        assert_eq!(j.question(&f.runner.id, &open).unwrap().state, "cancelled");
        let q = j.question(&f.runner.id, &answered).unwrap();
        assert_eq!(q.state, "answered");
        assert_eq!(q.delivery.as_deref(), Some("unknown"));
        assert!(j.session(&f.runner.id).unwrap().question_ids.is_empty());
    }

    #[test]
    fn current_relay_cancellation_and_answer_resolution_confirm_outcomes() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        j.accept(&f.input(), "direct").unwrap();
        let mut st = f.runner.st.lock().unwrap();
        st.active_submission = Some("submission".into());
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Stopped(acp::StopReason::Cancelled), 1).unwrap();
        let submission = j.submission(&f.runner.id, "submission").unwrap();
        assert_eq!(submission.insertion, "confirmed");
        assert_eq!(submission.outcome, "cancelled");
        f.runner
            .sfp_project(
                &f.store,
                &mut st,
                &relay::Event::PermissionRequest { request: 1, tool_call_id: String::new(), title: "Run?".into(), options: vec![] },
                2,
            )
            .unwrap();
        let question_id = st.adapter.question_id(1).unwrap().to_string();
        f.runner
            .sfp_commit_answer(
                &f.store,
                &mut st,
                p::AnswerQuestion { session_id: f.runner.id.clone(), question_id: question_id.clone(), answer_id: "answer".into(), answer: p::Answer::Cancel },
            )
            .unwrap();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::Resolved { request: 1 }, 3).unwrap();
        assert_eq!(j.question(&f.runner.id, &question_id).unwrap().delivery.as_deref(), Some("confirmed"));
    }

    #[test]
    fn controls_and_image_support_seed_a_new_journal_idempotently() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = f.runner.st.lock().unwrap();
        st.controls = Some(acp::Controls {
            modes: Some(("auto".into(), vec![("auto".into(), "Auto".into()), ("read-only".into(), "Read only".into())])),
            options: vec![],
        });
        st.images = Some(true);
        assert!(j.session(&f.runner.id).unwrap().controls.is_none());
        assert_eq!(f.runner.sfp_initial_events(&f.store, &st).unwrap().len(), 1);
        let session = j.session(&f.runner.id).unwrap();
        assert_eq!(session.controls.unwrap().current_mode.as_deref(), Some("auto"));
        assert_eq!(session.capabilities.get("images"), Some(&true));
        let seq = j.snapshot(&f.runner.id).unwrap().0;
        assert!(f.runner.sfp_initial_events(&f.store, &st).unwrap().is_empty());
        assert_eq!(j.snapshot(&f.runner.id).unwrap().0, seq, "seeding is idempotent");
    }
    #[test]
    fn inline_images_are_stored_once_referenced_by_id_and_reach_the_agent_as_images() {
        let f = Fixture::new();
        let png = acp::base64(b"\x89PNG fake");
        let mut input = f.input();
        input.attachments = vec![p::Attachment::Inline { name: "screenshot-1.png (1180×518)".into(), mime: "image/png".into(), data_base64: png.clone() }];
        let stored = inline_as_uploads(&f.store, input.clone()).unwrap();
        let [p::Attachment::Uploaded { upload_id, name, mime }] = &stored.attachments[..] else { panic!("expected a reference: {:?}", stored.attachments) };
        assert_eq!((name.as_str(), mime.as_str()), ("screenshot-1.png (1180×518)", "image/png"));
        assert_eq!(inline_as_uploads(&f.store, input).unwrap(), stored, "a retried submission finds the same upload");
        let (accepted, _) = Journal::new(&f.store).unwrap().accept(&stored, "queued").unwrap();
        assert!(!serde_json::to_string(&accepted).unwrap().contains(&png), "the journal never carries the bytes");
        let fetched = super::super::super::uploads::get(&f.store, p::UploadGet { session_id: f.runner.id.clone(), upload_id: upload_id.clone() }).unwrap();
        assert_eq!(fetched.data_base64, png);
        let sent = attachments(&f.store, &f.runner.id, &accepted.attachments, true).unwrap();
        assert!(matches!(&sent[0].kind,acp::AttachmentKind::Image{data_base64} if data_base64==&png));
        // a long non-ASCII label is cut to the upload's 255 bytes, not 255 characters
        let mut long = f.input();
        long.submission_id = "long".into();
        long.attachments = vec![p::Attachment::Inline { name: "kuvakaappaus-ä".repeat(30), mime: "image/png".into(), data_base64: png.clone() }];
        let stored = inline_as_uploads(&f.store, long).unwrap();
        let [p::Attachment::Uploaded { name, .. }] = &stored.attachments[..] else { panic!("expected a reference") };
        assert!(name.len() <= 255 && name.starts_with("kuvakaappaus-ä"));
    }
    #[test]
    fn a_terminal_login_during_an_open_session_releases_the_queue() {
        // Claude opens its session without a login and asks at the first prompt; the person logs
        // in with the agent's own terminal command, and the frontend sends a retry when it closes
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = f.runner.st.lock().unwrap();
        let ask = relay::Event::AuthRequired { message: "Authentication required".into(), methods: vec![] };
        f.runner.sfp_project(&f.store, &mut st, &ask, 1).unwrap();
        f.runner.handle_locked(&f.store, &mut st, ask.clone(), false).unwrap();
        st.auth = Some(ask);
        j.accept(&f.input(), "queued").unwrap();
        assert!(!dispatch_ready(&st), "the queue waits for the login");
        assert!(j.session(&f.runner.id).unwrap().authentication.is_some());
        assert!(f.runner.sfp_login_events(&f.store, &mut st).unwrap().is_some());
        assert!(st.auth.is_none());
        assert!(j.session(&f.runner.id).unwrap().authentication.is_none(), "the login card closes");
        assert!(dispatch_ready(&st), "the queued prompt goes");
        assert!(f.runner.sfp_login_events(&f.store, &mut st).unwrap().is_none(), "a retry without a pending login changes nothing");
    }
    #[test]
    fn diagnostic_dumps_from_the_acp_loop_are_not_conversation() {
        let f = Fixture::new();
        let mut st = f.runner.st.lock().unwrap();
        let dump = relay::Event::Other("SessionInfoUpdate(SessionInfoUpdate { title: Undefined, updated_at: Undefined, meta: Some({\"codex\": Object".into());
        assert!(f.runner.sfp_project(&f.store, &mut st, &dump, 1).unwrap().is_empty());
        let failure = relay::Event::Other("set mode failed: internal error".into());
        assert_eq!(f.runner.sfp_project(&f.store, &mut st, &failure, 2).unwrap().len(), 1);
    }

    #[test]
    fn native_acceptance_survives_restart_without_claiming_completion_or_losing_insertion() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let (submission, _) = j.accept(&f.input(), "queued").unwrap();
        {
            let mut st = f.runner.st.lock().unwrap();
            f.runner
                .sfp_project(&f.store, &mut st, &relay::Event::V2Ready { session_id: "native".into(), resumed: false, controls: Default::default() }, 1)
                .unwrap();
            f.runner.sfp_commit_submission(&f.store, &f.dir, &mut st, submission, &[]).unwrap();
            f.runner.sfp_project(&f.store, &mut st, &relay::Event::TurnStarted, 2).unwrap();
            f.runner.sfp_project(&f.store, &mut st, &relay::Event::V2Update { update: json!({"sessionUpdate":"state_update","state":"idle"}) }, 3).unwrap();
            assert_eq!(j.submission(&f.runner.id, "submission").unwrap().outcome, "pending", "initial idle isn't completion");
            f.runner.sfp_project(&f.store, &mut st, &relay::Event::PromptAccepted { message_id: "user-native".into() }, 4).unwrap();
            let receipt = j.submission(&f.runner.id, "submission").unwrap();
            assert_eq!(receipt.insertion, "confirmed");
            assert_eq!(receipt.outcome, "pending");
            f.runner.save(&f.store, &st).unwrap();
        }
        let (store, runner) = f.reopened();
        let journal = Journal::new(&store).unwrap();
        let mut st = runner.st.lock().unwrap();
        assert!(st.adapter.is_v2());
        assert_eq!(st.active_submission.as_deref(), Some("submission"));
        runner.sfp_project(&store, &mut st, &relay::Event::Ended(Some("lost".into())), 5).unwrap();
        let receipt = journal.submission(&runner.id, "submission").unwrap();
        assert_eq!(receipt.insertion, "confirmed");
        assert_eq!(receipt.outcome, "unknown");
    }
    #[test]
    fn native_terminal_state_completes_once_and_cancellation_retires_permissions() {
        let f = Fixture::new();
        let j = Journal::new(&f.store).unwrap();
        let mut st = f.runner.st.lock().unwrap();
        f.runner
            .sfp_project(&f.store, &mut st, &relay::Event::V2Ready { session_id: "native".into(), resumed: false, controls: Default::default() }, 1)
            .unwrap();
        let (submission, _) = j.accept(&f.input(), "queued").unwrap();
        f.runner.sfp_commit_submission(&f.store, &f.dir, &mut st, submission, &[]).unwrap();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::TurnStarted, 2).unwrap();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::PromptAccepted { message_id: "u".into() }, 3).unwrap();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::V2Update { update: json!({"sessionUpdate":"state_update","state":"running"}) }, 4).unwrap();
        f.runner
            .sfp_project(
                &f.store,
                &mut st,
                &relay::Event::V2PermissionRequest {
                    request: 7,
                    tool_call_id: None,
                    title: "Run?".into(),
                    subject: Some(json!({"type":"command","command":"echo fixture","cwd":"/tmp"})),
                    options: vec![],
                },
                5,
            )
            .unwrap();
        let question = st.adapter.question_id(7).unwrap().to_string();
        f.runner.sfp_project(&f.store, &mut st, &relay::Event::V2PermissionsCancelled, 6).unwrap();
        assert_eq!(j.question(&f.runner.id, &question).unwrap().state, "cancelled");
        assert!(j.session(&f.runner.id).unwrap().question_ids.is_empty());
        let idle = relay::Event::V2Update { update: json!({"sessionUpdate":"state_update","state":"idle","stopReason":"cancelled"}) };
        f.runner.sfp_project(&f.store, &mut st, &idle, 7).unwrap();
        let receipt = j.submission(&f.runner.id, "submission").unwrap();
        assert_eq!(receipt.insertion, "confirmed");
        assert_eq!(receipt.outcome, "cancelled");
        assert!(st.active_submission.is_none());
        assert!(f.runner.sfp_project(&f.store, &mut st, &idle, 8).unwrap().is_empty(), "repeated idle is idempotent");
    }

    #[test]
    fn repeated_native_idle_does_not_append_duplicate_memory_text() {
        let f = Fixture::new();
        let mut st = f.runner.st.lock().unwrap();
        let events = [
            relay::Event::V2Ready { session_id: "native".into(), resumed: false, controls: Default::default() },
            relay::Event::TurnStarted,
            relay::Event::PromptAccepted { message_id: "user".into() },
            relay::Event::V2Update { update: json!({"sessionUpdate":"state_update","state":"running"}) },
            relay::Event::V2Update { update: json!({"sessionUpdate":"agent_message","messageId":"answer","content":[{"type":"text","text":"old"}]}) },
            relay::Event::V2Update { update: json!({"sessionUpdate":"agent_message","messageId":"answer","content":[{"type":"text","text":"final"}]}) },
            relay::Event::V2Update { update: json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}) },
            relay::Event::V2Update { update: json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}) },
        ];
        for (index, event) in events.into_iter().enumerate() {
            f.runner.sfp_project(&f.store, &mut st, &event, index as u64 + 1).unwrap();
            f.runner.handle_locked(&f.store, &mut st, event, false).unwrap();
        }
        let entries = f.store.entries(&f.runner.id).unwrap();
        let agent = entries.iter().filter(|e| e.kind == "agent").collect::<Vec<_>>();
        assert_eq!(agent.len(), 1);
        assert_eq!(agent[0].text, "final");
    }
}
