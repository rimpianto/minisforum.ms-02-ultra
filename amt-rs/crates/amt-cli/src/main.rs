mod config;
mod gui;

use anyhow::Result;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use clap::{Parser, Subcommand};
use config::{load, with_env_overrides, Machine};
use amt_wsman::{self, PowerState, AmtConfig as WsmanConfig};

/// Lines produced by the session thread while the prompt is open. They go
/// through rustyline's external printer, which clears the line, writes the
/// message and redraws the prompt with the text being typed; a plain eprintln
/// leaves the cursor in the middle of the user's input.
static KLOG: std::sync::OnceLock<
    std::sync::Mutex<Option<Box<dyn rustyline::ExternalPrinter + Send>>>,
> = std::sync::OnceLock::new();

fn klog_line(mut line: String) {
    // The external printer writes the string as given: without a trailing
    // newline it is glued to the prompt that gets redrawn after it.
    if !line.ends_with('\n') {
        line.push('\n');
    }
    if let Some(slot) = KLOG.get() {
        if let Ok(mut guard) = slot.lock() {
            if let Some(printer) = guard.as_mut() {
                if printer.print(line.clone()).is_ok() {
                    return;
                }
            }
        }
    }
    eprint!("{line}");
}

macro_rules! klog {
    ($($arg:tt)*) => { klog_line(format!($($arg)*)) };
}

/// ver. 0.1.<commit count>: Cargo.toml is kept in step by
/// scripts/sync-version.sh, so cargo and the program print the same number.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Colour is opt-in and only on a TTY: escape codes in a pipe are noise, and
/// NO_COLOR asks us to stay quiet.
static COLOR: AtomicU8 = AtomicU8::new(0);

fn enable_color(on: bool) {
    COLOR.store(if on { 1 } else { 0 }, Ordering::Relaxed);
}

fn color() -> bool {
    COLOR.load(Ordering::Relaxed) == 1
}

