//! Per-login usage windows (5-hour session, 7-day, and per-model weekly).
//!
//! Data comes from two places, cheapest first:
//!
//! - **Cache**: Claude Code stores its last usage snapshot in the profile's
//!   `.claude.json` under `cachedUsageUtilization`, tagged with the account
//!   UUID it belongs to. Sessions refresh it constantly while they run, and
//!   an idle login's utilization can only fall (windows reset), so cache plus
//!   reset math answers most questions with zero credential access.
//! - **Live** (opt-in): the same usage API Claude Code polls,
//!   `GET https://api.anthropic.com/api/oauth/usage`, authorized with the
//!   login's keychain token (read-only; see `keychain`).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::claude_json;
use crate::keychain;
use crate::paths::AppPaths;
use crate::state::{self, Profile, State, Workspace};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

#[derive(Debug, Clone)]
pub struct Window {
    pub label: String,
    pub pct: f64,
    pub resets_at_unix: Option<i64>,
    /// The reset moment passed after the data was captured, so the real
    /// utilization is 0 (a fresh window started).
    pub reset_since: bool,
}

#[derive(Debug, Clone)]
pub enum Source {
    Live,
    Cache { age_secs: u64 },
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct MemberUsage {
    pub profile: String,
    pub email: Option<String>,
    pub windows: Vec<Window>,
    pub source: Source,
    /// When the underlying data was captured; lets `watch` persist the
    /// freshest observation per member.
    pub fetched_at_unix: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub heading: String,
    pub dir: PathBuf,
    pub is_workspace: bool,
    pub selected: Option<String>,
    pub is_default_target: bool,
    pub members: Vec<MemberUsage>,
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// A profile's account UUID as recorded in its identity metadata.
fn identity_account_uuid(profile: &Profile) -> Option<&str> {
    profile
        .identity
        .as_ref()?
        .get("oauthAccount")?
        .get("accountUuid")?
        .as_str()
}

/// Claude Code's cached usage snapshot from `<dir>/.claude.json`:
/// `(account_uuid, fetched_at_unix, windows)`.
pub fn cached_snapshot(dir: &Path) -> Option<(String, i64, Vec<Window>)> {
    let raw = fs::read(dir.join(".claude.json")).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let cached = value.get("cachedUsageUtilization")?;
    let account_uuid = cached.get("accountUuid")?.as_str()?.to_owned();
    let fetched_unix = (cached.get("fetchedAtMs")?.as_f64()? / 1000.0) as i64;
    let utilization = cached.get("utilization")?.as_object()?;
    let mut windows = Vec::new();
    for (key, entry) in utilization {
        if key == "extra_usage" {
            // Pay-as-you-go spend is a separate axis from rate-limit windows.
            continue;
        }
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(pct) = entry.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        windows.push(Window {
            label: window_label(key),
            pct,
            resets_at_unix: entry
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339),
            reset_since: false,
        });
    }
    sort_windows(&mut windows);
    Some((account_uuid, fetched_unix, windows))
}

fn window_label(key: &str) -> String {
    match key {
        "five_hour" => "5h".to_owned(),
        "seven_day" => "7d".to_owned(),
        other => {
            let name = other.strip_prefix("seven_day_").unwrap_or(other);
            let pretty: Vec<String> = name
                .split('_')
                .map(|part| {
                    let mut chars = part.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().chain(chars).collect(),
                        None => String::new(),
                    }
                })
                .collect();
            format!("{} 7d", pretty.join(" "))
        }
    }
}

fn window_rank(label: &str) -> u8 {
    match label {
        "5h" => 0,
        "7d" => 1,
        _ => 2,
    }
}

fn sort_windows(windows: &mut [Window]) {
    windows.sort_by(|a, b| {
        window_rank(&a.label)
            .cmp(&window_rank(&b.label))
            .then_with(|| a.label.cmp(&b.label))
    });
}

