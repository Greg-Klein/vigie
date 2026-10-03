//! vigie watches GitLab projects for the tickets assigned to you that carry a
//! label and a status, and writes them to one file as a snapshot. It calls no
//! model: only `glab`. Any other tool can read that file to pick the tickets
//! up; its format is described in the README.
//!
//! On macOS the watch is a launchd job that runs one pass per interval, so
//! nothing stays in memory between two passes. Elsewhere, or with
//! `start --resident`, it is a process that sleeps between passes.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, io, process, thread};

const MIN_INTERVAL: u64 = 10;
const PAGE_SIZE: usize = 100;
const JOB: &str = "dev.vigie.watch";
/// The log is cut back to its last half once it passes this size.
const LOG_LIMIT: usize = 256 * 1024;

// ---------------------------------------------------------------- display

fn colored() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn paint(code: &str, text: &str) -> String {
    if colored() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn bold(text: &str) -> String { paint("1", text) }
fn dim(text: &str) -> String { paint("2", text) }
fn green(text: &str) -> String { paint("32", text) }
fn yellow(text: &str) -> String { paint("33", text) }
fn blue(text: &str) -> String { paint("94", text) }

const BANNER: [&str; 6] = [
    "        _       _",
    " __   _(_) __ _(_) ___",
    " \\ \\ / / |/ _` | |/ _ \\",
    "  \\ V /| | (_| | |  __/",
    "   \\_/ |_|\\__, |_|\\___|",
    "          |___/",
];

fn banner() {
    println!();
    for line in BANNER {
        println!("{}", blue(line));
    }
    println!("  {}\n", dim("watches GitLab, lists the tickets waiting for you"));
}

fn ok(text: &str) { println!("  {} {text}", green("✔")); }
fn warn(text: &str) { println!("  {} {text}", yellow("!")); }
fn info(text: &str) { println!("  {} {text}", dim("·")); }

fn fail(text: &str) -> ! {
    eprintln!("  {} {text}", yellow("✖"));
    process::exit(1);
}

fn plural(count: usize, word: &str) -> String {
    format!("{count} {word}{}", if count > 1 { "s" } else { "" })
}

// ---------------------------------------------------------------- time

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

/// Days since 1970-01-01 to a civil date, and back (Howard Hinnant's algorithms).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn days(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn iso(seconds: u64) -> String {
    let (year, month, day) = civil((seconds / 86_400) as i64);
    let rest = seconds % 86_400;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// Seconds since the epoch of a timestamp written by `iso`.
fn parse_iso(text: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let seconds = days(number(0..4)?, number(5..7)?, number(8..10)?) * 86_400 + number(11..13)? * 3600 + number(14..16)? * 60 + number(17..19)?;
    u64::try_from(seconds).ok()
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..=89 => format!("{seconds} s ago"),
        90..=5399 => format!("{} min ago", seconds / 60),
        5400..=172_799 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

// ---------------------------------------------------------------- files

fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| fail("HOME is not set.")))
}

/// Where the configuration, the log and the pid live. `VIGIE_HOME` moves them, for a second setup or a test.
fn state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("VIGIE_HOME") {
        return PathBuf::from(dir);
    }
    env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("vigie")
}

fn config_file() -> PathBuf { state_dir().join("config.json") }
fn pid_file() -> PathBuf { state_dir().join("vigie.pid") }
fn log_file() -> PathBuf { state_dir().join("vigie.log") }
fn job_file() -> PathBuf { home().join("Library/LaunchAgents").join(format!("{JOB}.plist")) }

/// Written whole, then renamed into place: a reader never sees half a file.
fn write_whole(file: &Path, content: &str) {
    let write = || -> io::Result<()> {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = file.with_extension("tmp");
        fs::write(&temporary, content)?;
        fs::rename(&temporary, file)
    };
    if let Err(error) = write() {
        fail(&format!("Cannot write {}: {error}", file.display()));
    }
}

fn expand_home(value: &str) -> PathBuf {
    match value.strip_prefix('~') {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    }
}

