//! `claude account dashboard`: a full-screen, auto-refreshing comparison of
//! every login — workspaces grouped with their members, standalone profiles
//! separate — with usage bars per window. Plain ANSI output; no TUI
//! dependency.

use std::io::{IsTerminal, Write};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::paths::AppPaths;
use crate::state::{self, State};
use crate::usage::{self, Group, Source};

pub fn command_dashboard(
    paths: &AppPaths,
    live: bool,
    interval: Option<u64>,
    once: bool,
) -> Result<()> {
    let interval = interval.unwrap_or(15).max(5);
    loop {
        let state = state::load(paths)?;
        let now = usage::unix_now();
        let groups = usage::collect(paths, &state, live, now);
        let frame = render(&state, &groups, now, live, interval, use_color());
        if once {
            print!("{frame}");
            return Ok(());
        }
        // Clear, home, redraw.
        print!("\x1b[2J\x1b[H{frame}");
        let _ = std::io::stdout().flush();
        thread::sleep(Duration::from_secs(interval));
    }
}

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(text: &str, pct: f64, color: bool) -> String {
    if !color {
        return text.to_owned();
    }
    let code = if pct >= 90.0 {
        "31" // red
    } else if pct >= 70.0 {
        "33" // yellow
    } else {
        "32" // green
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn bar(pct: f64, color: bool) -> String {
    let filled = ((pct / 10.0).round() as usize).min(10);
    let cells = format!("{}{}", "█".repeat(filled), "·".repeat(10 - filled));
    paint(&cells, pct, color)
}

fn window_cell(window: &usage::Window, now: i64, color: bool) -> String {
    let reset = match (window.reset_since, window.resets_at_unix) {
        (true, _) => " (reset)".to_owned(),
        (false, Some(at)) => format!(" ⟳{}", usage::format_countdown(at - now)),
        (false, None) => String::new(),
    };
    format!(
        "{} {} {}{reset}",
        window.label,
        bar(window.pct, color),
        paint(&format!("{:>3.0}%", window.pct), window.pct, color)
    )
}

fn source_cell(source: &Source) -> String {
    match source {
        Source::Live => "live".to_owned(),
        Source::Cache { age_secs } => {
            format!("cache {}", usage::format_countdown(*age_secs as i64))
        }
        Source::Unavailable => "no data".to_owned(),
    }
}

fn render(
    state: &State,
    groups: &[Group],
    now: i64,
    live: bool,
    interval: u64,
    color: bool,
) -> String {
    let mut out = String::new();
    let logins = state.profiles.len();
    out.push_str(&format!(
        "claude-account · {logins} login{} · {} data · refreshes every {interval}s · Ctrl+C \
         exits\n\n",
        if logins == 1 { "" } else { "s" },
        if live { "live" } else { "cached" },
    ));

    let name_width = groups
        .iter()
        .flat_map(|group| group.members.iter())
        .map(|member| member.profile.len())
        .max()
        .unwrap_or(4);
    let email_width = groups
        .iter()
        .flat_map(|group| group.members.iter())
        .map(|member| member.email.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(1);

    for group in groups {
        let kind = if group.is_workspace {
            "workspace"
        } else {
            "profile"
        };
        let default_tag = if group.is_default_target {
            "  [default target]"
        } else {
            ""
        };
        out.push_str(&format!(
            "━━ {kind} {}  ({}){default_tag}\n",
            group.heading,
            group.dir.display()
        ));
        if group.is_workspace {
            if let Some(workspace) = state.workspaces.get(&group.heading) {
                if let Some(watch) = &workspace.watch {
                    let rotated = workspace
                        .last_rotated_at
                        .map(|at| {
                            format!(
                                ", last rotation {} ago",
                                usage::format_countdown(now - at as i64)
                            )
                        })
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "   watch: rotate at {:.0}%, {}, models {}{rotated}\n",
                        watch.threshold, watch.strategy, watch.models
                    ));
                }
            }
        }
        for member in &group.members {
            let marker = if group.is_workspace
                && group.selected.as_deref() == Some(member.profile.as_str())
            {
                "▶"
            } else {
                " "
            };
            let email = member.email.as_deref().unwrap_or("-");
            let windows = if member.windows.is_empty() {
                "no usage data yet (run a session there, or use --live)".to_owned()
            } else {
                member
                    .windows
                    .iter()
                    .map(|window| window_cell(window, now, color))
                    .collect::<Vec<String>>()
                    .join("   ")
            };
            out.push_str(&format!(
                " {marker} {:<name_width$}  {:<email_width$}  {windows}  [{}]\n",
                member.profile,
                email,
                source_cell(&member.source)
            ));
        }
        out.push('\n');
    }

    if !state.mappings.is_empty() {
        out.push_str("bindings:\n");
        for (directory, target) in &state.mappings {
            out.push_str(&format!("   {} → {target}\n", directory.display()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{MemberUsage, Window};
    use std::path::PathBuf;

    #[test]
    fn frames_group_workspaces_and_mark_the_selection() {
        let mut state = State {
            active: Some("work".to_owned()),
            ..State::default()
        };
        let mut workspace = crate::state::Workspace::new(PathBuf::from("/tmp/work"), true);
        workspace.selected = Some("jack".to_owned());
        state.workspaces.insert("work".to_owned(), workspace);

        let groups = vec![
            Group {
                heading: "work".to_owned(),
                dir: PathBuf::from("/tmp/work"),
                is_workspace: true,
                selected: Some("jack".to_owned()),
                is_default_target: true,
                members: vec![
                    MemberUsage {
                        profile: "jack".to_owned(),
                        email: Some("jack@work.example".to_owned()),
                        windows: vec![Window {
                            label: "7d".to_owned(),
                            pct: 42.0,
                            resets_at_unix: Some(1_000_000),
                            reset_since: false,
                        }],
                        source: Source::Cache { age_secs: 60 },
                        fetched_at_unix: Some(0),
                    },
                    MemberUsage {
                        profile: "jackg".to_owned(),
                        email: None,
                        windows: Vec::new(),
                        source: Source::Unavailable,
                        fetched_at_unix: None,
                    },
                ],
            },
            Group {
                heading: "personal".to_owned(),
                dir: PathBuf::from("/tmp/personal"),
                is_workspace: false,
                selected: None,
                is_default_target: false,
                members: vec![MemberUsage {
                    profile: "personal".to_owned(),
                    email: Some("me@example.com".to_owned()),
                    windows: Vec::new(),
                    source: Source::Unavailable,
                    fetched_at_unix: None,
                }],
            },
        ];

        let frame = render(&state, &groups, 500_000, false, 15, false);
        assert!(frame.contains("workspace work"), "{frame}");
        assert!(frame.contains("[default target]"), "{frame}");
        assert!(frame.contains("▶ jack "), "{frame}");
        assert!(frame.contains("42%"), "{frame}");
        assert!(frame.contains("profile personal"), "{frame}");
        assert!(frame.contains("no usage data yet"), "{frame}");
        assert!(!frame.contains('\x1b'), "colors must be off: {frame:?}");
    }
}