/// Windows whose reset moment has passed carry no load anymore.
pub fn apply_reset_decay(windows: &mut [Window], now: i64) {
    for window in windows {
        if let Some(resets_at) = window.resets_at_unix {
            if resets_at <= now {
                window.pct = 0.0;
                window.resets_at_unix = None;
                window.reset_since = true;
            }
        }
    }
}

/// Normalize a live usage-API response into windows: the legacy
/// `five_hour`/`seven_day` objects plus per-model weekly entries from the
/// newer `limits` array (`scope.model.display_name`, `percent`).
pub fn windows_from_live(data: &Value) -> Vec<Window> {
    let mut windows = Vec::new();
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        let Some(entry) = data.get(key) else {
            continue;
        };
        let Some(pct) = entry.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        windows.push(Window {
            label: label.to_owned(),
            pct,
            resets_at_unix: entry
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339),
            reset_since: false,
        });
    }
    if let Some(limits) = data.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let name = limit
                .get("scope")
                .and_then(|scope| scope.get("model"))
                .and_then(|model| model.get("display_name"))
                .and_then(Value::as_str);
            let pct = limit.get("percent").and_then(Value::as_f64);
            let (Some(name), Some(pct)) = (name, pct) else {
                continue;
            };
            windows.push(Window {
                label: format!("{name} 7d"),
                pct,
                resets_at_unix: limit
                    .get("resets_at")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339),
                reset_since: false,
            });
        }
    }
    sort_windows(&mut windows);
    windows
}

/// Fetch the login's usage windows from the usage API. The token rides in a
/// 0600 curl config file (never in argv) that is deleted immediately after.
pub fn fetch_live(paths: &AppPaths, profile: &Profile, home: Option<&Path>) -> Result<Vec<Window>> {
    let token = keychain::read_access_token(&profile.config_dir, home)?;
    state::ensure_private_dir(&paths.data_dir)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let config_path = paths
        .data_dir
        .join(format!(".usage-curl.{}.{}", std::process::id(), nonce));
    let config = format!(
        "url = \"{USAGE_URL}\"\nheader = \"Authorization: Bearer {token}\"\nheader = \
         \"anthropic-beta: {OAUTH_BETA_HEADER}\"\nheader = \"User-Agent: claude-account/{}\"\n",
        env!("CARGO_PKG_VERSION")
    );
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&config_path)
            .with_context(|| format!("failed to create {}", config_path.display()))?;
        file.write_all(config.as_bytes())
            .context("failed to write the request configuration")?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }

    let output = Command::new("curl")
        .args(["-sf", "--max-time", "8", "--config"])
        .arg(&config_path)
        .output();
    let _ = fs::remove_file(&config_path);
    let output = output.context("failed to run curl")?;
    if !output.status.success() {
        bail!(
            "usage API request failed (curl exit {})",
            output.status.code().unwrap_or(-1)
        );
    }
    let data: Value =
        serde_json::from_slice(&output.stdout).context("usage API returned invalid JSON")?;
    Ok(windows_from_live(&data))
}

/// Shared inputs for assembling one member's usage row.
struct CollectContext<'a> {
    paths: &'a AppPaths,
    live: bool,
    home: Option<&'a Path>,
    now: i64,
}

