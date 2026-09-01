use super::*;
use crate::{scratch::ScratchDir, RuntimeEvents};

fn machine(value: &str) -> MachineId {
    MachineId::new(value).expect("non-empty")
}

fn session(machine_id: &str, sid: &str) -> SessionKey {
    SessionKey::new(machine(machine_id), SessionId::new(sid).expect("non-empty"))
}

fn workspace(machine_id: &str, path: &str) -> WorkspaceId {
    WorkspaceId::new(machine(machine_id), path).expect("non-empty")
}

fn live(key: SessionKey, generation: u64) -> LiveSession {
    LiveSession {
        workspace: workspace(key.machine().as_str(), "/repo"),
        key,
        lifecycle: SessionLifecycle::Attached,
        attachment_generation: generation,
        replay: ReplayState {
            cursor: Some("leaf-a".to_string()),
            repair: RepairState::Idle,
        },
        model_catalog: ModelsState::NotRequested,
        model_selection: ModelSelection::LivePending,
        branch: None,
        replica: SessionReplica::default(),
        dialogs: Vec::new(),
        event_revision: 0,
    }
}

fn engine() -> (Engine, RuntimeEvents) {
    let events = Arc::new(RuntimeEventHub::default());
    let receiver = events.subscribe();

    (
        Engine::new(
            Fleet::inert(),
            None,
            Arc::new(Mutex::new(Arc::new(ClientSnapshot::default()))),
            events,
        ),
        receiver,
    )
}

fn insert_live(engine: &mut Engine, session: LiveSession) {
    Arc::make_mut(&mut engine.state.live_sessions).insert(session.key.clone(), Arc::new(session));
}

fn question(id: &str) -> Value {
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "confirm",
        "title": "Continue?",
        "message": "Proceed"
    })
}

#[test]
fn two_same_sid_sessions_on_different_machines_route_independently() {
    let first = session("first", "same");
    let second = session("second", "same");
    let (mut engine, _) = engine();

    insert_live(&mut engine, live(first.clone(), 1));
    insert_live(&mut engine, live(second.clone(), 1));
    engine.handle_pi_event(
        first.machine().clone(),
        first.session().to_string(),
        1,
        json!({"type": "message_end", "message": {"role": "user", "content": "remote"}}),
    );

    assert_eq!(
        engine
            .state
            .live_session(&first)
            .expect("first")
            .replica
            .transcript()
            .items()
            .len(),
        1
    );
    assert!(engine
        .state
        .live_session(&second)
        .expect("second")
        .replica
        .transcript()
        .items()
        .is_empty());
}

#[test]
fn changing_one_session_keeps_unrelated_session_values_shared() {
    let first = session("first", "one");
    let second = session("second", "two");
    let (mut engine, _) = engine();

    insert_live(&mut engine, live(first.clone(), 1));
    insert_live(&mut engine, live(second.clone(), 1));
    let published = engine.state.clone();
    let second_before = published
        .live_sessions
        .get(&second)
        .expect("second session")
        .clone();

    engine.handle_pi_event(
        first.machine().clone(),
        first.session().to_string(),
        1,
        json!({"type": "agent_start"}),
    );

    let second_after = engine
        .state
        .live_sessions
        .get(&second)
        .expect("second session");

    assert!(Arc::ptr_eq(&second_before, second_after));
}

#[test]
fn close_reopen_generations_never_reuse_identity() {
    let key = session("machine", "sid");
    let (mut engine, _) = engine();

    engine.session_generations.insert(key.clone(), 1);
    insert_live(&mut engine, live(key.clone(), 1));
    engine.handle_detached(key.machine().clone(), key.session().to_string(), 1);

    assert!(engine.state.live_session(&key).is_none());
    assert_eq!(engine.next_session_generation(&key), 2);
    assert_eq!(engine.next_session_generation(&key), 3);
}

