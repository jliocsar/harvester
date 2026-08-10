//! Layout and user intent (docs/SPEC.md §UI): sidebar (machines, workspaces,
//! sessions), content (chat or empty state), statusbar.
//!
//! Everything that is *not* layout or user intent lives in the
//! [`FleetStore`](crate::store::FleetStore) — the fleet, the pumps and every
//! table. This view observes it and reads it whenever it renders; what it owns
//! is the chat on screen, the folder picker, and which sections are collapsed.
//! It is also where the store's events become window work: building a chat,
//! raising a notification, dismissing a dialog.

mod picker;

use gpui::{div, prelude::*, px, App, Context, Div, Entity, SharedString, Subscription, Window};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::list::ListItem;
use gpui_component::notification::NotificationType;
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, IconName, Root, Sizable as _,
    WindowExt as _,
};
use harvester_client_core::{DialogAnswer, DialogRequest, MachineState, LOCAL_MACHINE};
use harvester_proto::SessionStatus;
use serde_json::Value;

use crate::chat::{Chat, ChatRequest, ChatTarget};
use crate::choices;
use crate::dialogs::{self, DialogQueue, PendingDialog};
use crate::store::{FleetStore, MachineSession, StoreEvent};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::machines;
use picker::FolderPicker;

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;
/// The empty content pane when there is no argv machine to report on — the
/// workspace rows are the way in.
const NO_ARGV_MACHINE_HINT: &str = "open a workspace to begin";

pub struct Workbench {
    /// Every table, the fleet and both pumps. Observed, never pushed into.
    store: Entity<FleetStore>,
    /// The open folder picker, kept because the machines worker's answers are
    /// routed to it. Whether it is *on screen* is its dialog's business, so a
    /// dismissed walk lingers here until the next one replaces it. Nothing
    /// renders it meanwhile, and its `RequestTokens` guard drops answers meant
    /// for an earlier walk; the one visible cost is that a `$HOME` still in
    /// flight when the user dismisses will spend one more `ls` on a directory
    /// nobody will see.
    picker: Option<Entity<FolderPicker>>,
    /// Collapsed sidebar sections (all expanded at launch).
    machines_collapsed: bool,
    workspaces_collapsed: bool,
    sessions_collapsed: bool,
    /// The one open conversation, whether or not a session exists behind it
    /// yet — an unstarted chat is the same entity in an earlier state (see
    /// [`Chat`]), so there is nothing else here for it to be.
    chat: Option<Entity<Chat>>,
    /// Keeps [`Workbench::handle_chat_request`] subscribed for as long as the
    /// chat it belongs to is the open one; dropped with the next chat's.
    chat_requests: Option<Subscription>,
    /// Questions an extension is blocked on. The ordering rules are the
    /// queue's; what is left here is the window work of showing one.
    dialogs: DialogQueue,
    /// Repaint on any table change, and act on what the store cannot finish
    /// itself. Held rather than detached so both die with this view.
    _store: [Subscription; 2],
}

impl Workbench {
    pub fn new(
        argv_machine: Option<String>,
        daemon_binary: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = cx.new(|cx| FleetStore::new(argv_machine, daemon_binary, cx));
        let store_subscriptions = [
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&store, window, Self::handle_store_event),
        ];

