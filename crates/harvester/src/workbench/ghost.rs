//! The ghost chat (docs/SPEC.md §Flows "Start session"): a chat that exists
//! only client-side until its first prompt mints a session.
//!
//! A module with an interface, not a class split across two files. Every
//! decision the ghost makes about itself is a method on [`GhostChat`]; what it
//! cannot do — reach a bridge, resolve a machine's label, insert a sidebar row
//! — it returns as a [`GhostRequest`] for the workbench to carry out. The
//! workbench therefore owns three ghost-shaped methods instead of fifteen.

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, Focusable, SharedString, Subscription, Window,
};
use harvester_client_core::bridge::Command;
use harvester_client_core::{RequestToken, RequestTokens};

use super::{folder_name, Workbench};
use crate::input::TextInput;
use crate::model_control::ModelControl;
use crate::models::ModelsState;
use crate::theme;
use crate::ui;
use crate::workspaces::Workspace;

/// Cap on the session name derived from the first prompt's first line — enough
/// to recognize, short enough for pi's session list and the sidebar.
const SESSION_NAME_MAX_CHARS: usize = 48;

/// One (machine key, display label) the ghost could run on.
pub(super) type MachineCandidate = (String, String);

/// What the ghost needs the workbench to send. The ghost decides *what* to ask
/// for — it owns the launch line's arguments — and the workbench owns the
/// bridges, so it does the asking.
pub(super) enum GhostRequest {
    ListModels {
        machine: String,
        cwd: String,
        token: RequestToken,
    },
    /// The create → attach → prompt chain. `sid` is echoed so the workbench can
    /// register its open guard against the same session.
    Start {
        machine: String,
        sid: String,
        command: Command,
    },
}

/// What `Attached` needs to finish a ghost start: the sid to match on, the name
/// for the synthesized sidebar row, the prompt to seed the chat with.
pub(super) struct StartedSession {
    pub sid: String,
    pub name: String,
    pub first_prompt: String,
}

pub(super) struct GhostChat {
    /// Where the session will run; the path is the pi cwd.
    workspace: Workspace,
    /// The model list, its request and its modal — the live chat's control too
    /// (see `model_control`); here a pick rides the launch line.
    model: ModelControl,
    /// Index into the loaded list; `None` = let pi pick its default.
    selected_model: Option<usize>,
    machine_picker_open: bool,
    /// Set once the first prompt fired the create → attach chain; cleared by
    /// `Attached` (ghost becomes a chat) or `RequestFailed` (retryable).
    started: Option<StartedSession>,
    /// Focus the prompt input on the next frame (same trick as `Chat`).
    focus_input: bool,
    input: Entity<TextInput>,
    _input_subscription: Subscription,
}