#[test]
fn stale_prompt_history_cursor_and_dialog_events_land_nowhere_after_reopen() {
    let key = session("machine", "sid");
    let submission = SubmissionId(9);
    let (mut engine, _) = engine();

    insert_live(&mut engine, live(key.clone(), 2));
    engine.pending_prompts.insert(
        submission,
        PendingPrompt {
            chat: ChatId::Session(key.clone()),
            text: "new".to_string(),
            session: key.clone(),
            generation: 2,
            delivered: false,
            settled: false,
            turn_was_busy: false,
            event_revision: 0,
        },
    );
    engine.handle_prompt_failed(
        key.machine().clone(),
        key.session().to_string(),
        1,
        submission,
        "stale".to_string(),
    );
    engine.handle_history(
        key.machine().clone(),
        key.session().to_string(),
        1,
        vec![json!({"type": "custom", "id": "stale"})],
        Some("stale-leaf".to_string()),
        true,
    );
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        question("stale-dialog"),
    );

    let current = engine.state.live_session(&key).expect("reopened");

    assert!(engine.pending_prompts.contains_key(&submission));
    assert!(current.replica.entries().is_empty());
    assert_eq!(current.replay.cursor.as_deref(), Some("leaf-a"));
    assert!(current.dialogs.is_empty());
    assert!(engine.dialogs.front().is_none());
}

#[test]
fn stale_room_retirement_cannot_remove_a_replacement_generations_dialog() {
    let key = session("machine", "sid");
    let (mut engine, _) = engine();

    insert_live(&mut engine, live(key.clone(), 2));
    engine.handle_room_joined(key.machine().clone(), key.session().to_string(), 2);
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        2,
        question("current"),
    );
    engine.handle_room_left(key.machine().clone(), key.session().to_string(), 1);

    assert_eq!(
        engine
            .dialogs
            .front()
            .map(|obligation| obligation.request.id.as_str()),
        Some("current")
    );
}

#[test]
fn room_retirement_and_replay_deduplicate_dialog_obligations() {
    let key = session("machine", "sid");
    let (mut engine, _) = engine();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.handle_room_joined(key.machine().clone(), key.session().to_string(), 1);
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        question("dialog"),
    );
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        question("dialog"),
    );

    assert_eq!(
        engine.state.live_session(&key).expect("live").dialogs.len(),
        1
    );

    engine.handle_room_left(key.machine().clone(), key.session().to_string(), 1);

    assert!(engine.dialogs.front().is_none());
    assert!(engine
        .state
        .live_session(&key)
        .expect("live")
        .dialogs
        .is_empty());
}

#[test]
fn successful_dialog_acknowledgement_retires_the_obligation() {
    let key = session("machine", "sid");
    let (mut engine, events) = engine();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.handle_room_joined(key.machine().clone(), key.session().to_string(), 1);
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        question("dialog"),
    );
    let dialog = engine.dialogs.front().expect("pending dialog").key();

    engine
        .answering_dialogs
        .insert(dialog, DialogAnswer::Confirmed(true));
    engine.handle_dialog_answered(
        key.machine().clone(),
        key.session().to_string(),
        1,
        "dialog".to_string(),
        Ok(()),
    );

    assert!(engine.dialogs.front().is_none());
    assert!(engine
        .state
        .live_session(&key)
        .expect("live")
        .dialogs
        .is_empty());
    assert!(events.try_next().is_none());
}

#[test]
fn failed_dialog_acknowledgement_preserves_the_obligation_for_retry() {
    let key = session("machine", "sid");
    let (mut engine, events) = engine();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.handle_room_joined(key.machine().clone(), key.session().to_string(), 1);
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        question("dialog"),
    );
    let dialog = engine.dialogs.front().expect("pending dialog").key();

    engine
        .answering_dialogs
        .insert(dialog.clone(), DialogAnswer::Confirmed(true));
    engine.handle_dialog_answered(
        key.machine().clone(),
        key.session().to_string(),
        1,
        "dialog".to_string(),
        Err("rejected".to_string()),
    );

    assert_eq!(
        engine
            .dialogs
            .front()
            .map(|obligation| obligation.request.id.as_str()),
        Some("dialog")
    );
    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::DialogAnswerFailed {
            dialog: failed,
            detail
        }) if failed == dialog && detail == "rejected"
    ));
    assert!(events.try_next().is_none());
}

