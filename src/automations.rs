//! Automations: a bot runs saved instructions on a schedule (once, hourly, daily, weekdays,
//! weekly, monthly, annually or custom days and times), and you get a notification when it is done.
//! Times are the computer's local time. A run missed while the computer was off or asleep happens
//! when Fleet-51 is next running, if that is within MISSED_GRACE; otherwise it waits for the next time.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone, Weekday};
use serde::{Deserialize, Serialize};

use crate::daemon::Daemon;
use crate::events::{Ev, Live};
use crate::home::{now_ms, short_id};

const MISSED_GRACE_MS: u64 = 12 * 3600 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Once,
    Hourly,
    Daily,
    Weekdays,
    Weekly,
    Monthly,
    Annually,
    Custom,
}

/// `times` are "HH:MM". `date` ("YYYY-MM-DD") is the day for Once, the day of the month for
/// Monthly and the month and day for Annually. `days` (0 = Sunday) are for Weekly and Custom.
/// Hourly runs at the minute of `times[0]` in every hour from `from_hour` to `to_hour`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Schedule {
    pub repeat: Repeat,
    #[serde(default)]
    pub times: Vec<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub days: Vec<u8>,
    #[serde(default)]
    pub from_hour: Option<u32>,
    #[serde(default)]
    pub to_hour: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Automation {
    pub id: String,
    pub bot: String,
    pub name: String,
    pub instructions: String,
    pub schedule: Schedule,
    pub notify: bool,
    pub enabled: bool,
    pub created: u64,
    pub next_run: Option<u64>,
    pub last_run: Option<u64>,
    /// "running", "done" or "failed".
    pub last_status: Option<String>,
    pub last_result: Option<String>,
    /// Finished runs, newest first.
    #[serde(default)]
    pub history: Vec<RunRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub at: u64,
    pub ok: bool,
    pub result: String,
}

const KEEP_RUNS: usize = 50;

fn record(a: &mut Automation, ok: bool, result: &str) {
    a.history.insert(
        0,
        RunRecord {
            id: short_id("r"),
            at: a.last_run.unwrap_or_else(now_ms),
            ok,
            result: result.chars().take(4000).collect(),
        },
    );
    a.history.truncate(KEEP_RUNS);
}

/// Delete one run from an automation's history, or all of it.
pub fn delete_runs(d: &Daemon, id: &str, run: Option<&str>) -> Result<()> {
    update(d, |list| {
        let a = list
            .iter_mut()
            .find(|a| a.id == id)
            .with_context(|| format!("no automation {id}"))?;
        match run {
            Some(r) => a.history.retain(|x| x.id != r),
            None => a.history.clear(),
        }
        Ok(())
    })
}

/// What the app sends to create or change an automation.
#[derive(Clone, Debug, Deserialize)]
pub struct Draft {
    pub bot: String,
    pub name: String,
    pub instructions: String,
    pub schedule: Schedule,
    #[serde(default = "yes")]
    pub notify: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

fn parse_time(t: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(t.trim(), "%H:%M").with_context(|| format!("not a time: {t}"))
}

fn parse_date(d: &Option<String>) -> Result<NaiveDate> {
    let d = d.as_deref().context("pick a date")?;
    NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").with_context(|| format!("not a date: {d}"))
}

impl Schedule {
    fn check(&self) -> Result<()> {
        if self.times.is_empty() {
            bail!("pick a time");
        }
        for t in &self.times {
            parse_time(t)?;
        }
        if matches!(
            self.repeat,
            Repeat::Once | Repeat::Monthly | Repeat::Annually
        ) {
            parse_date(&self.date)?;
        }
        if matches!(self.repeat, Repeat::Weekly | Repeat::Custom) {
            if self.days.is_empty() {
                bail!("pick at least one day");
            }
            if self.days.iter().any(|d| *d > 6) {
                bail!("days are 0 (Sunday) to 6 (Saturday)");
            }
        }
        let (from, to) = self.hours();
        if from > 23 || to > 23 || from > to {
            bail!("pick hours between 0 and 23, the first no later than the second");
        }
        Ok(())
    }

    fn hours(&self) -> (u32, u32) {
        (self.from_hour.unwrap_or(9), self.to_hour.unwrap_or(17))
    }

    fn runs_on(&self, day: NaiveDate) -> bool {
        let wd = day.weekday().num_days_from_sunday() as u8;
        let date = parse_date(&self.date).ok();
        match self.repeat {
            Repeat::Once => date == Some(day),
            Repeat::Hourly | Repeat::Daily => true,
            Repeat::Weekdays => !matches!(day.weekday(), Weekday::Sat | Weekday::Sun),
            Repeat::Weekly | Repeat::Custom => self.days.contains(&wd),
            // A monthly run on the 31st happens on the last day of shorter months.
            Repeat::Monthly => date.is_some_and(|d| {
                let last = last_day_of_month(day);
                day.day() == d.day().min(last)
            }),
            Repeat::Annually => date.is_some_and(|d| {
                d.month() == day.month()
                    && (d.day() == day.day()
                        || (d.month() == 2 && d.day() == 29 && day.day() == last_day_of_month(day)))
            }),
        }
    }

