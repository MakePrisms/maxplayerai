use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use maxplayer_core::episode::{Episode, EpisodeLog};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([780.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Maxplayer Seller Hub",
        options,
        Box::new(|_cc| Ok(Box::new(MaxplayerDesktopApp::new()))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Home,
    Agents,
    Activity,
    Money,
    Accounts,
}

impl Tab {
    const ALL: [Self; 5] = [
        Self::Home,
        Self::Agents,
        Self::Activity,
        Self::Money,
        Self::Accounts,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Agents => "Agents",
            Self::Activity => "Activity",
            Self::Money => "Money",
            Self::Accounts => "Accounts",
        }
    }
}

struct MaxplayerDesktopApp {
    home: HomeState,
    active_tab: Tab,
    agents: AgentsState,
    money: MoneyState,
    activity: ActivityState,
    health: HealthState,
}

impl MaxplayerDesktopApp {
    fn new() -> Self {
        let home = HomeState::resolve();
        let mut money = MoneyState::default();
        money.refresh(home.path.clone());
        let mut health = HealthState::default();
        health.refresh(home.path.clone());

        Self {
            activity: ActivityState::new(home.path.clone()),
            home,
            active_tab: Tab::Home,
            agents: AgentsState::default(),
            money,
            health,
        }
    }
}

impl eframe::App for MaxplayerDesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.money.poll();
        self.activity.poll();
        self.agents.poll();
        self.health.poll();

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("Maxplayer Seller Hub");
                ui.separator();
                ui.label("pass-one scaffold");
            });
        });

        egui::SidePanel::left("tabs")
            .resizable(false)
            .default_width(160.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                for tab in Tab::ALL {
                    let response = ui.selectable_label(self.active_tab == tab, tab.label());
                    if response.clicked() {
                        self.active_tab = tab;
                    }
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| match self.active_tab {
            Tab::Home => self.show_home(ui),
            Tab::Agents => self.show_agents(ui),
            Tab::Activity => self.show_activity(ui),
            Tab::Money => self.show_money(ui),
            Tab::Accounts => self.show_accounts(ui),
        });

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl MaxplayerDesktopApp {
    fn show_home(&mut self, ui: &mut egui::Ui) {
        ui.heading("Home");
        ui.add_space(8.0);
        ui.label("Desktop scaffold is running against one resolved Maxplayer home.");
        ui.add_space(8.0);
        ui.monospace(self.home.path.display().to_string());
        if let Some(error) = &self.home.error {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::from_rgb(190, 66, 66), error);
        }

        ui.add_space(16.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Health check");
            let running = self.health.receiver.is_some();
            if ui
                .add_enabled(!running, egui::Button::new("Run health check"))
                .clicked()
            {
                self.health.refresh(self.home.path.clone());
            }
        });
        ui.add_space(4.0);
        ui.label("Seller preflight via `maxplayer doctor` — git, credential helper, relay, mint, agent.");
        ui.label(&self.health.status);
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.monospace(&self.health.output);
        });
    }

    fn show_agents(&mut self, ui: &mut egui::Ui) {
        ui.heading("Agents");
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Preset");
            egui::ComboBox::from_id_salt("agent_preset")
                .selected_text(self.agents.preset.as_str())
                .show_ui(ui, |ui| {
                    for preset in AgentPreset::ALL {
                        ui.selectable_value(&mut self.agents.preset, preset, preset.as_str());
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("Display name");
            ui.text_edit_singleline(&mut self.agents.display_name);
        });
        ui.horizontal(|ui| {
            ui.label("Rate sats");
            ui.add(egui::DragValue::new(&mut self.agents.rate_sats).range(1..=1_000_000));
        });

        ui.add_space(8.0);
        let starting = self.agents.receiver.is_some();
        if ui
            .add_enabled(!starting, egui::Button::new("Configure and start seller"))
            .clicked()
        {
            self.agents.start_seller(self.home.path.clone());
        }
        ui.add_space(6.0);
        ui.label(&self.agents.status);
        ui.separator();
        ui.monospace(&self.agents.output);
        ui.add_space(8.0);
        ui.label("Task/system-prompt fields are deferred: the current `maxplayer seller` CLI has no matching flag.");
        ui.label("Stop is deferred: no supported `maxplayer` stop verb exists yet.");
    }

    fn show_activity(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Activity");
            if ui.button("Refresh").clicked() {
                self.activity.refresh();
            }
        });
        ui.add_space(6.0);
        ui.label(format!("Tailing {}", self.activity.episodes_path.display()));
        ui.label(&self.activity.status);
        ui.separator();

        if self.activity.rows.is_empty() {
            ui.label("No parseable episodes yet.");
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in &self.activity.rows {
                ui.group(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(&row.outcome);
                        ui.label(format!("job {}", row.job_id));
                        ui.label(format!("amount {} {}", row.amount, row.unit));
                    });
                    if !row.task.is_empty() {
                        ui.label(&row.task);
                    }
                    ui.small(format!("captured_at={} mint={}", row.captured_at, row.mint));
                });
                ui.add_space(6.0);
            }
        });
    }

    fn show_money(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Money");
            if ui.button("Refresh balance").clicked() {
                self.money.refresh(self.home.path.clone());
            }
        });
        ui.add_space(6.0);
        ui.label(format!("Home: {}", self.home.path.display()));
        ui.label(&self.money.status);
        ui.separator();
        ui.monospace(&self.money.output);
    }

    fn show_accounts(&self, ui: &mut egui::Ui) {
        ui.heading("Accounts");
        ui.add_space(8.0);
        ui.label("ACP agent login launchers remain deferred: the current `maxplayer` CLI has no login/accounts verb.");
        ui.add_space(8.0);
        ui.label("This scaffold does not inspect or persist agent credentials.");
    }
}