#[test]
fn stale_state_response_cannot_undo_newer_streaming_events() {
    let key = session("machine", "sid");
    let (mut engine, _) = engine();
    let token = engine.tokens.mint();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.latest_session_state.insert(key.clone(), (token, 0));
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        json!({"type": "agent_start"}),
    );
    engine.handle_session_state_answer(
        &key,
        token,
        Ok(json!({"model": null, "isStreaming": false})),
    );

    assert!(engine
        .state
        .live_session(&key)
        .expect("live")
        .replica
        .transcript()
        .is_busy());
}

#[test]
fn settlement_invalidates_an_older_streaming_state_response() {
    let key = session("machine", "sid");
    let (mut engine, _) = engine();
    let token = engine.tokens.mint();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.latest_session_state.insert(key.clone(), (token, 0));
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        json!({"type": PI_AGENT_SETTLED}),
    );
    engine.handle_session_state_answer(
        &key,
        token,
        Ok(json!({"model": null, "isStreaming": true})),
    );

    assert!(!engine
        .state
        .live_session(&key)
        .expect("live")
        .replica
        .transcript()
        .is_busy());
}

#[test]
fn authoritative_initial_streaming_state_enables_stop_mid_turn() {
    let key = session("machine", "sid");
    let (mut engine, _) = engine();
    let token = engine.tokens.mint();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.latest_session_state.insert(key.clone(), (token, 0));
    engine.handle_session_state_answer(
        &key,
        token,
        Ok(json!({"model": null, "isStreaming": true})),
    );

    assert!(engine.state.live_session(&key).expect("live").can_stop());
}

#[test]
fn stale_model_and_branch_answers_are_submitted_to_real_handlers_and_ignored() {
    let key = session("machine", "sid");
    let chat = ChatId::Session(key.clone());
    let (mut engine, _) = engine();
    let stale_model = engine.tokens.mint();
    let current_model = engine.tokens.mint();
    let stale_branch = engine.tokens.mint();
    let current_branch = engine.tokens.mint();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.latest_models.insert(chat.clone(), current_model);
    engine.latest_branch.insert(chat.clone(), current_branch);
    engine.branch_requests.insert(stale_branch, chat.clone());
    engine.branch_requests.insert(current_branch, chat.clone());
    engine.handle_models_answer_for(
        &chat,
        stale_model,
        Ok(json!({"models": [{"provider": "test", "id": "stale"}]})),
    );
    engine.handle_branch_answer(stale_branch, Ok("stale".to_string()));

    let current = engine.state.live_session(&key).expect("live");

    assert!(matches!(current.model_catalog, ModelsState::NotRequested));
    assert_eq!(current.branch, None);
}

#[test]
fn cancelling_a_browser_invalidates_real_pending_answers_and_removes_the_run() {
    let browser = WorkspaceBrowserId(3);
    let (mut engine, _) = engine();
    let token = engine.tokens.mint();

    Arc::make_mut(&mut engine.state.browsers).insert(
        browser,
        WorkspaceBrowser {
            id: browser,
            machine: machine("machine"),
            machine_label: "Machine".to_string(),
            path: "/before".to_string(),
            entries: Vec::new(),
            status: WorkspaceBrowserStatus::LoadingDirectory,
            error: None,
        },
    );
    engine.latest_browser.insert(browser, token);
    engine.browser_requests.insert(
        token,
        (browser, BrowserRequest::Listing("/stale".to_string())),
    );
    engine.cancel_browser(browser);
    engine.handle_browser_listing(token, Ok(Vec::new()));

    assert!(engine.state.browser(browser).is_none());
    assert!(!engine.latest_browser.contains_key(&browser));
}