/// Wrap `text` in an SGR sequence when colour is on.
fn paint(text: &str, code: &str) -> String {
    if color() {
        format!("\u{1b}[{code}m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

fn ok(text: &str) -> String {
    paint(text, "32")
}

fn warn(text: &str) -> String {
    paint(text, "33")
}

fn bad(text: &str) -> String {
    paint(text, "31")
}

fn bold(text: &str) -> String {
    paint(text, "1")
}

/// Prompt for the line editor: plain `paint` is enough, because rustyline
/// measures ANSI sequences itself and does not need ignore markers.
fn paint_prompt(text: &str, code: &str) -> String {
    if color() {
        // rustyline measures the prompt by parsing the escape sequences itself
        // and knows nothing about the \x01/\x02 markers used by linenoise: they
        // would be written to the terminal as literal bytes.
        format!("\u{1b}[{code}m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

fn dim(text: &str) -> String {
    paint(text, "2")
}

/// Colour a power state by what it means, so "8" is not just a number.
fn paint_state(v: u8) -> String {
    let name = format!("{v} ({})", state_name(v));
    match v {
        2 => ok(&name),
        8 | 6 | 7 | 12 | 14 => warn(&name),
        5 | 9 | 10 | 11 | 13 | 15 | 16 | 17 => paint(&name, "36"),
        _ => name,
    }
}
pub const GIT_HASH: &str = env!("AMT_GIT_HASH");
pub const VERSION_LONG: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("AMT_GIT_HASH"), ")");

#[derive(Parser)]
#[command(name = "amt-cli", version = VERSION, long_version = VERSION_LONG, about = "Intel AMT command-line client")]
struct Cli {
    /// Which configured machine to talk to; see `amt-cli machines`
    #[arg(long, global = true)]
    machine: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List the machines in the config file
    Machines,
    /// Change or query the power state
    Power {
        /// Desired state: "on", "off", or "reset"
        #[arg(long)]
        state: Option<String>,
    },
    /// Open a KVM (remote console) session
    Kvm {
        /// How long to stay connected, in seconds
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Save the first full frame to this PNG file instead of opening a window
        #[arg(long)]
        screenshot: Option<String>,
        /// Key to send before capturing: a name (space, enter, tab, escape,
        /// shift, delete, f1..f12), a single character, or a chord like
        /// "ctrl+alt+f2". Re-sent every 1.5s, which also wakes a blank console.
        #[arg(long)]
        key: Option<String>,
        /// Send the pointer to this pixel position ("x,y") before capturing
        #[arg(long)]
        pointer: Option<String>,
        /// Click the left button after moving the pointer
        #[arg(long)]
        click: bool,
    },
    /// Ask the firmware to stop in BIOS setup on the next boot
    Bios {
        /// Also reset the machine now, so it goes straight to setup
        #[arg(long)]
        reset: bool,
        /// Only report whether a setup boot is pending
        #[arg(long)]
        status: bool,
        /// Clear the request instead of setting it
        #[arg(long)]
        cancel: bool,
    },
    /// Rebuild a PNG from tiles captured with AMT_DUMP (offline decoder test)
    Replay {
        /// Directory written by AMT_DUMP
        dir: String,
        /// Output PNG
        #[arg(long)]
        out: String,
        /// Screen width
        #[arg(long, default_value_t = 1920)]
        width: usize,
        /// Screen height
        #[arg(long, default_value_t = 1080)]
        height: usize,
    },
    /// Open a Serial-over-LAN session
    Sol {
        /// How long to stay connected, in seconds (ignored with --interactive)
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Send what you type on stdin to the console, and print what comes back
        #[arg(long)]
        interactive: bool,
    },
}

/// How many connection attempts in a row may fail before the session thread
/// stops hammering an AMT that is not there. The console stays usable; a
/// `reconnect` (or `use <name>`) starts it again.
const MAX_ATTEMPTS: u32 = 3;

/// Everything needed to attach to one machine, in one place: the session
/// thread reads it before every connection, so `use <name>` in the console
/// swaps machine without restarting the program.
#[derive(Clone)]
struct Target {
    name: String,
    host: String,
    port: u16,
    redir_port: u16,
    user: String,
    pass: String,
}

impl Target {
    fn from_machine(m: &Machine) -> Self {
        Self {
            name: m.name.clone(),
            host: m.amt.host.clone(),
            port: m.amt.port,
            redir_port: m.amt.redirection_port(),
            user: m.amt.username.clone(),
            pass: m.amt.password.clone(),
        }
    }

    fn wsman(&self) -> WsmanConfig {
        WsmanConfig {
            host: self.host.clone(),
            port: self.port,
            username: self.user.clone(),
            password: self.pass.clone(),
        }
    }
}

/// Where `shot` writes when no path is given: a place you can find later,
/// instead of the working directory (launched from a source tree, that fills
/// up with PNGs nobody looks at).
fn default_shot_path() -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = std::env::var_os("XDG_PICTURES_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Pictures"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = base.join("amt");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("amt-shot-{stamp}.png"))
}

/// `amt-cli machines`: what is configured, which one a bare command would use,
/// and where it listens. No passwords, no key material.
fn print_machines(cfg: &config::Config, requested: Option<&str>) -> Result<()> {
    match config::config_path() {
        Some(p) => println!("{}", dim(&format!("config: {}", p.display()))),
        None => println!("{}", dim("config: none found")),
    }
    let all = cfg.list();
    if all.is_empty() {
        println!(
            "no machines configured; create {}",
            config::candidates()
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
        return Ok(());
    }

    // Resolution can fail (two machines, no default): that is not an error
    // here, the listing is exactly how you find out.
    let current = cfg
        .resolve(requested)
        .map(|m| m.name)
        .unwrap_or_default();
    let width = all.iter().map(|m| m.name.len()).max().unwrap_or(0);

    for m in &all {
        let mark = if m.name == current { "*" } else { " " };
        println!(
            "{} {:<width$}  {}:{}  wsman {}  redirection {}  user {}{}",
            mark,
            m.name,
            m.amt.host,
            m.amt.port,
            m.amt.port,
            m.amt.redirection_port(),
            m.amt.username,
            match m.amt.mac.as_deref() {
                Some(mac) => format!("  mac {mac}"),
                None => String::new(),
            },
            width = width
        );
    }
    if current.is_empty() {
        println!("(no default: pick one with --machine <name> or set `default:` in the file)");
    }
    Ok(())
}

/// Async entry point – we need a Tokio runtime because the WS‑Man
/// client is asynchronous.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    {
        use std::io::IsTerminal;
        enable_color(
            std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true),
        );
    }
    eprintln!("{}", bold(&format!("amt-cli {} ({})", VERSION, GIT_HASH)));
    if let Commands::Replay { dir, out, width, height } = &cli.command {
        return replay_tiles(dir, out, *width, *height);
    }
    let cfg = load()?; // load once, reuse for all sub‑commands
    let requested = cli.machine.clone();

    // `machines` is the one command that does not need a valid choice: its job
    // is to tell you what can be chosen.
    if matches!(cli.command, Commands::Machines) {
        print_machines(&cfg, requested.as_deref())?;
        return Ok(());
    }

    // One machine at a time: the rest of the program only sees this one.
    let machine = with_env_overrides(cfg.resolve(requested.as_deref())?)?;
    let amt = machine.amt.clone();

    match cli.command {
        Commands::Machines => unreachable!("handled before the config is resolved"),
        Commands::Power { state } => {
            // Convert CLI config into the WS‑Man config struct.
            let ws_cfg = WsmanConfig {
                host: amt.host.clone(),
                port: amt.port,
                username: amt.username.clone(),
                password: amt.password.clone(),
            };

            match state.as_deref() {
                // No --state: read the current state with a WS‑Man Get on
                // CIM_AssociatedPowerManagementService.
                None => {
                    let (ps, rps) = amt_wsman::get_power_state(&ws_cfg).await?;
                    match rps {
                        Some(rps) => println!(
                            "PowerState={} ({}) RequestedPowerState={} ({})",
                            ps,
                            state_name(ps),
                            rps,
                            state_name(rps)
                        ),
                        // Some firmware (the MS-02 does) omits the attribute
                        // entirely: say so instead of inventing a "0 (Unknown)".
                        None => println!(
                            "PowerState={} ({}) RequestedPowerState not reported by this firmware",
                            ps,
                            state_name(ps)
                        ),
                    }
                }
                Some(requested) => {
                    let power_state = PowerState::from_str(requested).ok_or_else(|| {
                        anyhow::anyhow!(format!(
                            "Invalid power state '{}'. Use 'on', 'off', or 'reset'",
                            requested
                        ))
                    })?;

                    amt_wsman::set_power(&ws_cfg, power_state).await?;
                    println!("Power state change request sent (state = {})", requested);
                }
            }
        }
        Commands::Bios { reset, status, cancel } => {
            let ws_cfg = WsmanConfig {
                host: amt.host.clone(),
                port: amt.port,
                username: amt.username.clone(),
                password: amt.password.clone(),
            };
            if status {
                let pending = amt_wsman::bios_boot_pending(&ws_cfg).await?;
                println!("BIOSSetup pending: {}", if pending { "yes" } else { "no" });
                return Ok(());
            }
            amt_wsman::set_bios_setup(&ws_cfg, !cancel).await?;
            if cancel {
                println!("{}", ok("BIOS setup request cleared"));
            } else {
                println!("next boot will stop in BIOS setup");
            }
            if cancel {
                // nothing else to do
            } else if reset {
                amt_wsman::set_power(&ws_cfg, PowerState::Reset).await?;
                println!("reset requested - keep the KVM window open to see the setup screen");
            } else {
                println!("when you are ready: amt-cli power --state reset");
            }
        }
        Commands::Replay { .. } => unreachable!("replay is handled before the config"),
        Commands::Kvm { seconds, screenshot, key, pointer, click } => {
            // The firmware serves redirection at WS-Man + 2 (16993 -> 16995);
            // `redirection_port:` in the config, or AMT_REDIR_PORT, overrides
            // that for a machine configured differently.
            let redir_port = amt.redirection_port();

            let pointer_xy = match pointer.as_deref() {
                None => None,
                Some(spec) => {
                    let (x, y) = spec
                        .split_once(',')
                        .ok_or_else(|| anyhow::anyhow!("--pointer wants \"x,y\""))?;
                    Some((x.trim().parse::<u16>()?, y.trim().parse::<u16>()?))
                }
            };

            if screenshot.is_none() {
                // On macOS the winit event loop must own the main thread, so the
                // session gets its own thread and its own tokio runtime.
                let (frame_tx, input_rx, gui) = gui::channels();
                // The session thread follows this: `use <name>` rewrites it and
                // asks the current session to let go.
                let target = Arc::new(std::sync::Mutex::new(Target::from_machine(&machine)));
                let many = cfg.list().len() > 1;
                let input_rx = std::sync::Arc::new(std::sync::Mutex::new(input_rx));
                // 0 = attached, 1 = release for a power action, 2 = released,
                // 3 = release to attach to another machine
                let link = Arc::new(AtomicU8::new(0));
                // Where the console asks for a PNG of the frame in hand.
                let shot: Arc<std::sync::Mutex<Option<String>>> =
                    Arc::new(std::sync::Mutex::new(None));

                // Session thread: reconnects by itself, because a power reset
                // kills it and the interesting part (POST, BIOS) comes after.
                {
                    let frame_tx = frame_tx.clone();
                    let input_rx = input_rx.clone();
                    let key = key.clone();
                    let link = link.clone();
                    let target = target.clone();
                    let shot = shot.clone();
                    std::thread::spawn(move || {
                        let rt = match tokio::runtime::Runtime::new() {
                            Ok(rt) => rt,
                            Err(e) => {
                                klog!("tokio runtime: {e}");
                                std::process::exit(1);
                            }
                        };
                        rt.block_on(async move {
                            let deadline = tokio::time::Instant::now()
                                + std::time::Duration::from_secs(seconds);
                            let mut first = true;
                            let mut failures = 0u32;
                            loop {
                                // Stay off the device while a power action is
                                // in progress: the firmware refuses to touch
                                // the power if a client slips back in first.
                                while link.load(Ordering::Relaxed) != 0 {
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                }
                                let started = tokio::time::Instant::now();
                                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                                if left.is_zero() {
                                    break;
                                }
                                // Read the target every time round: `use <name>`
                                // may have pointed it somewhere else while the
                                // previous session was letting go.
                                let t = { target.lock().unwrap().clone() };
                                let outcome = run_kvm_session(
                                    &t.host,
                                    t.redir_port,
                                    &t.user,
                                    &t.pass,
                                    left.as_secs().max(1),
                                    if first { key.clone() } else { None },
                                    if first { pointer_xy } else { None },
                                    first && click,
                                    Some((frame_tx.clone(), input_rx.clone())),
                                    None,
                                    Some(link.clone()),
                                    Some(shot.clone()),
                                )
                                .await;
                                first = false;
                                if let Err(e) = outcome {
                                    klog!("[kvm] session ended: {e}");
                                }
                                match link.load(Ordering::Relaxed) {
                                    // The socket is closed: tell the control
                                    // thread it can talk to the firmware.
                                    1 => link.store(2, Ordering::Relaxed),
                                    // Switching machine, or a `reconnect`: attach
                                    // again straight away, the old host is not
                                    // why we left.
                                    3 | 4 => {
                                        link.store(0, Ordering::Relaxed);
                                        continue;
                                    }
                                    _ => {}
                                }
                                if tokio::time::Instant::now() >= deadline {
                                    break;
                                }
                                // A machine that is off drops the session
                                // straight away: hammering it every two
                                // seconds fills the terminal for nothing.
                                if started.elapsed() < std::time::Duration::from_secs(5) {
                                    failures = failures.saturating_add(1);
                                } else {
                                    failures = 0;
                                }
                                // Nothing answers: stop instead of spending the
                                // rest of the run retrying, but keep the window
                                // and the console alive so the machine can be
                                // switched or retried by hand.
                                if failures >= MAX_ATTEMPTS {
                                    klog!("[kvm] {} attempts in a row failed on {}:{}",
                                        failures, t.host, t.redir_port
                                    );
                                    klog!("[kvm] the AMT is not answering: check the link, the address and that redirection is enabled"
                                    );
                                    klog!("[kvm] the console stays open - 'reconnect' tries again, 'use <name>' switches machine"
                                    );
                                    loop {
                                        let state = link.load(Ordering::Relaxed);
                                        if state == 3 || state == 4 {
                                            link.store(0, Ordering::Relaxed);
                                            failures = 0;
                                            break;
                                        }
                                        if tokio::time::Instant::now() >= deadline {
                                            std::process::exit(0);
                                        }
                                        tokio::time::sleep(std::time::Duration::from_millis(200))
                                            .await;
                                    }
                                    continue;
                                }
                                let delay = 2u64 << failures.min(3);
                                klog!("[kvm] reconnecting in {delay}s...");
                                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                            }
                            std::process::exit(0);
                        });
                    });
                }

                // Control thread: the terminal stays usable while the window
                // shows the console, so power actions and their effects are one
                // line apart.
                {
                    use std::io::IsTerminal;
                    let target = target.clone();
                    let shot = shot.clone();
                    let cfg = cfg;
                    std::thread::spawn(move || {
                        let rt = match tokio::runtime::Runtime::new() {
                            Ok(rt) => rt,
                            Err(_) => return,
                        };
                        let link = link.clone();
                        let tty = std::io::stdin().is_terminal();

                        if tty {
                            println!();
                            println!("{} - KVM window is live", bold(&format!("amt-rs {VERSION}")));
                            if many {
                                println!("  machine: {} (use <name> to switch, machines to list)", target.lock().unwrap().name);
                            }
                            println!("  status | bios | bios status | bios cancel | boot-bios");
                            println!("  on | off | reset | cycle | quit  (help for this list)");
                            println!("  up/down recall the previous commands");
                        }

                        // One place that runs a command, used by both readers.
                        // `raw` keeps the case of a path; `cmd` is the lowered
                        // form used for matching.
                        let run = |raw: &str, cmd: &str| {
                            if cmd == "quit" || cmd == "q" || cmd == "exit" {
                                std::process::exit(0);
                            }
                            if cmd == "help" || cmd == "?" {
                                // One table, one column width: the description
                                // starts at the same column for every command.
                                let rows: [(&str, &str); 13] = [
                                    ("status", "read the power state"),
                                    ("bios", "stop in BIOS setup on the next boot"),
                                    ("bios status", "is a setup boot pending?"),
                                    ("bios cancel", "clear the request"),
                                    ("boot-bios", "request it and reset now"),
                                    ("on|off|reset", "power actions (off and cycle release the console)"),
                                    ("cycle", "release the console, power cycle, reattach"),
                                    ("machines", "list the configured machines"),
                                    ("use <name>", "attach this window to another machine"),
                                    ("reconnect", "try the console again after giving up"),
                                    ("shot [file]", "save the current frame as a PNG"),
                                    ("help", "this list"),
                                    ("quit", "leave the session and close the window"),
                                ];
                                for (name, desc) in rows {
                                    println!("  {}  {}", bold(&format!("{name:<16}")), desc);
                                }
                                return;
                            }
                            if cmd == "shot"
                                || cmd == "screenshot"
                                || cmd.starts_with("shot ")
                                || cmd.starts_with("screenshot ")
                            {
                                // The session thread owns the framebuffer: leave
                                // a path here and it writes it on its next turn.
                                let given = raw
                                    .split_once(' ')
                                    .map(|(_, rest)| rest.trim())
                                    .filter(|s| !s.is_empty());
                                let path = match given {
                                    Some(p) => p.to_string(),
                                    None => default_shot_path().to_string_lossy().to_string(),
                                };
                                match shot.lock() {
                                    Ok(mut slot) => {
                                        *slot = Some(path.clone());
                                        // Honest wording: the write happens on the
                                        // session thread, and only if there is a
                                        // session with a frame in hand.
                                        println!("next frame will be written to {path}");
                                    }
                                    Err(_) => println!("{} capture slot unavailable", bad("error:")),
                                }
                                return;
                            }
                            if cmd == "machines" || cmd == "hosts" {
                                let current = target.lock().unwrap().name.clone();
                                for m in cfg.list() {
                                    let mark = if m.name == current { "*" } else { " " };
                                    println!(
                                        "{} {}  {}:{}",
                                        mark, m.name, m.amt.host, m.amt.port
                                    );
                                }
                                return;
                            }
                            if cmd == "reconnect" || cmd == "retry" {
                                if link.load(Ordering::Relaxed) != 0 {
                                    println!("the current session is still busy, retry in a moment");
                                    return;
                                }
                                let t = target.lock().unwrap().clone();
                                println!(
                                    "reconnecting to '{}' ({}:{})",
                                    t.name, t.host, t.redir_port
                                );
                                // The session thread treats 4 like a machine
                                // switch: drop what it has and attach again.
                                link.store(4, Ordering::Relaxed);
                                return;
                            }
                            // `use <name>`: keep the window, change the machine.
                            // The session lets go, the session thread reads the
                            // new target and attaches to it.
                            if let Some(name) = cmd
                                .strip_prefix("use ")
                                .map(|s| s.trim())
                                .filter(|s| !s.is_empty())
                            {
                                match cfg.resolve(Some(name)).and_then(with_env_overrides) {
                                    Ok(m) => {
                                        if link.load(Ordering::Relaxed) != 0 {
                                            println!("the current session is still letting go, retry in a moment");
                                            return;
                                        }
                                        let t = Target::from_machine(&m);
                                        println!(
                                            "switching to '{}' ({}:{})",
                                            t.name, t.host, t.redir_port
                                        );
                                        *target.lock().unwrap() = t;
                                        link.store(3, Ordering::Relaxed);
                                    }
                                    Err(e) => println!("{} {e}", bad("error:")),
                                }
                                return;
                            }
                            // The machine can have changed: every command reads
                            // the target as it is now, not as it was at startup.
                            let ws_cfg = target.lock().unwrap().wsman();
                            rt.block_on(async {
                                if let Err(e) = run_control(cmd, &ws_cfg, &link).await {
                                    println!("{} {e}", bad("error:"));
                                }
                            });
                        };

                        if tty {
                            // rustyline gives arrows, history and editing; the
                            // history lives next to the config.
                            let mut rl = match rustyline::DefaultEditor::new() {
                                Ok(rl) => rl,
                                Err(e) => {
                                    eprintln!("line editor: {e}");
                                    return;
                                }
                            };
                            // Session-thread output goes through the editor, so
                            // a "[kvm] reconnecting" never lands on top of the
                            // line being typed: rustyline clears the row, prints
                            // the message and redraws prompt and input.
                            match rl.create_external_printer() {
                                Ok(printer) => {
                                    let boxed: Box<dyn rustyline::ExternalPrinter + Send> =
                                        Box::new(printer);
                                    match KLOG.get() {
                                        Some(slot) => {
                                            if let Ok(mut guard) = slot.lock() {
                                                *guard = Some(boxed);
                                            }
                                        }
                                        None => {
                                            let _ = KLOG.set(std::sync::Mutex::new(Some(boxed)));
                                        }
                                    }
                                }
                                Err(e) => eprintln!("external printer: {e}"),
                            }
                            // History sits beside the config file, wherever that
                            // was found (XDG location or the old ~/.amt).
                            let hist = config::history_path().to_string_lossy().to_string();
                            let _ = rl.load_history(&hist);
                            loop {
                                let prompt = if many {
                                    format!("amt-rs [{}]>", target.lock().unwrap().name)
                                } else {
                                    "amt-rs>".to_string()
                                };
                                match rl.readline(&format!("{} ", paint_prompt(&prompt, "1;36"))) {
                                    Ok(line) => {
                                        let raw = line.trim().to_string();
                                        if raw.is_empty() {
                                            continue;
                                        }
                                        let _ = rl.add_history_entry(raw.as_str());
                                        run(&raw, &raw.to_ascii_lowercase());
                                    }
                                    Err(rustyline::error::ReadlineError::Interrupted) => continue,
                                    Err(rustyline::error::ReadlineError::Eof) => break,
                                    Err(e) => {
                                        eprintln!("input: {e}");
                                        break;
                                    }
                                }
                            }
                            if let Some(dir) = std::path::Path::new(&hist).parent() {
                                let _ = std::fs::create_dir_all(dir);
                            }
                            let _ = rl.save_history(&hist);
                        } else {
                            // Piped input: no editing, no history, just lines.
                            let stdin = std::io::stdin();
                            for line in std::io::BufRead::lines(stdin.lock()) {
                                let line = match line {
                                    Ok(l) => l,
                                    Err(_) => break,
                                };
                                let raw = line.trim().to_string();
                                if raw.is_empty() {
                                    continue;
                                }
                                run(&raw, &raw.to_ascii_lowercase());
                            }
                        }
                    });
                }

                gui::run_gui(gui)?;
                return Ok(());
            }

            run_kvm_session(
                &amt.host,
                redir_port,
                &amt.username,
                &amt.password,
                seconds,
                key,
                pointer_xy,
                click,
                None,
                screenshot,
                None,
                None,
            )
            .await?;
        }
        Commands::Sol { seconds, interactive } => {
            let redir_port = amt.redirection_port();
            let mut session = amt_redir::sol::open(
                &amt.host,
                redir_port,
                &amt.username,
                &amt.password,
            )
            .await?;

            println!(
                "SOL session established with {}:{}",
                amt.host, redir_port
            );
            if let Some(settings) = &session.serial_settings {
                println!("serial settings: {:02x?}", settings);
            }

            // A bare CR nudges the remote console into producing output.
            session.sol_send(b"\r").await?;

            if interactive {
                use std::io::Write as _;
                use tokio::io::AsyncBufReadExt as _;

                let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
                let mut line = String::new();
                println!("(interactive: type, Enter sends CR; Ctrl-D quits)");
                loop {
                    tokio::select! {
                        res = session.sol_read() => match res {
                            Ok(Some(data)) => {
                                print!("{}", String::from_utf8_lossy(&data));
                                let _ = std::io::stdout().flush();
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("SOL read error: {e}");
                                break;
                            }
                        },
                        read = stdin.read_line(&mut line) => match read {
                            Ok(0) => break,
                            Ok(_) => {
                                let mut out = line.trim_end_matches('\n').as_bytes().to_vec();
                                out.push(b'\r');
                                session.sol_send(&out).await?;
                                line.clear();
                            }
                            Err(e) => {
                                eprintln!("stdin: {e}");
                                break;
                            }
                        }
                    }
                }
            } else {
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => break,
                        res = session.sol_read() => match res {
                            Ok(Some(data)) => print!("{}", String::from_utf8_lossy(&data)),
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("SOL read error: {e}");
                                break;
                            }
                        }
                    }
                }
            }
            println!();
        }
    }

    Ok(())
}