fn absolute(value: &str) -> String {
    let path = expand_home(value);
    let path = if path.is_absolute() { path } else { env::current_dir().unwrap_or_default().join(path) };
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------- config

/// Labels as typed: separated by commas, blanks around them dropped.
fn split_labels(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|label| !label.is_empty()).map(str::to_string).collect()
}

/// A list of labels, or the single `label` text the first version wrote.
fn label_list(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => split_labels(text),
        Value::Array(entries) => entries.iter().filter_map(Value::as_str).flat_map(split_labels).collect(),
        _ => Vec::new(),
    }
}

fn labels_from<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    Ok(label_list(&Value::deserialize(deserializer)?))
}

fn optional_labels_from<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<String>>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(if value.is_null() { None } else { Some(label_list(&value)) })
}

/// Whether a ticket is in the wanted status. Case is ignored, the name is not cut: `to do` finds `To do`, never `To do - QA`.
fn in_status(status: Option<&str>, wanted: &str) -> bool {
    status.is_some_and(|name| name.trim().to_lowercase() == wanted.trim().to_lowercase())
}

/// Whether a ticket carries every wanted label. Case is ignored: `team checkout` finds `Team Checkout`.
fn carries_labels(carried: &[String], wanted: &[String]) -> bool {
    wanted.iter().all(|label| carried.iter().any(|title| title.to_lowercase() == label.to_lowercase()))
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Defaults {
    #[serde(alias = "label", deserialize_with = "labels_from")]
    labels: Vec<String>,
    status: String,
    assignee: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { labels: Vec::new(), status: "To do".into(), assignee: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Source {
    path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    group: bool,
    #[serde(default, alias = "label", skip_serializing_if = "Option::is_none", deserialize_with = "optional_labels_from")]
    labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, rename_all = "camelCase")]
struct Config {
    output: String,
    interval_seconds: u64,
    defaults: Defaults,
    sources: Vec<Source>,
}

impl Default for Config {
    fn default() -> Self {
        let output = state_dir().join("tickets.json");
        Config { output: output.to_string_lossy().into_owned(), interval_seconds: 60, defaults: Defaults::default(), sources: Vec::new() }
    }
}

/// A source with the defaults filled in. An empty assignee means the account glab is logged in with.
struct Filter {
    path: String,
    group: bool,
    /// Every one of them is required.
    labels: Vec<String>,
    status: String,
    assignee: String,
}

impl Config {
    fn load(required: bool) -> Config {
        let file = config_file();
        let Ok(text) = fs::read_to_string(&file) else {
            if required {
                fail(&format!("No configuration. Run {} to get started.", bold("vigie setup")));
            }
            return Config::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|error| fail(&format!("{} is unreadable: {error}", file.display())))
    }

    fn save(&self) {
        write_whole(&config_file(), &format!("{}\n", serde_json::to_string_pretty(self).unwrap_or_default()));
    }

    fn filter(&self, source: &Source) -> Filter {
        Filter {
            path: source.path.clone(),
            group: source.group,
            labels: source.labels.clone().unwrap_or_else(|| self.defaults.labels.clone()),
            status: source.status.clone().unwrap_or_else(|| self.defaults.status.clone()),
            assignee: source.assignee.clone().unwrap_or_else(|| self.defaults.assignee.clone()),
        }
    }

    fn describe(&self, source: &Source) -> String {
        let filter = self.filter(source);
        let label = match filter.labels.len() {
            0 => "any label".to_string(),
            count => format!("label{} {}", if count > 1 { "s" } else { "" }, filter.labels.iter().map(|label| format!("\"{label}\"")).collect::<Vec<_>>().join(" + ")),
        };
        let assignee = if filter.assignee.is_empty() { "me" } else { filter.assignee.as_str() };
        format!(
            "{}{}  {}",
            bold(&source.path),
            if source.group { dim(" (group)") } else { String::new() },
            dim(&format!("{label} · status \"{}\" · assigned to {assignee}", filter.status))
        )
    }

    fn interval(&self) -> u64 {
        self.interval_seconds.max(MIN_INTERVAL)
    }

    fn require_sources(&self) {
        if self.sources.is_empty() {
            fail(&format!("No project to watch. Add one with {}.", bold("vigie add <group>/<project>")));
        }
    }
}

// ---------------------------------------------------------------- gitlab

fn glab(args: &[&str]) -> Result<String, String> {
    let output = Command::new("glab").args(args).stdin(Stdio::null()).output().map_err(|error| format!("glab not found or unusable: {error}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(stderr.lines().filter(|line| !line.trim().is_empty()).last().unwrap_or("glab failed").trim().to_string())
}

fn graphql(query: &str) -> Result<Value, String> {
    let answer: Value = serde_json::from_str(&glab(&["api", "graphql", "-f", &format!("query={query}")])?).map_err(|error| format!("unreadable answer: {error}"))?;
    // GraphQL answers HTTP 200 with a populated `errors` array on failure.
    if let Some(errors) = answer["errors"].as_array().filter(|errors| !errors.is_empty()) {
        return Err(errors.iter().filter_map(|error| error["message"].as_str()).collect::<Vec<_>>().join("; "));
    }
    Ok(answer["data"].clone())
}

fn current_username() -> Result<String, String> {
    let user: Value = serde_json::from_str(&glab(&["api", "user"])?).map_err(|error| format!("unreadable answer: {error}"))?;
    user["username"].as_str().map(str::to_string).ok_or_else(|| "glab account not found".to_string())
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// Every open ticket assigned to the user, with its status and its labels. The
/// labels are matched here and not by GitLab, which only finds a label typed
/// in its exact case.
fn query(filter: &Filter, assignee: &str, cursor: Option<&str>) -> String {
    format!(
        "query {{ {root}(fullPath: {path}) {{ workItems(types: [ISSUE], state: opened, assigneeUsernames: [{assignee}], first: {PAGE_SIZE} {descendants} {after}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ title webUrl widgets {{ type ... on WorkItemWidgetStatus {{ status {{ name }} }} ... on WorkItemWidgetLabels {{ labels {{ nodes {{ title }} }} }} }} }} }} }} }}",
        root = if filter.group { "group" } else { "project" },
        path = quoted(&filter.path),
        assignee = quoted(assignee),
        descendants = if filter.group { ", includeDescendants: true" } else { "" },
        after = cursor.map(|cursor| format!(", after: {}", quoted(cursor))).unwrap_or_default(),
    )
}

/// The tickets of one source that match its filter right now, and how many are assigned in all. Read-only.
fn fetch_source(filter: &Filter, me: &mut Option<String>) -> Result<(usize, Vec<Value>), String> {
    let assignee = if filter.assignee.is_empty() {
        if me.is_none() {
            *me = Some(current_username()?);
        }
        me.clone().unwrap_or_default()
    } else {
        filter.assignee.clone()
    };
    let root = if filter.group { "group" } else { "project" };
    let mut nodes: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let data = graphql(&query(filter, &assignee, cursor.as_deref()))?;
        if data[root].is_null() {
            return Err(format!("{} not found or not visible to this account", if filter.group { "group" } else { "project" }));
        }
        let page = &data[root]["workItems"];
        nodes.extend(page["nodes"].as_array().cloned().unwrap_or_default());
        if page["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            break;
        }
        cursor = page["pageInfo"]["endCursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    // Without a widget of type STATUS the project has no status field at all,
    // which is not the same as a status that does not match.
    let status_of = |node: &Value| -> Option<Option<String>> {
        let widget = node["widgets"].as_array()?.iter().find(|widget| widget["type"] == "STATUS")?;
        Some(widget["status"]["name"].as_str().map(str::to_string))
    };
    if !nodes.is_empty() && nodes.iter().all(|node| status_of(node).is_none()) {
        return Err("these tickets have no status field (it depends on the GitLab plan)".to_string());
    }
    let labels_of = |node: &Value| -> Vec<String> {
        let widgets = node["widgets"].as_array().cloned().unwrap_or_default();
        let widget = widgets.iter().find(|widget| widget["type"] == "LABELS").cloned().unwrap_or_default();
        widget["labels"]["nodes"].as_array().map(|labels| labels.iter().filter_map(|label| label["title"].as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let labelled: Vec<&Value> = nodes.iter().filter(|node| carries_labels(&labels_of(node), &filter.labels)).collect();
    let tickets = labelled
        .iter()
        .filter(|node| in_status(status_of(node).flatten().as_deref(), &filter.status))
        .map(|node| json!({ "url": node["webUrl"], "title": node["title"], "source": filter.path }))
        .collect();
    Ok((labelled.len(), tickets))
}

// ---------------------------------------------------------------- snapshot

fn previous_tickets(output: &str) -> Vec<Value> {
    fs::read_to_string(output)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|stored| stored["tickets"].as_array().cloned())
        .unwrap_or_default()
}

/// One pass over every source. The file is a snapshot of everything that
/// matches right now, written whole. A source that cannot be asked keeps the
/// tickets it had in the previous file: dropping them would tell a reader of
/// the file that they are gone, and bring them back as new at the next pass.
fn check(config: &Config, print: bool) -> (usize, usize) {
    let before = previous_tickets(&config.output);
    let mut tickets: Vec<Value> = Vec::new();
    let mut failures = 0;
    let mut me = None;
    for source in &config.sources {
        let filter = config.filter(source);
        match fetch_source(&filter, &mut me) {
            Ok((assigned, found)) => {
                ok(&format!("{}  {} {} {}", bold(&source.path), dim(&format!("{} assigned to me,", plural(assigned, "ticket"))), found.len(), dim(&format!("in \"{}\"", filter.status))));
                for ticket in &found {
                    info(&format!("{}  {}", ticket["title"].as_str().unwrap_or(""), dim(ticket["url"].as_str().unwrap_or(""))));
                }
                tickets.extend(found);
            }
            Err(error) => {
                failures += 1;
                tickets.extend(before.iter().filter(|ticket| ticket["source"] == source.path.as_str()).cloned());
                warn(&format!("{}  {error}", bold(&source.path)));
            }
        }
    }
    let mut unique: Vec<Value> = Vec::new();
    for ticket in tickets {
        if !unique.iter().any(|kept| kept["url"] == ticket["url"]) {
            unique.push(ticket);
        }
    }
    if print {
        info(&dim("Preview mode: nothing is written."));
    } else {
        let snapshot = json!({ "version": 1, "generatedAt": iso(now()), "tickets": unique });
        write_whole(Path::new(&config.output), &format!("{}\n", serde_json::to_string_pretty(&snapshot).unwrap_or_default()));
    }
    (unique.len(), failures)
}

/// Keeps the log from growing for ever: past the limit, only its last half stays.
fn trim_log() {
    let file = log_file();
    let Ok(content) = fs::read(&file) else { return };
    if content.len() <= LOG_LIMIT {
        return;
    }
    let tail = &content[content.len() - LOG_LIMIT / 2..];
    let start = tail.iter().position(|byte| *byte == b'\n').map_or(0, |index| index + 1);
    let _ = fs::write(&file, &tail[start..]);
}

/// One pass as the scheduler or the resident loop runs it: stamped, and never fatal.
fn scheduled_pass() {
    trim_log();
    println!("{}  checking", iso(now()));
    // Read again at every pass: a project added while the watch runs is watched at the next one.
    let config = Config::load(true);
    let (count, _) = check(&config, false);
    println!("{}  {} in {}", iso(now()), plural(count, "ticket"), config.output);
}

// ---------------------------------------------------------------- watch

fn uid() -> String {
    Command::new("id").arg("-u").output().map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string()).unwrap_or_default()
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn job_loaded() -> bool {
    cfg!(target_os = "macos")
        && Command::new("launchctl").args(["print", &format!("gui/{}/{JOB}", uid())]).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|status| status.success()).unwrap_or(false)
}

fn resident_pid() -> Option<u32> {
    let pid: u32 = fs::read_to_string(pid_file()).ok()?.trim().parse().ok()?;
    let alive = Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().map(|status| status.success()).unwrap_or(false);
    if !alive {
        let _ = fs::remove_file(pid_file());
        return None;
    }
    Some(pid)
}

fn executable() -> PathBuf {
    env::current_exe().unwrap_or_else(|error| fail(&format!("Binary path not found: {error}")))
}

/// A launchd job that runs one pass per interval. Nothing stays in memory between two passes.
fn install_job(config: &Config) {
    let file = job_file();
    let mut variables = format!("<key>PATH</key><string>{}</string>", xml(&env::var("PATH").unwrap_or_default()));
    for name in ["VIGIE_HOME", "XDG_CONFIG_HOME"] {
        if let Ok(value) = env::var(name) {
            variables.push_str(&format!("<key>{name}</key><string>{}</string>", xml(&value)));
        }
    }
    let log = xml(&log_file().to_string_lossy());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{JOB}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>pass</string></array>
  <key>StartInterval</key><integer>{interval}</integer>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key><dict>{variables}</dict>
</dict>
</plist>
"#,
        exe = xml(&executable().to_string_lossy()),
        interval = config.interval(),
    );
    write_whole(&file, &plist);
    let _ = fs::create_dir_all(state_dir());
    let status = Command::new("launchctl").args(["bootstrap", &format!("gui/{}", uid()), &file.to_string_lossy()]).status();
    if !status.map(|status| status.success()).unwrap_or(false) {
        let _ = fs::remove_file(&file);
        fail("launchd refused the job. Try vigie start --resident.");
    }
}

fn remove_job() {
    let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{JOB}", uid())]).stderr(Stdio::null()).status();
    let _ = fs::remove_file(job_file());
}

fn start(resident: bool) {
    if job_loaded() || resident_pid().is_some() {
        fail("The watch is already running.");
    }
    let config = Config::load(true);
    config.require_sources();
    banner();
    if cfg!(target_os = "macos") && !resident {
        install_job(&config);
        ok(&format!("Watch started, one pass every {} s {}", config.interval(), dim("(launchd, nothing in memory between two passes)")));
    } else {
        let _ = fs::create_dir_all(state_dir());
        let log = fs::OpenOptions::new().create(true).append(true).open(log_file()).unwrap_or_else(|error| fail(&format!("Cannot open the log: {error}")));
        let errors = log.try_clone().unwrap_or_else(|error| fail(&format!("Cannot open the log: {error}")));
        // Its own process group: it outlives the terminal that started it.
        let child = Command::new(executable()).arg("run").stdin(Stdio::null()).stdout(log).stderr(errors).process_group(0).spawn().unwrap_or_else(|error| fail(&format!("Cannot start: {error}")));
        write_whole(&pid_file(), &child.id().to_string());
        ok(&format!("Watch started {}, every {} s", dim(&format!("(pid {})", child.id())), config.interval()));
    }
    info(&format!("{} · log: {}", plural(config.sources.len(), "project"), bold("vigie logs")));
}

fn stop(quiet: bool) -> bool {
    let job = job_loaded();
    if job {
        remove_job();
    }
    let pid = resident_pid();
    if let Some(pid) = pid {
        let _ = Command::new("kill").arg(pid.to_string()).status();
        let _ = fs::remove_file(pid_file());
    }
    let stopped = job || pid.is_some();
    if !quiet {
        if stopped { ok("Watch stopped") } else { fail("The watch is not running.") }
    }
    stopped
}

/// The foreground loop, also what `start --resident` runs in the background.
fn run() -> ! {
    Config::load(true).require_sources();
    loop {
        scheduled_pass();
        thread::sleep(Duration::from_secs(Config::load(true).interval()));
    }
}

// ---------------------------------------------------------------- commands

#[derive(Default)]
struct Options {
    positional: Vec<String>,
    group: bool,
    print: bool,
    resident: bool,
    labels: Option<Vec<String>>,
    status: Option<String>,
    assignee: Option<String>,
}

fn options(args: &[String]) -> Options {
    let mut found = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--group" => found.group = true,
            "--print" => found.print = true,
            "--resident" => found.resident = true,
            "--label" | "--status" | "--assignee" => {
                index += 1;
                let value = args.get(index).cloned().unwrap_or_else(|| fail(&format!("{arg} expects a value.")));
                match arg {
                    // Given several times or as one list, the labels add up.
                    "--label" => found.labels.get_or_insert_with(Vec::new).extend(split_labels(&value)),
                    "--status" => found.status = Some(value),
                    _ => found.assignee = Some(value),
                }
            }
            _ if arg.starts_with("--") => fail(&format!("Unknown option: {arg}")),
            _ => found.positional.push(arg.to_string()),
        }
        index += 1;
    }
    found
}

/// Applies a setting to the running watch: only the interval is fixed when it starts.
fn reschedule(config: &Config) {
    if job_loaded() {
        remove_job();
        install_job(config);
        info(&format!("Watch restarted, one pass every {} s.", config.interval()));
    }
}

fn setup() {
    banner();
    let mut config = Config::load(false);
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    // Enter keeps the value shown between brackets, a dash empties it.
    let mut ask = |question: &str, current: &str| -> String {
        let shown = if current.is_empty() { String::new() } else { dim(&format!(" [{current}]")) };
        print!("  {} {question}{shown} ", blue("?"));
        let _ = io::stdout().flush();
        let answer = lines.next().and_then(Result::ok).unwrap_or_default();
        if !io::stdin().is_terminal() {
            println!();
        }
        match answer.trim() {
            "-" => String::new(),
            "" => current.to_string(),
            text => text.to_string(),
        }
    };

    println!("  {} {}", bold("Shared settings"), dim("(Enter keeps the value between brackets)"));
    config.defaults.labels = split_labels(&ask("Required labels, separated by commas (- for none):", &config.defaults.labels.join(", ")));
    let status = ask("Required status:", &config.defaults.status);
    if !status.is_empty() {
        config.defaults.status = status;
    }
    config.defaults.assignee = ask("Assignee (- for your glab account):", &config.defaults.assignee);
    if let Ok(seconds) = ask("How often to ask GitLab, in seconds:", &config.interval_seconds.to_string()).parse::<u64>() {
        config.interval_seconds = seconds.max(MIN_INTERVAL);
    }
    let output = ask("File to write:", &config.output);
    if !output.is_empty() {
        config.output = absolute(&output);
    }

    println!("\n  {}", bold("Watched projects"));
    for source in &config.sources {
        info(&config.describe(source));
    }
    loop {
        let added = ask("Add a project (group/project), empty to finish:", "");
        if added.is_empty() {
            break;
        }
        let group = ask("Is it a whole group? (y/N)", "N").to_lowercase().starts_with('y');
        config.sources.retain(|source| source.path != added);
        config.sources.push(Source { path: added.clone(), group, labels: None, status: None, assignee: None });
        ok(&format!("{added} added"));
    }
    config.save();
    println!();
    ok(&format!("Configuration written to {}", config_file().display()));
    reschedule(&config);
    info(&format!("Try it without writing anything: {}", bold("vigie check --print")));
    info(&format!("Then start the watch: {}", bold("vigie start")));
}

fn add(args: &[String]) {
    let found = options(args);
    let Some(target) = found.positional.first() else {
        fail("Usage: vigie add <group>/<project> [--group] [--label <name>]... [--status <name>] [--assignee <account>]");
    };
    let mut config = Config::load(false);
    let source = Source { path: target.clone(), group: found.group, labels: found.labels, status: found.status, assignee: found.assignee };
    config.sources.retain(|entry| &entry.path != target);
    config.sources.push(source.clone());
    config.save();
    ok(&format!("Watching: {}", config.describe(&source)));
}

fn remove(args: &[String]) {
    let mut config = Config::load(true);
    let Some(target) = args.first().filter(|target| config.sources.iter().any(|source| &source.path == *target)) else {
        fail("This project is not watched.");
    };
    config.sources.retain(|source| &source.path != target);
    config.save();
    ok(&format!("{target} is no longer watched"));
}

fn set(args: &[String]) {
    let usage = "Usage: vigie set <interval|output|label|status|assignee> <value>";
    let key = args.first().map(String::as_str).unwrap_or_else(|| fail(usage));
    let value = args[1..].join(" ");
    let mut config = Config::load(false);
    match key {
        "interval" => {
            config.interval_seconds = value.parse::<u64>().ok().filter(|seconds| *seconds >= MIN_INTERVAL).unwrap_or_else(|| fail(&format!("The interval is a number of seconds, {MIN_INTERVAL} at least.")));
        }
        "output" if !value.is_empty() => config.output = absolute(&value),
        "label" | "labels" => config.defaults.labels = split_labels(&value),
        "assignee" => config.defaults.assignee = value.clone(),
        "status" if !value.is_empty() => config.defaults.status = value.clone(),
        "output" | "status" => fail(&format!("{key} cannot be empty.")),
        _ => fail(usage),
    }
    config.save();
    ok(&format!("{key} = {}", if value.is_empty() { dim("(empty)") } else { value }));
    if key == "interval" {
        reschedule(&config);
        if resident_pid().is_some() {
            info("Applied after the current pass.");
        }
    }
}

fn list(config: &Config) {
    println!("  {}", bold("Watched projects"));
    if config.sources.is_empty() {
        info(&dim("none"));
    }
    for source in &config.sources {
        info(&config.describe(source));
    }
    println!("\n  {}", bold("Settings"));
    info(&format!("interval   {} s", config.interval()));
    info(&format!("file       {}", config.output));
    info(&format!("config     {}", config_file().display()));
}

fn status() {
    banner();
    if job_loaded() {
        ok(&format!("Watch running {}", dim("(launchd, one pass per interval)")));
    } else if let Some(pid) = resident_pid() {
        ok(&format!("Watch running {}", dim(&format!("(pid {pid})"))));
    } else {
        warn(&format!("Watch stopped. {}", dim("vigie start to launch it")));
    }
    if !config_file().exists() {
        info(&format!("Not configured yet: {}", bold("vigie setup")));
        return;
    }
    let config = Config::load(true);
    let stored = fs::read_to_string(&config.output).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
    match stored {
        Some(stored) => {
            let written = stored["generatedAt"].as_str().and_then(parse_iso).map(|at| ago(now().saturating_sub(at))).unwrap_or_else(|| "unknown date".to_string());
            info(&format!("Last write: {written}, {}", plural(stored["tickets"].as_array().map_or(0, Vec::len), "ticket")));
        }
        None => info(&dim("No file written yet")),
    }
    println!();
    list(&config);
}

fn logs() {
    let Ok(content) = fs::read_to_string(log_file()) else {
        fail("No log yet.");
    };
    let lines: Vec<&str> = content.lines().collect();
    println!("{}", lines[lines.len().saturating_sub(40)..].join("\n"));
}

fn help() {
    banner();
    let row = |command: &str, text: &str| println!("  {}{}", bold(&format!("{command:<34}")), dim(text));
    row("vigie setup", "step-by-step configuration");
    row("vigie add <group>/<project>", "watches a project (--group, --label, --status, --assignee)");
    row("vigie remove <group>/<project>", "stops watching it");
    row("vigie set interval <seconds>", "how often GitLab is asked");
    row("vigie set label <a>, <b>", "required labels, all of them, whatever their case");
    row("vigie set status|assignee", "filter shared by every project");
    row("vigie set output <file>", "file the tickets found are written to");
    row("vigie list", "projects and settings");
    row("vigie check [--print]", "one check; --print writes nothing");
    row("vigie start [--resident]", "watch in the background");
    row("vigie stop | status | logs", "stop, state, last lines of the log");
    row("vigie run", "watch in the foreground");
    println!();
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        None | Some("status") => status(),
        Some("setup") => setup(),
        Some("add") => add(rest),
        Some("remove") => remove(rest),
        Some("set") => set(rest),
        Some("list") => list(&Config::load(true)),
        Some("start") => start(options(rest).resident),
        Some("stop") => {
            stop(false);
        }
        Some("logs") => logs(),
        Some("run") => run(),
        // What launchd runs at every interval.
        Some("pass") => scheduled_pass(),
        Some("check") => {
            let config = Config::load(true);
            config.require_sources();
            let print = options(rest).print;
            let (count, failures) = check(&config, print);
            println!();
            let verb = if print { "found" } else { "written" };
            let place = if print { String::new() } else { format!(" to {}", config.output) };
            ok(&format!("{} {verb}{place}", plural(count, "ticket")));
            if failures > 0 {
                process::exit(1);
            }
        }
        Some("help" | "--help" | "-h") => help(),
        Some(other) => {
            help();
            fail(&format!("Unknown command: {other}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_write_a_timestamp_it_can_read_back() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_790_952_506), "2026-10-02T14:48:26.000Z");
        assert_eq!(parse_iso("2026-10-02T14:48:26.000Z"), Some(1_790_952_506));
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00.000Z");
        assert_eq!(parse_iso(&iso(951_782_400)), Some(951_782_400));
    }

    #[test]
    fn should_say_how_long_ago_in_the_largest_unit_that_fits() {
        assert_eq!(ago(42), "42 s ago");
        assert_eq!(ago(600), "10 min ago");
        assert_eq!(ago(7200), "2 h ago");
        assert_eq!(ago(259_200), "3 d ago");
    }

    #[test]
    fn should_ask_a_project_for_its_label_and_a_group_for_its_descendants() {
        let filter = Filter { path: "acme/shop".into(), group: false, labels: vec!["Squad A".into()], status: "To do".into(), assignee: String::new() };
        let project = query(&filter, "me", None);
        assert!(project.contains(r#"project(fullPath: "acme/shop")"#));
        assert!(project.contains(r#"assigneeUsernames: ["me"]"#));
        assert!(!project.contains("includeDescendants"));
        // Labels are read back and matched here, never filtered by GitLab.
        assert!(project.contains("WorkItemWidgetLabels"));
        assert!(!project.contains("labelName"));

        let group = query(&Filter { group: true, ..filter }, "me", Some("abc"));
        assert!(group.contains("group(fullPath:"));
        assert!(group.contains("includeDescendants: true"));
        assert!(group.contains(r#"after: "abc""#));
    }

    #[test]
    fn should_require_every_label_whatever_its_case() {
        let carried = vec!["Team Checkout".to_string(), "Frontend".to_string(), "bug".to_string()];
        let wanted = |labels: &[&str]| labels.iter().map(|label| label.to_string()).collect::<Vec<_>>();
        assert!(carries_labels(&carried, &wanted(&[])));
        assert!(carries_labels(&carried, &wanted(&["team checkout"])));
        assert!(carries_labels(&carried, &wanted(&["TEAM CHECKOUT", "frontend"])));
        assert!(!carries_labels(&carried, &wanted(&["team checkout", "backend"])));
        assert!(!carries_labels(&carried, &wanted(&["Squad"])));
    }

    #[test]
    fn should_match_the_status_whatever_its_case_and_only_on_its_whole_name() {
        assert!(in_status(Some("To do"), "To do"));
        assert!(in_status(Some("To do"), "TO DO"));
        assert!(in_status(Some("Ready to sprint"), " ready to sprint "));
        assert!(!in_status(Some("To do - QA"), "To do"));
        assert!(!in_status(Some("To do"), "To do - QA"));
        // A ticket with a status field that is not set matches nothing.
        assert!(!in_status(None, "To do"));
    }

    #[test]
    fn should_read_labels_as_a_list_or_as_text() {
        assert_eq!(split_labels(" Team Checkout , frontend,, "), vec!["Team Checkout", "frontend"]);
        let config: Config = serde_json::from_str(r#"{ "defaults": { "labels": ["A", "B"] }, "sources": [{ "path": "acme/shop", "labels": ["C"] }, { "path": "acme/api", "label": "D, E" }] }"#).unwrap();
        assert_eq!(config.defaults.labels, vec!["A", "B"]);
        assert_eq!(config.filter(&config.sources[0]).labels, vec!["C"]);
        assert_eq!(config.filter(&config.sources[1]).labels, vec!["D", "E"]);
    }

    #[test]
    fn should_read_the_configuration_the_first_version_wrote() {
        let config: Config = serde_json::from_str(r#"{ "output": "/tmp/t.json", "intervalSeconds": 30, "defaults": { "label": "Squad" }, "sources": [{ "path": "acme", "group": true }, { "path": "acme/shop", "status": "Ready" }] }"#).unwrap();
        assert_eq!(config.interval(), 30);
        assert_eq!(config.defaults.status, "To do");
        let filter = config.filter(&config.sources[1]);
        assert_eq!((filter.labels.join("+").as_str(), filter.status.as_str(), filter.group), ("Squad", "Ready", false));
        assert!(config.sources[0].group);
    }
}