struct HomeState {
    path: PathBuf,
    error: Option<String>,
}

impl HomeState {
    fn resolve() -> Self {
        match maxplayer_core::home::default_home_dir() {
            Ok(path) => Self { path, error: None },
            Err(error) => Self {
                path: fallback_home_dir(),
                error: Some(format!(
                    "maxplayer_core default home resolution failed: {error}; using fallback"
                )),
            },
        }
    }
}

fn fallback_home_dir() -> PathBuf {
    env::var_os("MAXPLAYER_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".maxplayer")))
        .unwrap_or_else(|| PathBuf::from(".maxplayer"))
}

#[derive(Default)]
struct MoneyState {
    status: String,
    output: String,
    receiver: Option<Receiver<MoneyResult>>,
    started_at: Option<Instant>,
}

impl MoneyState {
    fn refresh(&mut self, home: PathBuf) {
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.started_at = Some(Instant::now());
        self.status = "Refreshing wallet balance...".to_owned();

        thread::spawn(move || {
            let result = run_wallet_balance(&home);
            let _ = sender.send(result);
        });
    }

    fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            if let Some(started_at) = self.started_at {
                self.status = format!(
                    "Refreshing wallet balance... {}s",
                    started_at.elapsed().as_secs()
                );
            }
            return;
        };

        self.status = result.status;
        self.output = result.output;
        self.receiver = None;
        self.started_at = None;
    }
}

struct MoneyResult {
    status: String,
    output: String,
}

#[derive(Default)]
struct HealthState {
    status: String,
    output: String,
    receiver: Option<Receiver<HealthResult>>,
    started_at: Option<Instant>,
}

impl HealthState {
    fn refresh(&mut self, home: PathBuf) {
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.started_at = Some(Instant::now());
        self.status = "Running seller self-check...".to_owned();

        thread::spawn(move || {
            let result = run_doctor(&home);
            let _ = sender.send(result);
        });
    }

    fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            if let Some(started_at) = self.started_at {
                self.status = format!(
                    "Running seller self-check... {}s",
                    started_at.elapsed().as_secs()
                );
            }
            return;
        };

        self.status = result.status;
        self.output = result.output;
        self.receiver = None;
        self.started_at = None;
    }
}

struct HealthResult {
    status: String,
    output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentPreset {
    Claude,
    Cursor,
    Codex,
}

impl AgentPreset {
    const ALL: [Self; 3] = [Self::Claude, Self::Cursor, Self::Codex];

    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Codex => "codex",
        }
    }
}

struct AgentsState {
    preset: AgentPreset,
    display_name: String,
    rate_sats: u64,
    status: String,
    output: String,
    receiver: Option<Receiver<AgentsResult>>,
    started_at: Option<Instant>,
}

impl Default for AgentsState {
    fn default() -> Self {
        Self {
            preset: AgentPreset::Claude,
            display_name: "Maxplayer seller".to_owned(),
            rate_sats: maxplayer_core::home::DEFAULT_RATE_SATS,
            status: "Ready to configure a seller through `maxplayer seller`.".to_owned(),
            output: String::new(),
            receiver: None,
            started_at: None,
        }
    }
}

