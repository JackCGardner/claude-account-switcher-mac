//! `claude account watch WORKSPACE`: rotate a workspace's selected member
//! before a usage window hits its limit, so new and resumed launches land on
//! the subscription with headroom. Decision logic ported from claude-swap
//! (MIT, https://github.com/realiti4/claude-swap) onto this tool's model:
//! rotation changes which member *new* launches resolve to — running sessions
//! keep the login they started with and pick the new one up on
//! `claude --resume`/`-c`.

use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;

use crate::paths::AppPaths;
use crate::state::{self, StateLock, WatchSettings};
use crate::usage::{self, Window};
use crate::workspace;

/// A candidate must sit at least this far below the threshold, so two
/// accounts hovering at the line cannot flap.
pub const HYSTERESIS_PCT: f64 = 10.0;
/// Minimum seconds between automatic rotations (ignored once the selected
/// member is hard-limited at 100%).
pub const ROTATION_COOLDOWN_SECS: i64 = 600;
const HARD_LIMIT_PCT: f64 = 100.0;
/// Poll faster when the selected member is within this margin of the
/// threshold.
const URGENT_MARGIN_PCT: f64 = 15.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Strategy {
    /// Prefer the member whose weekly window resets soonest (no quota wasted)
    ConsumeFirst,
    /// Prefer the member with the most headroom
    Best,
    /// First member below the threshold, in name order
    NextAvailable,
}

impl Strategy {
    pub fn canonical(self) -> &'static str {
        match self {
            Strategy::ConsumeFirst => "consume-first",
            Strategy::Best => "best",
            Strategy::NextAvailable => "next-available",
        }
    }

    pub fn parse(value: &str) -> Strategy {
        match value {
            "best" => Strategy::Best,
            "next-available" => Strategy::NextAvailable,
            _ => Strategy::ConsumeFirst,
        }
    }
}

/// Which per-model weekly windows gate the decision, next to the always-on
/// 5-hour and 7-day windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelFilter {
    All,
    None,
    Named(Vec<String>),
}

impl ModelFilter {
    pub fn parse(value: &str) -> ModelFilter {
        match value.trim().to_lowercase().as_str() {
            "all" | "" => ModelFilter::All,
            "none" => ModelFilter::None,
            list => ModelFilter::Named(
                list.split(',')
                    .map(|name| name.trim().to_lowercase())
                    .filter(|name| !name.is_empty())
                    .collect(),
            ),
        }
    }
}

fn window_gates(window: &Window, filter: &ModelFilter) -> bool {
    match window.label.as_str() {
        "5h" | "7d" => true,
        scoped => match filter {
            ModelFilter::All => true,
            ModelFilter::None => false,
            ModelFilter::Named(names) => {
                let scoped = scoped.to_lowercase();
                names.iter().any(|name| scoped.starts_with(name.as_str()))
            }
        },
    }
}

/// The highest utilization among the windows that gate this member.
pub fn max_gate(windows: &[Window], filter: &ModelFilter) -> Option<f64> {
    windows
        .iter()
        .filter(|window| window_gates(window, filter))
        .map(|window| window.pct)
        .fold(None, |best, pct| {
            Some(best.map_or(pct, |best: f64| best.max(pct)))
        })
}

fn weekly_reset_unix(windows: &[Window]) -> i64 {
    windows
        .iter()
        .find(|window| window.label == "7d")
        .and_then(|window| window.resets_at_unix)
        .unwrap_or(i64::MAX)
}

#[derive(Debug, Clone)]
pub struct MemberSnapshot {
    pub name: String,
    pub windows: Vec<Window>,
    pub known: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Stay { reason: String },
    Switch { to: String, reason: String },
    AllSaturated,
}

