//! Teams: a group chat where several bots work together on a goal by themselves. They take turns:
//! each bot is sent what the others said since its last turn and adds its part, until one says
//! the goal is done ([DONE]), the turn limit is reached, or you stop it. You can join in at any
//! time; your message goes to whichever bot speaks next.
//!
//! Each bot keeps its own folder and its own limits (files, Run code), exactly as in its own chat:
//! a team only passes messages between bots, so teammates see what a bot says, never its files.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::daemon::Daemon;
use crate::events::{Ev, Live};
use crate::home::{now_ms, short_id};

pub const MIN_MEMBERS: usize = 2;
pub const MAX_MEMBERS: usize = 6;
pub const MAX_TURNS: u32 = 100;
/// What a bot writes to say the team's goal is reached.
pub const DONE: &str = "[DONE]";
/// Messages kept per team; older ones drop off (each bot still remembers its own chat).
const KEEP_MESSAGES: usize = 300;
/// Longest message passed on to a teammate, so a long reply can't swamp a small model.
const MAX_FORWARD: usize = 4000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeamMsg {
    pub id: String,
    /// A bot's id, "you" or "system".
    pub from: String,
    pub name: String,
    pub text: String,
    pub at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    /// Waiting for you to start it.
    Idle,
    Running,
    /// A bot said the goal is done, or the turn limit was reached.
    Done,
    Stopped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub goal: String,
    /// Bot ids, in speaking order.
    pub members: Vec<String>,
    /// Turns per run; Continue gives the team this many more.
    pub max_turns: u32,
    /// Turns taken in this run.
    pub turns: u32,
    pub status: TeamStatus,
    /// The bot whose turn it is, while its reply is on the way.
    pub current: Option<String>,
    /// Index into `members` of who speaks next.
    pub next: usize,
    pub messages: Vec<TeamMsg>,
    /// For each bot, the id of the last message it has been sent (or said).
    #[serde(default)]
    pub seen: HashMap<String, String>,
    pub created: u64,
    pub updated: u64,
}

/// What the app sends to create a team.
#[derive(Clone, Debug, Deserialize)]
pub struct Draft {
    pub name: String,
    pub goal: String,
    pub members: Vec<String>,
    #[serde(default = "default_turns")]
    pub max_turns: u32,
    /// Start working straight away (the default).
    #[serde(default = "yes")]
    pub start: bool,
}

fn default_turns() -> u32 {
    12
}

fn yes() -> bool {
    true
}

/// A turn that has been sent to a bot and not finished yet.
struct Turn {
    team: String,
    session: String,
    started: bool,
    texts: Vec<String>,
}

pub struct Teams {
    list: std::sync::Mutex<Vec<Team>>,
    /// Keyed by the id of the message the team sent the bot.
    turns: std::sync::Mutex<HashMap<String, Turn>>,
}

fn path(h: &crate::home::Home) -> std::path::PathBuf {
    h.root.join("teams.json")
}