impl AgentsState {
    fn start_seller(&mut self, home: PathBuf) {
        let request = StartSellerRequest {
            home,
            preset: self.preset,
            display_name: self.display_name.trim().to_owned(),
            rate_sats: self.rate_sats,
        };
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.started_at = Some(Instant::now());
        self.status = "Starting seller daemon...".to_owned();
        self.output.clear();

        thread::spawn(move || {
            let result = spawn_seller_daemon(&request);
            let _ = sender.send(result);
        });
    }

    fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            if let Some(started_at) = self.started_at {
                self.status = format!(
                    "Starting seller daemon... {}s",
                    started_at.elapsed().as_secs()
                );
            }
            return;
        };

        self.status = result.status;
        self.output = result.output;
        self.receiver = None;
        self.started_at = None;
    }
}

struct StartSellerRequest {
    home: PathBuf,
    preset: AgentPreset,
    display_name: String,
    rate_sats: u64,
}

struct AgentsResult {
    status: String,
    output: String,
}

fn seller_start_command(request: &StartSellerRequest) -> Command {
    let mut command = maxplayer_command();
    command.args([
        "seller",
        "--non-interactive",
        "--agent",
        request.preset.as_str(),
        "--rate-sats",
    ]);
    command.arg(request.rate_sats.to_string());
    command.args(["--home"]);
    command.arg(&request.home);
    if !request.display_name.is_empty() {
        command.args(["--name"]);
        command.arg(&request.display_name);
    }
    command
}

fn spawn_seller_daemon(request: &StartSellerRequest) -> AgentsResult {
    let mut command = seller_start_command(request);
    let display = format_command(&command);
    command.stdout(Stdio::null()).stderr(Stdio::null());

    match command.spawn() {
        Ok(child) => AgentsResult {
            status: format!("seller daemon launched with pid {}", child.id()),
            output: format!("$ {display}\n\nseller daemon is running in the background"),
        },
        Err(error) => AgentsResult {
            status: format!("seller daemon launch failed: {error}"),
            output: format!("$ {display}\n\n{error}"),
        },
    }
}

fn run_wallet_balance(home: &Path) -> MoneyResult {
    let mut command = maxplayer_command();
    command.args(["wallet", "balance", "--home"]);
    command.arg(home);

    let display = format_command(&command);
    match command.output() {
        Ok(output) => {
            let mut text = format!("$ {display}\n\n");
            if !output.stdout.is_empty() {
                text.push_str(&String::from_utf8_lossy(&output.stdout));
            }
            if !output.stderr.is_empty() {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("\nstderr:\n");
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            MoneyResult {
                status: format!("wallet balance exited with {}", output.status),
                output: text,
            }
        }
        Err(error) => MoneyResult {
            status: format!("wallet balance failed: {error}"),
            output: format!("$ {display}\n\n{error}"),
        },
    }
}

fn doctor_command(home: &Path) -> Command {
    // `maxplayer doctor` ignores argv and resolves its own home via `MAXPLAYER_HOME`
    // (maxplayer_core::home::default_home_dir). Pin it to the desktop's resolved
    // home so the probe reports on the same home the Home tab displays.
    let mut command = maxplayer_command();
    command.arg("doctor");
    command.env("MAXPLAYER_HOME", home);
    command
}

fn run_doctor(home: &Path) -> HealthResult {
    let mut command = doctor_command(home);
    let display = format_command(&command);
    match command.output() {
        Ok(output) => {
            let mut text = format!("$ {display}\n\n");
            if !output.stdout.is_empty() {
                text.push_str(&String::from_utf8_lossy(&output.stdout));
            }
            if !output.stderr.is_empty() {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("\nstderr:\n");
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            HealthResult {
                status: format!("doctor exited with {}", output.status),
                output: text,
            }
        }
        Err(error) => HealthResult {
            status: format!("doctor failed to launch: {error}"),
            output: format!("$ {display}\n\n{error}"),
        },
    }
}

/// Name of the CLI binary this desktop app drives. Since #262 the CLI ships as
/// `maxplayer` (see the crate's `[[bin]]` name, the flake `mainProgram`, and
/// `apps.*.program`). Keep this the single source of truth for the spawned
/// name so the three resolution sites cannot drift apart again.
const CLI_BIN: &str = "maxplayer";

fn maxplayer_command() -> Command {
    if let Some(path) = env::var_os("MAXPLAYER_BIN").filter(|value| !value.is_empty()) {
        return Command::new(path);
    }

    resolve_cli_command(CLI_BIN, env::current_exe().ok().as_deref())
}

/// Resolve the CLI command for `bin`, preferring a sibling next to the desktop
/// executable and otherwise falling back to `bin` on `PATH`. Split out from
/// `maxplayer_command` so the spawned name can be asserted in a unit test without
/// depending on the process' real `current_exe`.
fn resolve_cli_command(bin: &str, current_exe: Option<&Path>) -> Command {
    if let Some(dir) = current_exe.and_then(Path::parent) {
        let sibling = dir.join(executable_name(bin));
        if sibling.exists() {
            return Command::new(sibling);
        }
    }

    Command::new(bin)
}

fn executable_name(base: &str) -> OsString {
    #[cfg(windows)]
    {
        OsString::from(format!("{base}.exe"))
    }
    #[cfg(not(windows))]
    {
        OsString::from(base)
    }
}

fn format_command(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    let mut redact_next = false;
    for arg in command.get_args() {
        let rendered = arg.to_string_lossy();
        if redact_next {
            parts.push("[redacted]".to_owned());
            redact_next = false;
            continue;
        }

        if is_sensitive_flag(&rendered) {
            parts.push(rendered.into_owned());
            redact_next = true;
        } else {
            parts.push(redact_inline_secret(&rendered));
        }
    }
    parts.join(" ")
}

fn is_sensitive_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--key"
            | "--secret-key"
            | "--private-key"
            | "--token"
            | "--password"
            | "--auth-token"
            | "--bearer-token"
    )
}

