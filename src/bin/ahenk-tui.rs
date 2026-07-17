use ahenk::cli::config::Config;
use ahenk::cli::daemon as daemon_utils;
use ahenk::db::operations::initialize_database;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Tabs},
    Terminal,
};
use rusqlite::Connection;
use std::{
    error::Error,
    fs::File,
    io::{self, BufRead, BufReader, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant},
};

/// Navigation tabs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabItem {
    Dashboard = 0,
    Peers = 1,
    Devices = 2,
    OpLog = 3,
    DbQuery = 4,
    LogViewer = 5,
    Help = 6,
}

impl TabItem {
    fn from_index(index: usize) -> Self {
        match index {
            0 => TabItem::Dashboard,
            1 => TabItem::Peers,
            2 => TabItem::Devices,
            3 => TabItem::OpLog,
            4 => TabItem::DbQuery,
            5 => TabItem::LogViewer,
            _ => TabItem::Help,
        }
    }
}

/// Input mode for active text fields
#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    EditingQuery,
    EditingAddPeer,
    EditingPairDevice,
    EditingAuthorizeDevice,
}

struct App {
    config: Config,
    active_tab: TabItem,
    input_mode: InputMode,
    
    // Status metrics
    daemon_running: bool,
    daemon_pid: Option<i32>,
    daemon_uptime: Option<u64>,
    db_size_bytes: u64,
    
    // Loaded lists
    peers: Vec<Vec<String>>,
    devices: Vec<Vec<String>>,
    oplog_entries: Vec<Vec<String>>,
    
    // SQL Query State
    query_input: String,
    query_error: Option<String>,
    query_results: Vec<Vec<String>>,
    query_headers: Vec<String>,
    
    // Input prompts for dialogs
    generic_input: String,
    info_message: Option<String>,
    error_message: Option<String>,
    show_dialog: bool,
    
    // Logs State
    log_lines: Vec<String>,
    log_scroll: usize,
    
    last_refresh: Instant,
}

impl App {
    fn new(config: Config) -> Self {
        let db_path = config.db_path();
        let db_size = Path::new(&db_path)
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);

