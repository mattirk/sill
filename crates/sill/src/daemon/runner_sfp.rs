//! Durable SFP acceptance and the ACP v1 dispatch boundary.
use super::*;
use sill_core::sfp::{Journal, Source};
use sill_proto::{sfp as p, rpc::Error};

impl Runner {
    pub(in crate::daemon) fn sfp_control(&self, hub:&Hub, control:p::Control)->Result<()> {
        let command=match control {
            p::Control::Authenticate{method_id}=>wire::Command::Authenticate(method_id),
            p::Control::RetrySession=>wire::Command::RetrySession,
            p::Control::SetMode{mode_id}=>wire::Command::SetMode(mode_id),
            p::Control::SetConfig{option_id,value}=>wire::Command::SetConfig(option_id,match value{p::ConfigValue::Id{value}=>wire::ConfigValue::Id(value),p::ConfigValue::Bool{value}=>wire::ConfigValue::Bool(value)}),
        };
        self.command(hub,0,command)
    }
    pub(in crate::daemon) fn sfp_cancel_turn(&self,hub:&Hub,turn_id:&str)->Result<()> {
        hub.with_store(|store| {
            let mut st=self.st.lock().unwrap();
            if st.adapter.turn_id()!=Some(turn_id) || !st.busy {return Err(Error::domain("invalid_state","target turn is no longer active").into());}
            self.commit_command(store,&hub.home,&mut st,wire::Command::Cancel)?;
            self.drain(store,&mut st)
        })
    }
    pub(in crate::daemon) fn sfp_initialize(&self, store: &Store, hub:&Hub) -> Result<()> {
        let st = self.st.lock().unwrap();
        self.sfp_initialize_locked(store,hub,&st)
    }
    fn sfp_initialize_locked(&self,store:&Store,hub:&Hub,st:&State)->Result<()> {
        let tx=store.conn().unchecked_transaction()?;
        let events=self.sfp_initial_events(store,st)?;
        // Persist the generation before any submission can become accepted under it.
        self.save(store,st)?;
        tx.commit()?;
        hub.sfp_publish(&self.id,&events);
        Ok(())
    }
    fn sfp_initial_events(&self,store:&Store,st:&State)->Result<Vec<p::StoredEvent>> {
        let journal=Journal::new(store)?;
        let state=journal.ensure(&self.id,&st.generation_id)?;
        let mut events=if state.agent_generation_id!=st.generation_id {
            journal.replace_generation(&self.id,&st.generation_id)?
        } else {vec![]};
        // A relay adopted from before SFP (or a journal created after Ready) already delivered
        // its Ready/Capabilities: the runner holds them, the journal never saw them.
        let session=journal.session(&self.id)?;
        let mut seeded=session.clone();
        if seeded.controls.is_none() && let Some(c)=&st.controls { seeded.controls=Some(super::super::sfp_adapter::session_controls(c)); }
        if !seeded.capabilities.contains_key("images") && let Some(images)=st.images { seeded.capabilities.insert("images".into(),images); }
        if seeded!=session { events.extend(journal.append(&self.id,None,&[p::Event::SessionUpdated{session:seeded}])?); }
        Ok(events)
    }
    pub(super) fn sfp_recover(&self,store:&Store,hub:&Hub)->Result<()> {
        let mut st=self.st.lock().unwrap();
        self.sfp_initialize_locked(store,hub,&st)?;
        self.drain(store,&mut st)?;
        // A crash may have happened after durable acceptance but before dispatch, with no
        // future relay event available to wake an idle queue.
        self.sfp_dispatch(store,hub,&mut st)
    }
    pub(super) fn sfp_project(&self, store:&Store, st:&mut State, event:&relay::Event, seq:u64)->Result<Vec<p::StoredEvent>> {
        let journal=Journal::new(store)?;
        let mut session=journal.ensure(&self.id,&st.generation_id)?;
        let mut out=vec![];
        if session.agent_generation_id != st.generation_id {
            out.extend(journal.replace_generation(&self.id,&st.generation_id)?);
            session=journal.session(&self.id)?;
        }
        let source=(seq>0).then(||Source{generation_id:st.generation_id.clone(),seq});
        if let Some(source)=&source { if journal.source_seen(&self.id,source)? { return Ok(out); } }
        // ACP v1 session/load replay has no durable source identities. Existing history stays;
        // do not invent mappings from matching text or create duplicate history.
        if st.replaying && matches!(event,relay::Event::Text(_)|relay::Event::Thought(_)|relay::Event::UserText(_)|relay::Event::ToolCall{..}|relay::Event::ToolCallUpdate{..}|relay::Event::SubagentText{..}) {
            out.extend(journal.append(&self.id,source,&[])?); return Ok(out);
        }
        // An older relay reports a prompt error as Cancelled too. A cancel this daemon sent
        // during the turn makes cancellation the explanation (a coincident error would read
        // as cancelled; it is never resent either way).
        let cancel_evidence=self.relay.outcome_evidence() || st.cancel_sent;
        let question=match event { relay::Event::Resolved{request}=>st.adapter.question_id(*request).map(|id|journal.question(&self.id,id)).transpose()?, _=>None };
        // The relay can outlive this daemon version. The older success note carries the same
        // evidence as the explicit private marker introduced with SFP.
        let authenticated=matches!(event,relay::Event::Other(text) if text=="_sill_authenticated" || text=="logged in; send the message again");
        let mut events=if authenticated {
            st.auth=None;
            let mut next=session.clone();next.authentication=None;
            if next.question_ids.is_empty() && next.turn_id.is_none() {next.activity="idle".into();}
            vec![p::Event::SessionUpdated{session:next}]
        } else {st.adapter.project(&session,event,question.as_ref())};
        // Older live relays used Cancelled for both remote cancellation and local errors, and
        // emitted Resolved even after a callback disappeared. Their handshake cannot prove more.
        if !self.relay.outcome_evidence() && matches!(event,relay::Event::Resolved{..}) {
            for e in &mut events {
                if let p::Event::QuestionUpdated{question}=e {
                    if question.delivery.as_deref()==Some("confirmed") {question.delivery=Some("unknown".into());}
                }
            }
        }
        if !cancel_evidence && matches!(event,relay::Event::Stopped(acp::StopReason::Cancelled)) {
            for e in &mut events {
                if let p::Event::TurnUpdated{turn}=e {
                    turn.outcome="unknown".into();
                    turn.reason=Some(p::Reason{code:"legacy_ambiguous_stop".into(),message:"The relay cannot distinguish cancellation from a prompt error".into()});
                }
            }
        }
        let interrupted=matches!(event,relay::Event::Stopped(acp::StopReason::Other(reason)) if reason=="_sill_prompt_error")
            || matches!(event,relay::Event::Stopped(acp::StopReason::Cancelled)) && !cancel_evidence
            || matches!(event,relay::Event::AuthRequired{..}|relay::Event::Ended(_));
        if interrupted {
            let reason=if matches!(event,relay::Event::AuthRequired{..}) {"authentication_required"}
                else if matches!(event,relay::Event::Ended(_)) {"agent_lost"} else {"adapter_error"};
            let (_,records)=journal.snapshot(&self.id)?;
            for record in records {
                match record {
                    p::Record::Submission{mut submission} if submission.delivery=="queued" && submission.insertion=="pending" && submission.outcome=="pending" => {
                        submission.held=true;submission.hold_reason=Some(p::Reason{code:reason.into(),message:"Resume explicitly after the agent is ready".into()});
                        events.push(p::Event::SubmissionUpdated{submission});
                    }
                    p::Record::Question{mut question} if matches!(event,relay::Event::Ended(_)) => {
                        if question.state=="open" {
                            question.state="cancelled".into();question.reason=Some(p::Reason{code:"agent_lost".into(),message:"The agent connection ended".into()});
                            events.push(p::Event::QuestionUpdated{question});
                        } else if question.state=="answered" && question.delivery.as_deref()==Some("pending") {
                            question.delivery=Some("unknown".into());events.push(p::Event::QuestionUpdated{question});
                        }
                    }
                    _=>{}
                }
            }
        }
        if matches!(event,relay::Event::Ended(_)) {
            st.answer_outbox.clear();st.outbox.clear();
            // A terminal relay will not send Resolved for its remaining questions.
            let mut next=events.iter().rev().find_map(|e|match e {p::Event::SessionUpdated{session}=>Some(session.clone()),_=>None}).unwrap_or_else(||session.clone());
            next.question_ids.clear();next.activity="unknown".into();
            events.retain(|e|!matches!(e,p::Event::SessionUpdated{..}));
            events.push(p::Event::SessionUpdated{session:next});
        }
        if let Some(id)=st.active_submission.clone() {
            let mut submission=journal.submission(&self.id,&id)?;
            match event {
                // ACP v1 run loop emits TurnStarted when it begins this command's prompt call;
                // it does not prove remote insertion. Completion is stronger evidence below.
                relay::Event::Stopped(reason)=>{
                    let known = !matches!(reason, acp::StopReason::Other(_)) && (*reason!=acp::StopReason::Cancelled || cancel_evidence);
                    submission.insertion=if known {"confirmed"} else {"unknown"}.into();
                    submission.outcome=if !known {"unknown"} else if *reason==acp::StopReason::Cancelled {"cancelled"} else if *reason==acp::StopReason::Refusal {"failed"} else {"completed"}.into();
                    events.push(p::Event::SubmissionUpdated{submission}); st.active_submission=None;
                }
                relay::Event::Ended(_) | relay::Event::AuthRequired{..}=>{
                    submission.insertion="unknown".into(); submission.outcome="unknown".into();
                    submission.reason=Some(p::Reason{code:"adapter_interrupted".into(),message:"No conclusive prompt result from the agent".into()});
                    events.push(p::Event::SubmissionUpdated{submission}); st.active_submission=None;
                }
                _=>{}
            }
        }
        out.extend(journal.append(&self.id,source,&events)?);
        Ok(out)
    }
    pub(in crate::daemon) fn sfp_submit(&self,hub:&Hub,input:p::Submit)->Result<p::Submission> {
        if input.session_id != self.id { return Err(Error::domain("not_found","session not found").into()); }
        hub.with_store(|store| {
            let mut st=self.st.lock().unwrap();
            self.sfp_initialize_locked(store,hub,&st)?;
            if st.ended || st.retired {return Err(Error::domain("invalid_state","the agent connection ended; explicitly restart the session before submitting new work").into());}
            validate_attachments(store,&self.id,&input.attachments)?;
            let journal=Journal::new(store)?;
            // Accept records even while the adapter is starting/busy; dispatch owns readiness.
            let (submission,events)=journal.accept(&input,"queued")?;
            hub.sfp_publish(&self.id,&events);
            // Once accepted, delivery failure cannot turn success into an ambiguous rejection.
            if let Err(e)=self.sfp_dispatch(store,hub,&mut st) { eprintln!("sill daemon: SFP dispatch {}: {e:#}",self.id); }
            Ok(journal.submission(&self.id,&submission.submission_id)?)
        })
    }
    pub(super) fn sfp_dispatch(&self,store:&Store,hub:&Hub,st:&mut State)->Result<()> {
        if !dispatch_ready(st) { return Ok(()); }
        let journal=Journal::new(store)?;
        let (_,records)=journal.snapshot(&self.id)?;
        let mut queued:Vec<_>=records.into_iter().filter_map(|r|match r {p::Record::Submission{submission} if submission.delivery=="queued" && submission.outcome=="pending"=>Some(submission),_=>None}).collect();
        queued.sort_by_key(|s|s.accepted_seq.parse::<u64>().unwrap_or(u64::MAX));
        let Some(submission)=queued.into_iter().next() else { return Ok(()); };
        if submission.held { return Ok(()); }
        let text=submission.text.clone();
        let events=self.sfp_commit_submission(store,&hub.home,st,submission)?;
        hub.sfp_publish(&self.id,&events);
        broadcast(&self.id,&mut st.subs,&wire::Event::UserText(text));
        self.drain(store,st)
    }
    /// Durable delivery intent and exact prepared payload, before any relay send.
    fn sfp_commit_submission(&self,store:&Store,home:&std::path::Path,st:&mut State,mut submission:p::Submission)->Result<Vec<p::StoredEvent>> {
        let journal=Journal::new(store)?;
        let before=st.clone();
        let result=(||->Result<Vec<p::StoredEvent>>{
            let tx=store.conn().unchecked_transaction()?;
            let prepared=st.context.prepare(store,&self.id,home,&submission.text)?;
            let attachments=attachments(store,&self.id,&submission.attachments)?;
            Self::queue(st,relay::Command::Prompt{text:prepared.text.clone(),attachments});
            st.context.commit(prepared); st.busy=true; st.active_submission=Some(submission.submission_id.clone());
            submission.delivery="direct".into();
            let events=journal.append(&self.id,None,&[p::Event::SubmissionUpdated{submission:submission.clone()}])?;
            self.save(store,st)?;tx.commit()?;Ok(events)
        })();
        match result {Ok(events)=>Ok(events),Err(e)=>{*st=before;Err(e)}}
    }
    pub(in crate::daemon) fn sfp_queue_action(&self,hub:&Hub,key:&p::SubmissionKey,resume:bool)->Result<p::Submission> {
        hub.with_store(|store| {
            let mut st=self.st.lock().unwrap();self.sfp_initialize_locked(store,hub,&st)?;let journal=Journal::new(store)?;
            let (_,events)=if resume {journal.resume(key)?}else{journal.cancel(key)?};
            hub.sfp_publish(&self.id,&events);
            if let Err(e)=self.sfp_dispatch(store,hub,&mut st){eprintln!("sill daemon: queued dispatch: {e:#}");}
            journal.submission(&self.id,&key.submission_id)
        })
    }
    pub(in crate::daemon) fn sfp_answer(&self,hub:&Hub,input:p::AnswerQuestion)->Result<p::Question> {
        hub.with_store(|store| {let mut st=self.st.lock().unwrap();self.sfp_initialize_locked(store,hub,&st)?;self.sfp_answer_locked(store,hub,&mut st,input)})
    }
    pub(super) fn sfp_answer_locked(&self,store:&Store,hub:&Hub,st:&mut State,input:p::AnswerQuestion)->Result<p::Question> {
        let (question,events)=self.sfp_commit_answer(store,st,input)?;
        hub.sfp_publish(&self.id,&events);
        if let Err(e)=self.drain(store,st){eprintln!("sill daemon: answer awaiting delivery: {e:#}");}
        Ok(question)
    }
    fn sfp_commit_answer(&self,store:&Store,st:&mut State,input:p::AnswerQuestion)->Result<(p::Question,Vec<p::StoredEvent>)> {
        let journal=Journal::new(store)?;
        let before=st.clone();
        let result=(||->Result<(p::Question,Vec<p::StoredEvent>)>{
            let tx=store.conn().unchecked_transaction()?;
            let (question,events)=journal.answer(&input)?;
            if !events.is_empty() {
                let request=st.adapter.request_for_question(&question.question_id).ok_or_else(||Error::domain("invalid_state","question belongs to an unavailable agent"))?;
                let answer=match &input.answer {
                    p::Answer::Permission{option_id}=>relay::Answer::Permission(Some(option_id.clone())),
                    p::Answer::Form{values}=>relay::Answer::Form(Some(values.clone())),
                    p::Answer::Cancel if question.kind=="permission"=>relay::Answer::Permission(None),
                    p::Answer::Cancel=>relay::Answer::Form(None),
                };
                st.answer_outbox.insert(request,answer);self.save(store,st)?;
            }
            tx.commit()?;Ok((question,events))
        })();
        match result{Ok(x)=>Ok(x),Err(e)=>{*st=before;Err(e)}}
    }
    pub(super) fn sfp_auto_answer(&self,store:&Store,hub:&Hub,st:&mut State,event:&relay::Event)->Result<()> {
        if let relay::Event::PermissionRequest{request,options,..}=event {
            let Some(question_id)=st.adapter.question_id(*request).map(str::to_owned) else{return Ok(())};
            let journal=Journal::new(store)?;
            if journal.question(&self.id,&question_id)?.state!="open" {return Ok(());}
            let option=options.iter().find(|o|o.kind==acp::PermissionOptionKind::AllowOnce).or_else(||options.iter().find(|o|o.kind==acp::PermissionOptionKind::AllowAlways));
            let answer=option.map(|o|p::Answer::Permission{option_id:o.id.clone()}).unwrap_or(p::Answer::Cancel);
            self.sfp_answer_locked(store,hub,st,p::AnswerQuestion{session_id:self.id.clone(),question_id,answer_id:format!("policy-{}-{request}",st.generation_id),answer})?;
        }
        Ok(())
    }
}
fn dispatch_ready(st:&State)->bool {
    !st.busy && st.ready.is_some() && !st.ended && !st.retired && !st.context.restoring()
        && st.active_submission.is_none() && st.auth.is_none() && st.summary.is_none() && st.completed.is_none()
}
fn validate_attachments(store:&Store,session_id:&str,input:&[p::Attachment])->Result<()> {
    let mut size=0usize;
    if input.len()>32 {return Err(Error::domain("limit_exceeded","a submission supports at most 32 attachments").into());}
    for a in input {
        match a {
            p::Attachment::Inline{mime,data_base64,..}=>{
                if !mime.starts_with("image/") {return Err(Error::domain("unsupported","inline attachments support images; upload other files first").into());}
                size=size.saturating_add(super::super::uploads::decode(data_base64)?.len());
            }
            p::Attachment::Uploaded {upload_id,name,mime}=>super::super::uploads::validate(store,session_id,upload_id,name,mime)?,
        }
    }
    if size>1024*1024 {return Err(Error::domain("limit_exceeded","inline attachments exceed 1 MiB").into());}Ok(())
}
fn attachments(store:&Store,session_id:&str,input:&[p::Attachment])->Result<Vec<acp::Attachment>> {
    validate_attachments(store,session_id,input)?;
    input.iter().map(|a|match a {
        p::Attachment::Inline{name,mime,data_base64}=>Ok(acp::Attachment{name:name.clone(),mime:mime.clone(),kind:acp::AttachmentKind::Image{data_base64:data_base64.clone()}}),
        p::Attachment::Uploaded{upload_id,..}=>super::super::uploads::resolve(store,session_id,upload_id),
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead,BufReader,Write};
    use std::os::unix::net::UnixListener;
    struct Fixture {
        dir:std::path::PathBuf,
        store:Store,
        runner:Arc<Runner>,
        received:std::sync::mpsc::Receiver<serde_json::Value>,
    }
    impl Fixture {
        fn new()->Self {Self::with_evidence(false)}
        fn with_evidence(outcome_evidence:bool)->Self {
            let dir=std::env::temp_dir().join(format!("sill-sfp-runner-{}",sill_core::sfp::new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let socket=dir.join("relay.sock"); let listener=UnixListener::bind(&socket).unwrap();
            let (tx,received)=std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let (mut stream,_)=listener.accept().unwrap();let mut reader=BufReader::new(stream.try_clone().unwrap());
                let mut hello=String::new();reader.read_line(&mut hello).unwrap();
                writeln!(stream,"{}",serde_json::json!({"Hello":{"acknowledged":0,"last":0,"version":relay::RELAY_PROTOCOL,"outcome_evidence":outcome_evidence}})).unwrap();
                for line in reader.lines() {
                    let Ok(line)=line else {break};let message:serde_json::Value=serde_json::from_str(&line).unwrap();
                    let response=if let Some(c)=message.get("Command") {serde_json::json!({"Accepted":{"id":c["id"],"error":null}})}
                        else if let Some(a)=message.get("Answer") {serde_json::json!({"Answered":{"request":a["request"],"error":null}})} else {continue};
                    tx.send(message).ok(); if writeln!(stream,"{response}").is_err(){break;}
                }
            });
            let (client,_)=relay::connect(&socket,0).unwrap();
            let store=Store::open(&dir.join("s.db")).unwrap();let id=store.create("test","default").unwrap().id;
            Runner::load(&id,&store).unwrap();let runner=Runner::new(&id,client,serde_json::Value::Null).unwrap();
            {let mut st=runner.st.lock().unwrap();st.ready=Some(("agent".into(),false));Journal::new(&store).unwrap().ensure(&id,&st.generation_id).unwrap();runner.save(&store,&st).unwrap();}
            Self {dir,store,runner,received}
        }
        fn input(&self)->p::Submit {p::Submit {session_id:self.runner.id.clone(),submission_id:"submission".into(),text:"work".into(),attachments:vec![]}}
        fn reopened(&self)->(Store,Arc<Runner>) {
            let store=Store::open(&self.dir.join("s.db")).unwrap();
            let runner=Runner::new(&self.runner.id,self.runner.relay.clone(),Runner::load(&self.runner.id,&store).unwrap()).unwrap();
            (store,runner)
        }
    }
    impl Drop for Fixture {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.dir);}}

    #[test]
    fn dispatch_commit_keeps_exact_payload_and_identity_after_reopen() {
        let f=Fixture::new();let j=Journal::new(&f.store).unwrap();
        let (accepted,_)=j.accept(&f.input(),"queued").unwrap();
        let payload;
        {
            let mut st=f.runner.st.lock().unwrap();
            f.runner.sfp_commit_submission(&f.store,&f.dir,&mut st,accepted).unwrap();
            payload=serde_json::to_value(&st.outbox).unwrap();
        }
        assert!(f.received.try_recv().is_err(),"commit must precede any relay delivery");
        let (store,runner)=f.reopened();
        {
            let mut st=runner.st.lock().unwrap();
            assert_eq!(serde_json::to_value(&st.outbox).unwrap(),payload);
            assert_eq!(st.active_submission.as_deref(),Some("submission"));
            let (retry,events)=Journal::new(&store).unwrap().accept(&f.input(),"queued").unwrap();
            assert!(events.is_empty());assert_eq!(retry.delivery,"direct");
            runner.drain(&store,&mut st).unwrap();assert!(st.outbox.is_empty());
        }
        let delivered=f.received.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(delivered["Command"]["id"],1);
        assert_eq!(delivered["Command"]["command"],payload["1"]);
        assert_eq!(store.entry_count(&runner.id).unwrap(),1);
    }

    #[test]
    fn failed_dispatch_commit_rolls_back_delivery_and_preserves_accepted_queue() {
        let f=Fixture::new();let j=Journal::new(&f.store).unwrap();let (accepted,_)=j.accept(&f.input(),"queued").unwrap();
        f.store.conn().execute_batch("CREATE TRIGGER fail_dispatch BEFORE INSERT ON daemon_runners BEGIN SELECT RAISE(ABORT,'test failure'); END").unwrap();
        let mut st=f.runner.st.lock().unwrap();let before=serde_json::to_value(&*st).unwrap();
        assert!(f.runner.sfp_commit_submission(&f.store,&f.dir,&mut st,accepted).is_err());
        assert_eq!(serde_json::to_value(&*st).unwrap(),before);
        assert_eq!(j.submission(&f.runner.id,"submission").unwrap().delivery,"queued");
        assert_eq!(j.snapshot(&f.runner.id).unwrap().1.iter().filter(|r|matches!(r,p::Record::Item{..})).count(),1,"accepted user item survives dispatch failure");
        assert!(f.received.try_recv().is_err());
    }

    #[test]
    fn winning_answer_and_delivery_intent_survive_restart_before_send() {
        let f=Fixture::new();let input;
        {
            let mut st=f.runner.st.lock().unwrap();
            f.runner.sfp_project(&f.store,&mut st,&relay::Event::PermissionRequest {request:7,tool_call_id:String::new(),title:"Run?".into(),options:vec![acp::PermissionOption {id:"yes".into(),name:"Yes".into(),kind:acp::PermissionOptionKind::AllowOnce}]},1).unwrap();
            input=p::AnswerQuestion {session_id:f.runner.id.clone(),question_id:st.adapter.question_id(7).unwrap().into(),answer_id:"answer".into(),answer:p::Answer::Permission {option_id:"yes".into()}};
            f.runner.sfp_commit_answer(&f.store,&mut st,input.clone()).unwrap();
        }
        assert!(f.received.try_recv().is_err());
        let (store,runner)=f.reopened();
        {
            let mut st=runner.st.lock().unwrap();
            assert!(st.answer_outbox.contains_key(&7));
            let (question,events)=runner.sfp_commit_answer(&store,&mut st,input).unwrap();
            assert!(events.is_empty());assert_eq!(question.answer_id.as_deref(),Some("answer"));
            runner.drain(&store,&mut st).unwrap();assert!(st.answer_outbox.is_empty());
        }
        let delivered=f.received.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(delivered["Answer"]["request"],7);
        assert_eq!(delivered["Answer"]["answer"],serde_json::json!({"Permission":"yes"}));
    }

    #[test]
    fn prompt_error_does_not_claim_insertion_or_success() {
        let f=Fixture::new();let j=Journal::new(&f.store).unwrap();j.accept(&f.input(),"direct").unwrap();
        let mut st=f.runner.st.lock().unwrap();st.active_submission=Some("submission".into());
        f.runner.sfp_project(&f.store,&mut st,&relay::Event::Stopped(acp::StopReason::Other("_sill_prompt_error".into())),1).unwrap();
        let submission=j.submission(&f.runner.id,"submission").unwrap();
        assert_eq!(submission.insertion,"unknown");assert_eq!(submission.outcome,"unknown");
        assert!(st.active_submission.is_none());
    }
    #[test]
    fn ending_summary_and_authentication_block_queue_dispatch() {
        let mut st=State::default();st.ready=Some(("agent".into(),false));
        assert!(dispatch_ready(&st));
        st.completed=Some(SummaryDone {kind:SummaryKind::End,text:Some("summary".into())});
        assert!(!dispatch_ready(&st),"end completion must retire before another prompt");
        st.completed=None;st.summary=Some((SummaryKind::Checkpoint,String::new(),1));
        assert!(!dispatch_ready(&st));st.summary=None;
        st.auth=Some(relay::Event::AuthRequired {message:"login".into(),methods:vec![]});
        assert!(!dispatch_ready(&st));
    }
    #[test]
    fn prompt_error_holds_queue_and_successful_auth_does_not_resume_it_implicitly() {
        let f=Fixture::new();let j=Journal::new(&f.store).unwrap();
        let mut input=f.input();input.submission_id="queued".into();j.accept(&input,"queued").unwrap();
        let mut st=f.runner.st.lock().unwrap();
        f.runner.sfp_project(&f.store,&mut st,&relay::Event::Stopped(acp::StopReason::Other("_sill_prompt_error".into())),1).unwrap();
        assert!(j.submission(&f.runner.id,"queued").unwrap().held);
        let auth=relay::Event::AuthRequired {message:"login".into(),methods:vec![]};
        f.runner.sfp_project(&f.store,&mut st,&auth,2).unwrap();st.auth=Some(auth);
        f.runner.sfp_project(&f.store,&mut st,&relay::Event::Other("_sill_authenticated".into()),3).unwrap();
        assert!(st.auth.is_none());assert!(j.session(&f.runner.id).unwrap().authentication.is_none());
        assert!(j.submission(&f.runner.id,"queued").unwrap().held,"login success alone does not authorize queued delivery after interruption");
    }
    #[test]
    fn terminal_relay_cancels_open_questions_and_marks_unconfirmed_answer_unknown() {
        let f=Fixture::new();let j=Journal::new(&f.store).unwrap();let mut st=f.runner.st.lock().unwrap();
        for request in [1,2] {
            f.runner.sfp_project(&f.store,&mut st,&relay::Event::PermissionRequest {request,tool_call_id:String::new(),title:"Run?".into(),options:vec![acp::PermissionOption {id:"yes".into(),name:"Yes".into(),kind:acp::PermissionOptionKind::AllowOnce}]},request).unwrap();
        }
        let open=st.adapter.question_id(1).unwrap().to_string();let answered=st.adapter.question_id(2).unwrap().to_string();
        let input=p::AnswerQuestion {session_id:f.runner.id.clone(),question_id:answered.clone(),answer_id:"a".into(),answer:p::Answer::Permission {option_id:"yes".into()}};
        f.runner.sfp_commit_answer(&f.store,&mut st,input).unwrap();
        f.runner.sfp_project(&f.store,&mut st,&relay::Event::Ended(Some("lost".into())),3).unwrap();
        assert_eq!(j.question(&f.runner.id,&open).unwrap().state,"cancelled");
        let q=j.question(&f.runner.id,&answered).unwrap();assert_eq!(q.state,"answered");assert_eq!(q.delivery.as_deref(),Some("unknown"));
        assert!(j.session(&f.runner.id).unwrap().question_ids.is_empty());
    }

    #[test]
    fn adopted_relays_cannot_claim_outcomes_their_old_events_do_not_prove() {
        for evidence in [false,true] {
            let f=Fixture::with_evidence(evidence);let j=Journal::new(&f.store).unwrap();
            j.accept(&f.input(),"direct").unwrap();let mut st=f.runner.st.lock().unwrap();
            st.active_submission=Some("submission".into());
            f.runner.sfp_project(&f.store,&mut st,&relay::Event::Stopped(acp::StopReason::Cancelled),1).unwrap();
            let submission=j.submission(&f.runner.id,"submission").unwrap();
            assert_eq!(submission.insertion,if evidence {"confirmed"} else {"unknown"});
            assert_eq!(submission.outcome,if evidence {"cancelled"} else {"unknown"});
            f.runner.sfp_project(&f.store,&mut st,&relay::Event::PermissionRequest {request:1,tool_call_id:String::new(),title:"Run?".into(),options:vec![]},2).unwrap();
            let question_id=st.adapter.question_id(1).unwrap().to_string();
            f.runner.sfp_commit_answer(&f.store,&mut st,p::AnswerQuestion {session_id:f.runner.id.clone(),question_id:question_id.clone(),answer_id:"answer".into(),answer:p::Answer::Cancel}).unwrap();
            f.runner.sfp_project(&f.store,&mut st,&relay::Event::Resolved{request:1},3).unwrap();
            assert_eq!(j.question(&f.runner.id,&question_id).unwrap().delivery.as_deref(),Some(if evidence {"confirmed"} else {"unknown"}));
        }
    }

    #[test]
    fn an_old_relay_cancelled_by_this_daemon_records_a_cancellation() {
        let f=Fixture::new();let j=Journal::new(&f.store).unwrap();
        j.accept(&f.input(),"direct").unwrap();let mut st=f.runner.st.lock().unwrap();
        st.active_submission=Some("submission".into());st.busy=true;
        f.runner.commit_command(&f.store,&f.dir,&mut st,wire::Command::Cancel).unwrap();
        assert!(st.cancel_sent);
        let stopped=relay::Event::Stopped(acp::StopReason::Cancelled);
        let events=f.runner.sfp_project(&f.store,&mut st,&stopped,1).unwrap();
        f.runner.handle_locked(&f.store,&mut st,stopped,false).unwrap();
        let submission=j.submission(&f.runner.id,"submission").unwrap();
        assert_eq!((submission.insertion.as_str(),submission.outcome.as_str()),("confirmed","cancelled"));
        assert!(!events.iter().any(|e|matches!(&e.event,p::Event::TurnUpdated{turn} if turn.outcome=="unknown")));
        assert!(!st.cancel_sent,"the evidence belongs to that turn only");
    }
    #[test]
    fn controls_and_image_support_an_adopted_relay_reported_before_sfp_reach_the_journal() {
        let f=Fixture::new();let j=Journal::new(&f.store).unwrap();
        let mut st=f.runner.st.lock().unwrap();
        st.controls=Some(acp::Controls{modes:Some(("auto".into(),vec![("auto".into(),"Auto".into()),("read-only".into(),"Read only".into())])),options:vec![]});
        st.images=Some(true);
        assert!(j.session(&f.runner.id).unwrap().controls.is_none());
        assert_eq!(f.runner.sfp_initial_events(&f.store,&st).unwrap().len(),1);
        let session=j.session(&f.runner.id).unwrap();
        assert_eq!(session.controls.unwrap().current_mode.as_deref(),Some("auto"));
        assert_eq!(session.capabilities.get("images"),Some(&true));
        let seq=j.snapshot(&f.runner.id).unwrap().0;
        assert!(f.runner.sfp_initial_events(&f.store,&st).unwrap().is_empty());
        assert_eq!(j.snapshot(&f.runner.id).unwrap().0,seq,"seeding is idempotent");
    }
    #[test]
    fn diagnostic_dumps_from_the_acp_loop_are_not_conversation() {
        let f=Fixture::new();let mut st=f.runner.st.lock().unwrap();
        let dump=relay::Event::Other("SessionInfoUpdate(SessionInfoUpdate { title: Undefined, updated_at: Undefined, meta: Some({\"codex\": Object".into());
        assert!(f.runner.sfp_project(&f.store,&mut st,&dump,1).unwrap().is_empty());
        let failure=relay::Event::Other("set mode failed: internal error".into());
        assert_eq!(f.runner.sfp_project(&f.store,&mut st,&failure,2).unwrap().len(),1);
    }

}