/// Human-readable name for a CIM power state value.
fn state_name(v: u8) -> &'static str {
    // Intel AMT power state values, as reported by
    // CIM_AssociatedPowerManagementService.
    match v {
        0 => "Unknown",
        1 => "Full Power",
        2 => "On",
        3 => "Sleep-Light",
        4 => "Sleep-Deep",
        5 => "Power Cycle (Off-Soft)",
        6 => "Off-Hard",
        7 => "Hibernate (Off-Soft)",
        8 => "Off-Soft",
        9 => "Power Cycle (Off-Hard)",
        10 => "Reset (Hard)",
        11 => "Diagnostic Interrupt (NMI)",
        12 => "Off-Hard Graceful",
        13 => "Reset Graceful",
        14 => "Off-Soft Graceful",
        15 => "Power Cycle (Off-Soft Graceful)",
        16 => "Power Cycle (Off-Hard Graceful)",
        17 => "Reset to Diagnostic",
        200 => "Power On to Diagnostic",
        _ => "Unrecognised",
    }
}

fn be16(b: &[u8]) -> usize {
    u16::from_be_bytes([b[0], b[1]]) as usize
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// RFB (VNC) handshake inside the redirection session. Returns the desktop
/// size and the bytes per pixel the device announced.
async fn rfb_handshake(session: &mut amt_redir::Session) -> Result<(usize, usize, usize)> {
    let version = session.read_raw(12).await?;
    klog!("[kvm] server RFB version: {}", String::from_utf8_lossy(&version).trim());
    let reply: Vec<u8> = if version.starts_with(b"RFB 003.") {
        version
    } else {
        b"RFB 003.008\n".to_vec()
    };
    session.write_raw(&reply).await?;

    let count = session.read_raw(1).await?[0] as usize;
    if count == 0 {
        let len = be32(&session.read_raw(4).await?) as usize;
        let reason = session.read_raw(len).await?;
        anyhow::bail!(
            "RFB refused the session: {}",
            String::from_utf8_lossy(&reason)
        );
    }
    let _types = session.read_raw(count).await?;

    // Security type 1 = None: authentication already happened in the
    // redirection handshake.
    session.write_raw(&[1]).await?;
    let result = session.read_raw(4).await?;
    if be32(&result) != 0 {
        anyhow::bail!("RFB security handshake failed");
    }

    // Shared desktop flag.
    session.write_raw(&[1]).await?;

    let init = session.read_raw(24).await?;
    let width = be16(&init[0..2]);
    let height = be16(&init[2..4]);
    let bits_per_pixel = init[4] as usize;
    let name_len = be32(&init[20..24]) as usize;
    let _name = session.read_raw(name_len).await?;

    let bpp = if bits_per_pixel <= 8 { 1 } else { 2 };
    Ok((width, height, bpp))
}

async fn request_update(
    session: &mut amt_redir::Session,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Result<()> {
    let mut msg = vec![3u8, 0];
    msg.extend_from_slice(&(x as u16).to_be_bytes());
    msg.extend_from_slice(&(y as u16).to_be_bytes());
    msg.extend_from_slice(&(w as u16).to_be_bytes());
    msg.extend_from_slice(&(h as u16).to_be_bytes());
    session.write_raw(&msg).await
}


fn print_center(fb: &amt_kvm::Framebuffer) {
    if fb.width == 0 || fb.height == 0 {
        return;
    }
    let (cx, cy) = (fb.width / 2, fb.height / 2);
    let i = (cy * fb.width + cx) * 4;
    println!(
        "center pixel: R={} G={} B={}",
        fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]
    );
    let mut lit = 0usize;
    let mut sampled = 0usize;
    for px in fb.pixels.chunks_exact(4).step_by(97) {
        sampled += 1;
        if px[0] > 8 || px[1] > 8 || px[2] > 8 {
            lit += 1;
        }
    }
    println!("non-black pixels: {lit}/{sampled} (sampled)");
}

/// X11 keysyms for the keys we care about (AMT takes RFB KeyEvent frames).
fn keysym_for(name: &str) -> Result<u32> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "space" => 0x20,
        "enter" | "return" => 0xFF0D,
        "tab" => 0xFF09,
        "escape" | "esc" => 0xFF1B,
        "shift" => 0xFFE1,
        "ctrl" | "control" => 0xFFE3,
        "alt" => 0xFFE9,
        "delete" | "del" => 0xFFFF,
        "home" => 0xFF50,
        "end" => 0xFF57,
        "pageup" | "pgup" => 0xFF55,
        "pagedown" | "pgdn" => 0xFF56,
        "insert" | "ins" => 0xFF63,
        other if other.len() == 1 => other.as_bytes()[0] as u32,
        // F1..F12 are 0xFFBE..0xFFC9, consecutive.
        other if other.len() >= 2
            && other.starts_with('f')
            && other[1..].parse::<u32>().map(|n| (1..=12).contains(&n)).unwrap_or(false) =>
        {
            0xFFBE + other[1..].parse::<u32>()? - 1
        }
        other => anyhow::bail!("unknown key '{other}'"),
    })
}

