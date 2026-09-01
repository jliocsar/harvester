mod snapshot;

pub use snapshot::{
    ChatTarget, ClientSnapshot, DraftLifecycle, DraftSession, LiveSession, MachineSnapshot,
    MachineStatus, RepairState, ReplayState, SessionLifecycle, WorkspaceBrowser,
    WorkspaceBrowserStatus,
};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use harvester_proto::{lock, unix_millis_now, SessionStatus, PI_GET_AVAILABLE_MODELS};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use crate::bridge::{Command as BridgeCommand, Event as BridgeEvent, Operation as BridgeOperation};
use crate::dialog::{DialogKey, DialogObligation, DialogObligations, DialogRoom};
use crate::fleet::{Fleet, FleetEvents, MachineState};
use crate::machines;
use crate::model::{extract_model_choices, model_choice_from_value};
use crate::session_index::SessionIndex;
use crate::token::{RequestToken, RequestTokens};
use crate::workspace;
use crate::{
    ChatId, DialogAnswer, DialogRequest, DraftId, MachineId, ModelChoice, ModelSelection,
    ModelsState, RemoteDirEntry, SessionId, SessionKey, SessionReplica, SubmissionId,
    WorkspaceBrowserId, WorkspaceId, PI_AGENT_SETTLED,
};

const SESSION_NAME_MAX_CHARS: usize = 48;
const PI_GET_STATE: &str = "get_state";
const PI_SET_MODEL: &str = "set_model";
const CONNECTION_LOST_DETAIL: &str = "Connection lost";

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    Changed,
    Failed(String),
    Rejected(String),
    PromptAccepted {
        submission: SubmissionId,
        chat: ChatId,
    },
    PromptRejected {
        submission: SubmissionId,
        chat: ChatId,
        text: String,
        detail: String,
    },
    PromptCompleted {
        submission: SubmissionId,
    },
    DialogAnswerFailed {
        dialog: DialogKey,
        detail: String,
    },
    DraftPromoted {
        draft: DraftId,
        session: SessionKey,
    },
    WorkspaceAdded {
        browser: WorkspaceBrowserId,
        workspace: WorkspaceId,
    },
    MachineForgotten(MachineId),
    MachineForgetFailed {
        machine: MachineId,
        detail: String,
    },
}

pub struct RuntimeEvents {
    queue: Arc<RuntimeEventQueue>,
}

impl RuntimeEvents {
    pub async fn next(&mut self) -> Option<RuntimeEvent> {
        loop {
            let notified = self.queue.notified.notified();

            if let Some(event) = self.queue.pop() {
                return Some(event);
            }

            if self.queue.closed.load(Ordering::Acquire) {
                return None;
            }

            notified.await;
        }
    }

    #[cfg(test)]
    fn try_next(&self) -> Option<RuntimeEvent> {
        self.queue.pop()
    }
}

#[derive(Default)]
struct RuntimeEventQueue {
    events: Mutex<VecDeque<RuntimeEvent>>,
    notified: Notify,
    closed: AtomicBool,
}

impl RuntimeEventQueue {
    fn push(&self, event: RuntimeEvent) {
        let mut events = lock(&self.events, "runtime events");

        if matches!(event, RuntimeEvent::Changed)
            && events
                .iter()
                .any(|queued| matches!(queued, RuntimeEvent::Changed))
        {
            return;
        }

        events.push_back(event);
        drop(events);
        self.notified.notify_one();
    }

    fn pop(&self) -> Option<RuntimeEvent> {
        lock(&self.events, "runtime events").pop_front()
    }
}

#[derive(Default)]
struct RuntimeEventHub {
    subscribers: Mutex<Vec<Weak<RuntimeEventQueue>>>,
    closed: AtomicBool,
}

impl Drop for RuntimeEventHub {
    fn drop(&mut self) {
        self.close();
    }
}

impl RuntimeEventHub {
    fn subscribe(&self) -> RuntimeEvents {
        let queue = Arc::new(RuntimeEventQueue::default());
        let mut subscribers = lock(&self.subscribers, "runtime subscribers");

        if self.closed.load(Ordering::Acquire) {
            queue.closed.store(true, Ordering::Release);
        } else {
            subscribers.push(Arc::downgrade(&queue));
        }

        drop(subscribers);

        RuntimeEvents { queue }
    }

