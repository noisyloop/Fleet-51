//! Each bot is an append-only event log. Its status, history and resume behaviour are a fold over
//! that log, so "was this message delivered?" and "should this restart?" have one answer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: u64,
    #[serde(flatten)]
    pub ev: Ev,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Ev {
    Created {
        cwd: String,
        title: String,
        model: Option<String>,
    },
    Profile {
        name: String,
        shape: String,
        color: String,
    },
    ModelChanged {
        model: String,
    },
    ThinkChanged {
        think: bool,
    },
    /// Whether the bot may run code (in the sandbox). Off unless you turn it on.
    RunCodeChanged {
        run_code: bool,
    },
    Spawned {
        pid: Option<u32>,
        generation: u64,
        resume: Option<String>,
    },
    Ready,
    PromptQueued {
        id: String,
        text: String,
        /// The automation that sent this message, if it was not typed by the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        automation: Option<String>,
        /// Files the user added to the bot's folder with this message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    PromptDelivered {
        id: String,
    },
    PromptAcked {
        id: String,
    },
    ProviderSession {
        id: String,
    },
    Reasoning {
        text: String,
    },
    Text {
        text: String,
    },
    ToolCall {
        name: String,
        input: Value,
    },
    TurnDone {
        ok: bool,
        summary: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
    },
    StopRequested,
    Suspended {
        generation: u64,
    },
    Orphaned {
        generation: u64,
    },
    Exited {
        code: Option<i32>,
        clean: bool,
    },
    Archived,
    /// The bot copied its folder aside before changing it; Undo restores this copy.
    Checkpoint {
        id: String,
    },
    Undone {
        id: String,
    },
    /// The chat was cleared: earlier messages are hidden and the bot starts a new conversation.
    Cleared,
    Note {
        level: String,
        message: String,
    },
}

/// What the daemon-wide stream carries: every bot's events and state changes on one
/// connection, so a window showing many bots never runs out of browser connections.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Live {
    State(Box<State>),
    Event {
        session: String,
        event: Event,
    },
    Pull(crate::models::Pull),
    Runtime(crate::runtime::Runtime),
    /// The full list, sent whenever an automation is added, changed or runs.
    Automations {
        automations: Vec<crate::automations::Automation>,
    },
    /// An automation run finished (shown in the app; also sent as a system notification).
    Notice {
        bot: String,
        title: String,
        body: String,
    },
    Network {
        online: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Starting,
    Idle,
    Working,
    Suspended,
    Orphaned,
    Exited,
    Failed,
}