#[test]
fn arbitrary_workspace_and_model_intents_are_rejected_by_the_runtime() {
    let draft_id = DraftId(4);
    let allowed_workspace = workspace("machine", "/allowed");
    let allowed_model = ModelChoice {
        name: "Allowed".to_string(),
        provider: "test".to_string(),
        id: "allowed".to_string(),
    };
    let (mut engine, events) = engine();

    engine.state.workspaces = Arc::new(vec![allowed_workspace.clone()]);
    Arc::make_mut(&mut engine.state.drafts).insert(
        draft_id,
        DraftSession {
            id: draft_id,
            target: ChatTarget {
                machine: Some(machine("machine")),
                workspace: Some(allowed_workspace),
            },
            lifecycle: DraftLifecycle::Ready,
            models: ModelsState::Loaded(vec![allowed_model]),
            model_selection: ModelSelection::LaunchDefault,
            branch: None,
        },
    );
    engine.select_workspace(draft_id, workspace("machine", "/arbitrary"));
    engine.pick_model(
        ChatId::Draft(draft_id),
        Some(ModelChoice {
            name: "Arbitrary".to_string(),
            provider: "test".to_string(),
            id: "arbitrary".to_string(),
        }),
    );

    assert!(matches!(events.try_next(), Some(RuntimeEvent::Rejected(_))));
    assert!(matches!(events.try_next(), Some(RuntimeEvent::Rejected(_))));
    assert!(matches!(
        engine.state.draft(draft_id).expect("draft").model_selection,
        ModelSelection::LaunchDefault
    ));
}

#[test]
fn closed_bridge_channel_rejects_prompt_without_an_optimistic_turn() {
    let key = session("machine", "sid");
    let submission = SubmissionId(8);
    let chat = ChatId::Session(key.clone());
    let (mut engine, events) = engine();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.handle_intent(Intent::SubmitPrompt {
        submission,
        chat: chat.clone(),
        text: "keep me".to_string(),
    });
    let event = events.try_next().expect("rejection event");

    assert!(matches!(
        event,
        RuntimeEvent::PromptRejected {
            submission: rejected,
            chat: rejected_chat,
            text,
            ..
        } if rejected == submission && rejected_chat == chat && text == "keep me"
    ));
    assert!(engine
        .state
        .live_session(&key)
        .expect("live")
        .replica
        .transcript()
        .items()
        .is_empty());
}

#[test]
fn settlement_before_delivery_ack_cannot_hide_a_later_prompt_failure() {
    let key = session("machine", "sid");
    let submission = SubmissionId(10);
    let chat = ChatId::Session(key.clone());
    let (mut engine, events) = engine();
    let mut current = live(key.clone(), 1);

    current
        .replica
        .push_user_prompt(submission, "preserve".to_string());
    insert_live(&mut engine, current);
    engine.pending_prompts.insert(
        submission,
        PendingPrompt {
            chat: chat.clone(),
            text: "preserve".to_string(),
            session: key.clone(),
            generation: 1,
            delivered: false,
            settled: false,
            turn_was_busy: false,
            event_revision: 0,
        },
    );
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        json!({"type": PI_AGENT_SETTLED}),
    );

    assert!(engine.pending_prompts.contains_key(&submission));

    engine.handle_prompt_failed(
        key.machine().clone(),
        key.session().to_string(),
        1,
        submission,
        "not delivered".to_string(),
    );

    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::PromptRejected { text, .. }) if text == "preserve"
    ));
}

#[test]
fn delivery_ack_after_settlement_emits_acceptance_then_completion() {
    let key = session("machine", "sid");
    let submission = SubmissionId(11);
    let chat = ChatId::Session(key.clone());
    let (mut engine, events) = engine();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.pending_prompts.insert(
        submission,
        PendingPrompt {
            chat: chat.clone(),
            text: "delivered".to_string(),
            session: key.clone(),
            generation: 1,
            delivered: false,
            settled: false,
            turn_was_busy: false,
            event_revision: 0,
        },
    );
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        json!({"type": PI_AGENT_SETTLED}),
    );
    engine.handle_prompt_delivered(
        key.machine().clone(),
        key.session().to_string(),
        1,
        submission,
    );

    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::PromptAccepted {
            submission: accepted,
            chat: accepted_chat
        }) if accepted == submission && accepted_chat == chat
    ));
    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::PromptCompleted { submission: completed }) if completed == submission
    ));
    assert!(events.try_next().is_none());
}