impl Teams {
    pub fn load(h: &crate::home::Home) -> Teams {
        let mut list: Vec<Team> = std::fs::read_to_string(path(h))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // A turn that was on its way when Fleet-51 stopped can't be followed any more.
        for t in &mut list {
            if t.status == TeamStatus::Running {
                t.status = TeamStatus::Stopped;
                t.current = None;
                push(
                    t,
                    "system",
                    "Fleet-51",
                    "Paused because Fleet-51 restarted. Press Continue to carry on.",
                );
            }
        }
        Teams {
            list: std::sync::Mutex::new(list),
            turns: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn list(&self) -> Vec<Team> {
        self.list.lock().unwrap().clone()
    }
}

fn push(t: &mut Team, from: &str, name: &str, text: &str) {
    t.messages.push(TeamMsg {
        id: short_id("m"),
        from: from.into(),
        name: name.into(),
        text: text.into(),
        at: now_ms(),
    });
    if t.messages.len() > KEEP_MESSAGES {
        let extra = t.messages.len() - KEEP_MESSAGES;
        t.messages.drain(..extra);
    }
    t.updated = now_ms();
}

fn save(d: &Daemon, list: &[Team]) {
    match serde_json::to_string_pretty(list) {
        Ok(t) => {
            if let Err(e) = crate::home::write_private(&path(&d.home), &t) {
                eprintln!("fleet51: could not save teams: {e:#}");
            }
        }
        Err(e) => eprintln!("fleet51: could not save teams: {e:#}"),
    }
    let _ = d.global.send(Live::Teams {
        teams: list.to_vec(),
    });
}

/// Change the list, save it and tell every window.
fn update<T>(d: &Daemon, f: impl FnOnce(&mut Vec<Team>) -> Result<T>) -> Result<T> {
    let mut list = d.teams.list.lock().unwrap();
    let out = f(&mut list)?;
    save(d, &list);
    Ok(out)
}

fn with_team<T>(d: &Daemon, id: &str, f: impl FnOnce(&mut Team) -> Result<T>) -> Result<T> {
    update(d, |list| {
        let t = list
            .iter_mut()
            .find(|t| t.id == id)
            .with_context(|| format!("no team {id}"))?;
        f(t)
    })
}

fn bot_name(d: &Daemon, id: &str) -> String {
    d.bot(id)
        .ok()
        .and_then(|s| s.state().profile.map(|p| p.name))
        .unwrap_or_else(|| "a deleted bot".into())
}

fn check_members(d: &Daemon, members: &[String]) -> Result<()> {
    if members.len() < MIN_MEMBERS || members.len() > MAX_MEMBERS {
        bail!("pick {MIN_MEMBERS} to {MAX_MEMBERS} bots for the team");
    }
    for (i, m) in members.iter().enumerate() {
        d.bot(m)?;
        if members[..i].contains(m) {
            bail!("each bot can only be in the team once");
        }
    }
    Ok(())
}

pub fn create(d: &Arc<Daemon>, draft: Draft) -> Result<Team> {
    let name = draft.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        bail!("give the team a name (up to 80 characters)");
    }
    if draft.goal.trim().is_empty() {
        bail!("tell the team what to work on");
    }
    if draft.max_turns == 0 || draft.max_turns > MAX_TURNS {
        bail!("pick between 1 and {MAX_TURNS} turns");
    }
    check_members(d, &draft.members)?;
    let t = Team {
        id: short_id("t"),
        name: name.into(),
        goal: draft.goal.trim().into(),
        members: draft.members,
        max_turns: draft.max_turns,
        turns: 0,
        status: TeamStatus::Idle,
        current: None,
        next: 0,
        messages: vec![],
        seen: HashMap::new(),
        created: now_ms(),
        updated: now_ms(),
    };
    update(d, |list| {
        list.push(t.clone());
        Ok(())
    })?;
    if draft.start {
        start(d, &t.id)?;
    }
    Ok(d.teams
        .list()
        .into_iter()
        .find(|x| x.id == t.id)
        .unwrap_or(t))
}

/// Start, or carry on with another `max_turns` turns.
pub fn start(d: &Arc<Daemon>, id: &str) -> Result<()> {
    with_team(d, id, |t| {
        t.members.retain(|m| d.bot(m).is_ok());
        if t.members.len() < MIN_MEMBERS {
            bail!("a team needs at least {MIN_MEMBERS} bots; some were deleted");
        }
        if t.status != TeamStatus::Running {
            t.status = TeamStatus::Running;
            t.turns = 0;
            t.updated = now_ms();
        }
        Ok(())
    })?;
    advance(d, id);
    Ok(())
}

/// Stop after the bot that is speaking finishes (its reply still lands in the team chat).
pub fn stop(d: &Daemon, id: &str) -> Result<()> {
    with_team(d, id, |t| {
        if t.status == TeamStatus::Running {
            t.status = TeamStatus::Stopped;
            push(
                t,
                "system",
                "Fleet-51",
                "Stopped. Press Continue to carry on.",
            );
        }
        Ok(())
    })
}

/// You join in. A team that isn't working starts again, so your message gets an answer.
pub fn say(d: &Arc<Daemon>, id: &str, text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        bail!("empty message");
    }
    with_team(d, id, |t| {
        push(t, "you", "You", text);
        Ok(())
    })?;
    start(d, id)
}