fn member_usage(
    context: &CollectContext,
    profile_name: &str,
    profile: &Profile,
    email: Option<String>,
    account_uuid: Option<&str>,
    shared_snapshot: Option<&(String, i64, Vec<Window>)>,
    own_directory: bool,
) -> MemberUsage {
    if context.live {
        match fetch_live(context.paths, profile, context.home) {
            Ok(mut windows) => {
                apply_reset_decay(&mut windows, context.now);
                return MemberUsage {
                    profile: profile_name.to_owned(),
                    email,
                    windows,
                    source: Source::Live,
                    fetched_at_unix: Some(context.now),
                };
            }
            Err(error) => {
                eprintln!("warning: live usage for `{profile_name}` unavailable: {error:#}");
            }
        }
    }
    if let Some((snapshot_uuid, fetched_unix, windows)) = shared_snapshot {
        // A standalone profile owns whatever snapshot sits in its own
        // directory; a workspace member must match the snapshot's account
        // UUID, because the shared file holds only the last login's data.
        let belongs_here = own_directory || account_uuid == Some(snapshot_uuid.as_str());
        if belongs_here {
            let mut windows = windows.clone();
            apply_reset_decay(&mut windows, context.now);
            return MemberUsage {
                profile: profile_name.to_owned(),
                email,
                windows,
                source: Source::Cache {
                    age_secs: context.now.saturating_sub(*fetched_unix).max(0) as u64,
                },
                fetched_at_unix: Some(*fetched_unix),
            };
        }
    }
    if let Some((fetched_unix, mut windows)) = profile
        .last_usage
        .as_ref()
        .and_then(stored_usage_from_value)
    {
        apply_reset_decay(&mut windows, context.now);
        return MemberUsage {
            profile: profile_name.to_owned(),
            email,
            windows,
            source: Source::Cache {
                age_secs: context.now.saturating_sub(fetched_unix).max(0) as u64,
            },
            fetched_at_unix: Some(fetched_unix),
        };
    }
    MemberUsage {
        profile: profile_name.to_owned(),
        email,
        windows: Vec::new(),
        source: Source::Unavailable,
        fetched_at_unix: None,
    }
}

/// Serialize observed windows for `Profile::last_usage`.
pub fn stored_usage_to_value(fetched_at_unix: i64, windows: &[Window]) -> Value {
    serde_json::json!({
        "fetched_at": fetched_at_unix,
        "windows": windows
            .iter()
            .map(|window| {
                serde_json::json!({
                    "label": window.label,
                    "pct": window.pct,
                    "resets_at_unix": window.resets_at_unix,
                })
            })
            .collect::<Vec<Value>>(),
    })
}

pub fn stored_usage_from_value(value: &Value) -> Option<(i64, Vec<Window>)> {
    let fetched_at = value.get("fetched_at")?.as_i64()?;
    let mut windows = Vec::new();
    for entry in value.get("windows")?.as_array()? {
        windows.push(Window {
            label: entry.get("label")?.as_str()?.to_owned(),
            pct: entry.get("pct")?.as_f64()?,
            resets_at_unix: entry.get("resets_at_unix").and_then(Value::as_i64),
            reset_since: false,
        });
    }
    sort_windows(&mut windows);
    Some((fetched_at, windows))
}

fn members_for_workspace(
    context: &CollectContext,
    state: &State,
    workspace_name: &str,
    workspace: &Workspace,
) -> Vec<MemberUsage> {
    let snapshot = cached_snapshot(&workspace.dir);
    let mut members = Vec::new();
    for (profile_name, profile) in &state.profiles {
        if profile.workspace.as_deref() != Some(workspace_name) {
            continue;
        }
        let email = profile
            .identity
            .as_ref()
            .and_then(claude_json::identity_email)
            .map(str::to_owned);
        let uuid = identity_account_uuid(profile).map(str::to_owned);
        members.push(member_usage(
            context,
            profile_name,
            profile,
            email,
            uuid.as_deref(),
            snapshot.as_ref(),
            false,
        ));
    }
    members
}

/// Usage rows for one workspace's members (used by `watch`).
pub fn workspace_member_usage(
    paths: &AppPaths,
    state: &State,
    workspace_name: &str,
    live: bool,
    now: i64,
) -> Vec<MemberUsage> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let context = CollectContext {
        paths,
        live,
        home: home.as_deref(),
        now,
    };
    match state.workspaces.get(workspace_name) {
        Some(workspace) => members_for_workspace(&context, state, workspace_name, workspace),
        None => Vec::new(),
    }
}