fn redact_inline_secret(arg: &str) -> String {
    let Some((name, _value)) = arg.split_once('=') else {
        return arg.to_owned();
    };
    if is_sensitive_flag(name) {
        format!("{name}=[redacted]")
    } else {
        arg.to_owned()
    }
}

struct ActivityState {
    episodes_path: PathBuf,
    rows: Vec<ActivityRow>,
    status: String,
    last_refresh: Instant,
}

impl ActivityState {
    fn new(home: PathBuf) -> Self {
        let mut state = Self {
            episodes_path: EpisodeLog::path_for(&home),
            rows: Vec::new(),
            status: String::new(),
            last_refresh: Instant::now() - Duration::from_secs(10),
        };
        state.refresh();
        state
    }

    fn poll(&mut self) {
        if self.last_refresh.elapsed() >= Duration::from_secs(2) {
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        match read_episodes(&self.episodes_path) {
            Ok(rows) => {
                self.status = format!("{} parseable episode(s)", rows.len());
                self.rows = rows;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.status = "episodes.jsonl does not exist yet".to_owned();
                self.rows.clear();
            }
            Err(error) => {
                self.status = format!("episode read failed: {error}");
            }
        }
    }
}

#[derive(Clone)]
struct ActivityRow {
    captured_at: u64,
    job_id: String,
    task: String,
    amount: u64,
    unit: String,
    mint: String,
    outcome: String,
}

impl From<Episode> for ActivityRow {
    fn from(episode: Episode) -> Self {
        Self {
            captured_at: episode.captured_at,
            job_id: episode.job_id,
            task: episode.offer_task,
            amount: episode.amount,
            unit: episode.unit,
            mint: episode.mint,
            outcome: format!("{:?}", episode.outcome),
        }
    }
}

fn read_episodes(path: &Path) -> std::io::Result<Vec<ActivityRow>> {
    let file = File::open(path)?;
    let mut rows = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(episode) = serde_json::from_str::<Episode>(trimmed) {
            rows.push(ActivityRow::from(episode));
        }
    }
    rows.reverse();
    rows.truncate(50);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use maxplayer_core::episode::{EpisodeKind, EpisodeOutcome};

    use super::*;