pub fn delete(d: &Daemon, id: &str) -> Result<()> {
    update(d, |list| {
        let before = list.len();
        list.retain(|t| t.id != id);
        if list.len() == before {
            bail!("no team {id}");
        }
        Ok(())
    })?;
    d.teams.turns.lock().unwrap().retain(|_, r| r.team != id);
    Ok(())
}

/// A deleted bot leaves its teams.
pub fn delete_for_bot(d: &Daemon, bot: &str) {
    let _ = update(d, |list| {
        for t in list.iter_mut() {
            if !t.members.iter().any(|m| m == bot) {
                continue;
            }
            let pos = t.members.iter().position(|m| m == bot).unwrap_or(0);
            t.members.retain(|m| m != bot);
            if t.next > pos {
                t.next -= 1;
            }
            if t.current.as_deref() == Some(bot) {
                t.current = None;
            }
            if t.members.len() < MIN_MEMBERS && t.status == TeamStatus::Running {
                t.status = TeamStatus::Stopped;
                push(
                    t,
                    "system",
                    "Fleet-51",
                    "Stopped: a team needs at least two bots.",
                );
            }
        }
        Ok(())
    });
}

/// What a bot is sent for its turn: who it is in the team, the goal, and what was said since
/// its last turn.
pub fn turn_prompt(t: &Team, me: &str, names: &HashMap<String, String>) -> String {
    let my_name = names.get(me).cloned().unwrap_or_default();
    let others: Vec<String> = t
        .members
        .iter()
        .filter(|m| *m != me)
        .map(|m| names.get(m).cloned().unwrap_or_default())
        .collect();
    let since = t
        .seen
        .get(me)
        .and_then(|last| t.messages.iter().position(|m| &m.id == last))
        .map(|i| i + 1)
        .unwrap_or(0);
    let new: Vec<String> = t.messages[since..]
        .iter()
        .filter(|m| m.from != me && m.from != "system")
        .map(|m| {
            let who = if m.from == "you" {
                "The user".to_string()
            } else {
                m.name.clone()
            };
            let text: String = m.text.chars().take(MAX_FORWARD).collect();
            format!("{who}: {text}")
        })
        .collect();
    let mut p = format!(
        "[Team \"{name}\": you are {my_name}, working together with {others} on a shared goal. \
         The user set this up and may not be watching, so don't wait for them: decide, do your \
         part and hand over. Build on what your teammates said, split up the work, and point out \
         mistakes. Your teammates can't see your files, so put anything they need in your reply. \
         Keep it focused. When the whole goal is achieved, end your reply with {DONE}.]\n\nGoal: {goal}",
        name = t.name,
        others = others.join(" and "),
        goal = t.goal,
    );
    if new.is_empty() {
        p.push_str("\n\nYou go first.");
    } else {
        p.push_str("\n\nSince your last turn:\n\n");
        p.push_str(&new.join("\n\n"));
    }
    p.push_str(&format!("\n\nYour turn, {my_name}."));
    p
}