#[test]
fn delivery_ack_and_settlement_emit_acceptance_then_completion_once() {
    let key = session("machine", "sid");
    let submission = SubmissionId(11);
    let chat = ChatId::Session(key.clone());
    let (mut engine, events) = engine();

    insert_live(&mut engine, live(key.clone(), 1));
    engine.pending_prompts.insert(
        submission,
        PendingPrompt {
            chat: chat.clone(),
            text: "delivered".to_string(),
            session: key.clone(),
            generation: 1,
            delivered: false,
            settled: false,
            turn_was_busy: false,
            event_revision: 0,
        },
    );
    engine.handle_prompt_delivered(
        key.machine().clone(),
        key.session().to_string(),
        1,
        submission,
    );
    engine.handle_prompt_delivered(
        key.machine().clone(),
        key.session().to_string(),
        1,
        submission,
    );
    engine.handle_pi_event(
        key.machine().clone(),
        key.session().to_string(),
        1,
        json!({"type": PI_AGENT_SETTLED}),
    );

    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::PromptAccepted {
            submission: accepted,
            chat: accepted_chat
        }) if accepted == submission && accepted_chat == chat
    ));
    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::PromptCompleted { submission: completed }) if completed == submission
    ));
    assert!(events.try_next().is_none());
}

#[test]
fn semantic_runtime_events_are_not_discarded_by_changed_bursts() {
    let events = RuntimeEventHub::default();
    let receiver = events.subscribe();
    let submission = SubmissionId(12);
    let chat = ChatId::Draft(DraftId(2));

    for _ in 0..2_000 {
        events.emit(RuntimeEvent::Changed);
    }

    events.emit(RuntimeEvent::PromptAccepted {
        submission,
        chat: chat.clone(),
    });

    for _ in 0..2_000 {
        events.emit(RuntimeEvent::Changed);
    }

    events.emit(RuntimeEvent::PromptRejected {
        submission,
        chat: chat.clone(),
        text: "retry".to_string(),
        detail: "failed".to_string(),
    });

    assert!(matches!(receiver.try_next(), Some(RuntimeEvent::Changed)));
    assert!(matches!(
        receiver.try_next(),
        Some(RuntimeEvent::PromptAccepted {
            submission: accepted,
            chat: accepted_chat
        }) if accepted == submission && accepted_chat == chat
    ));
    assert!(matches!(
        receiver.try_next(),
        Some(RuntimeEvent::PromptRejected {
            submission: rejected,
            text,
            ..
        }) if rejected == submission && text == "retry"
    ));
    assert!(receiver.try_next().is_none());
}

#[test]
fn unavailable_initial_history_preserves_the_reconnect_replica() {
    let key = session("machine", "sid");
    let (mut engine, _) = engine();
    let mut current = live(key.clone(), 2);
    let entry = json!({"type": "custom", "id": "retained", "data": {"raw": true}});

    current.replica = SessionReplica::from_entries(std::slice::from_ref(&entry));
    insert_live(&mut engine, current);
    engine.handle_attached(
        key.machine().clone(),
        key.session().to_string(),
        2,
        Vec::new(),
        None,
        false,
    );

    assert_eq!(
        engine
            .state
            .live_session(&key)
            .expect("live")
            .replica
            .entries(),
        &[entry]
    );
}