    #[test]
    fn read_episodes_skips_bad_lines_and_returns_newest_first() {
        let root = env::temp_dir().join(format!(
            "maxplayer-desktop-episodes-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("temp root");
        let path = root.join("episodes.jsonl");

        let mut first = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            10,
            "seller",
            "job-a",
        );
        first.offer_task = "first task".to_owned();
        first.amount = 21;
        first.unit = "sat".to_owned();
        first.mint = "testnut".to_owned();

        let mut second = Episode::new(
            EpisodeKind::Refused,
            EpisodeOutcome::Refused,
            20,
            "seller",
            "job-b",
        );
        second.offer_task = "second task".to_owned();

        let body = format!(
            "{}\nnot json\n{}\n",
            serde_json::to_string(&first).expect("first json"),
            serde_json::to_string(&second).expect("second json")
        );
        let mut file = File::options()
            .create_new(true)
            .append(true)
            .open(&path)
            .expect("open episodes");
        file.write_all(body.as_bytes()).expect("write episodes");

        let rows = read_episodes(&path).expect("read episodes");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].job_id, "job-b");
        assert_eq!(rows[1].job_id, "job-a");
        assert_eq!(rows[1].amount, 21);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_cli_command_spawns_the_shipped_maxplayer_binary() {
        use std::ffi::OsStr;

        // Point the resolver at a directory that has no sibling binary, so it
        // exercises the PATH-fallback branch and the spawned name is exactly
        // the one it was asked to resolve.
        let no_sibling = Path::new("/nonexistent/desktop-dir/maxplayer-desktop");

        // The real resolver must target the binary that actually ships today.
        let shipped = resolve_cli_command(CLI_BIN, Some(no_sibling));
        assert_eq!(
            shipped.get_program(),
            OsStr::new("maxplayer"),
            "resolver must spawn the shipped `maxplayer` binary, got {:?}",
            shipped.get_program()
        );

        // Red leg / positive control: the same resolver pointed at the retired
        // `mobee` name resolves to `mobee` — a binary nothing has installed
        // since #262. If this differed from the assertion above the test would
        // be a no-op guard; instead it proves the name is what makes it pass.
        let retired = resolve_cli_command("mobee", Some(no_sibling));
        assert_eq!(retired.get_program(), OsStr::new("mobee"));
        assert_ne!(
            shipped.get_program(),
            retired.get_program(),
            "shipped and retired binary names must differ"
        );
    }

    /// #487: the desktop's pre-filled rate is a first-run seller default like the `sell` wizard's,
    /// so it must be the same number. Asserting the literal too keeps this from passing if the
    /// shared constant is ever moved off the market floor.
    #[test]
    fn default_seller_rate_is_the_shared_market_floor() {
        let prefilled = AgentsState::default().rate_sats;
        assert_eq!(prefilled, maxplayer_core::home::DEFAULT_RATE_SATS);
        assert_eq!(prefilled, 100);
    }

    #[test]
    fn seller_start_command_uses_maxplayer_sell_without_secret_args() {
        let request = StartSellerRequest {
            home: PathBuf::from("/tmp/maxplayer-home"),
            preset: AgentPreset::Codex,
            display_name: "Desk Seller".to_owned(),
            rate_sats: 7,
        };

        let command = seller_start_command(&request);
        let rendered = format_command(&command);

        assert!(rendered.contains("seller --non-interactive --agent codex --rate-sats 7"));
        assert!(rendered.contains("--home /tmp/maxplayer-home"));
        assert!(rendered.contains("--name Desk Seller"));
        assert!(!rendered.contains("--key"));
        assert!(!rendered.contains("--secret-key"));
        assert!(!rendered.contains("--private-key"));
    }

    #[test]
    fn doctor_command_is_bound_to_home_via_env() {
        let home = PathBuf::from("/tmp/maxplayer-home");
        let command = doctor_command(&home);

        let rendered = format_command(&command);
        assert!(rendered.ends_with("doctor"), "rendered: {rendered}");

        // doctor ignores argv --home; the home is pinned through MAXPLAYER_HOME instead.
        assert!(!rendered.contains("--home"));
        let maxplayer_home = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("MAXPLAYER_HOME"))
            .and_then(|(_, value)| value)
            .expect("MAXPLAYER_HOME env set");
        assert_eq!(maxplayer_home, home.as_os_str());
    }

    #[test]
    fn format_command_redacts_sensitive_argv_values() {
        let mut command = Command::new(CLI_BIN);
        command.args([
            "wallet",
            "receive",
            "--token",
            "cashu-secret",
            "--password=opensesame",
            "--agent",
            "codex",
        ]);

        let rendered = format_command(&command);

        assert!(rendered.contains("--token [redacted]"));
        assert!(rendered.contains("--password=[redacted]"));
        assert!(rendered.contains("--agent codex"));
        assert!(!rendered.contains("cashu-secret"));
        assert!(!rendered.contains("opensesame"));
    }
}