        Self {
            config,
            active_tab: TabItem::Dashboard,
            input_mode: InputMode::Normal,
            daemon_running: false,
            daemon_pid: None,
            daemon_uptime: None,
            db_size_bytes: db_size,
            peers: Vec::new(),
            devices: Vec::new(),
            oplog_entries: Vec::new(),
            query_input: "SELECT * FROM users;".to_string(),
            query_error: None,
            query_results: Vec::new(),
            query_headers: Vec::new(),
            generic_input: String::new(),
            info_message: None,
            error_message: None,
            show_dialog: false,
            log_lines: Vec::new(),
            log_scroll: 0,
            last_refresh: Instant::now() - Duration::from_secs(10), // force immediate load
        }
    }

    fn open_db(&self) -> Result<Connection, Box<dyn Error>> {
        let db_path = self.config.db_path();
        let conn = initialize_database(&db_path)?;
        Ok(conn)
    }

    fn refresh_status(&mut self) {
        let pid_file = Config::pid_file();
        self.daemon_running = daemon_utils::is_running(&pid_file);
        self.daemon_pid = daemon_utils::get_pid(&pid_file).ok();
        self.daemon_uptime = daemon_utils::get_uptime(&pid_file).ok();
        
        let db_path = self.config.db_path();
        self.db_size_bytes = Path::new(&db_path)
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);
    }

    fn refresh_data(&mut self) {
        self.refresh_status();
        let conn = match self.open_db() {
            Ok(c) => c,
            Err(_) => return,
        };

        // Load peers
        if let Ok(mut stmt) = conn.prepare("SELECT peer_id, multiaddr, last_seen FROM peers ORDER BY last_seen DESC") {
            let peer_rows = stmt.query_map([], |row| {
                let last_seen_val = row.get::<_, Option<i64>>(2).ok().flatten()
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| "Never".to_string());
                Ok(vec![
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                    last_seen_val,
                ])
            });
            if let Ok(rows) = peer_rows {
                self.peers = rows.filter_map(|r| r.ok()).collect();
            }
        }

        // Load devices
        if let Ok(mut stmt) = conn.prepare("SELECT device_id, device_type, device_name, authorized_at FROM devices") {
            let device_rows = stmt.query_map([], |row| {
                let authorized_at_val = row.get::<_, Option<i64>>(3).ok().flatten()
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| "N/A".to_string());
                Ok(vec![
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                    authorized_at_val,
                ])
            });
            if let Ok(rows) = device_rows {
                self.devices = rows.filter_map(|r| r.ok()).collect();
            }
        }

        // Load oplog
        if let Ok(mut stmt) = conn.prepare("SELECT id, table_name, op_type, timestamp FROM oplog ORDER BY timestamp DESC LIMIT 50") {
            let oplog_rows = stmt.query_map([], |row| {
                Ok(vec![
                    row.get::<_, i64>(0).unwrap_or_default().to_string(),
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                    row.get::<_, i64>(3).unwrap_or_default().to_string(),
                ])
            });
            if let Ok(rows) = oplog_rows {
                self.oplog_entries = rows.filter_map(|r| r.ok()).collect();
            }
        }

        self.last_refresh = Instant::now();
    }

    fn execute_sql_query(&mut self) {
        let conn = match self.open_db() {
            Ok(c) => c,
            Err(e) => {
                self.query_error = Some(format!("Database error: {}", e));
                return;
            }
        };

        let mut stmt = match conn.prepare(&self.query_input) {
            Ok(s) => s,
            Err(e) => {
                self.query_error = Some(e.to_string());
                return;
            }
        };

        let col_count = stmt.column_count();
        self.query_headers = stmt.column_names().iter().map(|s| s.to_string()).collect();

        let rows = stmt.query_map([], |row| {
            let mut val_strings = Vec::new();
            for i in 0..col_count {
                let v = row.get::<_, Option<String>>(i)
                    .unwrap_or(None)
                    .unwrap_or_else(|| "NULL".to_string());
                val_strings.push(v);
            }
            Ok(val_strings)
        });

        match rows {
            Ok(mapped_rows) => {
                self.query_error = None;
                self.query_results = mapped_rows.filter_map(|r| r.ok()).collect();
            }
            Err(e) => {
                self.query_error = Some(e.to_string());
            }
        }
    }

    fn read_logs(&mut self) {
        let log_file_path = self.config.logging.file.clone();
        let path = Path::new(&log_file_path);
        if !path.exists() {
            self.log_lines = vec![format!("Log file not found at: {}", log_file_path)];
            return;
        }

        if let Ok(file) = File::open(path) {
            let reader = BufReader::new(file);
            // Read last 100 lines
            let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
            let start = if lines.len() > 100 { lines.len() - 100 } else { 0 };
            self.log_lines = lines[start..].to_vec();
        }
    }

    fn execute_add_peer(&mut self) {
        let multiaddr = self.generic_input.trim().to_string();
        if multiaddr.is_empty() {
            return;
        }
        
        let conn = match self.open_db() {
            Ok(c) => c,
            Err(e) => {
                self.error_message = Some(format!("Database error: {}", e));
                return;
            }
        };

        // Extract peer id from multiaddress (usually ends in /p2p/<peer-id>)
        let peer_id = if let Some(idx) = multiaddr.rfind("/p2p/") {
            multiaddr[idx + 5..].to_string()
        } else {
            uuid::Uuid::new_v4().to_string() // fallback dummy
        };

        let now = chrono::Utc::now().timestamp();
        let res = conn.execute(
            "INSERT INTO peers (peer_id, multiaddr, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(peer_id) DO UPDATE SET multiaddr=?2, last_seen=?3",
            rusqlite::params![peer_id, multiaddr, now],
        );

        match res {
            Ok(_) => {
                self.info_message = Some(format!("Successfully added peer: {}", peer_id));
                self.refresh_data();
            }
            Err(e) => {
                self.error_message = Some(format!("Error: {}", e));
            }
        }
        self.show_dialog = false;
        self.generic_input.clear();
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // Load config
    let config = Config::load(None).unwrap_or_else(|_| Config::default());

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new(config);
    app.refresh_data();
    app.read_logs();

    let res = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{:?}", err);
    }

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        // Refresh data periodically
        if app.last_refresh.elapsed() > Duration::from_secs(5) {
            app.refresh_data();
            if app.active_tab == TabItem::LogViewer {
                app.read_logs();
            }
        }

        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == event::KeyEventKind::Press {
                    if app.show_dialog {
                        handle_dialog_input(key, app);
                        continue;
                    }

                    match app.input_mode {
                        InputMode::Normal => {
                            if handle_normal_input(key, app) {
                                return Ok(());
                            }
                        }
                        InputMode::EditingQuery => {
                            handle_query_input(key, app);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn handle_normal_input(key: KeyEvent, app: &mut App) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::F(1) => app.active_tab = TabItem::Dashboard,
        KeyCode::F(2) => app.active_tab = TabItem::Peers,
        KeyCode::F(3) => app.active_tab = TabItem::Devices,
        KeyCode::F(4) => app.active_tab = TabItem::OpLog,
        KeyCode::F(5) => {
            app.active_tab = TabItem::DbQuery;
            app.input_mode = InputMode::EditingQuery;
        }
        KeyCode::F(6) => {
            app.active_tab = TabItem::LogViewer;
            app.read_logs();
        }
        KeyCode::F(7) => app.active_tab = TabItem::Help,
        
        // Refresh manually
        KeyCode::Char('r') => {
            app.refresh_data();
            app.read_logs();
            app.info_message = Some("Data manually refreshed!".to_string());
        }

        // Add peer
        KeyCode::Char('a') if app.active_tab == TabItem::Peers => {
            app.show_dialog = true;
            app.input_mode = InputMode::EditingAddPeer;
            app.generic_input.clear();
        }

        // Scroll logs
        KeyCode::Up if app.active_tab == TabItem::LogViewer => {
            if app.log_scroll > 0 {
                app.log_scroll -= 1;
            }
        }
        KeyCode::Down if app.active_tab == TabItem::LogViewer => {
            if app.log_scroll < app.log_lines.len().saturating_sub(10) {
                app.log_scroll += 1;
            }
        }

        _ => {}
    }
    false
}

fn handle_query_input(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            app.execute_sql_query();
        }
        KeyCode::Backspace => {
            app.query_input.pop();
        }
        KeyCode::Char(c) => {
            app.query_input.push(c);
        }
        _ => {}
    }
}

