mod app;
mod config;
mod model;
mod remote;
mod tree;
mod ui;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use app::{App, AutoRefresh, RefreshRequest, ScanTask, attach_host};
use clap::{Parser, Subcommand};
use config::{Config, default_config_path, load_config};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use remote::collect_hosts;
use tree::print_tree;
use ui::draw_app;
#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(
        short,
        long,
        help = "Config file path; defaults to ~/.config/tmux-gateway/config.toml"
    )]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print a server/session/window/pane tree.
    List,

    /// Attach to tmux on a configured host.
    Attach {
        host: String,

        #[arg(short, long)]
        session: Option<String>,

        #[arg(short, long)]
        window: Option<String>,

        #[arg(short, long)]
        pane: Option<String>,
    },
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);
    let config = load_config(&config_path)?;

    match cli.command {
        Some(Commands::List) => {
            let trees = collect_hosts(&config);
            print_tree(&trees, &config.line_formats);
        }
        Some(Commands::Attach {
            host,
            session,
            window,
            pane,
        }) => {
            ensure_configured_host(&config, &host)?;
            attach_host(
                &host,
                session.as_deref(),
                window.as_deref(),
                pane.as_deref(),
                config.connect_timeout_secs,
            )?;
        }
        None => {
            run_tui(config)?;
        }
    }

    Ok(())
}

fn ensure_configured_host(config: &Config, host: &str) -> Result<()> {
    if config.hosts.iter().any(|item| item == host) {
        return Ok(());
    }
    bail!("host {host:?} is not listed in config");
}

fn run_tui(config: Config) -> Result<()> {
    let mut app = App::new(config);
    let mut scan_task = ScanTask::new(app.config.clone());
    let mut auto_refresh = AutoRefresh::new(&app.config);
    scan_task.start_all();

    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    loop {
        app.apply_scan_results(scan_task.drain());
        if let Some(request) = app.take_refresh_request() {
            match request {
                RefreshRequest::All => scan_task.start_all(),
                RefreshRequest::Hosts(hosts) => scan_task.start_hosts(hosts),
            }
        }
        if auto_refresh.should_start(&scan_task) {
            scan_task.start_all();
            app.set_temp_status("refreshing in background");
        }
        terminal.draw(|frame| draw_app(frame, &mut app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if app.handle_key(key)? {
                    break;
                }
            }
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            _ => {}
        }

        if let Some(target) = app.take_attach_request() {
            terminal.clear()?;
            disable_raw_mode()?;
            execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;

            let result = attach_host(
                &target.host,
                Some(&target.session),
                Some(&target.window),
                Some(&target.pane),
                app.config.connect_timeout_secs,
            );

            enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

            scan_task.start_hosts(vec![target.host.clone()]);
            app.set_temp_status(match result {
                Ok(()) => "detached; refreshing tree".to_string(),
                Err(err) => format!("attach failed: {err}"),
            });
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests;