impl GhostChat {
    /// A ghost on `workspace`, already asking for the models behind it — the
    /// seconds the uncached `list_models` costs hide behind the user typing.
    pub fn new(
        workspace: Workspace,
        input: Entity<TextInput>,
        input_subscription: Subscription,
        tokens: &RequestTokens,
    ) -> (GhostChat, GhostRequest) {
        let mut model = ModelControl::default();
        let request = GhostRequest::ListModels {
            machine: workspace.machine.clone(),
            cwd: workspace.path.clone(),
            token: model.fetching(tokens),
        };

        (
            GhostChat {
                workspace,
                model,
                selected_model: None,
                machine_picker_open: false,
                started: None,
                focus_input: true,
                input,
                _input_subscription: input_subscription,
            },
            request,
        )
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn input(&self) -> Entity<TextInput> {
        self.input.clone()
    }

    /// The sid this ghost is waiting on `Attached` for.
    pub fn started_sid(&self) -> Option<&str> {
        Some(self.started.as_ref()?.sid.as_str())
    }

    /// The start landed: the ghost is spent, and what it knew about the session
    /// goes to the sidebar row and the chat that replaces it.
    pub fn into_started(self) -> Option<(Workspace, StartedSession)> {
        Some((self.workspace, self.started?))
    }

    /// The start failed. Back to editable — a retry mints a fresh sid.
    pub fn abandon_start(&mut self) {
        self.started = None;
    }

    /// The machine's bridge died: the model list will not arrive.
    pub fn fail_models(&mut self, detail: String) {
        self.abandon_start();
        self.model.fail(detail);
    }

    pub fn accept_models(
        &mut self,
        token: RequestToken,
        result: Result<serde_json::Value, String>,
    ) {
        self.model.accept(token, result);
    }

    pub fn model_picker_open(&self) -> bool {
        self.model.is_open()
    }

    /// What the shared model modal renders for this ghost: the loaded list and
    /// the picked index (`None` = pi's default, which the ghost alone offers).
    pub fn picker_contents(&self) -> (Vec<crate::models::ModelChoice>, Option<usize>) {
        (self.model.choices().to_vec(), self.selected_model)
    }

    /// Only before the start fires: once the launch line is out, its model is
    /// what the session has.
    pub fn open_model_picker(&mut self) {
        if self.started.is_none() {
            self.model.open();
        }
    }

    pub fn close_model_picker(&mut self) {
        self.model.close();
    }

    /// `None` selects pi's default model.
    pub fn select_model(&mut self, choice: Option<usize>) {
        self.selected_model = choice;
        self.model.close();
    }

    pub fn machine_picker_open(&self) -> bool {
        self.machine_picker_open
    }

    /// Offered only while at least two stored workspaces share this path —
    /// below that the control reads as the fixed destination.
    pub fn open_machine_picker(&mut self, candidates: &[MachineCandidate]) {
        if self.started.is_none() && candidates.len() >= 2 {
            self.machine_picker_open = true;
        }
    }

    pub fn close_machine_picker(&mut self) {
        self.machine_picker_open = false;
    }

    /// Retarget to another machine: the typed prompt survives, only the
    /// destination changes. `None` when the pick is the current machine (or no
    /// machine at all), so nothing is refetched. The stale `ModelsListed` from
    /// the old machine is dropped by the control's own token.
    pub fn pick_machine(
        &mut self,
        candidate_index: usize,
        candidates: &[MachineCandidate],
        tokens: &RequestTokens,
    ) -> Option<GhostRequest> {
        self.machine_picker_open = false;

        let (machine, _label) = candidates.get(candidate_index)?;

        if &self.workspace.machine == machine {
            return None;
        }

        self.workspace.machine = machine.clone();
        self.selected_model = None;

        Some(GhostRequest::ListModels {
            machine: machine.clone(),
            cwd: self.workspace.path.clone(),
            token: self.model.fetching(tokens),
        })
    }

    /// The first prompt: mint the sid, derive the name, resolve the launch
    /// line's model, and hand the chain to the workbench. The ghost stays
    /// visible ("starting…") until `Attached` swaps it for the real chat.
    ///
    /// The sid is a **uuidv7 minted here** because that is pi's own session-id
    /// format and the client is the one that mints it (docs/DAEMON.md §the pi
    /// launch line). It sits in a gpui module only because that is where the
    /// prompt is; nothing about it is presentation.
    pub fn start(&mut self, prompt: String) -> Option<GhostRequest> {
        if prompt.trim().is_empty() || self.started.is_some() {
            return None;
        }

        let sid = uuid::Uuid::now_v7().to_string();
        let name = session_name_from_prompt(&prompt);
        let (provider, model) = self.launch_model();

        self.started = Some(StartedSession {
            sid: sid.clone(),
            name: name.clone(),
            first_prompt: prompt.clone(),
        });

        Some(GhostRequest::Start {
            machine: self.workspace.machine.clone(),
            sid: sid.clone(),
            command: Command::StartSession {
                sid,
                cwd: self.workspace.path.clone(),
                provider,
                model,
                name,
                first_prompt: prompt,
            },
        })
    }

    /// The launch line's `--provider`/`--model`, or pi's own default when
    /// nothing was picked (or no list ever arrived).
    fn launch_model(&self) -> (Option<String>, Option<String>) {
        let index = match self.selected_model {
            Some(index) => index,
            None => return (None, None),
        };
        let Some(choice) = self.model.choices().get(index) else {
            return (None, None);
        };

        (Some(choice.provider.clone()), Some(choice.id.clone()))
    }

    /// Focus the input once, on the frame after it is rendered.
    pub fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_input)
    }

    /// The ghost pane: header, empty body, prompt box — or "starting…" once
    /// the first prompt fired. `machine_label` and `candidates` are the
    /// workbench's to resolve; everything else here is the ghost's own.
    pub fn render(
        &self,
        machine_label: SharedString,
        candidates: &[MachineCandidate],
        window: &Window,
        cx: &mut Context<Workbench>,
    ) -> impl IntoElement {
        let subtitle = format!("{} on {}", folder_name(&self.workspace.path), machine_label);
        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let bottom_row = div().flex_none().px_3().pb_3();
        let bottom_row = match self.started {
            Some(_) => bottom_row.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child("starting session…"),
            ),
            None => bottom_row.child(self.render_prompt_box(input_focused, candidates, cx)),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(ui::chat_header("New session".into(), subtitle.into()))
            .child(
                div().flex_1().min_h(px(0.)).p_3().children(
                    // Model failure is informational, not blocking: pi still
                    // launches with its default.
                    match self.model.state() {
                        ModelsState::Failed(detail) => Some(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::STATUS_ERROR))
                                .child(format!("{detail} — pi will use its default model")),
                        ),
                        _ => None,
                    },
                ),
            )
            .child(bottom_row)
    }

    fn render_prompt_box(
        &self,
        input_focused: bool,
        candidates: &[MachineCandidate],
        cx: &mut Context<Workbench>,
    ) -> impl IntoElement {
        // The prompt box only renders before the start fires, so a loaded list
        // alone decides whether the model control is clickable.
        let (model_label, model_enabled): (SharedString, bool) = match self.model.state() {
            ModelsState::Loading => ("loading models…".into(), false),
            ModelsState::Failed(_) => ("default model".into(), false),
            ModelsState::Loaded(models) => (
                match self.selected_model.and_then(|index| models.get(index)) {
                    Some(choice) => choice.name.clone(),
                    None => "default model".into(),
                },
                true,
            ),
        };
        // Live only while ≥2 stored workspaces share this path.
        let machine_enabled = candidates.len() >= 2;
        let machine_label: SharedString = candidates
            .iter()
            .find(|(machine, _)| machine == &self.workspace.machine)
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| self.workspace.machine.clone())
            .into();

        ui::prompt_box(input_focused)
            .child(ui::prompt_box_input(self.input.clone()))
            .child(
                ui::prompt_box_controls()
                    .child(
                        ui::prompt_box_control("ghost-model", model_label, model_enabled).when(
                            model_enabled,
                            |control| {
                                control.on_click(cx.listener(|workbench, _, _, cx| {
                                    workbench.with_ghost(cx, GhostChat::open_model_picker)
                                }))
                            },
                        ),
                    )
                    // Read-only: the repository is the workspace the ghost was
                    // opened from.
                    .child(ui::prompt_box_control(
                        "ghost-repo",
                        folder_name(&self.workspace.path).to_string().into(),
                        false,
                    ))
                    .child(
                        ui::prompt_box_control("ghost-machine", machine_label, machine_enabled)
                            .when(machine_enabled, |control| {
                                control.on_click(cx.listener(|workbench, _, _, cx| {
                                    let candidates = workbench.ghost_machine_candidates();

                                    workbench.with_ghost(cx, |ghost| {
                                        ghost.open_machine_picker(&candidates)
                                    })
                                }))
                            }),
                    )
                    .child(div().flex_1())
                    .child(ui::prompt_box_send("ghost-send").on_click(
                        cx.listener(|workbench, _, _, cx| workbench.submit_ghost_prompt(cx)),
                    )),
            )
    }

    /// The ghost's machine modal: the workspaces sharing its path, the current
    /// destination checked.
    pub fn render_machine_picker(
        &self,
        candidates: &[MachineCandidate],
        cx: &mut Context<Workbench>,
    ) -> impl IntoElement {
        let rows: Vec<_> = candidates
            .iter()
            .enumerate()
            .map(|(index, (machine, label))| {
                ui::model_row(
                    ("ghost-machine-choice", index),
                    label.clone().into(),
                    machine == &self.workspace.machine,
                )
                .on_click(
                    cx.listener(move |workbench, _, _, cx| workbench.ghost_pick_machine(index, cx)),
                )
                .into_any_element()
            })
            .collect();

        ui::modal_list(
            "ghost-machine",
            "Choose machine",
            ui::ModalListHeight::FitContent,
            rows,
        )
        .on_click(cx.listener(|workbench, _, _, cx| {
            workbench.with_ghost(cx, GhostChat::close_machine_picker)
        }))
    }
}