pub fn decide(
    selected: &str,
    members: &[MemberSnapshot],
    settings_threshold: f64,
    strategy: Strategy,
    filter: &ModelFilter,
    last_rotated_at: Option<i64>,
    now: i64,
) -> Decision {
    let current_gate = members
        .iter()
        .find(|member| member.name == selected)
        .filter(|member| member.known)
        .and_then(|member| max_gate(&member.windows, filter));
    let Some(current_gate) = current_gate else {
        return Decision::Stay {
            reason: format!("no usage data for selected `{selected}` yet"),
        };
    };
    if current_gate < settings_threshold {
        return Decision::Stay {
            reason: format!(
                "`{selected}` at {current_gate:.0}% (rotates at {settings_threshold:.0}%)"
            ),
        };
    }
    if current_gate < HARD_LIMIT_PCT {
        if let Some(last) = last_rotated_at {
            let since = now - last;
            if since < ROTATION_COOLDOWN_SECS {
                return Decision::Stay {
                    reason: format!(
                        "`{selected}` at {current_gate:.0}% but rotated {}s ago (cooldown {}s)",
                        since, ROTATION_COOLDOWN_SECS
                    ),
                };
            }
        }
    }

    let mut candidates: Vec<(&MemberSnapshot, f64)> = members
        .iter()
        .filter(|member| member.name != selected && member.known)
        .filter_map(|member| max_gate(&member.windows, filter).map(|gate| (member, gate)))
        .filter(|(_, gate)| *gate <= settings_threshold - HYSTERESIS_PCT)
        .collect();
    if candidates.is_empty() {
        return Decision::AllSaturated;
    }
    candidates.sort_by(|a, b| match strategy {
        Strategy::Best => a.1.total_cmp(&b.1),
        Strategy::NextAvailable => a.0.name.cmp(&b.0.name),
        Strategy::ConsumeFirst => weekly_reset_unix(&a.0.windows)
            .cmp(&weekly_reset_unix(&b.0.windows))
            .then(a.1.total_cmp(&b.1)),
    });
    let (chosen, chosen_gate) = &candidates[0];
    Decision::Switch {
        to: chosen.name.clone(),
        reason: format!(
            "`{selected}` reached {current_gate:.0}% (threshold {settings_threshold:.0}%); \
             `{}` has {chosen_gate:.0}%",
            chosen.name
        ),
    }
}