    fn emit(&self, event: RuntimeEvent) {
        let mut queues = Vec::new();
        let mut subscribers = lock(&self.subscribers, "runtime subscribers");

        subscribers.retain(|subscriber| {
            let Some(queue) = subscriber.upgrade() else {
                return false;
            };

            queues.push(queue);

            true
        });
        drop(subscribers);
        let Some((last, preceding)) = queues.split_last() else {
            return;
        };

        for queue in preceding {
            queue.push(event.clone());
        }

        last.push(event);
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }

        for subscriber in lock(&self.subscribers, "runtime subscribers").drain(..) {
            if let Some(queue) = subscriber.upgrade() {
                queue.closed.store(true, Ordering::Release);
                queue.notified.notify_waiters();
            }
        }
    }
}

struct RuntimeHandle {
    commands: UnboundedSender<Intent>,
    snapshot: Arc<Mutex<Arc<ClientSnapshot>>>,
    events: Arc<RuntimeEventHub>,
    next_draft: AtomicU64,
    next_browser: AtomicU64,
    next_submission: AtomicU64,
}

#[derive(Clone)]
pub struct ClientRuntime {
    handle: Arc<RuntimeHandle>,
}

pub struct ClientRuntimeOwner {
    runtime: ClientRuntime,
}

impl ClientRuntimeOwner {
    pub fn start(argv_machine: Option<MachineId>, daemon_binary: Vec<u8>) -> std::io::Result<Self> {
        let (commands, command_receiver) = unbounded_channel();
        let events = Arc::new(RuntimeEventHub::default());
        let snapshot = Arc::new(Mutex::new(Arc::new(ClientSnapshot::default())));
        let runtime_snapshot = snapshot.clone();
        let runtime_events = events.clone();
        let tokio_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        std::thread::Builder::new()
            .name("harvester-runtime".to_string())
            .spawn(move || {
                tokio_runtime.block_on(engine::run(
                    argv_machine,
                    daemon_binary,
                    command_receiver,
                    runtime_events,
                    runtime_snapshot,
                ));
            })?;

        Ok(Self {
            runtime: ClientRuntime {
                handle: Arc::new(RuntimeHandle {
                    commands,
                    snapshot,
                    events,
                    next_draft: AtomicU64::new(1),
                    next_browser: AtomicU64::new(1),
                    next_submission: AtomicU64::new(1),
                }),
            },
        })
    }

    pub fn handle(&self) -> ClientRuntime {
        self.runtime.clone()
    }
}

impl ClientRuntime {
    pub fn subscribe(&self) -> RuntimeEvents {
        self.handle.events.subscribe()
    }

    pub fn snapshot(&self) -> Arc<ClientSnapshot> {
        lock(&self.handle.snapshot, "client snapshot").clone()
    }

    pub fn create_draft(&self) -> DraftId {
        let id = DraftId(reserve_id(&self.handle.next_draft, "draft id"));

        self.send(Intent::CreateDraft { id });

        id
    }

    pub fn retarget_draft(&self, draft: DraftId, machine: MachineId) {
        self.send(Intent::RetargetDraft { draft, machine });
    }

    pub fn select_workspace(&self, draft: DraftId, workspace: WorkspaceId) {
        self.send(Intent::SelectWorkspace { draft, workspace });
    }

    pub fn request_controls(&self, chat: ChatId) {
        self.send(Intent::RequestControls { chat });
    }

    pub fn open_session(&self, session: SessionKey) {
        self.send(Intent::OpenSession { session });
    }

    pub fn reserve_prompt_submission(&self) -> SubmissionId {
        SubmissionId(reserve_id(
            &self.handle.next_submission,
            "prompt submission id",
        ))
    }

    pub fn submit_prompt(&self, submission: SubmissionId, chat: ChatId, text: String) {
        self.send(Intent::SubmitPrompt {
            submission,
            chat,
            text,
        });
    }

    pub fn pick_model(&self, chat: ChatId, model: Option<ModelChoice>) {
        self.send(Intent::PickModel { chat, model });
    }