#[test]
fn prompt_failure_marks_the_optimistic_turn_and_emits_the_original_text() {
    let key = session("machine", "sid");
    let submission = SubmissionId(6);
    let chat = ChatId::Session(key.clone());
    let (mut engine, events) = engine();
    let mut current = live(key.clone(), 1);

    current
        .replica
        .push_user_prompt(submission, "restore".to_string());
    insert_live(&mut engine, current);
    engine.pending_prompts.insert(
        submission,
        PendingPrompt {
            chat: chat.clone(),
            text: "restore".to_string(),
            session: key.clone(),
            generation: 1,
            delivered: false,
            settled: false,
            turn_was_busy: false,
            event_revision: 0,
        },
    );
    engine.handle_prompt_failed(
        key.machine().clone(),
        key.session().to_string(),
        1,
        submission,
        "bridge closed".to_string(),
    );
    let event = events.try_next().expect("prompt rejection");

    assert!(matches!(
        event,
        RuntimeEvent::PromptRejected { text, detail, .. }
            if text == "restore" && detail == "bridge closed"
    ));
    assert!(!engine
        .state
        .live_session(&key)
        .expect("live")
        .replica
        .transcript()
        .is_busy());
}

#[test]
fn machine_policy_keeps_connectable_unknown_and_retryable_rows() {
    let mut snapshot = ClientSnapshot::default();

    snapshot.machines = Arc::new(vec![
        MachineSnapshot {
            id: machine("reachable"),
            label: "Reachable".to_string(),
            provider: None,
            status: MachineStatus::Reachable,
            connectable: true,
        },
        MachineSnapshot {
            id: machine("unknown"),
            label: "Unknown".to_string(),
            provider: None,
            status: MachineStatus::Unknown,
            connectable: true,
        },
        MachineSnapshot {
            id: machine("failed"),
            label: "Failed".to_string(),
            provider: None,
            status: MachineStatus::ConnectFailed("no route".to_string()),
            connectable: true,
        },
        MachineSnapshot {
            id: machine("unreachable"),
            label: "Unreachable".to_string(),
            provider: None,
            status: MachineStatus::Unreachable,
            connectable: true,
        },
    ]);

    let candidates: Vec<&str> = snapshot
        .machine_candidates()
        .into_iter()
        .map(|machine| machine.id.as_str())
        .collect();

    assert_eq!(candidates, ["reachable", "unknown", "failed"]);
}

#[test]
fn fresh_connectable_machine_and_latest_workspace_form_the_default_target() {
    let local = MachineId::local();
    let older = WorkspaceId::new(local.clone(), "/older").expect("workspace");
    let newer = WorkspaceId::new(local.clone(), "/newer").expect("workspace");
    let (mut engine, _) = engine();

    engine.state.machines = Arc::new(vec![MachineSnapshot {
        id: local.clone(),
        label: "local".to_string(),
        provider: None,
        status: MachineStatus::Unknown,
        connectable: true,
    }]);
    engine.state.workspaces = Arc::new(vec![older, newer.clone()]);

    assert_eq!(
        engine.default_target(),
        ChatTarget {
            machine: Some(local),
            workspace: Some(newer),
        }
    );
}