/// Give the next bot its turn, if the team is working and nobody is speaking.
fn advance(d: &Arc<Daemon>, id: &str) {
    let mut announce = None;
    let r = with_team(d, id, |t| {
        if t.status != TeamStatus::Running || t.current.is_some() {
            return Ok(());
        }
        if t.turns >= t.max_turns {
            t.status = TeamStatus::Done;
            push(
                t,
                "system",
                "Fleet-51",
                &format!(
                    "Reached the limit of {} turns. Press Continue for {} more, or say something.",
                    t.max_turns, t.max_turns
                ),
            );
            announce = Some((
                format!("{} paused", t.name),
                "Reached its turn limit.".to_string(),
            ));
            return Ok(());
        }
        t.members.retain(|m| d.bot(m).is_ok());
        if t.members.len() < MIN_MEMBERS {
            t.status = TeamStatus::Stopped;
            push(
                t,
                "system",
                "Fleet-51",
                "Stopped: a team needs at least two bots.",
            );
            return Ok(());
        }
        let i = t.next % t.members.len();
        let me = t.members[i].clone();
        let s = d.bot(&me)?;
        let names: HashMap<String, String> = t
            .members
            .iter()
            .map(|m| (m.clone(), bot_name(d, m)))
            .collect();
        let text = turn_prompt(t, &me, &names);
        // Hold the list of turns while sending, so the bot's first events can't arrive first.
        let mut turns = d.teams.turns.lock().unwrap();
        let prompt = d.prompt(&s, &text, None, Some(&t.name), &[]);
        turns.insert(
            prompt,
            Turn {
                team: t.id.clone(),
                session: s.id.clone(),
                started: false,
                texts: vec![],
            },
        );
        if let Some(last) = t.messages.last() {
            t.seen.insert(me.clone(), last.id.clone());
        }
        t.next = i + 1;
        t.turns += 1;
        t.current = Some(me);
        t.updated = now_ms();
        Ok(())
    });
    if let Err(e) = r {
        let _ = with_team(d, id, |t| {
            t.status = TeamStatus::Stopped;
            t.current = None;
            push(t, "system", "Fleet-51", &format!("Stopped: {e}"));
            Ok(())
        });
    }
    if let Some((title, body)) = announce {
        notify(d, id, &title, &body);
    }
}

fn notify(d: &Daemon, team: &str, title: &str, body: &str) {
    let _ = d.global.send(Live::Notice {
        bot: team.into(),
        title: title.into(),
        body: body.into(),
    });
    crate::automations::system_notification(title, body);
}

/// A bot finished its turn: put its reply in the team chat and hand over to the next bot.
fn finish(d: &Arc<Daemon>, turn: Turn, ok: bool, summary: Option<String>) {
    // A bot is its session: the ids are the same.
    let bot = turn.session.clone();
    let name = bot_name(d, &bot);
    let text = turn.texts.join("\n\n");
    let mut done = None;
    let _ = with_team(d, &turn.team, |t| {
        if t.current.as_deref() == Some(bot.as_str()) {
            t.current = None;
        }
        if !ok {
            let why = summary.unwrap_or_else(|| "the reply failed".into());
            push(
                t,
                "system",
                "Fleet-51",
                &format!("{name} could not reply: {why}"),
            );
            if t.status == TeamStatus::Running {
                t.status = TeamStatus::Stopped;
                push(
                    t,
                    "system",
                    "Fleet-51",
                    "Stopped. Press Continue to try again.",
                );
            }
            return Ok(());
        }
        let text = if text.trim().is_empty() {
            "(no reply)".to_string()
        } else {
            text.clone()
        };
        push(t, &bot, &name, &text);
        t.seen.insert(
            bot.clone(),
            t.messages.last().map(|m| m.id.clone()).unwrap_or_default(),
        );
        if text.contains(DONE) && t.status == TeamStatus::Running {
            t.status = TeamStatus::Done;
            push(
                t,
                "system",
                "Fleet-51",
                &format!(
                    "{name} says the goal is done. Say something or press Continue to keep going."
                ),
            );
            let short: String = text
                .replace(DONE, "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            done = Some((
                format!("{} is done", t.name),
                short.chars().take(200).collect::<String>(),
            ));
        }
        Ok(())
    });
    if let Some((title, body)) = done {
        notify(d, &turn.team, &title, &body);
    }
    advance(d, &turn.team);
}