        Self {
            store,
            picker: None,
            machines_collapsed: false,
            workspaces_collapsed: false,
            sessions_collapsed: false,
            chat: None,
            chat_requests: None,
            dialogs: DialogQueue::default(),
            _store: store_subscriptions,
        }
    }

    /// Pure router for what the store could not finish on its own: the window
    /// work (a chat needs one to build its input, a notification needs one to
    /// be raised) and the routing into whichever surface the answer belongs to.
    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // The bridge events this view takes are the ones needing a
            // `Window`: a chat to build its input in, a dialog to build its
            // text box in, and a dead session's dialog to take off the screen.
            // Every other answer is routed by the surface that wants it — the
            // chat filters the store's events by its own sid and machine, so
            // there is no routing table here to keep in step with it.
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Attached { sid, entries },
            } => self.handle_attached(machine, sid, entries, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Pi { sid, event },
            } => self.handle_pi_event(machine, sid, event, window, cx),
            StoreEvent::Bridge {
                event: BridgeEvent::SessionExit { sid, .. },
                ..
            } => self.abandon_dialogs_of(sid, window, cx),
            StoreEvent::Bridge { .. } => {}
            StoreEvent::Machines(event) => self.handle_machines_event(event, cx),
            StoreEvent::Failed(detail) => self.report(detail.clone(), window, cx),
        }
    }

    /// A failure the user should see, with a lifetime of its own — it fades
    /// instead of sticking in the statusbar until something else fails
    /// (SPEC C12).
    fn report(&self, detail: String, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification((NotificationType::Error, detail), cx);
    }

    /// The picker's answers. Nothing else in this window talks to the machines
    /// worker; the store already took the machine table and the probes.
    fn handle_machines_event(&mut self, event: &machines::Event, cx: &mut Context<Self>) {
        match event {
            machines::Event::HomeResolved { token, result } => {
                self.handle_home_resolved(*token, result.clone(), cx)
            }
            machines::Event::DirListed { token, result } => {
                self.handle_dir_listed(*token, result.clone(), cx)
            }
            machines::Event::WorkspaceValidated { token, error } => {
                self.handle_workspace_validated(*token, error.clone(), cx)
            }
            // Consumed by the store; it does not pass these on.
            machines::Event::Listed { .. }
            | machines::Event::Probed { .. }
            | machines::Event::ProviderFailed { .. } => {}
        }
    }

    fn handle_attached(
        &mut self,
        machine: &str,
        sid: &str,
        entries: &[Value],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.keep_only_dialogs_of(sid, window, cx);

        // The chat that minted this sid is already on screen with the user's
        // first prompt rendered in it, so it is promoted in place. Building a
        // fresh entity here would throw away the pane they are already
        // watching — the same mistake C7 fixed for resyncs.
        if let Some(chat) = self.take_starting_chat(sid, cx) {
            chat.update(cx, |chat, cx| {
                chat.promote_to_live(sid.to_string(), entries, cx);
                chat.request_model_controls(cx);
            });

            return;
        }

        let target = ChatTarget {
            machine: machine.to_string(),
            workspace_path: self.workspace_path_of(sid, cx),
        };
        let store = self.store.clone();
        let chat = cx.new(|cx| Chat::live(sid.to_string(), target, entries, store, window, cx));

        // Subscribe before the chat asks for anything: an entity cannot emit
        // to a subscriber that does not exist yet.
        self.chat_requests = Some(cx.subscribe_in(&chat, window, Self::handle_chat_request));
        chat.update(cx, |chat, cx| chat.request_model_controls(cx));
        self.chat = Some(chat);
        cx.notify();
    }

    /// A pi event, watched for the one kind this view owes an answer to.
    /// Everything else on this stream is the chat's, which reads it too.
    fn handle_pi_event(
        &mut self,
        machine: &str,
        sid: &str,
        event: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = DialogRequest::parse(event) else {
            return;
        };

        self.queue_dialog(machine, sid, request, window, cx);
    }

    fn queue_dialog(
        &mut self,
        machine: &str,
        sid: &str,
        request: DialogRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = PendingDialog::new(machine.to_string(), sid.to_string(), request, window, cx);

        if !self.dialogs.push(dialog) {
            return;
        }

        self.show_front_dialog(window, cx);
    }

    fn show_front_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialogs.front() else {
            return;
        };

        dialogs::open(dialog, Self::answer_front_dialog, window, cx);
    }

    /// The user decided. Send it, drop the question, show whatever queued
    /// behind it.
    fn answer_front_dialog(
        &mut self,
        answer: DialogAnswer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The queue is also the guard against answering twice: some routes out
        // of a dialog run more than one callback, and the second finds the
        // question already gone rather than sending pi a second response.
        let Some(dialog) = self.dialogs.take_front() else {
            return;
        };

        self.send_command(
            &dialog.machine,
            Command::AnswerDialog {
                sid: dialog.sid,
                dialog_id: dialog.request.id,
                answer,
            },
            cx,
        );

        // Deferred because the library closes the answered dialog *after* this
        // callback returns, and closing pops the top of its stack — opening the
        // next question here would close that one instead.
        let workbench = cx.entity();

        window.defer(cx, move |window, cx| {
            workbench.update(cx, |workbench, cx| workbench.show_front_dialog(window, cx));
        });
    }

    /// A session's questions die with its pi: there is nothing left to answer,
    /// and nothing left to answer it to.
    fn abandon_dialogs_of(&mut self, sid: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.retain_dialogs(|dialog| dialog.sid != sid, window, cx);
    }

    /// Attaching elsewhere detaches from whichever session asked, and an empty
    /// room is exactly where the daemon cancels every dialog still pending
    /// (docs/DAEMON.md §Dialogs) — so another session's question has already
    /// been answered without us, and showing it would be a lie.
    fn keep_only_dialogs_of(&mut self, sid: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.retain_dialogs(|dialog| dialog.sid == sid, window, cx);
    }

    /// Drop the questions `keep` rejects, replacing the modal on screen if it
    /// was one of them.
    fn retain_dialogs(
        &mut self,
        keep: impl Fn(&PendingDialog) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.dialogs.retain(keep) {
            return;
        }

        // Programmatic, so it runs no callback and nothing is sent for the
        // question being abandoned — which is the intent: pi is not waiting.
        window.close_dialog(cx);
        self.show_front_dialog(window, cx);
    }

    fn workspace_path_of(&self, sid: &str, cx: &Context<Self>) -> String {
        self.store
            .read(cx)
            .session_cwd(sid)
            .unwrap_or_default()
            .to_string()
    }

    /// The chat holds no command sender (see [`ChatTarget::machine`]), so every
    /// request it makes is routed here against the bridge that is live *now*.
    /// A machine with no usable bridge is the case that used to vanish: the
    /// prompt was rendered, the turn went busy, and the send failed into
    /// stderr. It raises a notification instead.
    fn handle_chat_request(
        &mut self,
        chat: &Entity<Chat>,
        request: &ChatRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let machine = chat.read(cx).machine().to_string();

        if !self.machine_ready_for(request, &machine, cx) {
            let detail = format!(
                "{}: not connected — nothing was sent",
                self.store.read(cx).label(&machine)
            );

            // The refusal has to reach the chat, not just the notification: a
            // model-list request records as outstanding when it is *made*
            // (`Pending::ask`), so a chat whose request was never sent would
            // sit on "loading models…" for the life of the pane.
            chat.update(cx, |chat, cx| {
                chat.fail_pending_requests(detail.clone(), cx)
            });
            self.report(detail, window, cx);

            return;
        }

        match request {
            ChatRequest::ListModels { cwd, token } => self.send_command(
                &machine,
                Command::ListModels {
                    cwd: cwd.clone(),
                    token: *token,
                },
                cx,
            ),
            ChatRequest::Start {
                sid,
                cwd,
                provider,
                model,
                name,
                first_prompt,
            } => {
                self.store.update(cx, |store, cx| {
                    store.begin_start(machine.clone(), sid.clone(), cx)
                });
                self.send_command(
                    &machine,
                    Command::StartSession {
                        sid: sid.clone(),
                        cwd: cwd.clone(),
                        provider: provider.clone(),
                        model: model.clone(),
                        name: name.clone(),
                        first_prompt: first_prompt.clone(),
                    },
                    cx,
                );
            }
            ChatRequest::Prompt { sid, text } => self.send_command(
                &machine,
                Command::Prompt {
                    sid: sid.clone(),
                    text: text.clone(),
                },
                cx,
            ),
            ChatRequest::SetModel {
                sid,
                provider,
                model_id,
                token,
            } => self.send_command(
                &machine,
                Command::SetModel {
                    sid: sid.clone(),
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                    token: *token,
                },
                cx,
            ),
            ChatRequest::ModelControls { sid, models, state } => {
                self.send_command(
                    &machine,
                    Command::ListSessionModels {
                        sid: sid.clone(),
                        token: *models,
                    },
                    cx,
                );
                self.send_command(
                    &machine,
                    Command::GetSessionState {
                        sid: sid.clone(),
                        token: *state,
                    },
                    cx,
                );
            }
        }
    }

    /// Whether `machine` can carry this request now. The two halves of a chat
    /// want different answers: a chat with no session yet connects lazily and
    /// lets its commands queue in the bridge channel behind the bootstrap
    /// (SPEC §Flows "Start session" — no readiness gate), while a live one
    /// refuses to send into a bridge that is not up, because a prompt that
    /// fails into stderr looks to the user exactly like one pi ignored.
    fn machine_ready_for(
        &mut self,
        request: &ChatRequest,
        machine: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if request.is_pre_session() {
            return self
                .store
                .update(cx, |store, cx| store.ensure_connection(machine, cx));
        }

        self.store.read(cx).is_live(machine)
    }

    /// A sidebar session row was clicked.
    fn open_session(&mut self, sid: &str, cx: &mut Context<Self>) {
        if !self
            .store
            .update(cx, |store, _| store.may_open_session(sid))
        {
            return;
        }

        // Navigating to a session abandons a chat that never became one —
        // deliberate move away from something that exists nowhere but here.
        // A chat mid-start keeps its place: its pi is on the way.
        self.discard_unstarted_chat(cx);

        // Only an open chat we have not seen exit can be re-shown as-is;
        // anything else goes through Open, which resumes if needed.
        let already_open = self.chat.as_ref().is_some_and(|chat| {
            let chat = chat.read(cx);

            chat.sid() == Some(sid) && chat.is_live()
        });

        if !already_open {
            self.store
                .update(cx, |store, cx| store.open_session(sid, cx));
        }

        cx.notify();
    }

    /// A chat with no session behind it exists only here, so navigating away
    /// from it is the whole of discarding it.
    fn discard_unstarted_chat(&mut self, cx: &Context<Self>) {
        let unstarted = self
            .chat
            .as_ref()
            .is_some_and(|chat| chat.read(cx).sid().is_none());

        if unstarted {
            self.chat = None;
            self.chat_requests = None;
        }
    }

    /// If this `Attached` completes a start the open chat made: add its
    /// session to the sidebar and hand the chat back to be promoted in place.
    /// `None` for plain opens and resumes, which build their own chat.
    fn take_starting_chat(&mut self, sid: &str, cx: &mut Context<Self>) -> Option<Entity<Chat>> {
        let chat = self.chat.clone()?;
        let (machine, cwd, name) = {
            let chat = chat.read(cx);
            let (starting_sid, name) = chat.starting_session()?;

            if starting_sid != sid {
                return None;
            }

            (
                chat.machine().to_string(),
                chat.workspace_path().to_string(),
                name.to_string(),
            )
        };

        self.store.update(cx, |store, cx| {
            store.insert_started_session(machine, sid.to_string(), name, cwd, cx)
        });

        Some(chat)
    }

    /// A workspace row's "+" was clicked: open a chat there with no session
    /// behind it yet (SPEC §Flows "Start session"). The workspace's machine
    /// connects lazily when the chat's first request routes through
    /// [`Workbench::machine_ready_for`].
    fn open_new_session_chat(
        &mut self,
        workspace_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self
            .store
            .read(cx)
            .workspaces()
            .get(workspace_index)
            .cloned()
        else {
            return;
        };

        // Re-clicking the row of the chat already on screen keeps it, and the
        // text typed into it.
        let same_workspace = self.chat.as_ref().is_some_and(|chat| {
            let chat = chat.read(cx);

            chat.starting_session().is_none()
                && chat.sid().is_none()
                && chat.machine() == workspace.machine
                && chat.workspace_path() == workspace.path
        });

        if self.store.read(cx).opening_sid().is_some() || same_workspace {
            return;
        }

        let target = ChatTarget {
            machine: workspace.machine,
            workspace_path: workspace.path,
        };
        let store = self.store.clone();
        let chat = cx.new(|cx| Chat::unstarted(target, store, window, cx));

        // Subscribed before anything can be emitted — including the model
        // fetch the constructor itself starts.
        self.chat_requests = Some(cx.subscribe_in(&chat, window, Self::handle_chat_request));
        self.chat = Some(chat);
        cx.notify();
    }

    /// The workspaces-header "+": one qualifying machine goes straight to
    /// browsing; several open the "which machine?" step first. No re-entry
    /// guard, because a dialog's overlay is what stops the second click.
    fn open_add_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let candidates = self.store.read(cx).machine_candidates();

        match candidates.len() {
            0 => self.report(
                "no machines available to add a workspace on".to_string(),
                window,
                cx,
            ),
            1 => self.open_picker(candidates[0].key.clone(), window, cx),
            _ => self.open_machine_choice(candidates, window, cx),
        }
    }

    /// The add-workspace machine step; picking one opens the folder picker
    /// there. The keys are snapshotted into the pick, so a probe landing behind
    /// the modal cannot move a row out from under the click.
    fn open_machine_choice(
        &mut self,
        candidates: Vec<harvester_client_core::MachineCandidate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys: Vec<String> = candidates
            .iter()
            .map(|candidate| candidate.key.clone())
            .collect();
        let rows = candidates
            .into_iter()
            .map(|candidate| choices::ChoiceRow {
                label: candidate.label.into(),
                selected: false,
            })
            .collect();

        choices::open(
            "Add workspace — choose machine",
            choices::ChoiceList::Short,
            rows,
            move |workbench, index, window, cx| {
                let Some(machine) = keys.get(index).cloned() else {
                    return;
                };

                workbench.open_picker(machine, window, cx);
            },
            window,
            cx,
        );
    }

    fn send_command(&self, machine: &str, command: Command, cx: &Context<Self>) {
        self.store.read(cx).send(machine, command);
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                ui::section_header("machines-header", "machines", cx)
                    .child(ui::section_chevron(!self.machines_collapsed))
                    .on_click(cx.listener(|workbench, _, _, cx| {
                        workbench.machines_collapsed = !workbench.machines_collapsed;
                        cx.notify();
                    })),
            )
            .when(!self.machines_collapsed, |sidebar| {
                sidebar.child(self.render_machine_list(cx))
            })
            .child(self.render_workspaces_header(cx))
            .when(!self.workspaces_collapsed, |sidebar| {
                sidebar.child(self.render_workspace_list(cx))
            })
            .child(
                ui::section_header("sessions-header", "sessions", cx)
                    .child(ui::section_chevron(!self.sessions_collapsed))
                    .on_click(cx.listener(|workbench, _, _, cx| {
                        workbench.sessions_collapsed = !workbench.sessions_collapsed;
                        cx.notify();
                    })),
            )
            .when(!self.sessions_collapsed, |sidebar| {
                sidebar.child(self.render_session_list(cx))
            })
    }

    /// Rows navigate by sid, never by position: the store's visible-session
    /// filter means display positions don't map back to its table.
    fn render_session_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Snapshotted before the rows are built: `cx.listener` wants the
        // context mutably, and the store is read through it.
        let rows: Vec<SessionRow> = self
            .store
            .read(cx)
            .visible_sessions()
            .into_iter()
            .map(|session| self.session_row(session, cx))
            .collect();
        let empty = rows.is_empty();

        div()
            .id("sessions")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, row)| self.render_session_row(index, row, cx)),
            )
            .when(empty, |list| {
                list.child(sidebar_empty_note("no sessions yet", cx))
            })
    }

    /// "WORKSPACES" with a "+" before the chevron (SPEC §Flows "Add
    /// workspace"); the "+" enables once at least one machine qualifies and
    /// stops propagation so adding doesn't also collapse the section.
    fn render_workspaces_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_add = !self.store.read(cx).machine_candidates().is_empty();

        ui::section_header("workspaces-header", "workspaces", cx)
            .child(
                Button::new("add-workspace")
                    .icon(IconName::Plus)
                    .ghost()
                    .xsmall()
                    .disabled(!can_add)
                    .on_click(cx.listener(|workbench, _, window, cx| {
                        cx.stop_propagation();
                        workbench.open_add_workspace(window, cx)
                    })),
            )
            .child(ui::section_chevron(!self.workspaces_collapsed))
            .on_click(cx.listener(|workbench, _, _, cx| {
                workbench.workspaces_collapsed = !workbench.workspaces_collapsed;
                cx.notify();
            }))
    }

    /// Read-only rows; the per-row "+" is the one click target (opens an
    /// unstarted chat in that workspace), so the row itself is a disabled
    /// [`ListItem`] — nothing about it invites a click.
    fn render_workspace_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let rows: Vec<(SharedString, SharedString)> = store
            .workspaces()
            .iter()
            .map(|workspace| {
                (
                    folder_name(&workspace.path).to_string().into(),
                    store.label(&workspace.machine).into(),
                )
            })
            .collect();
        let empty = rows.is_empty();

        div()
            .flex()
            .flex_col()
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, (folder, machine_label))| {
                        self.render_workspace_row(index, folder, machine_label, cx)
                    }),
            )
            .when(empty, |list| {
                list.child(sidebar_empty_note("no workspaces yet", cx))
            })
    }

    fn render_workspace_row(
        &self,
        index: usize,
        folder: SharedString,
        machine_label: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        sidebar_row(("workspace", index))
            .disabled(true)
            .child(
                h_flex()
                    .w_full()
                    .items_baseline()
                    .gap_1()
                    .child(
                        div()
                            .flex_none()
                            // Cap the folder name so a long one still leaves
                            // the machine label readable in the sidebar.
                            .max_w(px(140.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(folder),
                    )
                    .child(
                        div()
                            .text_xs()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(format!("in {machine_label}")),
                    )
                    .child(div().flex_1()),
            )
            .suffix({
                let workbench = cx.entity();

                move |window, _| {
                    let workbench = workbench.clone();

                    Button::new(("workspace-add", index))
                        .icon(IconName::Plus)
                        .ghost()
                        .xsmall()
                        .on_click(window.listener_for(
                            &workbench,
                            move |workbench, _, window, cx| {
                                workbench.open_new_session_chat(index, window, cx)
                            },
                        ))
                }
            })
    }

    /// The machines section: the hardcoded `local` row first (SPEC §Machine —
    /// connection support lands with the local-daemon stage), then provider
    /// machines, then the tracked machines their providers no longer list, then
    /// ad-hoc SSH destinations. All rows read-only.
    ///
    /// The missing rows are why this section is not simply the listing. A
    /// deleted machine that some workspace still points at would otherwise have
    /// no row anywhere, and its workspace would go on reading as healthy while
    /// nothing ever connected (SPEC §Build state, "A dead workspace says
    /// nothing").
    fn render_machine_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let provider_rows: Vec<_> = store
            .provider_machines()
            .iter()
            .enumerate()
            .map(|(index, row)| {
                self.render_machine_row(
                    ("provider-machine", index),
                    row.label.clone().into(),
                    Some(row.provider.clone().into()),
                    &row.key,
                    cx,
                )
            })
            .collect();
        let missing_rows: Vec<_> = store
            .missing_tracked_machines()
            .into_iter()
            .enumerate()
            .map(|(index, machine)| {
                self.render_machine_row(
                    ("missing-machine", index),
                    store.label(&machine).into(),
                    Some("no longer listed".into()),
                    &machine,
                    cx,
                )
            })
            .collect();
        let ad_hoc_rows: Vec<_> = store
            .ad_hoc_machines()
            .into_iter()
            .enumerate()
            .map(|(index, machine)| {
                self.render_machine_row(
                    ("ad-hoc-machine", index),
                    machine.clone().into(),
                    Some("ssh".into()),
                    &machine,
                    cx,
                )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            // The local row has no subtitle — it comes from nowhere.
            .child(self.render_machine_row(
                "local-machine",
                LOCAL_MACHINE.into(),
                None,
                LOCAL_MACHINE,
                cx,
            ))
            .children(provider_rows)
            .children(missing_rows)
            .children(ad_hoc_rows)
    }

    /// One machine row, read-only by design — machines are listed, not
    /// clicked. Disabled is exactly that statement to the list: no hover, no
    /// pointer, quieter text. The dot keeps its own colour and stays the row's
    /// whole signal.
    fn render_machine_row(
        &self,
        id: impl Into<gpui::ElementId>,
        label: SharedString,
        detail: Option<SharedString>,
        key: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        sidebar_row(id).disabled(true).child(stacked_row(
            self.machine_dot(key, cx),
            label,
            detail,
            None,
            cx,
        ))
    }

    /// The dot is the machine's whole story now that rows carry no link text
    /// (SPEC §UI machine dots): green = daemon connected, yellow =
    /// connecting/bootstrapping, red = connect failed/lost, the worker's `ssh
    /// <machine> true` probe says unreachable, or the machine is gone from its
    /// provider's listing; filled gray = probed fine, nothing has connected
    /// yet; hollow gray = nothing is known at all. Never provider-reported
    /// status (SPEC §Machine).
    ///
    /// One `match` over one value: the precedence between a probe verdict and
    /// a daemon link used to live here, in a renderer, reading two maps. It
    /// is the fleet's invariant now.
    fn machine_dot(&self, key: &str, cx: &Context<Self>) -> ui::Dot {
        present_machine(self.store.read(cx).state(key), key).dot
    }

    /// One session row's presentation, resolved against the store before the
    /// element that carries a click listener is built.
    fn session_row(&self, session: &MachineSession, cx: &Context<Self>) -> SessionRow {
        let store = self.store.read(cx);
        let info = &session.info;

        SessionRow {
            title: match &info.name {
                Some(name) => name.clone().into(),
                None => sid_preview(&info.sid).into(),
            },
            subtitle: format!(
                "{} · {}",
                folder_name(&info.cwd),
                store.label(&session.machine)
            )
            .into(),
            tone: match info.status {
                SessionStatus::Running => Tone::Live,
                SessionStatus::Exited => Tone::Idle,
            },
            selected: self
                .chat
                .as_ref()
                .is_some_and(|chat| chat.read(cx).sid() == Some(info.sid.as_str())),
            opening: store.opening_sid() == Some(info.sid.as_str()),
            sid: info.sid.clone(),
        }
    }

    fn render_session_row(
        &self,
        index: usize,
        row: SessionRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sid = row.sid;
        let body = stacked_row(
            // A session's status is always known — it came from a listing.
            ui::Dot::Solid(row.tone),
            row.title,
            Some(row.subtitle),
            row.opening.then(|| "opening…".into()),
            cx,
        );

        sidebar_row(("session", index))
            .selected(row.selected)
            .child(body)
            .on_click(cx.listener(move |workbench, _, _, cx| workbench.open_session(&sid, cx)))
    }

    fn render_content(&self, cx: &Context<Self>) -> impl IntoElement {
        let content = div().flex_1().min_w(px(0.)).h_full();

        if let Some(chat) = &self.chat {
            return content.child(chat.clone());
        }

        let (hint, color) = match self.store.read(cx).argv_machine() {
            Some(machine) => self.argv_machine_hint(machine, cx),
            // No argv machine: the workspace rows are the way in.
            None => (NO_ARGV_MACHINE_HINT.to_string(), Tone::Idle),
        };

        content.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(color.color(cx))
                .child(hint),
        )
    }

    /// The empty content pane when the argv machine has a bridge; a machine
    /// that only ever got probed falls back to the workspace-rows hint,
    /// because there is nothing about its daemon to report yet.
    fn argv_machine_hint(&self, machine: &str, cx: &Context<Self>) -> (String, Tone) {
        let display = present_machine(self.store.read(cx).state(machine), machine);

        match display.pane_message {
            Some(message) => (message, display.dot.tone()),
            None => (NO_ARGV_MACHINE_HINT.to_string(), Tone::Idle),
        }
    }

    /// The argv machine's link when it has one, else a connected-machine
    /// count. Failures are notifications now, so this is the whole statusbar.
    fn render_statusbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let (link_text, link_tone) = self.statusbar_link(cx);

        ui::statusbar_frame(cx).child(div().text_color(link_tone.color(cx)).child(link_text))
    }

    fn statusbar_link(&self, cx: &Context<Self>) -> (String, Tone) {
        if let Some(link) = self.argv_statusbar_link(cx) {
            return link;
        }

        let store = self.store.read(cx);

        (
            format!(
                "{} of {} machines connected",
                store.connected_machine_count(),
                store.linked_machine_count()
            ),
            Tone::Idle,
        )
    }

    /// `None` when there is no argv machine, or when it has no bridge yet —
    /// the statusbar then shows the fleet-wide count instead. Returning the
    /// absence rather than inventing a string for the probe-only states is
    /// what keeps this a total match over [`MachineState`].
    fn argv_statusbar_link(&self, cx: &Context<Self>) -> Option<(String, Tone)> {
        let store = self.store.read(cx);
        let machine = store.argv_machine()?;
        let display = present_machine(store.state(machine), machine);

        Some((display.statusbar_link?, display.dot.tone()))
    }
}