    fn times_of_day(&self) -> Vec<NaiveTime> {
        let mut v: Vec<NaiveTime> = match self.repeat {
            Repeat::Hourly => {
                let minute = self
                    .times
                    .first()
                    .and_then(|t| parse_time(t).ok())
                    .map(|t| t.format("%M").to_string().parse().unwrap_or(0))
                    .unwrap_or(0);
                let (from, to) = self.hours();
                (from..=to)
                    .filter_map(|h| NaiveTime::from_hms_opt(h, minute, 0))
                    .collect()
            }
            Repeat::Custom => self
                .times
                .iter()
                .filter_map(|t| parse_time(t).ok())
                .collect(),
            _ => self
                .times
                .first()
                .and_then(|t| parse_time(t).ok())
                .into_iter()
                .collect(),
        };
        v.sort();
        v.dedup();
        v
    }

    /// The first run strictly after `after`, looking up to about four years ahead (for 29 February).
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let times = self.times_of_day();
        let mut day = after.date_naive();
        for _ in 0..1500 {
            if self.runs_on(day) {
                for t in &times {
                    if let Some(at) = Local.from_local_datetime(&day.and_time(*t)).earliest() {
                        if at > after {
                            return Some(at);
                        }
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }
}

fn last_day_of_month(day: NaiveDate) -> u32 {
    let (y, m) = (day.year(), day.month());
    let first_next = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1)
    };
    first_next
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

fn at_ms(t: DateTime<Local>) -> u64 {
    t.timestamp_millis().max(0) as u64
}

fn local(ms: u64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .unwrap_or_else(Local::now)
}

/// A run that has been sent to its bot and not finished yet.
struct Run {
    automation: String,
    session: String,
    started: bool,
    last_text: Option<String>,
}

pub struct Automations {
    list: std::sync::Mutex<Vec<Automation>>,
    /// Keyed by the id of the message the automation sent.
    runs: std::sync::Mutex<HashMap<String, Run>>,
}

impl Automations {
    pub fn load(d: &crate::home::Home) -> Automations {
        let list = std::fs::read_to_string(d.root.join("automations.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Automations {
            list: std::sync::Mutex::new(list),
            runs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn list(&self) -> Vec<Automation> {
        self.list.lock().unwrap().clone()
    }
}

fn save(d: &Daemon, list: &[Automation]) {
    let path = d.home.root.join("automations.json");
    match serde_json::to_string_pretty(list) {
        Ok(t) => {
            if let Err(e) = crate::home::write_private(&path, &t) {
                eprintln!("fleet51: could not save automations: {e:#}");
            }
        }
        Err(e) => eprintln!("fleet51: could not save automations: {e:#}"),
    }
    let _ = d.global.send(Live::Automations {
        automations: list.to_vec(),
    });
}

/// Change the list, save it and tell every window.
fn update<T>(d: &Daemon, f: impl FnOnce(&mut Vec<Automation>) -> Result<T>) -> Result<T> {
    let mut list = d.automations.list.lock().unwrap();
    let out = f(&mut list)?;
    save(d, &list);
    Ok(out)
}

fn validate(d: &Daemon, draft: &Draft) -> Result<()> {
    d.bot(&draft.bot)?;
    if draft.name.trim().is_empty() || draft.name.chars().count() > 80 {
        bail!("give the automation a name (up to 80 characters)");
    }
    if draft.instructions.trim().is_empty() {
        bail!("tell the bot what to do");
    }
    draft.schedule.check()
}

fn next_run(s: &Schedule, enabled: bool) -> Result<Option<u64>> {
    if !enabled {
        return Ok(None);
    }
    match s.next_after(Local::now()) {
        Some(t) => Ok(Some(at_ms(t))),
        None if s.repeat == Repeat::Once => bail!("that time has already passed"),
        None => bail!("this schedule never runs"),
    }
}

pub fn create(d: &Daemon, draft: Draft) -> Result<Automation> {
    validate(d, &draft)?;
    let a = Automation {
        id: short_id("a"),
        bot: draft.bot,
        name: draft.name.trim().into(),
        instructions: draft.instructions.trim().into(),
        next_run: next_run(&draft.schedule, draft.enabled)?,
        schedule: draft.schedule,
        notify: draft.notify,
        enabled: draft.enabled,
        created: now_ms(),
        last_run: None,
        last_status: None,
        last_result: None,
        history: vec![],
    };
    update(d, |list| {
        list.push(a.clone());
        Ok(a)
    })
}

pub fn edit(d: &Daemon, id: &str, draft: Draft) -> Result<Automation> {
    validate(d, &draft)?;
    let next = next_run(&draft.schedule, draft.enabled)?;
    update(d, |list| {
        let a = list
            .iter_mut()
            .find(|a| a.id == id)
            .with_context(|| format!("no automation {id}"))?;
        a.bot = draft.bot;
        a.name = draft.name.trim().into();
        a.instructions = draft.instructions.trim().into();
        a.schedule = draft.schedule;
        a.notify = draft.notify;
        a.enabled = draft.enabled;
        a.next_run = next;
        Ok(a.clone())
    })
}

pub fn delete(d: &Daemon, id: &str) -> Result<()> {
    update(d, |list| {
        let before = list.len();
        list.retain(|a| a.id != id);
        if list.len() == before {
            bail!("no automation {id}");
        }
        Ok(())
    })
}

/// A deleted bot takes its automations with it.
pub fn delete_for_bot(d: &Daemon, bot: &str) {
    let _ = update(d, |list| {
        list.retain(|a| a.bot != bot);
        Ok(())
    });
}

/// Send the automation's instructions to its bot now.
pub fn run(d: &Arc<Daemon>, id: &str) -> Result<()> {
    let a = d
        .automations
        .list()
        .into_iter()
        .find(|a| a.id == id)
        .with_context(|| format!("no automation {id}"))?;
    let s = d.bot(&a.bot)?;
    let prompt = d.prompt(&s, &a.instructions, Some(&a.name), &[]);
    d.automations.runs.lock().unwrap().insert(
        prompt,
        Run {
            automation: a.id.clone(),
            session: s.id.clone(),
            started: false,
            last_text: None,
        },
    );
    update(d, |list| {
        if let Some(a) = list.iter_mut().find(|x| x.id == id) {
            a.last_run = Some(now_ms());
            a.last_status = Some("running".into());
        }
        Ok(())
    })
}

/// Run whatever is due. Called every few seconds.
fn tick(d: &Arc<Daemon>) {
    let now = now_ms();
    let due: Vec<Automation> = d
        .automations
        .list()
        .into_iter()
        .filter(|a| a.enabled && a.next_run.is_some_and(|t| t <= now))
        .collect();
    for a in due {
        let missed = a.next_run.is_some_and(|t| now - t > MISSED_GRACE_MS);
        let result = if missed {
            Ok(())
        } else if d.bot(&a.bot).is_err() {
            Err(anyhow::anyhow!("its bot was deleted"))
        } else {
            run(d, &a.id)
        };
        let _ = update(d, |list| {
            if let Some(x) = list.iter_mut().find(|x| x.id == a.id) {
                x.next_run = x.schedule.next_after(local(now)).map(at_ms);
                if x.schedule.repeat == Repeat::Once {
                    x.enabled = false;
                    x.next_run = None;
                }
                if let Err(e) = &result {
                    x.last_status = Some("failed".into());
                    x.last_result = Some(e.to_string());
                    record(x, false, &e.to_string());
                }
            }
            Ok(())
        });
    }
}

/// Follow automation runs through their bots' events, and record and announce the result.
fn watch(d: &Arc<Daemon>, live: Live) {
    let Live::Event { session, event } = live else {
        return;
    };
    let mut runs = d.automations.runs.lock().unwrap();
    match event.ev {
        Ev::PromptAcked { id } => {
            if let Some(r) = runs.get_mut(&id) {
                r.started = true;
            }
        }
        Ev::Text { text } => {
            if let Some(r) = runs
                .values_mut()
                .find(|r| r.started && r.session == session)
            {
                r.last_text = Some(text);
            }
        }
        Ev::TurnDone { ok, summary, .. } => {
            let Some(key) = runs
                .iter()
                .find(|(_, r)| r.started && r.session == session)
                .map(|(k, _)| k.clone())
            else {
                return;
            };
            let r = runs.remove(&key).expect("present");
            drop(runs);
            let result = if ok {
                r.last_text.unwrap_or_else(|| "Done.".into())
            } else {
                summary.unwrap_or_else(|| "The run failed.".into())
            };
            let short: String = result.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = short.chars().take(200).collect();
            let mut notify = None;
            let _ = update(d, |list| {
                if let Some(a) = list.iter_mut().find(|a| a.id == r.automation) {
                    a.last_status = Some(if ok { "done" } else { "failed" }.into());
                    a.last_result = Some(short.clone());
                    record(a, ok, &result);
                    if a.notify {
                        notify = Some(a.clone());
                    }
                }
                Ok(())
            });
            if let Some(a) = notify {
                let bot = d
                    .bot(&a.bot)
                    .ok()
                    .and_then(|s| s.state().profile.map(|p| p.name))
                    .unwrap_or_default();
                let title = if ok {
                    format!("{bot} finished {}", a.name)
                } else {
                    format!("{bot} could not finish {}", a.name)
                };
                let _ = d.global.send(Live::Notice {
                    bot: a.bot.clone(),
                    title: title.clone(),
                    body: short.clone(),
                });
                system_notification(&title, &short);
            }
        }
        _ => {}
    }
}

/// Show a notification from the computer itself, so it appears even with no window open.
pub fn system_notification(title: &str, body: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        // The text is passed as arguments, never pasted into the script.
        let mut c = std::process::Command::new("osascript");
        c.args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv) sound name \"Glass\"",
            "-e",
            "end run",
            title,
            body,
        ]);
        c
    } else {
        let mut c = std::process::Command::new("notify-send");
        c.args(["--app-name=Fleet-51", title, body]);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

pub fn start(d: Arc<Daemon>) {
    let mut rx = d.global.subscribe();
    let d2 = d.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(live) => watch(&d2, live),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    tokio::spawn(async move {
        loop {
            let d2 = d.clone();
            let _ = tokio::task::spawn_blocking(move || tick(&d2)).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched(repeat: Repeat, times: &[&str]) -> Schedule {
        Schedule {
            repeat,
            times: times.iter().map(|t| t.to_string()).collect(),
            date: None,
            days: vec![],
            from_hour: None,
            to_hour: None,
        }
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(y, m, d)
                    .unwrap()
                    .and_hms_opt(h, min, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap()
    }

    #[test]
    fn daily_weekdays_and_once() {
        // 2026-10-16 is a Friday.
        let daily = sched(Repeat::Daily, &["19:19"]);
        assert_eq!(
            daily.next_after(at(2026, 10, 16, 8, 0)),
            Some(at(2026, 10, 16, 19, 19))
        );
        assert_eq!(
            daily.next_after(at(2026, 10, 16, 19, 19)),
            Some(at(2026, 10, 17, 19, 19))
        );
        let weekdays = sched(Repeat::Weekdays, &["08:30"]);
        assert_eq!(
            weekdays.next_after(at(2026, 10, 16, 9, 0)),
            Some(at(2026, 10, 19, 8, 30))
        );
        let mut once = sched(Repeat::Once, &["10:00"]);
        once.date = Some("2026-10-20".into());
        assert_eq!(
            once.next_after(at(2026, 10, 16, 9, 0)),
            Some(at(2026, 10, 20, 10, 0))
        );
        assert_eq!(once.next_after(at(2026, 10, 21, 9, 0)), None);
    }

    #[test]
    fn hourly_custom_monthly_annually() {
        let mut hourly = sched(Repeat::Hourly, &["00:15"]);
        hourly.from_hour = Some(9);
        hourly.to_hour = Some(11);
        assert_eq!(
            hourly.next_after(at(2026, 10, 16, 9, 20)),
            Some(at(2026, 10, 16, 10, 15))
        );
        assert_eq!(
            hourly.next_after(at(2026, 10, 16, 11, 20)),
            Some(at(2026, 10, 17, 9, 15))
        );
        let mut custom = sched(Repeat::Custom, &["18:00", "07:45"]);
        custom.days = vec![2, 4]; // Tuesday and Thursday
        assert_eq!(
            custom.next_after(at(2026, 10, 16, 9, 0)),
            Some(at(2026, 10, 20, 7, 45))
        );
        assert_eq!(
            custom.next_after(at(2026, 10, 20, 8, 0)),
            Some(at(2026, 10, 20, 18, 0))
        );
        let mut monthly = sched(Repeat::Monthly, &["09:00"]);
        monthly.date = Some("2026-01-31".into());
        assert_eq!(
            monthly.next_after(at(2026, 11, 1, 0, 0)),
            Some(at(2026, 11, 30, 9, 0))
        );
        let mut yearly = sched(Repeat::Annually, &["12:00"]);
        yearly.date = Some("2024-02-29".into());
        assert_eq!(
            yearly.next_after(at(2026, 10, 16, 0, 0)),
            Some(at(2027, 2, 28, 12, 0))
        );
    }

    #[test]
    fn bad_schedules_are_refused() {
        assert!(sched(Repeat::Daily, &[]).check().is_err());
        assert!(sched(Repeat::Daily, &["25:00"]).check().is_err());
        assert!(sched(Repeat::Weekly, &["09:00"]).check().is_err());
        assert!(sched(Repeat::Once, &["09:00"]).check().is_err());
        assert!(sched(Repeat::Daily, &["09:00"]).check().is_ok());
    }
}