#[test]
fn browser_add_and_duplicate_persist_once_and_remove_completed_runs() {
    let scratch = ScratchDir::create("runtime-browser-persistence");
    let path = scratch.join("workspaces.json");
    let browser = WorkspaceBrowserId(12);
    let existing = workspace("machine", "/repo");
    let (mut engine, events) = engine();

    engine.workspace_store = workspace::WorkspaceStore::at(path.clone());
    engine
        .workspace_store
        .save(std::slice::from_ref(&existing))
        .expect("seed workspace");
    engine.state.workspaces = Arc::new(vec![existing.clone()]);
    Arc::make_mut(&mut engine.state.browsers).insert(
        browser,
        WorkspaceBrowser {
            id: browser,
            machine: machine("machine"),
            machine_label: "Machine".to_string(),
            path: "/repo".to_string(),
            entries: Vec::new(),
            status: WorkspaceBrowserStatus::Validating,
            error: None,
        },
    );
    let duplicate_token = engine.tokens.mint();

    engine.latest_browser.insert(browser, duplicate_token);
    engine.browser_requests.insert(
        duplicate_token,
        (browser, BrowserRequest::Validation("/repo".to_string())),
    );
    engine.handle_browser_validation(duplicate_token, Ok(()));

    assert!(engine.state.browser(browser).is_none());
    assert_eq!(engine.workspace_store.load(), vec![existing]);
    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::WorkspaceAdded {
            browser: completed,
            ..
        }) if completed == browser
    ));

    let added_browser = WorkspaceBrowserId(13);
    let added_token = engine.tokens.mint();

    Arc::make_mut(&mut engine.state.browsers).insert(
        added_browser,
        WorkspaceBrowser {
            id: added_browser,
            machine: machine("machine"),
            machine_label: "Machine".to_string(),
            path: "/new".to_string(),
            entries: Vec::new(),
            status: WorkspaceBrowserStatus::Validating,
            error: None,
        },
    );
    engine.latest_browser.insert(added_browser, added_token);
    engine.browser_requests.insert(
        added_token,
        (
            added_browser,
            BrowserRequest::Validation("/new".to_string()),
        ),
    );
    engine.handle_browser_validation(added_token, Ok(()));

    assert!(engine.state.browser(added_browser).is_none());
    assert_eq!(engine.workspace_store.load().len(), 2);
    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::WorkspaceAdded {
            browser: completed,
            ..
        }) if completed == added_browser
    ));
}

#[test]
fn browser_navigation_rejects_names_outside_the_current_listing() {
    let browser = WorkspaceBrowserId(14);
    let (mut engine, events) = engine();

    Arc::make_mut(&mut engine.state.browsers).insert(
        browser,
        WorkspaceBrowser {
            id: browser,
            machine: machine("machine"),
            machine_label: "Machine".to_string(),
            path: "/repo".to_string(),
            entries: vec![RemoteDirEntry {
                name: "listed".to_string(),
                is_dir: true,
            }],
            status: WorkspaceBrowserStatus::Idle,
            error: None,
        },
    );
    engine.open_browser_directory(browser, "../escape");

    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::Rejected(detail)) if detail.contains("current directory listing")
    ));
    assert!(engine.browser_requests.is_empty());
}

#[test]
fn forgetting_a_missing_machine_persists_the_cascade_and_keeps_other_rows() {
    let scratch = ScratchDir::create("runtime-forget-persistence");
    let path = scratch.join("workspaces.json");
    let forgotten = workspace("missing", "/gone");
    let kept = workspace("kept", "/repo");
    let (mut engine, _) = engine();

    engine.workspace_store = workspace::WorkspaceStore::at(path);
    engine.state.workspaces = Arc::new(vec![forgotten, kept.clone()]);
    engine.state.machines = Arc::new(vec![MachineSnapshot {
        id: machine("missing"),
        label: "Missing".to_string(),
        provider: None,
        status: MachineStatus::Missing,
        connectable: false,
    }]);
    engine.forget_machine(&machine("missing"));

    assert_eq!(engine.state.workspaces(), std::slice::from_ref(&kept));
    assert_eq!(engine.workspace_store.load(), vec![kept]);
}

#[test]
fn forgetting_a_nonmissing_machine_is_rejected_without_a_write() {
    let scratch = ScratchDir::create("runtime-forget-rejection");
    let path = scratch.join("workspaces.json");
    let (mut engine, events) = engine();

    engine.workspace_store = workspace::WorkspaceStore::at(path.clone());
    engine.state.machines = Arc::new(vec![MachineSnapshot {
        id: machine("live"),
        label: "Live".to_string(),
        provider: None,
        status: MachineStatus::Connected,
        connectable: true,
    }]);
    engine.forget_machine(&machine("live"));

    assert!(matches!(
        events.try_next(),
        Some(RuntimeEvent::Rejected(detail)) if detail.contains("not missing")
    ));
    assert!(!path.exists());
}