/// Collect usage for every profile, grouped by workspace with standalone
/// profiles as their own groups.
pub fn collect(paths: &AppPaths, state: &State, live: bool, now: i64) -> Vec<Group> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let home = home.as_deref();
    let context = CollectContext {
        paths,
        live,
        home,
        now,
    };
    let mut groups = Vec::new();

    for (workspace_name, workspace) in &state.workspaces {
        let members = members_for_workspace(&context, state, workspace_name, workspace);
        groups.push(Group {
            heading: workspace_name.clone(),
            dir: workspace.dir.clone(),
            is_workspace: true,
            selected: workspace.selected.clone(),
            is_default_target: state.active.as_deref() == Some(workspace_name.as_str()),
            members,
        });
    }

    for (profile_name, profile) in &state.profiles {
        if profile.workspace.is_some() {
            continue;
        }
        let identity = claude_json::read_identity(&profile.config_dir)
            .ok()
            .flatten();
        let email = identity
            .as_ref()
            .and_then(claude_json::identity_email)
            .map(str::to_owned);
        let snapshot = cached_snapshot(&profile.config_dir);
        let member = member_usage(
            &context,
            profile_name,
            profile,
            email,
            None,
            snapshot.as_ref(),
            true,
        );
        groups.push(Group {
            heading: profile_name.clone(),
            dir: profile.config_dir.clone(),
            is_workspace: false,
            selected: None,
            is_default_target: state.active.as_deref() == Some(profile_name.as_str()),
            members: vec![member],
        });
    }
    groups
}

pub fn format_countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return "now".to_owned();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn format_window(window: &Window, now: i64) -> String {
    if window.reset_since {
        return format!("{} 0% (window reset)", window.label);
    }
    match window.resets_at_unix {
        Some(resets_at) => format!(
            "{} {:.0}% (resets {})",
            window.label,
            window.pct,
            format_countdown(resets_at - now)
        ),
        None => format!("{} {:.0}%", window.label, window.pct),
    }
}

fn format_source(source: &Source) -> String {
    match source {
        Source::Live => "  [live]".to_owned(),
        Source::Cache { age_secs } => format!(
            "  [cache {} old]",
            format_countdown(*age_secs as i64).replace("now", "0m")
        ),
        Source::Unavailable => String::new(),
    }
}

pub fn render(groups: &[Group], now: i64) -> String {
    let mut out = String::new();
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
            "{kind} {}  ({}){default_tag}\n",
            group.heading,
            group.dir.display()
        ));
        for member in &group.members {
            let marker = if group.is_workspace {
                if group.selected.as_deref() == Some(member.profile.as_str()) {
                    "*"
                } else {
                    " "
                }
            } else {
                " "
            };
            let email = member.email.as_deref().unwrap_or("-");
            if member.windows.is_empty() {
                out.push_str(&format!(
                    "  {marker} {}  {email}  no usage data (run a session there, or use --live)\n",
                    member.profile
                ));
            } else {
                let windows: Vec<String> = member
                    .windows
                    .iter()
                    .map(|window| format_window(window, now))
                    .collect();
                out.push_str(&format!(
                    "  {marker} {}  {email}  {}{}\n",
                    member.profile,
                    windows.join(" · "),
                    format_source(&member.source)
                ));
            }
        }
    }
    out
}

pub fn command_usage(paths: &AppPaths, live: bool) -> Result<()> {
    let state = state::load(paths)?;
    if state.profiles.is_empty() {
        println!("No profiles. Add one with `claude account add NAME`.");
        return Ok(());
    }
    let now = unix_now();
    let groups = collect(paths, &state, live, now);
    print!("{}", render(&groups, now));
    if !live {
        println!(
            "\nCached snapshots refresh while sessions run; `--live` queries the usage API \
             with each login's keychain token (read-only)."
        );
    }
    Ok(())
}