impl Status {
    /// No terminal event yet: the process should still be alive.
    pub fn is_live(self) -> bool {
        matches!(self, Status::Starting | Status::Idle | Status::Working)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub text: String,
    pub delivered: bool,
    #[serde(default)]
    pub automation: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub shape: String,
    pub color: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub model: Option<String>,
    pub think: bool,
    #[serde(default)]
    pub run_code: bool,
    pub profile: Option<Profile>,
    pub status: Status,
    pub provider_session: Option<String>,
    /// Messages the bot has not acknowledged. Only these are ever re-sent after a restart.
    pub pending: Vec<Prompt>,
    /// An acknowledged message whose reply never finished (interrupted if the process died).
    pub open_turn: Option<String>,
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_text: Option<String>,
    pub pid: Option<u32>,
    pub archived: bool,
    pub created: u64,
    pub updated: u64,
    pub last_seq: u64,
    /// Events up to here were cleared from the chat.
    #[serde(default)]
    pub cleared_seq: u64,
}

impl State {
    pub fn apply(&mut self, e: &Event) {
        self.last_seq = e.seq;
        self.updated = e.ts;
        match &e.ev {
            Ev::Created { cwd, title, model } => {
                self.cwd = cwd.clone();
                self.title = title.clone();
                self.model = model.clone();
                self.think = true;
                self.created = e.ts;
            }
            Ev::Profile { name, shape, color } => {
                self.title = name.clone();
                self.profile = Some(Profile {
                    name: name.clone(),
                    shape: shape.clone(),
                    color: color.clone(),
                });
            }
            Ev::ModelChanged { model } => self.model = Some(model.clone()),
            Ev::ThinkChanged { think } => self.think = *think,
            Ev::RunCodeChanged { run_code } => self.run_code = *run_code,
            Ev::Spawned { pid, .. } => {
                self.pid = *pid;
                self.status = Status::Starting;
            }
            Ev::Ready => {
                if self.status == Status::Starting {
                    self.status = Status::Idle;
                }
            }
            Ev::PromptQueued {
                id,
                text,
                automation,
                attachments,
            } => self.pending.push(Prompt {
                id: id.clone(),
                text: text.clone(),
                delivered: false,
                automation: automation.clone(),
                attachments: attachments.clone(),
            }),
            Ev::PromptDelivered { id } => {
                if let Some(p) = self.pending.iter_mut().find(|p| &p.id == id) {
                    p.delivered = true;
                }
                self.status = Status::Working;
            }
            Ev::PromptAcked { id } => {
                if let Some(p) = self.pending.iter().find(|p| &p.id == id) {
                    self.open_turn = Some(p.text.clone());
                }
                self.pending.retain(|p| &p.id != id);
                self.status = Status::Working;
            }
            Ev::ProviderSession { id } => self.provider_session = Some(id.clone()),
            Ev::Text { text } => self.last_text = Some(text.clone()),
            Ev::TurnDone {
                input_tokens,
                output_tokens,
                ..
            } => {
                self.open_turn = None;
                self.turns += 1;
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                if self.status.is_live() {
                    self.status = Status::Idle;
                }
            }
            Ev::Suspended { .. } => self.status = Status::Suspended,
            Ev::Orphaned { .. } => self.status = Status::Orphaned,
            Ev::Exited { clean, .. } => {
                self.pid = None;
                self.status = if *clean {
                    Status::Exited
                } else {
                    Status::Failed
                };
            }
            Ev::Archived => self.archived = true,
            Ev::Cleared => {
                self.cleared_seq = e.seq;
                self.provider_session = None;
                self.open_turn = None;
                self.last_text = None;
            }
            Ev::Reasoning { .. }
            | Ev::ToolCall { .. }
            | Ev::StopRequested
            | Ev::Note { .. }
            | Ev::Checkpoint { .. }
            | Ev::Undone { .. } => {}
        }
    }

    /// Restart only what was interrupted. A cleanly stopped or deleted bot never restarts by itself.
    pub fn should_auto_resume(&self) -> bool {
        !self.archived && matches!(self.status, Status::Suspended | Status::Orphaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, ev: Ev) -> Event {
        Event { seq, ts: seq, ev }
    }

    #[test]
    fn acknowledged_messages_are_not_resent_and_clean_exits_stay_stopped() {
        let mut s = State::default();
        s.apply(&ev(
            1,
            Ev::PromptQueued {
                id: "a".into(),
                text: "one".into(),
                automation: None,
                attachments: vec![],
            },
        ));
        s.apply(&ev(
            2,
            Ev::PromptQueued {
                id: "b".into(),
                text: "two".into(),
                automation: None,
                attachments: vec![],
            },
        ));
        s.apply(&ev(3, Ev::PromptDelivered { id: "a".into() }));
        s.apply(&ev(4, Ev::PromptAcked { id: "a".into() }));
        assert_eq!(s.pending.len(), 1);
        assert_eq!(s.pending[0].id, "b");
        assert_eq!(s.open_turn.as_deref(), Some("one"));
        s.apply(&ev(
            5,
            Ev::Exited {
                code: Some(0),
                clean: true,
            },
        ));
        assert!(!s.should_auto_resume());
        s.apply(&ev(6, Ev::Orphaned { generation: 2 }));
        assert!(s.should_auto_resume());
    }

    #[test]
    fn bots_keep_profile_model_think_and_archive_state() {
        let mut s = State::default();
        s.apply(&ev(
            1,
            Ev::Created {
                cwd: "/x".into(),
                title: "x".into(),
                model: Some("qwen3:1.7b".into()),
            },
        ));
        assert!(s.think);
        s.apply(&ev(
            2,
            Ev::Profile {
                name: "Scout".into(),
                shape: "star".into(),
                color: "green".into(),
            },
        ));
        s.apply(&ev(
            3,
            Ev::ModelChanged {
                model: "qwen3:0.6b".into(),
            },
        ));
        s.apply(&ev(4, Ev::ThinkChanged { think: false }));
        assert_eq!(s.title, "Scout");
        assert_eq!(s.model.as_deref(), Some("qwen3:0.6b"));
        assert!(!s.think);
        s.apply(&ev(5, Ev::Orphaned { generation: 2 }));
        s.apply(&ev(6, Ev::Archived));
        assert!(!s.should_auto_resume());
    }
}