/// One session row, read off the store before any element is built. Rows carry
/// a click listener, which needs the context mutably — and the store is read
/// through that same context.
struct SessionRow {
    title: SharedString,
    subtitle: SharedString,
    tone: Tone,
    selected: bool,
    opening: bool,
    sid: String,
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(13.))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(self.render_content(cx)),
            )
            .child(self.render_statusbar(cx))
            // `Root` holds the dialogs and notifications but renders neither;
            // placing their layers is the root view's job, and omitting them
            // makes `open_dialog` and `push_notification` silent no-ops.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

fn sid_preview(sid: &str) -> String {
    sid.chars().take(SID_PREVIEW_CHARS).collect()
}

/// Every sidebar row is the library's [`ListItem`], inset so its hover and
/// selection read as the rounded pill the sidebar had before — 6px margin plus
/// 6px padding is the 12px text inset the section headers use.
fn sidebar_row(id: impl Into<gpui::ElementId>) -> ListItem {
    ListItem::new(id)
        .mx(px(6.))
        .px(px(6.))
        .py_1()
        .rounded_md()
        .text_size(px(13.))
}

/// The stacked row body the session and machine rows share: the status dot
/// beside the title, the quieter detail line under it, and an optional note
/// (`opening…`) at the end of the title line.
fn stacked_row(
    dot: ui::Dot,
    title: SharedString,
    detail: Option<SharedString>,
    note: Option<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .w_full()
        .min_w(px(0.))
        .child(
            h_flex()
                .gap_2()
                .child(ui::status_dot(dot, cx))
                .child(div().overflow_hidden().text_ellipsis().child(title))
                .children(note.map(|note| {
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(note)
                })),
        )
        .children(detail.map(|detail| {
            div()
                // Indents under the title, clearing the dot.
                .pl(px(14.))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .overflow_hidden()
                .text_ellipsis()
                .child(detail)
        }))
}