fn handle_dialog_input(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.show_dialog = false;
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            if app.input_mode == InputMode::EditingAddPeer {
                app.execute_add_peer();
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.generic_input.pop();
        }
        KeyCode::Char(c) => {
            app.generic_input.push(c);
        }
        _ => {}
    }
}

fn ui(f: &mut ratatui::Frame, app: &App) {
    let size = f.size();
    
    // Base layout: Header, Main Area, Footer/Status Bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Top Tabs
            Constraint::Min(10),   // Main View
            Constraint::Length(2), // Bottom Controls info
        ])
        .split(size);

    // Render Tabs Header
    let titles = vec![
        " F1 Dashboard ",
        " F2 Peers ",
        " F3 Devices ",
        " F4 OpLog ",
        " F5 SQL Query ",
        " F6 Log Viewer ",
        " F7 Help ",
    ];
    
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::BOTTOM).title(" Ahenk Sync Engine TUI "))
        .select(app.active_tab as usize)
        .style(Style::default().fg(Color::Cyan))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[0]);

    // Render current active tab content
    match app.active_tab {
        TabItem::Dashboard => render_dashboard(f, chunks[1], app),
        TabItem::Peers => render_peers(f, chunks[1], app),
        TabItem::Devices => render_devices(f, chunks[1], app),
        TabItem::OpLog => render_oplog(f, chunks[1], app),
        TabItem::DbQuery => render_query_pane(f, chunks[1], app),
        TabItem::LogViewer => render_log_viewer(f, chunks[1], app),
        TabItem::Help => render_help(f, chunks[1]),
    }

    // Render Footer/Status Bar
    let status_text = format!(
        " Daemon Status: {} | SQLite DB: {} ({:.2} MB) | Refreshing automatically (5s) | Press 'q' to quit",
        if app.daemon_running { format!("Running (PID {})", app.daemon_pid.unwrap_or(0)) } else { "Stopped".to_string() },
        app.config.db_path(),
        (app.db_size_bytes as f64) / (1024.0 * 1024.0)
    );
    let footer = Paragraph::new(status_text)
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(footer, chunks[2]);

    // Render dialog or message boxes overlays if necessary
    if app.show_dialog {
        render_dialog(f, app);
    }
}

