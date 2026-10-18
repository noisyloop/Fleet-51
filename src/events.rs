//! A session is an append-only event log. Every status, resume and replay decision is a fold
//! over this log, so "did this exit cleanly?" and "was this prompt delivered?" have one answer.

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
        agent: String,
        workspace: String,
        cwd: String,
        title: String,
        sandbox: String,
        permission: String,
        account: Option<String>,
        model: Option<String>,
        group: Option<String>,
    },
    Spawned {
        pid: Option<u32>,
        protocol: String,
        argv: Vec<String>,
        account: Option<String>,
        generation: u64,
        resume: Option<String>,
    },
    Ready,
    PromptQueued {
        id: String,
        text: String,
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
    Text {
        text: String,
    },
    ToolCall {
        name: String,
        input: Value,
    },
    Permission {
        tool: String,
        decision: String,
    },
    Output {
        data: String,
    },
    TurnDone {
        ok: bool,
        summary: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
    },
    RateLimited {
        account: Option<String>,
        message: String,
    },
    AccountSwitched {
        from: Option<String>,
        to: String,
    },
    Hook {
        name: String,
        payload: Value,
    },
    Annotation {
        id: String,
        file: String,
        line: u32,
        comment: String,
    },
    FeedbackSent {
        annotations: Vec<String>,
        prompt_id: String,
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
    Merged {
        checkout: String,
        branch: String,
        into: String,
        commit: String,
    },
    Note {
        level: String,
        message: String,
    },
}

/// What the daemon-wide stream carries: every session's events and state changes on one
/// connection, so a client watching many sessions never runs out of browser connections.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Live {
    State(State),
    Event { session: String, event: Event },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Starting,
    Idle,
    Working,
    RateLimited,
    Suspended,
    Orphaned,
    Exited,
    Failed,
}

impl Status {
    /// No terminal event yet: the process should still be alive.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Status::Starting | Status::Idle | Status::Working | Status::RateLimited
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub text: String,
    pub delivered: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnnotationState {
    pub id: String,
    pub file: String,
    pub line: u32,
    pub comment: String,
    pub sent: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub id: String,
    pub agent: String,
    pub workspace: String,
    pub cwd: String,
    pub title: String,
    pub sandbox: String,
    pub permission: String,
    pub account: Option<String>,
    pub model: Option<String>,
    pub group: Option<String>,
    pub status: Status,
    pub provider_session: Option<String>,
    /// Prompts the agent has not acknowledged. Only these are ever re-sent on resume.
    pub pending: Vec<Prompt>,
    /// Text of an acknowledged prompt whose turn has not finished (interrupted if the process dies).
    pub open_turn: Option<String>,
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub last_text: Option<String>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub stop_requested: bool,
    pub annotations: Vec<AnnotationState>,
    pub merged: bool,
    pub created: u64,
    pub updated: u64,
    pub last_seq: u64,
}

impl State {
    pub fn apply(&mut self, e: &Event) {
        self.last_seq = e.seq;
        self.updated = e.ts;
        match &e.ev {
            Ev::Created {
                agent,
                workspace,
                cwd,
                title,
                sandbox,
                permission,
                account,
                model,
                group,
            } => {
                self.agent = agent.clone();
                self.workspace = workspace.clone();
                self.cwd = cwd.clone();
                self.title = title.clone();
                self.sandbox = sandbox.clone();
                self.permission = permission.clone();
                self.account = account.clone();
                self.model = model.clone();
                self.group = group.clone();
                self.created = e.ts;
                self.status = Status::Starting;
            }
            Ev::Spawned { pid, account, .. } => {
                self.pid = *pid;
                if account.is_some() {
                    self.account = account.clone();
                }
                self.status = Status::Starting;
                self.stop_requested = false;
                self.exit_code = None;
            }
            Ev::Ready => {
                if self.status == Status::Starting {
                    self.status = Status::Idle;
                }
            }
            Ev::PromptQueued { id, text } => self.pending.push(Prompt {
                id: id.clone(),
                text: text.clone(),
                delivered: false,
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
                cost_usd,
                ..
            } => {
                self.open_turn = None;
                self.turns += 1;
                self.input_tokens += input_tokens;
                self.output_tokens += output_tokens;
                self.cost_usd += cost_usd;
                if self.status.is_live() {
                    self.status = Status::Idle;
                }
            }
            Ev::RateLimited { .. } => self.status = Status::RateLimited,
            Ev::AccountSwitched { to, .. } => self.account = Some(to.clone()),
            Ev::Annotation {
                id,
                file,
                line,
                comment,
            } => self.annotations.push(AnnotationState {
                id: id.clone(),
                file: file.clone(),
                line: *line,
                comment: comment.clone(),
                sent: false,
            }),
            Ev::FeedbackSent { annotations, .. } => {
                for a in self
                    .annotations
                    .iter_mut()
                    .filter(|a| annotations.contains(&a.id))
                {
                    a.sent = true;
                }
            }
            Ev::StopRequested => self.stop_requested = true,
            Ev::Suspended { .. } => self.status = Status::Suspended,
            Ev::Orphaned { .. } => self.status = Status::Orphaned,
            Ev::Exited { code, clean } => {
                self.exit_code = *code;
                self.pid = None;
                self.status = if *clean {
                    Status::Exited
                } else {
                    Status::Failed
                };
            }
            Ev::Merged { .. } => self.merged = true,
            Ev::Output { .. }
            | Ev::ToolCall { .. }
            | Ev::Permission { .. }
            | Ev::Hook { .. }
            | Ev::Note { .. } => {}
        }
    }

    /// Auto-resume only work that was interrupted. A clean exit is never resumed on its own.
    pub fn should_auto_resume(&self) -> bool {
        matches!(self.status, Status::Suspended | Status::Orphaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, ev: Ev) -> Event {
        Event { seq, ts: seq, ev }
    }

    #[test]
    fn acked_prompts_are_not_pending_and_clean_exit_is_not_resumed() {
        let mut s = State::default();
        s.apply(&ev(
            1,
            Ev::PromptQueued {
                id: "a".into(),
                text: "one".into(),
            },
        ));
        s.apply(&ev(
            2,
            Ev::PromptQueued {
                id: "b".into(),
                text: "two".into(),
            },
        ));
        s.apply(&ev(3, Ev::PromptDelivered { id: "a".into() }));
        s.apply(&ev(4, Ev::PromptAcked { id: "a".into() }));
        assert_eq!(s.pending.len(), 1);
        assert_eq!(s.pending[0].id, "b");
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
}