async fn send_pointer(
    session: &mut amt_redir::Session,
    x: u16,
    y: u16,
    buttons: u8,
) -> Result<()> {
    let mut msg = vec![5u8, buttons];
    msg.extend_from_slice(&x.to_be_bytes());
    msg.extend_from_slice(&y.to_be_bytes());
    session.write_raw(&msg).await?;
    Ok(())
}

async fn send_key(session: &mut amt_redir::Session, keysym: u32) -> Result<()> {
    let mut msg = vec![4u8, 1, 0, 0];
    msg.extend_from_slice(&keysym.to_be_bytes());
    session.write_raw(&msg).await?;
    // Short but non-zero: zero is dropped by some firmware, and anything near
    // the host's key-repeat delay turns one press into a burst.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let mut msg = vec![4u8, 0, 0, 0];
    msg.extend_from_slice(&keysym.to_be_bytes());
    session.write_raw(&msg).await?;
    Ok(())
}

/// "ctrl+l" means: hold ctrl, press l, release ctrl.
async fn send_chord(session: &mut amt_redir::Session, spec: &str) -> Result<()> {
    if std::env::var("AMT_TRACE").is_ok() {
        klog!("[kvm] sending key '{spec}'");
    }
    let parts: Vec<&str> = spec.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return send_key(session, keysym_for(spec)?).await;
    }
    let mods: Vec<u32> = parts[..parts.len() - 1]
        .iter()
        .map(|m| keysym_for(m))
        .collect::<Result<_>>()?;
    let last = keysym_for(parts[parts.len() - 1])?;

    for m in &mods {
        let mut msg = vec![4u8, 1, 0, 0];
        msg.extend_from_slice(&m.to_be_bytes());
        session.write_raw(&msg).await?;
    }
    send_key(session, last).await?;
    for m in mods.iter().rev() {
        let mut msg = vec![4u8, 0, 0, 0];
        msg.extend_from_slice(&m.to_be_bytes());
        session.write_raw(&msg).await?;
    }
    Ok(())
}