    pub fn stop_session(&self, session: SessionKey) {
        self.send(Intent::StopSession { session });
    }

    pub fn answer_dialog(&self, key: DialogKey, answer: DialogAnswer) {
        self.send(Intent::AnswerDialog { key, answer });
    }

    pub fn begin_workspace_browser(&self, machine: MachineId) -> WorkspaceBrowserId {
        let id = WorkspaceBrowserId(reserve_id(
            &self.handle.next_browser,
            "workspace browser id",
        ));

        self.send(Intent::BeginWorkspaceBrowser { id, machine });

        id
    }

    pub fn open_browser_directory(&self, browser: WorkspaceBrowserId, name: String) {
        self.send(Intent::OpenBrowserDirectory { browser, name });
    }

    pub fn browser_up(&self, browser: WorkspaceBrowserId) {
        self.send(Intent::BrowserUp { browser });
    }

    pub fn confirm_browser(&self, browser: WorkspaceBrowserId) {
        self.send(Intent::ConfirmBrowser { browser });
    }

    pub fn cancel_browser(&self, browser: WorkspaceBrowserId) {
        self.send(Intent::CancelBrowser { browser });
    }

    pub fn forget_machine(&self, machine: MachineId) {
        self.send(Intent::ForgetMachine { machine });
    }

    fn send(&self, intent: Intent) {
        let Err(error) = self.handle.commands.send(intent) else {
            return;
        };
        let detail = "client runtime has stopped".to_string();

        match error.0 {
            Intent::SubmitPrompt {
                submission,
                chat,
                text,
            } => self.handle.events.emit(RuntimeEvent::PromptRejected {
                submission,
                chat,
                text,
                detail,
            }),
            _ => self.handle.events.emit(RuntimeEvent::Failed(detail)),
        }
    }
}

fn reserve_id(counter: &AtomicU64, name: &str) -> u64 {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("{name} exhausted"))
}

#[derive(Debug)]
enum Intent {
    CreateDraft {
        id: DraftId,
    },
    RetargetDraft {
        draft: DraftId,
        machine: MachineId,
    },
    SelectWorkspace {
        draft: DraftId,
        workspace: WorkspaceId,
    },
    RequestControls {
        chat: ChatId,
    },
    OpenSession {
        session: SessionKey,
    },
    SubmitPrompt {
        submission: SubmissionId,
        chat: ChatId,
        text: String,
    },
    PickModel {
        chat: ChatId,
        model: Option<ModelChoice>,
    },
    StopSession {
        session: SessionKey,
    },
    AnswerDialog {
        key: DialogKey,
        answer: DialogAnswer,
    },
    BeginWorkspaceBrowser {
        id: WorkspaceBrowserId,
        machine: MachineId,
    },
    OpenBrowserDirectory {
        browser: WorkspaceBrowserId,
        name: String,
    },
    BrowserUp {
        browser: WorkspaceBrowserId,
    },
    ConfirmBrowser {
        browser: WorkspaceBrowserId,
    },
    CancelBrowser {
        browser: WorkspaceBrowserId,
    },
    ForgetMachine {
        machine: MachineId,
    },
}

mod engine;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_notifications_coalesce_without_dropping_semantic_outcomes() {
        let hub = RuntimeEventHub::default();
        let events = hub.subscribe();

        for _ in 0..2_048 {
            hub.emit(RuntimeEvent::Changed);
        }
        hub.emit(RuntimeEvent::PromptCompleted {
            submission: SubmissionId(7),
        });
        hub.emit(RuntimeEvent::Changed);

        assert!(matches!(events.try_next(), Some(RuntimeEvent::Changed)));
        assert!(matches!(
            events.try_next(),
            Some(RuntimeEvent::PromptCompleted {
                submission: SubmissionId(7)
            })
        ));
        assert!(events.try_next().is_none());
    }

    #[test]
    fn late_subscribers_observe_a_closed_runtime() {
        let hub = RuntimeEventHub::default();

        hub.close();
        let events = hub.subscribe();

        assert!(events.queue.closed.load(Ordering::Acquire));
    }
}