fn render_dashboard(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    // Left card: Logo, info, stats
    let logo = r#"
    █████╗ ██╗  ██╗███████╗███╗   ██╗██╗  ██╗
   ██╔══██╗██║  ██║██╔════╝████╗  ██║██║  ██║
   ███████║███████║█████╗  ██╔██╗ ██║███████║
   ██╔══██║██╔══██║██╔══╝  ██║╚██╗██║██╔══██║
   ██║  ██║██║  ██║███████╗██║ ╚████║██║  ██║
   ╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═══╝╚═╝  ╚═╝
  CROSS-PLATFORM P2P DATABASE SYNCHRONIZATION
    "#;

    let user_name = app.config.user.as_ref().map(|u| u.name.as_str()).unwrap_or("Unregistered");
    let user_email = app.config.user.as_ref().map(|u| u.email.as_str()).unwrap_or("N/A");
    
    let left_text = format!(
        "{}\n\nUser Profile:\n  Name: {}\n  Email: {}\n\nSystem Metrics:\n  Daemon status: {}\n  Uptime: {}\n  Database size: {} bytes",
        logo,
        user_name,
        user_email,
        if app.daemon_running { "ACTIVE 🟢" } else { "INACTIVE 🔴" },
        app.daemon_uptime.map(|u| format!("{}s", u)).unwrap_or_else(|| "N/A".to_string()),
        app.db_size_bytes
    );

    let left_panel = Paragraph::new(left_text)
        .block(Block::default().borders(Borders::ALL).title(" Overview "))
        .style(Style::default().fg(Color::White));
    f.render_widget(left_panel, chunks[0]);

    // Right card: P2P configuration settings
    let right_text = format!(
        "Network settings:\n  Listen port: {}\n  Listen address: {}\n\nSync Engine configurations:\n  mDNS discovery: {}\n  Relay support: {}\n  Heartbeat interval: {}s\n  Max Message Size: {} bytes\n\nBootstrap Nodes:\n{}\n\nRelay Servers:\n{}",
        app.config.network.listen_port,
        app.config.network.listen_address,
        if app.config.sync.enable_mdns { "Enabled" } else { "Disabled" },
        if app.config.sync.enable_relay { "Enabled" } else { "Disabled" },
        app.config.sync.heartbeat_interval_secs,
        app.config.sync.max_message_size,
        if app.config.network.bootstrap_nodes.is_empty() { "  None configured".to_string() } else { app.config.network.bootstrap_nodes.iter().map(|n| format!("  - {}", n)).collect::<Vec<_>>().join("\n") },
        if app.config.network.relay_servers.is_empty() { "  None configured".to_string() } else { app.config.network.relay_servers.iter().map(|n| format!("  - {}", n)).collect::<Vec<_>>().join("\n") }
    );

    let right_panel = Paragraph::new(right_text)
        .block(Block::default().borders(Borders::ALL).title(" P2P / Network Settings "))
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(right_panel, chunks[1]);
}