/// Best-effort desktop notification; falls back to stdout silently.
fn notify(summary: &str) {
    if cfg!(test) {
        return;
    }
    let sent = if cfg!(target_os = "macos") {
        Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!(
                "display notification {} with title \"claude-account\"",
                applescript_string(summary)
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    } else {
        Command::new("notify-send")
            .args(["claude-account", summary])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    let _ = sent;
}

fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Remember the freshest usage observation per member, so an idle member
/// keeps known (decaying) numbers even though the shared `.claude.json` only
/// carries the last-active login's snapshot.
fn persist_observations(paths: &AppPaths, members: &[usage::MemberUsage]) {
    let observed: Vec<&usage::MemberUsage> = members
        .iter()
        .filter(|member| member.fetched_at_unix.is_some() && !member.windows.is_empty())
        .collect();
    if observed.is_empty() {
        return;
    }
    let result = (|| -> Result<()> {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        let mut changed = false;
        for member in observed {
            let fetched = member.fetched_at_unix.expect("filtered above");
            let Some(profile) = state.profiles.get_mut(&member.profile) else {
                continue;
            };
            let known_fetched = profile
                .last_usage
                .as_ref()
                .and_then(usage::stored_usage_from_value)
                .map(|(stored_fetched, _)| stored_fetched);
            if known_fetched.is_none_or(|stored| fetched > stored) {
                profile.last_usage = Some(usage::stored_usage_to_value(fetched, &member.windows));
                changed = true;
            }
        }
        if changed {
            state::save(paths, &state)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("warning: could not record usage observations: {error:#}");
    }
}

fn rotate(paths: &AppPaths, workspace_name: &str, to: &str, now: i64) -> Result<()> {
    let _lock = StateLock::acquire(paths)?;
    let mut state = state::load(paths)?;
    if !state.profiles.contains_key(to) {
        bail!("member `{to}` no longer exists");
    }
    if !state.workspaces.contains_key(workspace_name) {
        bail!("workspace `{workspace_name}` no longer exists");
    }
    workspace::select_member(&mut state, workspace_name, to);
    state
        .workspaces
        .get_mut(workspace_name)
        .expect("workspace existence checked above")
        .last_rotated_at = Some(now.max(0) as u64);
    state::save(paths, &state)?;
    Ok(())
}

struct TickOutcome {
    line: String,
    rotated: bool,
    sleep_secs: u64,
}

fn tick(paths: &AppPaths, workspace_name: &str, settings: &WatchSettings) -> Result<TickOutcome> {
    let state = state::load(paths)?;
    let workspace = state
        .workspaces
        .get(workspace_name)
        .with_context(|| format!("workspace `{workspace_name}` no longer exists"))?;
    let selected = workspace.selected.clone().with_context(|| {
        format!(
            "workspace `{workspace_name}` has no selected member; run `claude account use MEMBER`"
        )
    })?;
    let now = usage::unix_now();
    let members = usage::workspace_member_usage(paths, &state, workspace_name, settings.live, now);
    persist_observations(paths, &members);
    let snapshots: Vec<MemberSnapshot> = members
        .iter()
        .map(|member| MemberSnapshot {
            name: member.profile.clone(),
            known: !member.windows.is_empty(),
            windows: member.windows.clone(),
        })
        .collect();
    let filter = ModelFilter::parse(&settings.models);
    let strategy = Strategy::parse(&settings.strategy);
    let decision = decide(
        &selected,
        &snapshots,
        settings.threshold,
        strategy,
        &filter,
        workspace.last_rotated_at.map(|value| value as i64),
        now,
    );

    let current_gate = snapshots
        .iter()
        .find(|member| member.name == selected)
        .and_then(|member| max_gate(&member.windows, &filter));
    let base = settings.interval_secs.max(15);
    let mut sleep_secs = match current_gate {
        Some(gate) if gate >= settings.threshold - URGENT_MARGIN_PCT => (base / 2).max(20),
        Some(_) => base,
        None => (base * 5).min(600),
    };

    match decision {
        Decision::Stay { reason } => Ok(TickOutcome {
            line: format!("watch {workspace_name}: {reason}"),
            rotated: false,
            sleep_secs,
        }),
        Decision::AllSaturated => {
            sleep_secs = (base * 5).min(600);
            Ok(TickOutcome {
                line: format!(
                    "watch {workspace_name}: every member is above {:.0}% — staying on \
                     `{selected}` until a window resets",
                    settings.threshold - HYSTERESIS_PCT
                ),
                rotated: false,
                sleep_secs,
            })
        }
        Decision::Switch { to, reason } => {
            rotate(paths, workspace_name, &to, now)?;
            let summary = format!("workspace {workspace_name}: switched to `{to}` — {reason}");
            notify(&format!(
                "Switched {workspace_name} to {to}. Resume sessions with claude --resume."
            ));
            Ok(TickOutcome {
                line: format!(
                    "watch {summary}\n  new sessions use `{to}`; resume running ones with \
                     `claude --resume` / `claude -c`"
                ),
                rotated: true,
                sleep_secs,
            })
        }
    }
}

/// Merge CLI overrides into the workspace's persisted watch settings.
#[allow(clippy::too_many_arguments)]
pub fn command_watch(
    paths: &AppPaths,
    workspace_name: &str,
    threshold: Option<f64>,
    strategy: Option<Strategy>,
    models: Option<String>,
    live: bool,
    cached: bool,
    interval: Option<u64>,
    once: bool,
) -> Result<()> {
    let settings = {
        let _lock = StateLock::acquire(paths)?;
        let mut state = state::load(paths)?;
        let workspace = state
            .workspaces
            .get_mut(workspace_name)
            .with_context(|| format!("workspace `{workspace_name}` does not exist"))?;
        let mut settings = workspace.watch.clone().unwrap_or_default();
        if let Some(threshold) = threshold {
            if !(1.0..=100.0).contains(&threshold) {
                bail!("--threshold must be between 1 and 100");
            }
            settings.threshold = threshold;
        }
        if let Some(strategy) = strategy {
            settings.strategy = strategy.canonical().to_owned();
        }
        if let Some(models) = models {
            settings.models = models;
        }
        if live {
            settings.live = true;
        }
        if cached {
            settings.live = false;
        }
        if let Some(interval) = interval {
            settings.interval_secs = interval.max(15);
        }
        workspace.watch = Some(settings.clone());
        state::save(paths, &state)?;
        settings
    };

    println!(
        "Watching workspace `{workspace_name}`: threshold {:.0}%, strategy {}, models {}, {} \
         data, every ~{}s.",
        settings.threshold,
        settings.strategy,
        settings.models,
        if settings.live { "live" } else { "cached" },
        settings.interval_secs.max(15)
    );
    if !settings.live {
        println!(
            "(cached mode: the selected member refreshes while its sessions run; enable --live \
             for idle members' exact numbers)"
        );
    }

    loop {
        let outcome = tick(paths, workspace_name, &settings)?;
        println!("{}", outcome.line);
        if once {
            if outcome.rotated {
                // Give the notification a moment on one-shot runs.
                thread::sleep(Duration::from_millis(100));
            }
            return Ok(());
        }
        thread::sleep(Duration::from_secs(outcome.sleep_secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(label: &str, pct: f64, resets_at_unix: Option<i64>) -> Window {
        Window {
            label: label.to_owned(),
            pct,
            resets_at_unix,
            reset_since: false,
        }
    }

    fn member(name: &str, windows: Vec<Window>) -> MemberSnapshot {
        MemberSnapshot {
            name: name.to_owned(),
            known: !windows.is_empty(),
            windows,
        }
    }

    #[test]
    fn stays_below_the_threshold() {
        let members = vec![
            member(
                "a",
                vec![window("5h", 50.0, None), window("7d", 80.0, None)],
            ),
            member("b", vec![window("7d", 10.0, None)]),
        ];
        let decision = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::All,
            None,
            1_000,
        );
        assert!(matches!(decision, Decision::Stay { .. }), "{decision:?}");
    }

    #[test]
    fn scoped_model_windows_gate_under_all_and_named_filters() {
        let members = vec![
            member(
                "a",
                vec![window("5h", 10.0, None), window("Fable 7d", 95.0, None)],
            ),
            member("b", vec![window("7d", 20.0, None)]),
        ];
        let all = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::All,
            None,
            1_000,
        );
        assert!(matches!(all, Decision::Switch { .. }), "{all:?}");

        let named = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::parse("fable,opus"),
            None,
            1_000,
        );
        assert!(matches!(named, Decision::Switch { .. }), "{named:?}");

        let none = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::None,
            None,
            1_000,
        );
        assert!(
            matches!(none, Decision::Stay { .. }),
            "scoped windows must not gate under `none`: {none:?}"
        );
    }

    #[test]
    fn consume_first_prefers_the_soonest_weekly_reset() {
        let members = vec![
            member("a", vec![window("7d", 95.0, Some(5_000))]),
            member("b", vec![window("7d", 40.0, Some(9_000))]),
            member("c", vec![window("7d", 50.0, Some(2_000))]),
        ];
        let decision = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::All,
            None,
            1_000,
        );
        match decision {
            Decision::Switch { to, .. } => assert_eq!(to, "c"),
            other => panic!("expected switch, got {other:?}"),
        }
    }

    #[test]
    fn best_prefers_the_most_headroom() {
        let members = vec![
            member("a", vec![window("7d", 95.0, Some(5_000))]),
            member("b", vec![window("7d", 40.0, Some(9_000))]),
            member("c", vec![window("7d", 50.0, Some(2_000))]),
        ];
        let decision = decide(
            "a",
            &members,
            90.0,
            Strategy::Best,
            &ModelFilter::All,
            None,
            1_000,
        );
        match decision {
            Decision::Switch { to, .. } => assert_eq!(to, "b"),
            other => panic!("expected switch, got {other:?}"),
        }
    }

    #[test]
    fn hysteresis_disqualifies_borderline_candidates() {
        let members = vec![
            member("a", vec![window("7d", 92.0, None)]),
            member("b", vec![window("7d", 85.0, None)]), // above 90 - 10
        ];
        let decision = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::All,
            None,
            1_000,
        );
        assert_eq!(decision, Decision::AllSaturated);
    }

    #[test]
    fn cooldown_blocks_rotation_unless_hard_limited() {
        let members = vec![
            member("a", vec![window("7d", 92.0, None)]),
            member("b", vec![window("7d", 10.0, None)]),
        ];
        let blocked = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::All,
            Some(900),
            1_000,
        );
        assert!(matches!(blocked, Decision::Stay { .. }), "{blocked:?}");

        let hard_limited = vec![
            member("a", vec![window("7d", 100.0, None)]),
            member("b", vec![window("7d", 10.0, None)]),
        ];
        let overridden = decide(
            "a",
            &hard_limited,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::All,
            Some(900),
            1_000,
        );
        assert!(
            matches!(overridden, Decision::Switch { .. }),
            "a hard limit must override the cooldown: {overridden:?}"
        );
    }

    #[test]
    fn unknown_selected_usage_stays_put() {
        let members = vec![
            member("a", vec![]),
            member("b", vec![window("7d", 10.0, None)]),
        ];
        let decision = decide(
            "a",
            &members,
            90.0,
            Strategy::ConsumeFirst,
            &ModelFilter::All,
            None,
            1_000,
        );
        assert!(matches!(decision, Decision::Stay { .. }), "{decision:?}");
    }

    #[test]
    fn model_filters_parse() {
        assert_eq!(ModelFilter::parse("all"), ModelFilter::All);
        assert_eq!(ModelFilter::parse(""), ModelFilter::All);
        assert_eq!(ModelFilter::parse("none"), ModelFilter::None);
        assert_eq!(
            ModelFilter::parse("Fable, Opus"),
            ModelFilter::Named(vec!["fable".to_owned(), "opus".to_owned()])
        );
    }
}