/// What a sidebar section says when it has nothing to list.
fn sidebar_empty_note(text: &'static str, cx: &App) -> Div {
    div()
        .px_3()
        .py_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// Everything the UI says about one machine's link, decided in one place.
///
/// There used to be three matches over [`MachineState`] in this file — the
/// dot, the empty pane's sentence, the statusbar link — and a fourth would
/// have been free to disagree with them. A state that means "failed" cannot
/// now be red in one and gray in another, because the tone is chosen once.
/// The strings differ per surface because they are different sentences, not
/// because the state means different things.
struct MachineDisplay {
    dot: ui::Dot,
    /// The content pane's sentence when this machine is the one on screen.
    /// `None` for the probe-only states: nothing about a daemon to report.
    pane_message: Option<String>,
    /// The statusbar's link text, same rule.
    statusbar_link: Option<String>,
}

fn present_machine(state: MachineState<'_>, machine: &str) -> MachineDisplay {
    match state {
        MachineState::Connecting => MachineDisplay {
            dot: ui::Dot::Solid(Tone::Pending),
            pane_message: Some(format!("connecting to {machine}…")),
            statusbar_link: Some("connecting…".to_string()),
        },
        MachineState::Connected { daemon_version } => MachineDisplay {
            dot: ui::Dot::Solid(Tone::Live),
            pane_message: Some("select a session".to_string()),
            statusbar_link: Some(format!("{machine} · harvesterd {daemon_version}")),
        },
        MachineState::ConnectFailed { detail } => MachineDisplay {
            dot: ui::Dot::Solid(Tone::Failed),
            pane_message: Some(detail.to_string()),
            statusbar_link: Some("connect failed".to_string()),
        },
        MachineState::ConnectionLost => MachineDisplay {
            dot: ui::Dot::Solid(Tone::Failed),
            pane_message: Some("connection lost".to_string()),
            statusbar_link: Some("connection lost".to_string()),
        },
        // The one state a *listing* reports, and the only sentence that names
        // the machine rather than the daemon — there was never a daemon.
        MachineState::Missing => MachineDisplay {
            dot: ui::Dot::Solid(Tone::Failed),
            pane_message: Some(format!("{machine} is no longer listed by its provider")),
            statusbar_link: Some("no longer listed".to_string()),
        },
        // A probe verdict is not a link: unreachable still reads as failed,
        // but there is nothing to say about a daemon nobody has reached for.
        MachineState::Unreachable => MachineDisplay {
            dot: ui::Dot::Solid(Tone::Failed),
            pane_message: None,
            statusbar_link: None,
        },
        MachineState::Reachable => MachineDisplay {
            dot: ui::Dot::Solid(Tone::Idle),
            pane_message: None,
            statusbar_link: None,
        },
        // Hollow, not grey: nothing has probed or connected, so this is the
        // absence of a verdict rather than a quiet one.
        MachineState::Unknown => MachineDisplay {
            dot: ui::Dot::Hollow(Tone::Idle),
            pane_message: None,
            statusbar_link: None,
        },
    }
}