fn render_peers(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let header_cells = ["Peer ID", "Multiaddress", "Last Seen"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows = app.peers.iter().map(|peer| {
        let cells = peer.iter().map(|val| Cell::from(val.as_str()));
        Row::new(cells).height(1)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(50),
            Constraint::Percentage(20),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Connected Peers [Press 'a' to add a peer] "));
    
    f.render_widget(table, area);
}

fn render_devices(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let header_cells = ["Device ID", "Type", "Name", "Authorized At"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows = app.devices.iter().map(|dev| {
        let cells = dev.iter().map(|val| Cell::from(val.as_str()));
        Row::new(cells).height(1)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Authorized Devices "));
    
    f.render_widget(table, area);
}

fn render_oplog(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let header_cells = ["Op ID", "Table", "Operation", "Timestamp"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows = app.oplog_entries.iter().map(|entry| {
        let cells = entry.iter().map(|val| Cell::from(val.as_str()));
        Row::new(cells).height(1)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(15),
            Constraint::Percentage(35),
            Constraint::Percentage(20),
            Constraint::Percentage(30),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Recent Operation Logs (OpLog) "));
    
    f.render_widget(table, area);
}

fn render_query_pane(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Input query box
            Constraint::Min(5),    // Table output
        ])
        .split(area);

    let query_box = Paragraph::new(app.query_input.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" SQL Query Prompt [Press Enter to run query, Esc to exit editing] ")
                .border_style(if app.input_mode == InputMode::EditingQuery {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                }),
        );
    f.render_widget(query_box, chunks[0]);

    if let Some(err) = &app.query_error {
        let error_msg = Paragraph::new(err.as_str())
            .block(Block::default().borders(Borders::ALL).title(" Query Error "))
            .style(Style::default().fg(Color::Red));
        f.render_widget(error_msg, chunks[1]);
    } else {
        let header_cells = app.query_headers.iter().map(|h| {
            Cell::from(h.as_str()).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        });
        let header = Row::new(header_cells).height(1).bottom_margin(1);

        let rows = app.query_results.iter().map(|row| {
            let cells = row.iter().map(|val| Cell::from(val.as_str()));
            Row::new(cells).height(1)
        });

        // dynamically create constraints
        let col_width_pct = if app.query_headers.is_empty() {
            100
        } else {
            100 / app.query_headers.len()
        };
        let constraints = vec![Constraint::Percentage(col_width_pct as u16); app.query_headers.len()];

        let table = Table::new(rows, constraints)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(" SQL Results "));
        
        f.render_widget(table, chunks[1]);
    }
}

fn render_log_viewer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let title = format!(
        " Live Tail: {} [Press Up/Down keys to scroll] ",
        app.config.logging.file
    );
    
    // Filter and colorize log levels
    let display_lines: Vec<Line> = app.log_lines
        .iter()
        .skip(app.log_scroll)
        .map(|line| {
            let mut span_style = Style::default().fg(Color::White);
            if line.contains("ERROR") || line.contains("err") {
                span_style = Style::default().fg(Color::Red);
            } else if line.contains("WARN") {
                span_style = Style::default().fg(Color::Yellow);
            } else if line.contains("INFO") {
                span_style = Style::default().fg(Color::Green);
            } else if line.contains("DEBUG") {
                span_style = Style::default().fg(Color::Blue);
            }
            Line::from(Span::styled(line.clone(), span_style))
        })
        .collect();

    let logs = Paragraph::new(display_lines)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(logs, area);
}

fn render_help(f: &mut ratatui::Frame, area: Rect) {
    let help_text = r#"
    AHENK SYNC ENGINE TERMINAL USER INTERFACE
    -------------------------------------------
    
    Keyboard Controls:
    
    [F1]        Go to Dashboard Panel (Overview & Networking Configs)
    [F2]        Go to Peers Panel (View & add multiaddress peers)
    [F3]        Go to Devices Panel (List authorized devices)
    [F4]        Go to Oplog Panel (Watch database changes feed)
    [F5]        Go to SQL Editor (Run SQLite queries interactively)
    [F6]        Go to Log Viewer (Tail and scroll log file output)
    [F7]        Show this Help Panel
    
    Panel-specific controls:
    
    - In Peers Panel:
      Press [a] to bring up dialog, input Multiaddress, and press Enter to save.
      
    - In SQL Editor:
      Type SQL queries directly. Press [Enter] to run. Press [Esc] to exit insert mode.
      
    - In Log Viewer:
      Press [Up Arrow] or [Down Arrow] keys to scroll log output.
      
    Global commands:
    - Press [r] to force refresh all panels manually.
    - Press [q] to quit TUI app immediately.
    
    "#;

    let panel = Paragraph::new(help_text)
        .block(Block::default().borders(Borders::ALL).title(" Controls & Help "))
        .style(Style::default().fg(Color::White));
    f.render_widget(panel, area);
}

fn render_dialog(f: &mut ratatui::Frame, app: &App) {
    let size = f.size();
    
    // Draw background block to cover main area
    let block = Block::default()
        .title(" Add Multiaddress Peer ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
        
    let area = centered_rect(60, 20, size);
    f.render_widget(Clear, area); // Clears the background
    
    let dialog_text = format!(
        "\nEnter Peer Multiaddress:\n(e.g., /ip4/127.0.0.1/tcp/49293/p2p/Qm...)\n\n> {}\n\n[Press Enter to confirm, Esc to close]",
        app.generic_input
    );
    
    let paragraph = Paragraph::new(dialog_text)
        .block(block)
        .style(Style::default().fg(Color::White));
    f.render_widget(paragraph, area);
}

/// Helper function to create a centered rect
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