/// Follow team turns through the bots' events.
fn watch(d: &Arc<Daemon>, live: Live) {
    let Live::Event { session, event } = live else {
        return;
    };
    let mut turns = d.teams.turns.lock().unwrap();
    let key = || {
        turns
            .iter()
            .find(|(_, r)| r.started && r.session == session)
            .map(|(k, _)| k.clone())
    };
    match event.ev {
        Ev::PromptAcked { id } => {
            if let Some(r) = turns.get_mut(&id) {
                r.started = true;
            }
        }
        Ev::Text { text } => {
            if let Some(k) = key() {
                turns.get_mut(&k).expect("present").texts.push(text);
            }
        }
        Ev::TurnDone { ok, summary, .. } => {
            let Some(k) = key() else { return };
            let r = turns.remove(&k).expect("present");
            drop(turns);
            finish(d, r, ok, summary);
        }
        // The bot stopped (deleted, or its process ended) in the middle of its turn.
        Ev::Exited { .. } => {
            let gone: Vec<String> = turns
                .iter()
                .filter(|(_, r)| r.session == session)
                .map(|(k, _)| k.clone())
                .collect();
            let rs: Vec<Turn> = gone.iter().filter_map(|k| turns.remove(k)).collect();
            drop(turns);
            for r in rs {
                finish(d, r, false, Some("the bot stopped".into()));
            }
        }
        _ => {}
    }
}

pub fn start_watching(d: Arc<Daemon>) {
    let mut rx = d.global.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(live) => {
                    let d = d.clone();
                    // Starting the next turn touches files and locks; keep it off the async threads.
                    let _ = tokio::task::spawn_blocking(move || watch(&d, live)).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team() -> Team {
        Team {
            id: "t1".into(),
            name: "Launch".into(),
            goal: "Plan the launch".into(),
            members: vec!["a".into(), "b".into(), "c".into()],
            max_turns: 6,
            turns: 0,
            status: TeamStatus::Running,
            current: None,
            next: 0,
            messages: vec![],
            seen: HashMap::new(),
            created: 0,
            updated: 0,
        }
    }

    fn names() -> HashMap<String, String> {
        [("a", "Ada"), ("b", "Bo"), ("c", "Cy")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn each_bot_is_sent_only_what_others_said_since_its_last_turn() {
        let mut t = team();
        let p = turn_prompt(&t, "a", &names());
        assert!(
            p.contains("you are Ada, working together with Bo and Cy"),
            "{p}"
        );
        assert!(
            p.contains("Goal: Plan the launch") && p.contains("You go first."),
            "{p}"
        );
        assert!(p.contains(DONE));
        push(&mut t, "a", "Ada", "I'll draft the timeline.");
        t.seen.insert("a".into(), t.messages[0].id.clone());
        push(&mut t, "you", "You", "Keep it under a week.");
        push(&mut t, "system", "Fleet-51", "Stopped.");
        push(&mut t, "b", "Bo", "I'll write the announcement.");
        // Bo has never had a turn: it sees Ada and the user, not system notes or itself.
        let pb = turn_prompt(&t, "b", &names());
        assert!(pb.contains("Ada: I'll draft the timeline."), "{pb}");
        assert!(pb.contains("The user: Keep it under a week."), "{pb}");
        assert!(
            !pb.contains("Bo: I'll write") && !pb.contains("Stopped."),
            "{pb}"
        );
        // Ada only gets what came after her own message.
        let pa = turn_prompt(&t, "a", &names());
        assert!(!pa.contains("I'll draft the timeline."), "{pa}");
        assert!(
            pa.contains("Bo: I'll write the announcement.") && pa.contains("The user:"),
            "{pa}"
        );
        assert!(pa.ends_with("Your turn, Ada."));
    }

    #[test]
    fn long_messages_are_shortened_and_old_ones_drop_off() {
        let mut t = team();
        push(&mut t, "b", "Bo", &"x".repeat(MAX_FORWARD + 500));
        let p = turn_prompt(&t, "a", &names());
        assert!(p.len() < MAX_FORWARD + 1500);
        for i in 0..KEEP_MESSAGES + 10 {
            push(&mut t, "b", "Bo", &i.to_string());
        }
        assert_eq!(t.messages.len(), KEEP_MESSAGES);
    }
}