/// Parse an RFC 3339 timestamp ("2026-08-15T05:00:00.365600+00:00") to unix
/// seconds. Hand-rolled to avoid a date-time dependency; fractions are
/// truncated.
pub fn parse_rfc3339(input: &str) -> Option<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let digits =
        |range: std::ops::Range<usize>| -> Option<i64> { input.get(range)?.parse::<i64>().ok() };
    let expect = |index: usize, expected: &[u8]| -> bool {
        bytes.get(index).is_some_and(|byte| expected.contains(byte))
    };
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    if !(expect(4, b"-")
        && expect(7, b"-")
        && expect(10, b"Tt ")
        && expect(13, b":")
        && expect(16, b":"))
    {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut rest = &input[19..];
    if rest.starts_with('.') {
        let fraction_end = rest[1..]
            .find(|character: char| !character.is_ascii_digit())
            .map(|index| index + 1)
            .unwrap_or(rest.len());
        rest = &rest[fraction_end..];
    }
    let offset_seconds = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let offset_hour: i64 = rest.get(1..3)?.parse().ok()?;
            if rest.as_bytes().get(3) != Some(&b':') {
                return None;
            }
            let offset_minute: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (offset_hour * 3_600 + offset_minute * 60)
        }
    };

    Some(
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
            - offset_seconds,
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil` algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parsing_matches_known_epochs() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-08-15T05:00:00.365600+00:00"),
            Some(1_786_770_000)
        );
        assert_eq!(
            parse_rfc3339("2026-01-02T03:04:05+02:00"),
            Some(1_767_315_845)
        );
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn countdowns_read_naturally() {
        assert_eq!(format_countdown(0), "now");
        assert_eq!(format_countdown(540), "9m");
        assert_eq!(format_countdown(7_500), "2h 5m");
        assert_eq!(format_countdown(3 * 86_400 + 4 * 3_600), "3d 4h");
    }

    #[test]
    fn cached_snapshots_parse_the_real_shape() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(".claude.json"),
            r#"{
              "oauthAccount": {"emailAddress": "a@b.c", "accountUuid": "uuid-a"},
              "cachedUsageUtilization": {
                "fetchedAtMs": 1786418023470,
                "accountUuid": "uuid-a",
                "utilization": {
                  "five_hour": {"utilization": 20, "resets_at": "2026-08-11T06:40:00.365582+00:00"},
                  "seven_day": {"utilization": 10, "resets_at": "2026-08-15T05:00:00.365600+00:00"},
                  "seven_day_opus": null,
                  "seven_day_fable": {"utilization": 55, "resets_at": "2026-08-15T05:00:00+00:00"},
                  "extra_usage": {"is_enabled": false}
                }
              }
            }"#,
        )
        .unwrap();

        let (uuid, fetched, windows) = cached_snapshot(temp.path()).unwrap();
        assert_eq!(uuid, "uuid-a");
        assert_eq!(fetched, 1_786_418_023);
        let labels: Vec<&str> = windows.iter().map(|window| window.label.as_str()).collect();
        assert_eq!(labels, ["5h", "7d", "Fable 7d"]);
        assert_eq!(windows[0].pct, 20.0);
        assert_eq!(windows[2].pct, 55.0);
    }

    #[test]
    fn passed_resets_zero_the_window() {
        let mut windows = vec![Window {
            label: "5h".to_owned(),
            pct: 87.0,
            resets_at_unix: Some(1_000),
            reset_since: false,
        }];
        apply_reset_decay(&mut windows, 2_000);
        assert_eq!(windows[0].pct, 0.0);
        assert!(windows[0].reset_since);
        assert!(windows[0].resets_at_unix.is_none());
    }

    #[test]
    fn live_responses_include_scoped_model_windows() {
        let data: Value = serde_json::from_str(
            r#"{
              "five_hour": {"utilization": 12, "resets_at": "2026-08-11T06:40:00+00:00"},
              "seven_day": {"utilization": 34, "resets_at": "2026-08-15T05:00:00+00:00"},
              "limits": [
                {"scope": {"model": {"display_name": "Fable"}}, "percent": 91.5,
                 "resets_at": "2026-08-15T05:00:00+00:00"},
                {"scope": {"other": true}, "percent": 5}
              ]
            }"#,
        )
        .unwrap();
        let windows = windows_from_live(&data);
        let labels: Vec<&str> = windows.iter().map(|window| window.label.as_str()).collect();
        assert_eq!(labels, ["5h", "7d", "Fable 7d"]);
        assert_eq!(windows[2].pct, 91.5);
    }
}