/// Pi's `--name` for the session, from the prompt that started it: its first
/// non-blank line, capped. Presentation, not protocol — pi accepts any string.
fn session_name_from_prompt(prompt: &str) -> String {
    prompt
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(SESSION_NAME_MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_prompt_is_its_own_session_name() {
        assert_eq!(session_name_from_prompt("fix the tests"), "fix the tests");
    }

    #[test]
    fn only_the_first_line_names_the_session() {
        assert_eq!(
            session_name_from_prompt("fix the tests\nand also the docs"),
            "fix the tests"
        );
    }

    #[test]
    fn a_long_first_line_is_capped() {
        let name = session_name_from_prompt(&"x".repeat(SESSION_NAME_MAX_CHARS + 10));

        assert_eq!(name.chars().count(), SESSION_NAME_MAX_CHARS);
    }

    /// Counted in characters, not bytes — `take` on chars, so a multi-byte
    /// line is never cut mid-character and never capped early.
    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let name = session_name_from_prompt(&"é".repeat(SESSION_NAME_MAX_CHARS + 5));

        assert_eq!(name.chars().count(), SESSION_NAME_MAX_CHARS);
    }

    #[test]
    fn a_leading_blank_line_is_not_the_name() {
        assert_eq!(session_name_from_prompt("\n\n  fix it  "), "fix it");
    }
}