fn replay_tiles(dir: &str, out: &str, width: usize, height: usize) -> Result<()> {

    let mut fb = amt_kvm::Framebuffer::new(width, height);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| anyhow::anyhow!("{dir}: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "raw").unwrap_or(false))
        .collect();
    files.sort();
    let (mut ok, mut bad) = (0usize, 0usize);
    let mut last_err: Option<String> = None;
    for f in files {
        let stem = f
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() < 3 {
            continue;
        }
        let (x, y) = match (
            parts[parts.len() - 2].parse::<usize>(),
            parts[parts.len() - 1].parse::<usize>(),
        ) {
            (Ok(x), Ok(y)) => (x, y),
            _ => continue,
        };
        let data = std::fs::read(&f)?;
        match amt_kvm::decoder::draw_rle_tiles(&data, 2, x, y, 64, 64, &mut fb) {
            Ok(()) => ok += 1,
            Err(e) => {
                bad += 1;
                last_err = Some(format!("{}: {e}", f.display()));
            }
        }
    }
    fb.save_png(&out).map_err(|e| anyhow::anyhow!(e))?;
    println!("{ok} tile disegnati, {bad} errori -> {out}");
    if let Some(e) = last_err {
        println!("ultimo errore: {e}");
    }
    Ok(())
}


async fn run_control(cmd: &str, ws: &WsmanConfig, link: &AtomicU8) -> Result<()> {
    match cmd {
        "reset" => {
            amt_wsman::set_power(ws, PowerState::Reset).await?;
            println!("{}", ok("reset requested"));
        }
        "on" => {
            amt_wsman::set_power(ws, PowerState::On).await?;
            println!("{}", ok("power on requested"));
        }
        "off" => {
            // Same rule as cycle: the firmware refuses to power off while a
            // client is attached, so let go of the session first. The console
            // reattaches by itself once the machine is back.
            link.store(1, Ordering::Relaxed);
            println!("releasing the console...");
            for _ in 0..50 {
                if link.load(Ordering::Relaxed) == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            let mut last_err = None;
            for attempt in 0..4 {
                match amt_wsman::set_power(ws, PowerState::Off).await {
                    Ok(()) => {
                        last_err = None;
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        if attempt < 3 {
                            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                        }
                    }
                }
            }
            link.store(0, Ordering::Relaxed);
            if let Some(e) = last_err {
                return Err(e);
            }
            println!("{}", ok("powered off - the console reattaches when the machine is on again"));
        }
        "status" => {
            let (p, r) = amt_wsman::get_power_state(ws).await?;
            match r {
                Some(r) => println!(
                    "PowerState={} RequestedPowerState={}",
                    paint_state(p),
                    paint_state(r)
                ),
                None => println!(
                    "PowerState={} RequestedPowerState not reported by this firmware",
                    paint_state(p)
                ),
            }
        }
        "bios" => {
            amt_wsman::set_bios_setup(ws, true).await?;
            println!("{}", ok("BIOS setup requested for the next boot"));
        }
        "bios status" => {
            let pending = amt_wsman::bios_boot_pending(ws).await?;
            let answer = if pending { warn("yes") } else { dim("no") };
            println!("BIOSSetup pending: {answer}");
        }
        "bios cancel" => {
            amt_wsman::set_bios_setup(ws, false).await?;
            println!("BIOS setup request cleared");
        }
        "cycle" | "powercycle" => {
            // Let go of the session first, otherwise the firmware refuses.
            link.store(1, Ordering::Relaxed);
            println!("releasing the console...");
            for _ in 0..50 {
                if link.load(Ordering::Relaxed) == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            // State 5 (power cycle) is done by the firmware itself: it powers
            // off and back on without needing the network, which matters here
            // because the site router is a card inside this very machine.
            let outcome = async {
                let mut last: Option<anyhow::Error> = None;
                for attempt in 0..4 {
                    match amt_wsman::set_power(ws, PowerState::PowerCycle).await {
                        Ok(()) => {
                            last = None;
                            break;
                        }
                        Err(e) => {
                            last = Some(e);
                            if attempt < 3 {
                                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                            }
                        }
                    }
                }
                match last {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
            .await;

            link.store(0, Ordering::Relaxed);
            outcome?;
            println!("{}", ok("power cycle requested - the console reattaches by itself"));
        }
        "boot-bios" => {
            amt_wsman::set_bios_setup(ws, true).await?;
            amt_wsman::set_power(ws, PowerState::Reset).await?;
            println!("{}", ok("setup requested, reset sent - watch the window"));
        }
        other => println!("{}", bad(&format!("unknown command '{other}'"))),
    }
    Ok(())
}

async fn run_kvm_session(
    host: &str,
    redir_port: u16,
    user: &str,
    pass: &str,
    seconds: u64,
    key: Option<String>,
    pointer: Option<(u16, u16)>,
    click: bool,
    window: Option<(
        std::sync::mpsc::Sender<gui::Frame>,
        std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<gui::Input>>>,
    )>,
    screenshot: Option<String>,
    link: Option<Arc<AtomicU8>>,
    shot: Option<Arc<std::sync::Mutex<Option<String>>>>,
) -> Result<()> {
    klog!("[kvm] connecting to {host}:{redir_port}");
    // Plain TCP probe first: if this fails the trouble is not TLS.
    match std::net::TcpStream::connect_timeout(
        &format!("{host}:{redir_port}").parse()?,
        std::time::Duration::from_secs(5),
    ) {
        Ok(_) => klog!("[kvm] tcp ok"),
        Err(e) => klog!("[kvm] tcp failed: {e}"),
    }
    let mut session = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        amt_redir::Session::connect(host, redir_port, amt_redir::Protocol::Kvm),
    )
    .await
    .map_err(|_| anyhow::anyhow!("TLS connect timed out after 10s"))??;
    klog!("[kvm] TLS up, authenticating");
    session.handshake(user, pass).await?;
    klog!("[kvm] KVM session established with {host}:{redir_port}");

    let (w, h, bpp) = rfb_handshake(&mut session).await?;
    klog!("[kvm] desktop {w}x{h} ({bpp} byte per pixel)");

    let fb = amt_kvm::Framebuffer::new(w, h);
    kvm_capture(
        &mut session,
        bpp,
        fb,
        seconds,
        screenshot.as_deref(),
        key,
        pointer,
        click,
        window,
        link,
        shot,
    )
    .await
}

async fn kvm_capture(
    session: &mut amt_redir::Session,
    bpp: usize,
    mut fb: amt_kvm::Framebuffer,
    seconds: u64,
    screenshot: Option<&str>,
    key: Option<String>,
    pointer: Option<(u16, u16)>,
    click: bool,
    window: Option<(
        std::sync::mpsc::Sender<gui::Frame>,
        std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<gui::Input>>>,
    )>,
    link: Option<Arc<AtomicU8>>,
    shot: Option<Arc<std::sync::Mutex<Option<String>>>>,
) -> Result<()> {
    // RGB565 is the device default; the reference only sends SetPixelFormat
    // when it wants 8 bpp. Sending it unconditionally is a byte the firmware
    // does not expect, so keep the wire identical to the reference.
    if bpp != 2 {
        let mut pf = vec![0u8, 0, 0, 0];
        pf.extend_from_slice(&[8, 8, 0, 1]);
        pf.extend_from_slice(&7u16.to_be_bytes());
        pf.extend_from_slice(&7u16.to_be_bytes());
        pf.extend_from_slice(&3u16.to_be_bytes());
        pf.extend_from_slice(&[5, 2, 0, 0, 0, 0]);
        session.write_raw(&pf).await?;
    }

    // RAW is mandatory for Intel AMT, RLE is what it actually sends, 1092 and
    // DesktopSize are what the reference always advertises.
    let encodings: [i32; 4] = [16, 0, 1092, -223];
    let mut msg = vec![2u8, 0];
    msg.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for encoding in encodings {
        msg.extend_from_slice(&encoding.to_be_bytes());
    }
    session.write_raw(&msg).await?;

    if let Some((px, py)) = pointer {
        // Pixel coordinates to RFB's 0..65535 absolute space.
        let ax = ((px as f64 / fb.width.max(1) as f64) * 65535.0).clamp(0.0, 65535.0) as u16;
        let ay = ((py as f64 / fb.height.max(1) as f64) * 65535.0).clamp(0.0, 65535.0) as u16;
        send_pointer(session, ax, ay, 0).await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if click {
            send_pointer(session, ax, ay, 1).await?;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            send_pointer(session, ax, ay, 0).await?;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    if std::env::var("AMT_BOGUS").is_ok() {
        // Does the device parse our stream at all? A bogus type should either
        // kill the session or be ignored, and that tells us a lot.
        session.write_raw(&[0x99, 0, 0, 0, 0]).await?;
        klog!("[kvm] sent a bogus message type 0x99");
    }

    request_update(session, 0, 0, fb.width, fb.height).await?;
    let mut last_key = tokio::time::Instant::now();
    let mut last_request = tokio::time::Instant::now();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut drawn = false;
    let mut updates = 0usize;
    let quiet = window.is_some();
    while tokio::time::Instant::now() < deadline {
        // A control command can ask the session to let go: the firmware
        // refuses power off and power cycle while a redirection client is
        // attached, so the only way to cycle the machine and still see the
        // boot is to disconnect on purpose and reconnect afterwards.
        if let Some(link) = link.as_ref() {
            if link.load(Ordering::Relaxed) != 0 {
                klog!("[kvm] releasing the session (power action or machine switch)");
                return Ok(());
            }
        }
        // The console can ask for a PNG at any moment: the frame in hand is
        // already decoded, so it costs one file write and no extra round trip.
        if let Some(slot) = shot.as_ref() {
            let wanted = slot.lock().ok().and_then(|mut s| s.take());
            if let Some(path) = wanted {
                match fb.save_png(&path) {
                    Ok(()) => println!(
                        "screenshot written to {} ({}x{})",
                        path, fb.width, fb.height
                    ),
                    Err(e) => println!("screenshot failed: {e}"),
                }
            }
        }
        // Input first: a static screen sends no framebuffer updates at all, so
        // draining input only when a frame arrives makes the keyboard and mouse
        // dead exactly when the console is idle.
        if let Some((_, input_rx)) = window.as_ref() {
            let mut pending = Vec::new();
            if let Ok(rx) = input_rx.lock() {
                while let Ok(ev) = rx.try_recv() {
                    pending.push(ev);
                }
            }
            for ev in pending {
                match ev {
                    // A keystroke is sent whole the moment the key goes down.
                    // Waiting for the release to send the key-up means the
                    // host holds the key while we decode a frame, and the
                    // console repeats it (spaces everywhere, doubled letters).
                    gui::Input::Key { keysym } => {
                        send_key(session, keysym).await?;
                    }
                    gui::Input::Modifier { keysym, down } => {
                        let mut msg = vec![4u8, down as u8, 0, 0];
                        msg.extend_from_slice(&keysym.to_be_bytes());
                        session.write_raw(&msg).await?;
                    }
                    gui::Input::Pointer { x, y, buttons } => {
                        send_pointer(session, x, y, buttons).await?;
                    }
                }
            }
        }
        // A blanked console sends nothing but a DesktopSize rect; the key has
        // to be repeated because the first ones land while the host wakes up.
        if let Some(spec) = key.as_deref() {
            if last_key.elapsed() > std::time::Duration::from_millis(1500) {
                send_chord(session, spec).await?;
                last_key = tokio::time::Instant::now();
            }
        }
        let msg_type = match tokio::time::timeout(
            std::time::Duration::from_millis(30),
            session.read_raw(1),
        )
        .await
        {
            Ok(Ok(buf)) => buf[0],
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                // Nothing to draw, but keep the session alive and the input
                // flowing; ask for a frame about once a second.
                if last_request.elapsed() > std::time::Duration::from_millis(800) {
                    request_update(session, 0, 0, fb.width, fb.height).await?;
                    last_request = tokio::time::Instant::now();
                }
                continue;
            }
        };
        match msg_type {
            0 => {
                let _pad = session.read_raw(1).await?;
                let nrects = be16(&session.read_raw(2).await?);
                if std::env::var("AMT_TRACE").is_ok() {
                    eprintln!("update with {nrects} rect(s)");
                }
                for _ in 0..nrects {
                    let hdr = session.read_raw(12).await?;
                    let (rx, ry) = (be16(&hdr[0..2]), be16(&hdr[2..4]));
                    let (rw, rh) = (be16(&hdr[4..6]), be16(&hdr[6..8]));
                    let encoding = be32(&hdr[8..12]) as i32;
                    if std::env::var("AMT_TRACE").ok().as_deref() == Some("2") {
                        eprintln!("rect x={rx} y={ry} w={rw} h={rh} enc={encoding}");
                    }
                    match encoding {
                        0 => {
                            let payload = session.read_raw(rw * rh * bpp).await?;
                            amt_kvm::decoder::draw_raw(&payload, bpp, rx, ry, rw, rh, &mut fb)
                                .map_err(|e| anyhow::anyhow!(e))?;
                            drawn = true;
                        }
                        16 => {
                            let len = be32(&session.read_raw(4).await?) as usize;
                            if len > 32 * 1024 * 1024 {
                                anyhow::bail!("RLE block too large: {len} bytes");
                            }
                            let mut block = Vec::with_capacity(4 + len);
                            block.extend_from_slice(&(len as u32).to_le_bytes());
                            block.extend_from_slice(&session.read_raw(len).await?);
                            if let Ok(dir) = std::env::var("AMT_DUMP") {
                                // Keep the raw block and the inflated tile stream
                                // so the decoder can be debugged offline.
                                let n = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_micros())
                                    .unwrap_or(0);
                                let _ = std::fs::create_dir_all(&dir);
                                let _ = std::fs::write(format!("{dir}/{n}-{rx}-{ry}.rle"), &block);
                                if let Ok(s) = amt_kvm::decoder::rle_stream(&block) {
                                    let _ = std::fs::write(format!("{dir}/{n}-{rx}-{ry}.raw"), &s);
                                }
                            }
                            match amt_kvm::decoder::rle_stream(&block)
                                .and_then(|stream| {
                                    amt_kvm::decoder::draw_rle_tiles(
                                        &stream, bpp, rx, ry, rw, rh, &mut fb,
                                    )
                                }) {
                                Ok(()) => drawn = true,
                                Err(e) => eprintln!(
                                    "rect {rx},{ry} {rw}x{rh}: {e} (tile skipped, frame kept)"
                                ),
                            }
                        }
                        -223 => fb.resize(rw, rh),
                        other => anyhow::bail!("unsupported encoding {other}"),
                    }
                }

                if drawn {
                    updates += 1;
                    let mut lit = 0usize;
                    let mut sampled = 0usize;
                    for px in fb.pixels.chunks_exact(4).step_by(97) {
                        sampled += 1;
                        if px[0] > 8 || px[1] > 8 || px[2] > 8 {
                            lit += 1;
                        }
                    }
                    if !quiet {
                        println!("update {updates}: {lit}/{sampled} non-black (sampled)");
                    }
                    if let Some((frame_tx, _)) = window.as_ref() {
                        let _ = frame_tx.send(gui::Frame {
                            width: fb.width,
                            height: fb.height,
                            pixels: fb.pixels.clone(),
                        });
                    }
                }
                // Ask again after every update, drawn or not: the first answer
                // is often just a DesktopSize rect, and a client that only
                // re-requests after painting stays black forever.
                request_update(session, 0, 0, fb.width, fb.height).await?;
            }
            1 => {
                // SetColourMapEntries: pad, first, count, then 6 bytes per entry
                let head = session.read_raw(5).await?;
                let count = be16(&head[3..5]);
                let _ = session.read_raw(6 * count).await?;
            }
            2 => {}
            3 => {
                let _ = session.read_raw(3).await?;
                let len = be32(&session.read_raw(4).await?) as usize;
                let _ = session.read_raw(len).await?;
            }
            other => anyhow::bail!("unexpected RFB message type {other}"),
        }
    }

    print_center(&fb);
    if let Some(path) = screenshot {
        fb.save_png(path).map_err(|e| anyhow::anyhow!(e))?;
        println!("screenshot written to {} ({}x{})", path, fb.width, fb.height);
    } else if window.is_none() {
        println!("no frame drawn within {seconds}s");
    }
    Ok(())
}
