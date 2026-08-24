// DirectShell — Universal Application Control Through the Accessibility Layer
// Linux Version with AT-SPI2 support and X11 overlay
// Copyright (C) 2026  Martin Gehrken (IamLumae)
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::SeqCst};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use x11rb::connection::Connection as _;
use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ClientMessageData, ClientMessageEvent, ClipOrdering,
    ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ExposeEvent,
    InputFocus, Point, PropMode, Rectangle, SelectionNotifyEvent, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

// ── Colors (COLORREF = 0x00BBGGRR) ──────────────────
const INVIS: u32 = 0x00FF00FF;
const TOP_CLR: u32 = 0x00827873;
const SIDE_CLR: u32 = 0x00736964;
const BOT_CLR: u32 = 0x005F5550;
const HL_CLR: u32 = 0x00D7CDC8;
const SH_CLR: u32 = 0x00413732;
const ICON_CLR: u32 = 0x00D0D0D0;
const CLOSE_BG: u32 = 0x004040C0;

const DEFAULT_TOP_H: i32 = 20;
const SIDE_W: i32 = 4;
const FALLBACK_BTN_X: i32 = 140;
const SNAP_THRESH: f64 = 0.20;
const TIMER_MS: u64 = 16;
const ANIM_MS: u64 = 33;
const LIGHT_PERIOD: f64 = 3000.0;
const LIGHT_LEN: f64 = 120.0;
const LIGHT_STEPS: i32 = 24;
const INIT_W: i32 = 500;
const INIT_H: i32 = 350;
const TREE_MS: u64 = 500;
const INJECT_MS: u64 = 30;
const ENUM_MS: u64 = 2000;
const SNAP_REQ_MS: u64 = 200;
const MAX_DEPTH: i32 = 30;
const MAX_NODES: usize = 20000;
const STREAM_BATCH: i32 = 200;
// ── Paths resolved relative to the executable, not CWD ───────────────────
// All ds_profiles/* files are always placed next to the binary so that
// DirectShell works correctly regardless of the working directory from
// which it (or an MCP client) is launched.

fn base_dir() -> std::path::PathBuf {
    // std::env::current_exe() follows symlinks; unwrap is safe at runtime
    // because the process cannot be running without a valid exe path.
    let exe = std::env::current_exe()
        .expect("cannot resolve executable path");
    exe.parent()
        .expect("executable has no parent directory")
        .join("ds_profiles")
}

/// Absolute path for a file inside ds_profiles/.
/// Use `prof("")` (empty name) to get the directory itself.
fn prof(name: &str) -> String {
    if name.is_empty() {
        base_dir().to_string_lossy().into_owned()
    } else {
        base_dir().join(name).to_string_lossy().into_owned()
    }
}

// Keep these as compile-time identifiers for the *names* only (no longer
// used as paths directly — call prof("name") to get the real path).
const ACTIVE_NAME:       &str = "is_active";
const LOG_NAME:          &str = "directshell.log";
const WINDOWS_NAME:      &str = "windows.json";
const CLIP_OUT_NAME:     &str = "clipboard_out";
const SNAP_REQUEST_NAME: &str = "snap_request";
const SNAP_RESULT_NAME:  &str = "snap_result";
const OVERLAY_MODE_NAME: &str = "overlay_mode";
const LOCK_NAME:         &str = "directshell.lock";

// ── Logging ──────────────────────────────────────────
static LOG_BUF: Mutex<Option<Vec<String>>> = Mutex::new(None);
const LOG_MAX: usize = 100;

fn log(msg: &str) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = ts.as_secs();
    let line = format!(
        "[{:02}:{:02}:{:02}.{:03}] {}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60,
        ts.subsec_millis(),
        msg
    );
    println!("{}", line);
    let mut guard = LOG_BUF.lock().unwrap();
    let buf = guard.get_or_insert_with(Vec::new);
    buf.push(line);
    while buf.len() > LOG_MAX {
        buf.remove(0);
    }
    let content: String =
        buf.iter().map(|l| l.as_str()).collect::<Vec<_>>().join("\n") + "\n";
    drop(guard);
    let _ = fs::write(prof(LOG_NAME), content);
}

// ── Global State ─────────────────────────────────────
static TARGET_HW: AtomicU32 = AtomicU32::new(0);
static TARGET_PID: AtomicU32 = AtomicU32::new(0);
static IS_SNAPPED: AtomicBool = AtomicBool::new(false);
static TREE_BUSY: AtomicBool = AtomicBool::new(false);
static CURRENT_DB: Mutex<String> = Mutex::new(String::new());
static LAST_X: AtomicI32 = AtomicI32::new(0);
static LAST_Y: AtomicI32 = AtomicI32::new(0);
static LAST_W: AtomicI32 = AtomicI32::new(0);
static LAST_H: AtomicI32 = AtomicI32::new(0);
static BTN_OFF_X: AtomicI32 = AtomicI32::new(FALLBACK_BTN_X);
static DYN_TOP_H: AtomicI32 = AtomicI32::new(DEFAULT_TOP_H);
static DS_HWND: AtomicU32 = AtomicU32::new(0);
static AGENT_MODE: AtomicBool = AtomicBool::new(false);
static OVERLAY_SHOWN: AtomicBool = AtomicBool::new(false);
static LAST_CLICK_X: AtomicI32 = AtomicI32::new(-1);
static LAST_CLICK_Y: AtomicI32 = AtomicI32::new(-1);
static EXIT_FLAG: AtomicBool = AtomicBool::new(false);
static START_TIME: OnceLock<Instant> = OnceLock::new();

fn tgt() -> u32 {
    TARGET_HW.load(SeqCst)
}
fn snapped() -> bool {
    IS_SNAPPED.load(SeqCst)
}
fn top_h() -> i32 {
    DYN_TOP_H.load(SeqCst)
}
fn save(x: i32, y: i32, w: i32, h: i32) {
    LAST_X.store(x, SeqCst);
    LAST_Y.store(y, SeqCst);
    LAST_W.store(w, SeqCst);
    LAST_H.store(h, SeqCst);
}
fn saved() -> (i32, i32, i32, i32) {
    (
        LAST_X.load(SeqCst),
        LAST_Y.load(SeqCst),
        LAST_W.load(SeqCst),
        LAST_H.load(SeqCst),
    )
}

fn db_name_from_title(title: &str) -> String {
    let app = title.rsplit(&['\u{2013}', '\u{2014}'][..]).next().unwrap_or(title);
    let app = app.rsplit(" - ").next().unwrap_or(app).trim();
    let clean: String = app
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let clean = clean.trim_matches('_');
    let name = if clean.is_empty() { "unknown" } else { clean };
    format!("{}/{}.db", prof(""), name)
}

fn get_db_path() -> String {
    CURRENT_DB.lock().unwrap().clone()
}
fn set_db_path(path: &str) {
    *CURRENT_DB.lock().unwrap() = path.to_string();
}

fn write_active_status(db_path: &str) {
    let content = if db_path.is_empty() {
        "none\n".to_string()
    } else {
        let base = db_path.trim_end_matches(".db");
        let app = base.rsplit('/').next().unwrap_or("unknown");
        format!("{}\n{}.a11y\n{}.snap\n", app, base, base)
    };
    let _ = fs::write(prof(ACTIVE_NAME), content);
}

fn anim_t() -> f64 {
    let ms = START_TIME.get_or_init(Instant::now).elapsed().as_millis() as f64;
    (ms % LIGHT_PERIOD) / LIGHT_PERIOD
}

fn overlap(al: i32, at: i32, ar: i32, ab: i32, bl: i32, bt: i32, br: i32, bb: i32) -> f64 {
    let ox = (ar.min(br) - al.max(bl)).max(0) as f64;
    let oy = (ab.min(bb) - at.max(bt)).max(0) as f64;
    let area = (ar - al) as f64 * (ab - at) as f64;
    if area > 0.0 { ox * oy / area } else { 0.0 }
}

fn lerp_clr(a: u32, b: u32, t: f64) -> u32 {
    let mix = |av: u32, bv: u32| -> u32 {
        (av as f64 + (bv as f64 - av as f64) * t).round() as u32
    };
    mix(a & 0xFF, b & 0xFF)
        | (mix((a >> 8) & 0xFF, (b >> 8) & 0xFF) << 8)
        | (mix((a >> 16) & 0xFF, (b >> 16) & 0xFF) << 16)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\x20' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// COLORREF (0x00BBGGRR) → X11 TrueColor pixel (RGB)
fn px(colorref: u32) -> u32 {
    let r = colorref & 0xFF;
    let g = (colorref >> 8) & 0xFF;
    let b = (colorref >> 16) & 0xFF;
    r << 16 | g << 8 | b
}

// ═════════════════════════════════════════════════════
// SQLite
// ═════════════════════════════════════════════════════
fn init_db(db_path: &str) -> Option<Connection> {
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("init_db: FAILED: {e}"));
            return None;
        }
    };
    let av: i32 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap_or(0);
    if av != 1 {
        let _ = conn.execute_batch("PRAGMA auto_vacuum=FULL; VACUUM;");
    }
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;");
    let _ = conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT
        );
        CREATE TABLE IF NOT EXISTS elements (
            id            INTEGER PRIMARY KEY,
            parent_id     INTEGER,
            depth         INTEGER,
            role          TEXT NOT NULL,
            name          TEXT,
            value         TEXT,
            automation_id TEXT,
            enabled       INTEGER DEFAULT 1,
            offscreen     INTEGER DEFAULT 0,
            x             INTEGER,
            y             INTEGER,
            w             INTEGER,
            h             INTEGER,
            actions       INTEGER DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_role      ON elements(role);
        CREATE INDEX IF NOT EXISTS idx_offscreen ON elements(offscreen);
        CREATE INDEX IF NOT EXISTS idx_visible   ON elements(offscreen, role) WHERE offscreen=0;
        CREATE TABLE IF NOT EXISTS inject (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            action TEXT DEFAULT 'text',
            text   TEXT NOT NULL,
            target TEXT DEFAULT '',
            done   INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS events (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp     INTEGER NOT NULL,
            event_type    TEXT NOT NULL,
            element_name  TEXT,
            element_role  TEXT,
            detail        TEXT,
            new_value     TEXT,
            summary       TEXT,
            consumed      INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS elements_prev (
            id            INTEGER PRIMARY KEY,
            parent_id     INTEGER,
            depth         INTEGER,
            role          TEXT NOT NULL,
            name          TEXT,
            value         TEXT,
            automation_id TEXT,
            enabled       INTEGER DEFAULT 1,
            offscreen     INTEGER DEFAULT 0,
            x             INTEGER,
            y             INTEGER,
            w             INTEGER,
            h             INTEGER,
            actions       INTEGER DEFAULT 0
        );
    ",
    );
    let _ = conn.execute_batch("ALTER TABLE inject ADD COLUMN target TEXT DEFAULT '';");
    let _ = conn.execute_batch("ALTER TABLE inject ADD COLUMN action TEXT DEFAULT 'text';");
    // NOTE: no DELETE FROM inject here — this runs on every tree-dump tick and
    // would race the INJECT thread, eating queued rows before they execute.
    log("init_db: OK");
    Some(conn)
}

struct StreamCtx<'a> {
    conn: &'a Connection,
    count: i64,
    batch: i32,
}

// ── File Generation (.snap / .a11y / .a11y.snap) ─────
fn input_tool(role: &str) -> Option<&'static str> {
    match role {
        "Edit" | "Document" => Some("keyboard"),
        "Button" | "Hyperlink" | "MenuItem" | "TabItem" | "ListItem"
        | "TreeItem" | "DataItem" | "SplitButton" => Some("click"),
        "CheckBox" | "RadioButton" => Some("toggle"),
        "ComboBox" => Some("select"),
        "Slider" => Some("slide"),
        "Spinner" => Some("spin"),
        _ => None,
    }
}

fn generate_snap(db_path: &str) {
    let snap_path = db_path.replace(".db", ".snap");
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
    let title: String = conn
        .query_row("SELECT value FROM meta WHERE key='window'", [], |r| r.get(0))
        .unwrap_or_default();
    let Ok(mut stmt) = conn.prepare(
        "SELECT role, name, automation_id, x, y, w, h FROM elements \
         WHERE enabled=1 AND offscreen=0 AND name IS NOT NULL AND name != '' \
         AND w > 0 AND h > 0 ORDER BY y, x",
    ) else {
        return;
    };
    let mut lines: Vec<String> = Vec::new();
    let snap_name = snap_path.split('/').last().unwrap_or("unknown");
    lines.push(format!("# {} — Generated by DirectShell", snap_name));
    lines.push(format!("# Window: {}", title));
    lines.push(String::new());
    let mut count = 0usize;
    if let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2).unwrap_or_default(),
            row.get::<_, i32>(3)?,
            row.get::<_, i32>(4)?,
            row.get::<_, i32>(5)?,
            row.get::<_, i32>(6)?,
        ))
    }) {
        for row in rows.flatten() {
            let (role, name, aid, x, y, w, h) = row;
            if let Some(tool) = input_tool(&role) {
                let mut line = format!("[{}] \"{}\" @ {},{} ({}x{})", tool, name, x, y, w, h);
                if !aid.is_empty() {
                    line.push_str(&format!(" id={}", aid));
                }
                lines.push(line);
                count += 1;
            }
        }
    }
    let content = lines.join("\n");
    let _ = fs::write(&snap_path, &content);
    log(&format!("snap: {} interactive elements → {}", count, snap_path));
}

fn generate_a11y(db_path: &str) {
    let a11y_path = db_path.replace(".db", ".a11y");
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
    let title: String = conn
        .query_row("SELECT value FROM meta WHERE key='window'", [], |r| r.get(0))
        .unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let a11y_name = a11y_path.split('/').last().unwrap_or("unknown");
    lines.push(format!("# {} — Screen Reader View (DirectShell)", a11y_name));
    lines.push(format!("# Window: {}", title));
    lines.push(String::new());

    lines.push("## Focus".to_string());
    lines.push("(none)".to_string());
    lines.push(String::new());

    lines.push("## Input Targets".to_string());
    if let Ok(mut stmt) = conn.prepare(
        "SELECT role, name, value, x, y, w, h FROM elements \
         WHERE enabled=1 AND offscreen=0 \
         AND name IS NOT NULL AND name != '' \
         AND w > 10 AND h > 10 \
         AND role IN ('Edit', 'Document', 'ComboBox') \
         ORDER BY y, x",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i32>(3)?,
                row.get::<_, i32>(4)?,
                row.get::<_, i32>(5)?,
                row.get::<_, i32>(6)?,
            ))
        }) {
            for row in rows.flatten() {
                let (role, name, value, x, y, w, h) = row;
                let tool = input_tool(&role).unwrap_or("keyboard");
                lines.push(format!("[{}] \"{}\" @ {},{} ({}x{})", tool, name, x, y, w, h));
                if let Some(ref v) = value {
                    if !v.is_empty() {
                        let preview = if v.len() > 100 { &v[..100] } else { v.as_str() };
                        lines.push(format!("  value: \"{}\"", preview));
                    }
                }
            }
        }
    }
    lines.push(String::new());

    lines.push("## Content".to_string());
    if let Ok(mut stmt) = conn.prepare(
        "SELECT name, value FROM elements \
         WHERE offscreen=0 \
         AND name IS NOT NULL AND name != '' \
         AND w > 20 AND h > 10 \
         AND role IN ('Text', 'Document', 'Hyperlink', 'Image', 'ListItem', 'TreeItem', 'DataItem', 'Group') \
         ORDER BY y, x",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        }) {
            for row in rows.flatten() {
                let (name, value) = row;
                if let Some(ref v) = value {
                    if !v.is_empty() && v != &name {
                        lines.push(format!("{} ({})", name, v));
                        continue;
                    }
                }
                lines.push(name);
            }
        }
    }

    let content = lines.join("\n");
    let _ = fs::write(&a11y_path, &content);
}

fn generate_a11y_snap(db_path: &str) {
    let snap_path = db_path.replace(".db", ".a11y.snap");
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
    let title: String = conn
        .query_row("SELECT value FROM meta WHERE key='window'", [], |r| r.get(0))
        .unwrap_or_default();
    let Ok(mut stmt) = conn.prepare(
        "SELECT role, name, x, y, w, h FROM elements \
         WHERE enabled=1 AND offscreen=0 \
         AND name IS NOT NULL AND name != '' \
         AND w > 10 AND h > 10 \
         ORDER BY y, x",
    ) else {
        return;
    };
    let mut lines: Vec<String> = Vec::new();
    let fname = snap_path.split('/').last().unwrap_or("unknown");
    lines.push(format!("# {} — Operable Elements (DirectShell)", fname));
    lines.push(format!("# Window: {}", title));
    lines.push("# Use 'target' column in inject table to aim at an element by name".to_string());
    lines.push(String::new());
    let mut idx = 0u32;
    if let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i32>(2)?,
            row.get::<_, i32>(3)?,
            row.get::<_, i32>(4)?,
            row.get::<_, i32>(5)?,
        ))
    }) {
        for row in rows.flatten() {
            let (role, name, x, y, w, h) = row;
            if let Some(tool) = input_tool(&role) {
                idx += 1;
                lines.push(format!("[{}] [{}] \"{}\" @ {},{} ({}x{})", idx, tool, name, x, y, w, h));
            }
        }
    }
    lines.push(String::new());
    lines.push(format!("# {} operable elements in viewport", idx));
    let content = lines.join("\n");
    let _ = fs::write(&snap_path, &content);
}

// ═════════════════════════════════════════════════════
// X11 Layer
// ═════════════════════════════════════════════════════
struct Atoms {
    net_client_list: u32,
    net_client_list_stacking: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    net_wm_state: u32,
    net_wm_state_hidden: u32,
    net_wm_state_above: u32,
    net_wm_state_skip_taskbar: u32,
    net_wm_state_skip_pager: u32,
    net_wm_window_type: u32,
    wt_dock: u32,
    wt_desktop: u32,
    wt_toolbar: u32,
    wt_menu: u32,
    wt_splash: u32,
    wt_notification: u32,
    wt_tooltip: u32,
    net_active_window: u32,
    net_moveresize_window: u32,
    wm_class: u32,
    wm_name: u32,
    wm_protocols: u32,
    wm_delete_window: u32,
    utf8_string: u32,
    clipboard: u32,
    targets: u32,
    ds_clip_prop: u32,
}

struct Keymap {
    map: HashMap<u32, (u8, u8)>,
    shift_code: u8,
    altgr_code: u8,
}

struct XState {
    conn: RustConnection,
    screen_num: usize,
    root: u32,
    overlay: u32,
    gc: u32,
    atoms: Atoms,
    keymap: Keymap,
}

static X: OnceLock<Mutex<XState>> = OnceLock::new();

/// Run a closure with exclusive X access. Returns None if X not initialized or call failed.
fn with_x<T>(f: impl FnOnce(&mut XState) -> Option<T>) -> Option<T> {
    let mut guard = X.get()?.lock().unwrap();
    f(&mut guard)
}

macro_rules! lx {
    ($closure:expr) => {
        with_x($closure)
    };
}

fn intern_all(conn: &RustConnection) -> Atoms {
    let mk = |name: &str| -> u32 {
        conn.intern_atom(false, name.as_bytes())
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
            .unwrap_or(0)
    };
    Atoms {
        net_client_list: mk("_NET_CLIENT_LIST"),
        net_client_list_stacking: mk("_NET_CLIENT_LIST_STACKING"),
        net_wm_name: mk("_NET_WM_NAME"),
        net_wm_pid: mk("_NET_WM_PID"),
        net_wm_state: mk("_NET_WM_STATE"),
        net_wm_state_hidden: mk("_NET_WM_STATE_HIDDEN"),
        net_wm_state_above: mk("_NET_WM_STATE_ABOVE"),
        net_wm_state_skip_taskbar: mk("_NET_WM_STATE_SKIP_TASKBAR"),
        net_wm_state_skip_pager: mk("_NET_WM_STATE_SKIP_PAGER"),
        net_wm_window_type: mk("_NET_WM_WINDOW_TYPE"),
        wt_dock: mk("_NET_WM_WINDOW_TYPE_DOCK"),
        wt_desktop: mk("_NET_WM_WINDOW_TYPE_DESKTOP"),
        wt_toolbar: mk("_NET_WM_WINDOW_TYPE_TOOLBAR"),
        wt_menu: mk("_NET_WM_WINDOW_TYPE_MENU"),
        wt_splash: mk("_NET_WM_WINDOW_TYPE_SPLASH"),
        wt_notification: mk("_NET_WM_WINDOW_TYPE_NOTIFICATION"),
        wt_tooltip: mk("_NET_WM_WINDOW_TYPE_TOOLTIP"),
        net_active_window: mk("_NET_ACTIVE_WINDOW"),
        net_moveresize_window: mk("_NET_MOVERESIZE_WINDOW"),
        wm_class: u32::from(AtomEnum::WM_CLASS),
        wm_name: u32::from(AtomEnum::WM_NAME),
        wm_protocols: mk("WM_PROTOCOLS"),
        wm_delete_window: mk("WM_DELETE_WINDOW"),
        utf8_string: mk("UTF8_STRING"),
        clipboard: mk("CLIPBOARD"),
        targets: mk("TARGETS"),
        ds_clip_prop: mk("DS_CLIP_PROP"),
    }
}

fn build_keymap(conn: &RustConnection) -> Keymap {
    let setup = &conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let count = max.saturating_sub(min).saturating_add(1);
    let syms = conn
        .get_keyboard_mapping(min, count)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.keysyms)
        .unwrap_or_default();
    let per = if count > 0 && !syms.is_empty() {
        (syms.len() / count as usize) as u8
    } else {
        0
    };
    let mut map = HashMap::new();
    for k in 0..count as usize {
        for c in 0..per as usize {
            let sym = syms[k * per as usize + c];
            if sym == 0 {
                continue;
            }
            map.entry(sym).or_insert(((min as usize + k) as u8, c as u8));
        }
    }
    let shift_code = map.get(&0xffe1).copied().unwrap_or((0, 0)).0;
    let altgr_code = map.get(&0xfe03).copied().unwrap_or((0, 0)).0;
    log(&format!(
        "keymap: {} syms (per={}), shift_code={}, 'a'->{:?}",
        map.len(),
        per,
        shift_code,
        map.get(&0x61)
    ));
    Keymap { map, shift_code, altgr_code }
}

pub fn x_connect() -> bool {
    let (conn, screen_num) = match x11rb::connect(None) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("X11 connect FAILED: {e}"));
            return false;
        }
    };
    let root = conn.setup().roots[screen_num].root;
    let atoms = intern_all(&conn);
    let keymap = build_keymap(&conn);
    let xs = XState {
        conn,
        screen_num,
        root,
        overlay: 0,
        gc: 0,
        atoms,
        keymap,
    };
    X.set(Mutex::new(xs)).is_ok()
}

// ── Property helpers ─────────────────────────────────
fn get_prop_raw(x: &XState, win: u32, atom: u32) -> Option<x11rb::protocol::xproto::GetPropertyReply> {
    if atom == 0 {
        return None;
    }
    let reply = x
        .conn
        .get_property(false, win, atom, AtomEnum::ANY, 0, 4096)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ == x11rb::NONE || reply.value_len == 0 {
        None
    } else {
        Some(reply)
    }
}

fn prop_u32s(x: &XState, win: u32, atom: u32) -> Vec<u32> {
    get_prop_raw(x, win, atom)
        .and_then(|r| r.value32().map(|it| it.collect::<Vec<u32>>()))
        .unwrap_or_default()
}

fn prop_string(x: &XState, win: u32, atom: u32) -> String {
    match get_prop_raw(x, win, atom) {
        Some(r) => String::from_utf8_lossy(&r.value).trim_end_matches('\0').to_string(),
        None => String::new(),
    }
}

// ── Window Info / Enumeration ────────────────────────
pub struct WinInfo {
    pub hwnd: u32,
    pub title: String,
    pub app: String,
    pub exe: String,
    pub pid: u32,
    pub geom: (i32, i32, i32, i32),
}

fn window_geom(x: &XState, win: u32) -> (i32, i32, i32, i32) {
    let geo = match x.conn.get_geometry(win).ok().and_then(|c| c.reply().ok()) {
        Some(g) => g,
        None => return (0, 0, 0, 0),
    };
    let t = match x
        .conn
        .translate_coordinates(win, x.root, 0, 0)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        Some(r) => (r.dst_x as i32, r.dst_y as i32),
        None => (0, 0),
    };
    (t.0, t.1, geo.width as i32, geo.height as i32)
}

fn is_hidden(x: &XState, win: u32) -> bool {
    if prop_u32s(x, win, x.atoms.net_wm_state).contains(&(x.atoms.net_wm_state_hidden)) {
        return true;
    }
    match x.conn.get_window_attributes(win).ok().and_then(|c| c.reply().ok()) {
        Some(a) => a.map_state == x11rb::protocol::xproto::MapState::UNVIEWABLE,
        None => true,
    }
}

fn bad_type(x: &XState, win: u32) -> bool {
    let wt = prop_u32s(x, win, x.atoms.net_wm_window_type);
    if wt.is_empty() {
        return false;
    }
    [
        x.atoms.wt_dock,
        x.atoms.wt_desktop,
        x.atoms.wt_toolbar,
        x.atoms.wt_menu,
        x.atoms.wt_splash,
        x.atoms.wt_notification,
        x.atoms.wt_tooltip,
    ]
    .iter()
    .any(|a| *a != 0 && wt.contains(a))
}

fn enumerate_windows(x: &XState) -> Vec<WinInfo> {
    let ds = DS_HWND.load(SeqCst);
    let clients = prop_u32s(x, x.root, x.atoms.net_client_list);
    let mut out = Vec::new();
    for w in clients {
        if w == ds || w == x.overlay || x.overlay == 0 && false {
            continue;
        }
        if bad_type(x, w) || is_hidden(x, w) {
            continue;
        }
        let mut title = prop_string(x, w, x.atoms.net_wm_name);
        if title.trim().is_empty() {
            title = prop_string(x, w, x.atoms.wm_name);
        }
        if title.trim().is_empty() {
            continue;
        }
        let cls_raw = get_prop_raw(x, w, x.atoms.wm_class)
            .map(|r| String::from_utf8_lossy(&r.value).to_string())
            .unwrap_or_default();
        let parts: Vec<&str> = cls_raw.split('\0').filter(|s| !s.is_empty()).collect();
        let exe = parts.last().copied().unwrap_or("").to_lowercase();
        let db_path = db_name_from_title(&title);
        let base_prefix = format!("{}/", prof(""));
        let app = db_path
            .trim_start_matches(base_prefix.as_str())
            .trim_end_matches(".db")
            .to_string();
        let pid = prop_u32s(x, w, x.atoms.net_wm_pid).first().copied().unwrap_or(0);
        let geom = window_geom(x, w);
        out.push(WinInfo { hwnd: w, title, app, exe, pid, geom });
    }
    out
}

fn enum_windows_to_json() {
    let json_opt = lx!(|xs: &mut XState| -> Option<String> {
        let wins = enumerate_windows(xs);
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let entries: Vec<String> = wins
            .iter()
            .map(|w| {
                format!(
                    r#"    {{"title":"{}","app":"{}","exe":"{}","hwnd":{}}}"#,
                    json_escape(&w.title),
                    json_escape(&w.app),
                    json_escape(&w.exe),
                    w.hwnd
                )
            })
            .collect();
        Some(format!(
            "{{\n  \"timestamp\":{},\n  \"windows\":[\n{}\n  ]\n}}",
            ts,
            entries.join(",\n")
        ))
    });
    if let Some(json) = json_opt {
        let _ = fs::write(prof(WINDOWS_NAME), json);
    }
}

fn find_best_overlap(rect: (i32, i32, i32, i32)) -> Option<(u32, String)> {
    lx!(|xs: &mut XState| -> Option<(u32, String)> {
        let wins = enumerate_windows(xs);
        let stacking = prop_u32s(xs, xs.root, xs.atoms.net_client_list_stacking);
        let (rl, rt, rw, rh) = rect;
        let rr = rl + rw;
        let rb = rt + rh;
        // best by overlap; ties broken by stacking so the window the user
        // actually SEES on top wins when several cover the drop point
        let mut best: Option<(f64, usize, u32, String)> = None;
        for w in wins {
            let (wl, wt, ww, wh) = w.geom;
            if ww == 0 || wh == 0 {
                continue;
            }
            let o = overlap(rl, rt, rr, rb, wl, wt, wl + ww, wt + wh);
            if o < SNAP_THRESH {
                continue;
            }
            let rank = stacking.iter().position(|&s| s == w.hwnd).unwrap_or(0);
            if best.as_ref().map(|(bo, br, _, _)| o > *bo || (o == *bo && rank > *br)).unwrap_or(true)
            {
                best = Some((o, rank, w.hwnd, w.title));
            }
        }
        best.map(|(_, _, hwnd, title)| (hwnd, title))
    })
}

// ── Overlay Painting ─────────────────────────────────
fn fill(x: &XState, color: u32, rx: i16, ry: i16, rw: u16, rh: u16) {
    let _ = x.conn.change_gc(x.gc, &ChangeGCAux::new().foreground(px(color)));
    let _ = x.conn.poly_fill_rectangle(
        x.overlay,
        x.gc,
        &[Rectangle { x: rx, y: ry, width: rw, height: rh }],
    );
}

fn shape_ring(x: &XState, w: i32, h: i32) {
    let th = top_h().max(4) as i16;
    let sw = SIDE_W as i16;
    let hw = w as i16;
    let hh = h as i16;
    let r: i16 = 8.min(th);
    let mut rects: Vec<Rectangle> = Vec::new();
    // Top bar with chamfered corners
    rects.push(Rectangle { x: r, y: 0, width: (hw - 2 * r).max(1) as u16, height: th as u16 });
    rects.push(Rectangle { x: 0, y: r, width: hw as u16, height: (th - r).max(0) as u16 });
    rects.push(Rectangle { x: 0, y: 0, width: r as u16, height: (2 * r).min(th) as u16 });
    rects.push(Rectangle { x: hw - r, y: 0, width: r as u16, height: (2 * r).min(th) as u16 });
    // Sides
    rects.push(Rectangle { x: 0, y: th, width: sw as u16, height: (hh - th - sw).max(0) as u16 });
    rects.push(Rectangle {
        x: hw - sw,
        y: th,
        width: sw as u16,
        height: (hh - th - sw).max(0) as u16,
    });
    // Bottom
    rects.push(Rectangle { x: 0, y: hh - sw, width: hw as u16, height: sw as u16 });

    let _ = x
        .conn
        .shape_rectangles(
            SO::SET,
            SK::BOUNDING,
            ClipOrdering::UNSORTED,
            x.overlay,
            0,
            0,
            &rects,
        );
    let _ = x.conn.shape_rectangles(
        SO::SET,
        SK::INPUT,
        ClipOrdering::UNSORTED,
        x.overlay,
        0,
        0,
        &rects,
    );
}

fn draw_light(x: &XState, w: i32, h: i32) {
    let th = top_h();
    let t = anim_t();
    let wf = w as f64;
    let sh = (h - th) as f64;
    let perim = 2.0 * wf + 2.0 * sh;
    if perim <= 0.0 {
        return;
    }
    let center = t * perim;
    let half = LIGHT_LEN / 2.0;
    let edges: [(f64, f64, u32, i32); 4] = [
        (0.0, wf, TOP_CLR, 0),
        (wf, wf + sh, SIDE_CLR, 1),
        (wf + sh, 2.0 * wf + sh, BOT_CLR, 2),
        (2.0 * wf + sh, perim, SIDE_CLR, 3),
    ];
    for &seg_center in &[center, center + perim, center - perim] {
        for &(e_s, e_e, bg_clr, edge_idx) in &edges {
            let s = (seg_center - half).max(e_s);
            let e = (seg_center + half).min(e_e);
            if s >= e {
                continue;
            }
            let edge_len = e_e - e_s;
            if edge_len <= 0.0 {
                continue;
            }
            let seg_len = e - s;
            let step_w = seg_len / LIGHT_STEPS as f64;
            for j in 0..LIGHT_STEPS {
                let ss = s + j as f64 * step_w;
                let se = s + (j + 1) as f64 * step_w;
                let mid = (ss + se) / 2.0;
                let dist = ((mid - seg_center) / half).abs().min(1.0);
                let c = (dist * std::f64::consts::FRAC_PI_2).cos();
                let intensity = c * c;
                if intensity < 0.02 {
                    continue;
                }
                let clr = lerp_clr(bg_clr, HL_CLR, intensity);
                let f0 = (ss - e_s) / edge_len;
                let f1 = (se - e_s) / edge_len;
                let rect = match edge_idx {
                    0 => Rectangle {
                        x: (f0 * wf) as i16,
                        y: 0,
                        width: (((f1 - f0) * wf) as i16 + 1).max(1) as u16,
                        height: th as u16,
                    },
                    1 => Rectangle {
                        x: (w - SIDE_W) as i16,
                        y: th as i16 + (f0 * sh) as i16,
                        width: SIDE_W as u16,
                        height: (((f1 - f0) * sh) as i16 + 1).max(1) as u16,
                    },
                    2 => Rectangle {
                        x: ((w as f64 - f1 * wf)) as i16,
                        y: (h - SIDE_W) as i16,
                        width: (((f1 - f0) * wf) as i16 + 1).max(1) as u16,
                        height: SIDE_W as u16,
                    },
                    _ => Rectangle {
                        x: 0,
                        y: ((h as f64) - f1 * sh) as i16,
                        width: SIDE_W as u16,
                        height: (((f1 - f0) * sh) as i16 + 1).max(1) as u16,
                    },
                };
                let _ = x.conn.change_gc(x.gc, &ChangeGCAux::new().foreground(px(clr)));
                let _ = x.conn.poly_fill_rectangle(x.overlay, x.gc, &[rect]);
            }
        }
    }
}

fn draw_line(x: &XState, color: u32, pts: &[Point]) {
    let _ = x.conn.change_gc(x.gc, &ChangeGCAux::new().foreground(px(color)));
    let _ = x
        .conn
        .poly_line(x11rb::protocol::xproto::CoordMode::ORIGIN, x.overlay, x.gc, pts);
}

fn close_area(w: i32) -> (i32, i32, i32, i32) {
    let th = top_h();
    let btn_h = (th - 2).max(4);
    let btn_w = (btn_h as f64 * 1.4) as i32;
    let x = w - btn_w - 1;
    let y = 1;
    (x, y, x + btn_w, y + btn_h)
}

fn btn_area(w: i32) -> (i32, i32, i32, i32) {
    let off = BTN_OFF_X.load(SeqCst).clamp(30, w - 40);
    let th = top_h();
    let btn_h = (th - 2).max(4);
    let btn_w = (btn_h as f64 * 1.2) as i32;
    let x = w - off - btn_w - 2;
    let y = 1;
    (x.max(SIDE_W), y, (w - off - 2).max(x + 4), y + btn_h)
}

fn repaint(x: &XState) {
    let geo = match x.conn.get_geometry(x.overlay).ok().and_then(|c| c.reply().ok()) {
        Some(g) => g,
        None => return,
    };
    let (w, h) = (geo.width as i32, geo.height as i32);
    if w < 10 || h < 10 {
        return;
    }
    let th = top_h();

    fill(x, TOP_CLR, 0, 0, w as u16, th as u16);
    fill(x, SIDE_CLR, 0, th as i16, SIDE_W as u16, (h - th - SIDE_W).max(0) as u16);
    fill(x, SIDE_CLR, (w - SIDE_W) as i16, th as i16, SIDE_W as u16, (h - th - SIDE_W).max(0) as u16);
    fill(x, BOT_CLR, 0, (h - SIDE_W) as i16, w as u16, SIDE_W as u16);

    draw_line(
        x,
        HL_CLR,
        &[Point { x: 8, y: 1 }, Point { x: w as i16 - 8, y: 1 }],
    );
    draw_line(
        x,
        SH_CLR,
        &[Point { x: 0, y: h as i16 - 1 }, Point { x: w as i16, y: h as i16 - 1 }],
    );

    if !snapped() {
        draw_light(x, w, h);
        let (cl, ct, cr, cb) = close_area(w);
        fill(x, CLOSE_BG, cl as i16, ct as i16, (cr - cl) as u16, (cb - ct) as u16);
        let cx = (cl + (cr - cl) / 2) as i16;
        let cy = (ct + (cb - ct) / 2) as i16;
        let rad = ((cb - ct).min(cr - cl) / 2 - 4).max(2) as i16;
        draw_line(
            x,
            ICON_CLR,
            &[Point { x: cx - rad, y: cy - rad }, Point { x: cx + rad + 1, y: cy + rad + 1 }],
        );
        draw_line(
            x,
            ICON_CLR,
            &[Point { x: cx + rad, y: cy - rad }, Point { x: cx - rad - 1, y: cy + rad + 1 }],
        );
    } else {
        let (bl, bt, br, bb) = btn_area(w);
        let bw = br - bl;
        let bh = bb - bt;
        fill(x, lerp_clr(TOP_CLR, HL_CLR, 0.08), bl as i16, bt as i16, bw as u16, bh as u16);
        let cx = (bl + bw / 2) as i16;
        let cy = (bt + bh / 2) as i16;
        let radius = (bh.min(bw) / 2 - 4).max(1) as i16;
        let oct: Vec<Point> = (0..=8)
            .map(|i| {
                let ang = (i as f64) * std::f64::consts::FRAC_PI_4;
                Point {
                    x: cx + (radius as f64 * ang.cos()) as i16,
                    y: cy + (radius as f64 * ang.sin()) as i16,
                }
            })
            .collect();
        draw_line(x, ICON_CLR, &oct);
        let cr2 = radius.saturating_sub(2);
        draw_line(x, ICON_CLR, &[Point { x: cx - cr2, y: cy }, Point { x: cx + cr2 + 1, y: cy }]);
        draw_line(x, ICON_CLR, &[Point { x: cx, y: cy - cr2 }, Point { x: cx, y: cy + cr2 + 1 }]);
    }
    let _ = x.conn.flush();
}

// ── Root Messages ────────────────────────────────────
fn send_root_msg(x: &XState, msg_type: u32, win: u32, data: [i32; 5], mask: EventMask) {
    let d = ClientMessageData::from([
        data[0] as u32,
        data[1] as u32,
        data[2] as u32,
        data[3] as u32,
        data[4] as u32,
    ]);
    let ev = ClientMessageEvent::new(32, win, msg_type, d);
    let _ = x.conn.send_event(false, x.root, mask, ev);
    let _ = x.conn.flush();
}

fn set_always_on_top(x: &XState, win: u32) {
    send_root_msg(
        x,
        x.atoms.net_wm_state,
        win,
        [
            1,
            x.atoms.net_wm_state_above as i32,
            x.atoms.net_wm_state_skip_taskbar as i32,
            x.atoms.net_wm_state_skip_pager as i32,
            0,
        ],
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
    );
}

fn activate_target(x: &XState, win: u32) {
    send_root_msg(
        x,
        x.atoms.net_active_window,
        win,
        [1, 0, 0, 0, 0],
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
    );
    // Belt and braces: point the X server's input focus at the target so
    // XTEST keystrokes land there even if the WM ignores the EWMH message.
    let _ = x
        .conn
        .set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME);
    let _ = x.conn.flush();
}

fn move_target_win(x: &XState, win: u32, tx: i32, ty: i32, tw: i32, th: i32) {
    // Flags: x(bit8)=0x100 y(bit9)=0x200 w(bit10)=0x400 h(bit11)=0x800, source=application(bits12-14)=0x1000
    let flags = 0x100 | 0x200 | 0x400 | 0x800 | 0x1000;
    send_root_msg(
        x,
        x.atoms.net_moveresize_window,
        win,
        [flags, tx, ty, tw, th],
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
    );
}

// ── Overlay Lifecycle ────────────────────────────────
pub fn create_overlay() -> bool {
    lx!(|xs: &mut XState| -> Option<()> {
        let screen = &xs.conn.setup().roots[xs.screen_num];
        let vis = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)?;
        if vis.class != x11rb::protocol::xproto::VisualClass::TRUE_COLOR {
            log(&format!("warning: root visual class={:?} — colors may look wrong", vis.class));
        }
        let win = xs.conn.generate_id().ok()?;
        let gc = xs.conn.generate_id().ok()?;
        xs.overlay = win;
        xs.gc = gc;

        let win_aux = CreateWindowAux::new()
            .background_pixel(px(INVIS))
            .border_pixel(0)
            // unmanaged by any WM — otherwise xfwm4 wraps it in a decorated
            // frame, steals titlebar drags, and the snap gesture never fires
            .override_redirect(1u32)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::STRUCTURE_NOTIFY,
            )
            .save_under(1);
        xs.conn
            .create_window(
                screen.root_depth,
                xs.overlay,
                xs.root,
                200,
                200,
                INIT_W as u16,
                INIT_H as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &win_aux,
            )
            .ok()?;

        let class_data: Vec<u8> = b"DirectShell\0DirectShell\0".to_vec();
        xs.conn
            .change_property(
                PropMode::REPLACE,
                xs.overlay,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                8,
                (class_data.len()) as u32,
                &class_data,
            )
            .ok()?;
        xs.conn
            .change_property(
                PropMode::REPLACE,
                xs.overlay,
                xs.atoms.net_wm_name,
                xs.atoms.utf8_string,
                8,
                "DirectShell".len() as u32,
                b"DirectShell",
            )
            .ok()?;
        // Graceful close protocol
        xs.conn
            .change_property32(
                PropMode::REPLACE,
                xs.overlay,
                xs.atoms.wm_protocols,
                AtomEnum::ATOM,
                &[xs.atoms.wm_delete_window],
            )
            .ok()?;
        // WM_HINTS: input=False — the overlay must NEVER take keyboard focus
        // (flags bit0 InputHint=1, input=0, rest unused)
        let hints: Vec<u32> = vec![1, 0, 0, 0, 0, 0, 0, 0, 0];
        xs.conn
            .change_property32(
                PropMode::REPLACE,
                xs.overlay,
                AtomEnum::WM_HINTS,
                AtomEnum::WM_HINTS,
                &hints,
            )
            .ok()?;

        xs.conn
            .create_gc(
                xs.gc,
                xs.overlay,
                &CreateGCAux::new().foreground(px(TOP_CLR)).graphics_exposures(0),
            )
            .ok()?;

        set_always_on_top(xs, xs.overlay);
        xs.conn.map_window(xs.overlay).ok()?;
        xs.conn.flush().ok()?;

        DS_HWND.store(xs.overlay, SeqCst);
        OVERLAY_SHOWN.store(true, SeqCst);
        shape_ring(xs, INIT_W, INIT_H);
        log(&format!("Window created: 0x{:X}", xs.overlay));
        Some(())
    })
    .is_some()
}

fn overlay_geom() -> (i32, i32, i32, i32) {
    lx!(|xs: &mut XState| -> Option<(i32, i32, i32, i32)> { Some(window_geom(xs, xs.overlay)) })
        .unwrap_or((0, 0, 0, 0))
}

fn place_overlay(x: i32, y: i32, w: i32, h: i32) {
    lx!(|xs: &mut XState| -> Option<()> {
        xs.conn
            .configure_window(
                xs.overlay,
                &ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(w.max(1) as u32)
                    .height(h.max(1) as u32),
            )
            .ok()?;
        shape_ring(xs, w, h);
        xs.conn.flush().ok()?;
        Some(())
    });
}

fn show_overlay(show: bool) {
    if OVERLAY_SHOWN.swap(show, SeqCst) == show {
        return;
    }
    lx!(|xs: &mut XState| -> Option<()> {
        if show {
            xs.conn.map_window(xs.overlay).ok()?;
        } else {
            xs.conn.unmap_window(xs.overlay).ok()?;
        }
        xs.conn.flush().ok()?;
        Some(())
    });
}

// ── Clipboard (X11 CLIPBOARD selection, native) ──────
static CLIP_WIN: AtomicU32 = AtomicU32::new(0);
static CLIP_DATA: Mutex<String> = Mutex::new(String::new());
static CLIP_REPLY: Mutex<Option<Vec<u8>>> = Mutex::new(None);
static CLIP_CV: Condvar = Condvar::new();

fn create_clip_window() {
    lx!(|xs: &mut XState| -> Option<()> {
        let win = xs.conn.generate_id().ok()?;
        let screen = &xs.conn.setup().roots[xs.screen_num];
        xs.conn
            .create_window(
                screen.root_depth,
                win,
                xs.root,
                -10,
                -10,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .ok()?;
        xs.conn.flush().ok()?;
        CLIP_WIN.store(win, SeqCst);
        Some(())
    });
}

fn clip_owned_by_us() -> bool {
    lx!(|xs: &mut XState| -> Option<bool> {
        let w = CLIP_WIN.load(SeqCst);
        if w == 0 {
            return Some(false);
        }
        let owner = xs
            .conn
            .get_selection_owner(xs.atoms.clipboard)
            .ok()?
            .reply()
            .ok()?
            .owner;
        Some(owner == w)
    })
    .unwrap_or(false)
}

fn set_clipboard(text: &str) -> bool {
    *CLIP_DATA.lock().unwrap() = text.to_string();
    if !clip_owned_by_us() {
        let ok = lx!(|xs: &mut XState| -> Option<()> {
            let w = CLIP_WIN.load(SeqCst);
            if w == 0 {
                return None;
            }
            xs.conn
                .set_selection_owner(w, xs.atoms.clipboard, x11rb::CURRENT_TIME)
                .ok()?
                .check()
                .ok()?;
            xs.conn.flush().ok()?;
            Some(())
        })
        .is_some();
        if !ok {
            log("clipboard: FAILED to take ownership");
            return false;
        }
    }
    log(&format!("clipboard: set {} bytes", text.len()));
    true
}

fn get_clipboard() -> Option<String> {
    if clip_owned_by_us() {
        return Some(CLIP_DATA.lock().unwrap().clone());
    }
    *CLIP_REPLY.lock().unwrap() = None;
    lx!(|xs: &mut XState| -> Option<()> {
        let w = CLIP_WIN.load(SeqCst);
        if w == 0 {
            return None;
        }
        xs.conn
            .convert_selection(
                w,
                xs.atoms.clipboard,
                xs.atoms.utf8_string,
                xs.atoms.ds_clip_prop,
                x11rb::CURRENT_TIME,
            )
            .ok()?;
        xs.conn.flush().ok()?;
        Some(())
    })?;
    let deadline = Instant::now() + Duration::from_millis(1200);
    let mut guard = CLIP_REPLY.lock().unwrap();
    while guard.is_none() {
        let left = deadline.checked_duration_since(Instant::now())?;
        let (g, _t) = CLIP_CV.wait_timeout(guard, left).ok()?;
        guard = g;
    }
    String::from_utf8(guard.take().unwrap()).ok()
}

// called from x_event_loop when the paste source answers our request
fn on_clip_notify(ev: &x11rb::protocol::xproto::SelectionNotifyEvent) {
    if ev.requestor != CLIP_WIN.load(SeqCst) || ev.property == 0 {
        return;
    }
    lx!(|xs: &mut XState| -> Option<()> {
        let r = xs
            .conn
            .get_property(false, ev.requestor, ev.property, 0u32, 0u32, u32::MAX)
            .ok()?
            .reply()
            .ok()?;
        *CLIP_REPLY.lock().unwrap() = Some(r.value);
        CLIP_CV.notify_all();
        Some(())
    });
}

// called from x_event_loop while we own the clipboard
fn on_selection_request(ev: &x11rb::protocol::xproto::SelectionRequestEvent) {
    lx!(|xs: &mut XState| -> Option<()> {
        let prop = if ev.property == 0 { ev.target } else { ev.property };
        if ev.target == xs.atoms.targets {
            let atoms_vec: [u32; 3] =
                [xs.atoms.targets, xs.atoms.utf8_string, u32::from(AtomEnum::STRING)];
            xs.conn
                .change_property32(
                    PropMode::REPLACE,
                    ev.requestor,
                    prop,
                    u32::from(AtomEnum::ATOM),
                    &atoms_vec,
                )
                .ok()?;
        } else {
            // serve anything string-ish (UTF8_STRING / STRING / TEXT) as bytes
            let data = CLIP_DATA.lock().unwrap().clone();
            xs.conn
                .change_property8(PropMode::REPLACE, ev.requestor, prop, ev.target, data.as_bytes())
                .ok()?;
        }
        xs.conn
            .send_event(
                false,
                ev.requestor,
                EventMask::NO_EVENT,
                &SelectionNotifyEvent {
                    response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: x11rb::CURRENT_TIME,
                    requestor: ev.requestor,
                    selection: ev.selection,
                    target: ev.target,
                    property: prop,
                },
            )
            .ok()?;
        xs.conn.flush().ok()?;
        Some(())
    });
}

// ── Drag Handling ────────────────────────────────────
struct Drag {
    start_px: i16,
    start_py: i16,
    orig: (i32, i32, i32, i32),
}

static DRAG: Mutex<Option<Drag>> = Mutex::new(None);

fn handle_button_press(ev: &x11rb::protocol::xproto::ButtonPressEvent) {
    let (wx, wy, ww, wh) = overlay_geom();
    if ww == 0 {
        return;
    }
    let lx = ev.event_x as i32;
    let ly = ev.event_y as i32;

    if ly < top_h() {
        if snapped() {
            let (bl, bt, br, bb) = btn_area(ww);
            if lx >= bl && lx <= br && ly >= bt && ly <= bb {
                do_unsnap();
                return;
            }
        } else {
            let (cl, ct, cr, cb) = close_area(ww);
            if lx >= cl && lx <= cr && ly >= ct && ly <= cb {
                log("close button pressed — exiting");
                EXIT_FLAG.store(true, SeqCst);
                return;
            }
        }
    }

    let grab_ok = lx!(|xs: &mut XState| -> Option<bool> {
        xs.conn
            .grab_pointer(
                false,
                xs.overlay,
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                x11rb::protocol::xproto::GrabMode::ASYNC,
                x11rb::protocol::xproto::GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                x11rb::CURRENT_TIME,
            )
            .ok()?
            .reply()
            .ok()?;
        Some(true)
    })
    .unwrap_or(false);

    if grab_ok {
        *DRAG.lock().unwrap() = Some(Drag {
            start_px: ev.root_x,
            start_py: ev.root_y,
            orig: (wx, wy, ww, wh),
        });
    }
}

fn handle_motion(ev: &x11rb::protocol::xproto::MotionNotifyEvent) {
    let drag_guard = DRAG.lock().unwrap();
    let drag = match &*drag_guard {
        Some(d) => d,
        None => return,
    };
    let dx = ev.root_x as i32 - drag.start_px as i32;
    let dy = ev.root_y as i32 - drag.start_py as i32;
    let nx = drag.orig.0 + dx;
    let ny = drag.orig.1 + dy;
    let (nw, nh) = (drag.orig.2, drag.orig.3);
    drop(drag_guard);
    place_overlay(nx, ny, nw, nh);
    save(nx, ny, nw, nh);
    if snapped() {
        let t = tgt();
        if t != 0 {
            lx!(|xs: &mut XState| -> Option<()> {
                move_target_win(xs, t, nx, ny, nw, nh);
                Some(())
            });
        }
    }
}

fn handle_button_release(_ev: &x11rb::protocol::xproto::ButtonReleaseEvent) {
    let was_dragging = DRAG.lock().unwrap().take().is_some();
    if !was_dragging {
        return;
    }
    lx!(|xs: &mut XState| -> Option<()> {
        let _ = xs.conn.ungrab_pointer(x11rb::CURRENT_TIME).ok()?.check();
        xs.conn.flush().ok()?;
        Some(())
    });
    if !snapped() {
        if let Some((target, title)) = find_best_overlap(overlay_geom()) {
            do_snap(target, &title);
        }
    }
}

// ── X Event Loop ─────────────────────────────────────
pub fn x_event_loop() {
    use x11rb::protocol::Event::*;
    loop {
        if EXIT_FLAG.load(SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        // Never hold the X mutex while blocking on the connection —
        // daemon threads need it every few ms. Poll instead of wait.
        let ev = {
            let guard = match X.get() {
                Some(g) => g.lock().unwrap(),
                None => break,
            };
            match guard.conn.poll_for_event() {
                Ok(e) => e,
                Err(_) => continue,
            }
        };
        let ev = match ev {
            Some(e) => e,
            None => continue,
        };
        match ev {
            Expose(ExposeEvent { count, .. }) => {
                if count == 0 {
                    lx!(|xs: &mut XState| -> Option<()> {
                        repaint(xs);
                        Some(())
                    });
                }
            }
            ButtonPress(ev) => handle_button_press(&ev),
            ButtonRelease(ev) => handle_button_release(&ev),
            MotionNotify(ev) => handle_motion(&ev),
            SelectionRequest(ev) => on_selection_request(&ev),
            SelectionNotify(ev) => on_clip_notify(&ev),
            SelectionClear(_) => {}
            ClientMessage(m) => {
                let del = lx!(|xs: &mut XState| -> Option<u32> { Some(xs.atoms.wm_delete_window) })
                    .unwrap_or(0);
                let proto =
                    lx!(|xs: &mut XState| -> Option<u32> { Some(xs.atoms.wm_protocols) }).unwrap_or(1);
                if m.type_ == proto && m.data.as_data32()[0] == del {
                    log("WM_DELETE received — exiting");
                    EXIT_FLAG.store(true, SeqCst);
                }
            }
            DestroyNotify(_) => break,
            _ => {}
        }
    }
    log("=== DirectShell EXIT ===");
    write_active_status("");
    std::process::exit(0);
}

// ═════════════════════════════════════════════════════
// Input Injection (XTEST)
// ═════════════════════════════════════════════════════
const XTEST_MOTION: u8 = 6;
const XTEST_PRESS: u8 = 4;
const XTEST_RELEASE: u8 = 5;
// Key events have their OWN XTEST event types — reusing the button ones makes
// the server treat keycodes as button numbers and silently drop them.
const XTEST_KEY_PRESS: u8 = 2;
const XTEST_KEY_RELEASE: u8 = 3;

fn fake(type_: u8, detail: u8, x: i16, y: i16) {
    lx!(|xs: &mut XState| -> Option<()> {
        xs.conn.xtest_fake_input(type_, detail, 0, xs.root, x, y, 0).ok()?;
        xs.conn.flush().ok()?;
        Some(())
    });
}

fn mouse_move_abs(x: i32, y: i32) {
    fake(XTEST_MOTION, 0, x.clamp(0, 32767) as i16, y.clamp(0, 32767) as i16);
}

fn mouse_click(cx: i32, cy: i32) {
    mouse_move_abs(cx, cy);
    std::thread::sleep(Duration::from_millis(15));
    fake(XTEST_PRESS, 1, 0, 0);
    std::thread::sleep(Duration::from_millis(15));
    fake(XTEST_RELEASE, 1, 0, 0);
    LAST_CLICK_X.store(cx, SeqCst);
    LAST_CLICK_Y.store(cy, SeqCst);
}

fn mouse_scroll(direction: &str) {
    let btn: u8 = match direction.to_lowercase().as_str() {
        "up" => 4,
        "down" => 5,
        "left" => 6,
        "right" => 7,
        _ => {
            log(&format!("scroll: unknown direction '{}'", direction));
            return;
        }
    };
    let (tx, ty, tw, th) = target_geom();
    if tw == 0 {
        return;
    }
    mouse_move_abs(tx + tw / 2, ty + th / 2);
    std::thread::sleep(Duration::from_millis(20));
    fake(XTEST_PRESS, btn, 0, 0);
    fake(XTEST_RELEASE, btn, 0, 0);
    log(&format!("scroll: {}", direction));
}

fn keysym_for_char(ch: char) -> u32 {
    let cp = ch as u32;
    if cp <= 0xFF {
        cp
    } else {
        0x0100_0000 | cp
    }
}

fn named_keysym(name: &str) -> Option<u32> {
    const F_KEYS: [u32; 12] = [
        0xffbe, 0xffbf, 0xffc0, 0xffc1, 0xffc2, 0xffc3, 0xffc4, 0xffc5, 0xffc6, 0xffc7, 0xffc8,
        0xffc9,
    ];
    let n = name.to_lowercase();
    Some(match n.as_str() {
        "enter" | "return" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "space" => 0x0020,
        "backspace" | "bs" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" | "ins" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "pgup" => 0xff55,
        "pagedown" | "pgdn" => 0xff56,
        "up" => 0xff52,
        "down" => 0xff54,
        "left" => 0xff51,
        "right" => 0xff53,
        "ctrl" | "control" => 0xffe3,
        "alt" | "menu" => 0xffe9,
        "shift" => 0xffe1,
        "win" | "super" | "lwin" | "meta" => 0xffeb,
        "printscreen" | "prtsc" => 0xff61,
        "scrolllock" => 0xff14,
        "pause" | "break" => 0xff13,
        "capslock" | "caps" => 0xffe5,
        "numlock" => 0xff7f,
        ";" | "semicolon" => 0x03b,
        "=" | "equals" => 0x03d,
        "," | "comma" => 0x02c,
        "-" | "minus" => 0x02d,
        "." | "period" => 0x02e,
        "/" | "slash" => 0x02f,
        "`" | "backtick" => 0x060,
        "[" | "lbracket" => 0x05b,
        "\\" | "backslash" => 0x05c,
        "]" | "rbracket" => 0x05d,
        "'" | "quote" => 0x027,
        other => {
            if let Some(idx) = other.strip_prefix('f').and_then(|d| d.parse::<usize>().ok()) {
                if (1..=12).contains(&idx) {
                    F_KEYS[idx - 1]
                } else {
                    return None;
                }
            } else if other.chars().count() == 1 {
                other.chars().next().unwrap() as u32
            } else {
                return None;
            }
        }
    })
}

fn press_keysym(x: &XState, sym: u32, down: bool) -> bool {
    let (code, col) = match x.keymap.map.get(&sym) {
        Some(e) => *e,
        None => return false,
    };
    let want_shift = col == 1 && x.keymap.shift_code != 0;
    let want_altgr = col >= 2 && x.keymap.altgr_code != 0;
    let t = if down { XTEST_KEY_PRESS } else { XTEST_KEY_RELEASE };
    if down && want_altgr {
        let _ = x.conn.xtest_fake_input(XTEST_KEY_PRESS, x.keymap.altgr_code, 0, x.root, 0, 0, 0);
    }
    if down && want_shift {
        let _ = x.conn.xtest_fake_input(XTEST_KEY_PRESS, x.keymap.shift_code, 0, x.root, 0, 0, 0);
    }
    let _ = x.conn.xtest_fake_input(t, code, 0, x.root, 0, 0, 0);
    if !down && want_shift {
        let _ = x.conn.xtest_fake_input(XTEST_KEY_RELEASE, x.keymap.shift_code, 0, x.root, 0, 0, 0);
    }
    if !down && want_altgr {
        let _ = x.conn.xtest_fake_input(XTEST_KEY_RELEASE, x.keymap.altgr_code, 0, x.root, 0, 0, 0);
    }
    true
}

fn xtest_type_text(text: &str) {
    lx!(|xs: &mut XState| -> Option<()> {
        for ch in text.chars() {
            let special = match ch {
                '\n' | '\r' => Some(0xff0d),
                '\t' => Some(0xff09),
                _ => None,
            };
            match special {
                Some(sym) => {
                    if let Some(code) = xs.keymap.map.get(&sym).copied() {
                        let _ =
                            xs.conn.xtest_fake_input(XTEST_KEY_PRESS, code.0, 0, xs.root, 0, 0, 0);
                        let _ = xs.conn
                            .xtest_fake_input(XTEST_KEY_RELEASE, code.0, 0, xs.root, 0, 0, 0);
                    }
                }
                None => {
                    let sym = keysym_for_char(ch);
                    if !press_keysym(xs, sym, true) {
                        log(&format!("keymap: no keycode for U+{:04X}", ch as u32));
                        continue;
                    }
                    press_keysym(xs, sym, false);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        xs.conn.flush().ok()?;
        Some(())
    });
}

fn is_modifier_sym(sym: u32) -> bool {
    matches!(sym, 0xffe3 | 0xffe9 | 0xffe1 | 0xffeb | 0xffe4 | 0xffea | 0xffec)
}

fn send_key_combo(combo: &str) {
    let parts: Vec<&str> = combo.split('+').map(|s| s.trim()).collect();
    let mut modifiers: Vec<u32> = Vec::new();
    let mut main_key: Option<u32> = None;
    for part in &parts {
        match named_keysym(part) {
            Some(sym) => {
                let is_mod_name = matches!(
                    part.to_lowercase().as_str(),
                    "ctrl" | "control" | "alt" | "menu" | "shift" | "win" | "super" | "meta" | "lwin"
                );
                if is_mod_name || (is_modifier_sym(sym) && parts.len() > 1) {
                    modifiers.push(sym);
                } else {
                    main_key = Some(sym);
                }
            }
            None => {
                log(&format!("key: unknown key '{}'", part));
                return;
            }
        }
    }
    lx!(|xs: &mut XState| -> Option<()> {
        for m in &modifiers {
            if !press_keysym(xs, *m, true) {
                log(&format!("key: modifier keysym 0x{:x} unmapped", m));
            }
        }
        if let Some(mk) = main_key {
            if !press_keysym(xs, mk, true) {
                log(&format!("key: keysym 0x{:x} unmapped", mk));
            }
            press_keysym(xs, mk, false);
        }
        for m in modifiers.iter().rev() {
            press_keysym(xs, *m, false);
        }
        xs.conn.flush().ok()?;
        Some(())
    });
    log(&format!("key: sent '{}'", combo));
}

fn target_geom() -> (i32, i32, i32, i32) {
    let t = tgt();
    if t == 0 {
        return (0, 0, 0, 0);
    }
    lx!(|xs: &mut XState| -> Option<(i32, i32, i32, i32)> { Some(window_geom(xs, t)) })
        .unwrap_or((0, 0, 0, 0))
}

fn active_window() -> u32 {
    lx!(|xs: &mut XState| -> Option<u32> {
        Some(prop_u32s(xs, xs.root, xs.atoms.net_active_window).first().copied().unwrap_or(0))
    })
    .unwrap_or(0)
}

fn target_alive(t: u32) -> bool {
    lx!(|xs: &mut XState| -> Option<bool> {
        Some(xs.conn.get_geometry(t).ok()?.reply().is_ok())
    })
    .unwrap_or(false)
}

// ═════════════════════════════════════════════════════
// AT-SPI2 Layer (D-Bus via zbus blocking)
// ═════════════════════════════════════════════════════
#[derive(Clone)]
struct AccRef {
    service: String,
    path: String,
}

struct A11y {
    conn: &'static zbus::blocking::Connection,
}

static A11Y: Mutex<Option<A11y>> = Mutex::new(None);

fn a11y_connect() -> bool {
    {
        let guard = A11Y.lock().unwrap();
        if guard.is_some() {
            return true;
        }
    }
    let session = match zbus::blocking::Connection::session() {
        Ok(s) => s,
        Err(e) => {
            log(&format!("a11y: session bus FAIL: {e}"));
            return false;
        }
    };
    let addr: String = {
        let bus_proxy = match zbus::blocking::Proxy::new(
            &session,
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
        ) {
            Ok(p) => p,
            Err(e) => {
                log(&format!("a11y: bus proxy FAIL: {e}"));
                return false;
            }
        };
        match bus_proxy.call::<_, _, String>("GetAddress", &()) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("a11y: GetAddress FAIL: {e}"));
                return false;
            }
        }
    };
    let conn = match zbus::blocking::connection::Builder::address(addr.trim()) {
        Ok(b) => b,
        Err(e) => {
            log(&format!("a11y: builder FAIL: {e}"));
            return false;
        }
    };
    let conn = match conn.build() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("a11y: connect FAIL ({}): {e}", addr.trim()));
            return false;
        }
    };
    let leaked: &'static zbus::blocking::Connection = Box::leak(Box::new(conn));
    *A11Y.lock().unwrap() = Some(A11y { conn: leaked });
    log("a11y: connected to accessibility bus");
    true
}

/// Clone the leaked connection pointer. Never hold A11Y across D-Bus calls —
/// every acc_* helper locks A11Y itself.
fn a11y_conn() -> Option<&'static zbus::blocking::Connection> {
    let guard = A11Y.lock().unwrap();
    guard.as_ref().map(|a| a.conn)
}

fn mk_proxy<'a>(
    conn: &'a zbus::blocking::Connection,
    service: &'a str,
    path: &'a str,
    iface: &'a str,
) -> Option<zbus::blocking::Proxy<'a>> {
    zbus::blocking::Proxy::new(conn, service, path, iface).ok()
}

fn acc_call<B, R>(service: &str, path: &str, iface: &str, method: &str, body: &B) -> Option<R>
where
    B: serde::ser::Serialize + zbus::zvariant::Type,
    R: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    let conn = a11y_conn()?;
    let p = mk_proxy(conn, service, path, iface)?;
    p.call(method, body).ok()
}

fn acc_getprop_raw(
    service: &str,
    path: &str,
    iface: &str,
    prop: &str,
) -> Option<zbus::zvariant::OwnedValue> {
    let conn = a11y_conn()?;
    let p = mk_proxy(conn, service, path, "org.freedesktop.DBus.Properties")?;
    p.call("Get", &(iface, prop)).ok()
}

fn acc_prop<T>(service: &str, path: &str, iface: &str, prop: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
    T::Error: std::fmt::Debug,
{
    let v = acc_getprop_raw(service, path, iface, prop)?;
    T::try_from(v).ok()
}

fn acc_name(a: &AccRef) -> String {
    acc_prop::<String>(&a.service, &a.path, "org.a11y.atspi.Accessible", "Name").unwrap_or_default()
}

fn acc_child_count(a: &AccRef) -> i32 {
    acc_prop::<i32>(&a.service, &a.path, "org.a11y.atspi.Accessible", "ChildCount").unwrap_or(0)
}

fn acc_child_at(a: &AccRef, idx: i32) -> Option<AccRef> {
    let (svc, path): (String, zbus::zvariant::OwnedObjectPath) =
        acc_call(&a.service, &a.path, "org.a11y.atspi.Accessible", "GetChildAtIndex", &idx)?;
    Some(AccRef { service: svc, path: path.to_string() })
}

fn acc_children(a: &AccRef) -> Vec<AccRef> {
    let n = acc_child_count(a);
    if n <= 0 || n > 500 {
        return Vec::new();
    }
    (0..n).filter_map(|i| acc_child_at(a, i)).collect()
}

const ST_EDITABLE: u64 = 7;
const ST_ENABLED: u64 = 8;
const ST_SENSITIVE: u64 = 24;
const ST_SHOWING: u64 = 25;
const ST_VISIBLE: u64 = 30;
const ST_FOCUSED: u64 = 12;

/// AT-SPI GetState returns an array of uint32s ('au'), each bit a StateType.
fn acc_states(a: &AccRef) -> u64 {
    let v: Vec<u32> =
        acc_call(&a.service, &a.path, "org.a11y.atspi.Accessible", "GetState", &()).unwrap_or_default();
    let mut set: u64 = 0;
    for (i, word) in v.iter().enumerate().take(2) {
        set |= (*word as u64) << (32 * i);
    }
    set
}

fn has_bit(set: u64, bit: u64) -> bool {
    set & (1u64 << bit) != 0
}

fn acc_extents(a: &AccRef) -> Option<(i32, i32, i32, i32)> {
    let (x, y, w, h): (i32, i32, i32, i32) =
        acc_call(&a.service, &a.path, "org.a11y.atspi.Component", "GetExtents", &(0u32))?;
    if w <= 0 || h <= 0 {
        return None;
    }
    // at-spi reports i32::MIN for objects with no on-screen position
    if x < -100_000 || y < -100_000 {
        return Some((0, 0, 0, 0));
    }
    Some((x, y, w, h))
}

fn acc_text(a: &AccRef) -> String {
    let count: i32 =
        acc_prop(&a.service, &a.path, "org.a11y.atspi.Text", "CharacterCount").unwrap_or(0);
    if count <= 0 {
        return String::new();
    }
    let end = count.min(1_000_000);
    acc_call::<_, String>(&a.service, &a.path, "org.a11y.atspi.Text", "GetText", &(0i32, end))
        .unwrap_or_default()
}

fn acc_set_text(a: &AccRef, text: &str) -> bool {
    let some = a11y_conn().and_then(|conn| mk_proxy(conn, &a.service, &a.path, "org.a11y.atspi.EditableText"));
    let Some(p) = some else {
        log("set_text: no conn/proxy");
        return false;
    };
    match p.call("SetTextContents", &text) {
        Ok(r) => r,
        Err(e) => {
            log(&format!("set_text ERR @{} {}: {}", a.service, a.path, e));
            false
        }
    }
}

fn acc_grab_focus(a: &AccRef) -> bool {
    acc_call::<_, bool>(&a.service, &a.path, "org.a11y.atspi.Component", "GrabFocus", &())
        .unwrap_or(false)
}

fn acc_role_name(a: &AccRef) -> String {
    acc_call::<_, String>(
        &a.service,
        &a.path,
        "org.a11y.atspi.Accessible",
        "GetRoleName",
        &(),
    )
    .unwrap_or_default()
}

fn role_from_atspi(role_name: &str, editable: bool) -> &'static str {
    let r = role_name.to_lowercase();
    let has = |pats: &[&str]| pats.iter().any(|p| r.contains(p));
    if has(&["check box"]) {
        "CheckBox"
    } else if has(&["radio button"]) {
        "RadioButton"
    } else if has(&["menu item", "menu bar"]) {
        if r.contains("bar") {
            "MenuBar"
        } else {
            "MenuItem"
        }
    } else if has(&["page tab list", "tab list"]) {
        "Tab"
    } else if has(&["page tab"]) {
        "TabItem"
    } else if has(&["list item"]) {
        "ListItem"
    } else if has(&["tree item"]) {
        "TreeItem"
    } else if has(&["table cell"]) {
        "DataItem"
    } else if has(&["push button", "toggle button"]) || r == "button" {
        "Button"
    } else if has(&["combo box"]) {
        "ComboBox"
    } else if has(&["spin button", "spinbutton"]) {
        "Spinner"
    } else if has(&["slider", "dial"]) {
        "Slider"
    } else if has(&["scroll bar"]) {
        "ScrollBar"
    } else if has(&["progress"]) {
        "ProgressBar"
    } else if has(&["status bar"]) {
        "StatusBar"
    } else if has(&["tool bar", "toolbar"]) {
        "ToolBar"
    } else if has(&["tool tip"]) {
        "ToolTip"
    } else if has(&["separator"]) {
        "Separator"
    } else if has(&["entry", "password"]) {
        "Edit"
    } else if has(&["document"]) {
        "Document"
    } else if has(&["hyper link", "hyperlink"]) || r == "link" {
        "Hyperlink"
    } else if has(&["image", "icon"]) {
        "Image"
    } else if has(&["heading", "column header", "row header", "header"]) {
        "Header"
    } else if has(&["frame", "window", "dialog", "alert"]) {
        "Window"
    } else if has(&["panel", "filler"]) {
        "Pane"
    } else if has(&["table"]) {
        "Table"
    } else if has(&["tree"]) {
        "Tree"
    } else if has(&["list box", "list"]) {
        "List"
    } else if has(&["terminal"]) {
        "Document"
    } else if has(&["label", "paragraph"]) {
        if editable {
            "Edit"
        } else {
            "Text"
        }
    } else if has(&[
        "text", "form", "section", "group", "root pane", "layered pane", "scroll pane",
        "split pane", "option pane", "glass pane", "viewport", "canvas", "drawing area",
        "html container", "directory pane", "file chooser", "font chooser", "color chooser",
        "date editor", "internal frame", "desktop", "application", "autocomplete", "editbar",
        "embedded", "chart", "caption", "notification", "menu",
    ]) {
        if r == "text" && editable {
            "Edit"
        } else if r == "text" || r == "label" || r == "paragraph" {
            "Text"
        } else if r == "menu" {
            "Menu"
        } else {
            "Group"
        }
    } else {
        "Unknown"
    }
}

fn desktop_root() -> AccRef {
    AccRef {
        service: "org.a11y.atspi.Registry".to_string(),
        path: "/org/a11y/atspi/accessible/root".to_string(),
    }
}

fn app_children_with_pids() -> Vec<(AccRef, u32)> {
    let root = desktop_root();
    let kids = acc_children(&root);
    let conn = match a11y_conn() {
        Some(c) => c,
        None => return Vec::new(),
    };
    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(conn) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for k in kids {
        let Ok(bn) = zbus::names::BusName::try_from(k.service.as_str()) else {
            continue;
        };
        let pid = dbus.get_connection_unix_process_id(bn).unwrap_or(0);
        out.push((k, pid));
    }
    out
}

/// Find the AT-SPI object matching an X11 window (by frame title and/or pid)
fn find_app_for_window(title: &str, pid: u32) -> Option<AccRef> {
    if !a11y_connect() {
        return None;
    }
    let apps = app_children_with_pids();
    // Pass 1: pid + frame title (or direct child title)
    for (app, apid) in &apps {
        if pid != 0 && *apid != pid {
            continue;
        }
        if acc_name(app) == title {
            return Some(app.clone());
        }
        for frame in acc_children(app) {
            if acc_name(&frame) == title {
                return Some(frame);
            }
        }
    }
    // Pass 2: pid only
    if pid != 0 {
        for (app, apid) in &apps {
            if *apid == pid {
                return Some(app.clone());
            }
        }
    }
    // Pass 3: title only (any pid)
    for (app, _) in &apps {
        if acc_name(app) == title {
            return Some(app.clone());
        }
        for frame in acc_children(app) {
            if acc_name(&frame) == title {
                return Some(frame);
            }
        }
    }
    None
}

static LAST_APP_ROOT: Mutex<Option<AccRef>> = Mutex::new(None);

fn stream_elements(
    ctx: &mut StreamCtx,
    node: &AccRef,
    walker: &mut HashSet<String>,
    parent_id: i64,
    depth: i32,
    budget: &mut usize,
) {
    if depth > MAX_DEPTH || ctx.count as usize >= MAX_NODES || *budget == 0 {
        return;
    }
    if !walker.insert(node.path.clone()) {
        return;
    }
    *budget -= 1;

    let role_raw = acc_role_name(node);
    let name = acc_name(node);
    let states = acc_states(node);
    let editable = has_bit(states, ST_EDITABLE);
    let enabled = has_bit(states, ST_ENABLED) || has_bit(states, ST_SENSITIVE);
    let visible = has_bit(states, ST_SHOWING) && has_bit(states, ST_VISIBLE);
    let offscreen = if visible { 0 } else { 1 };
    let extent = acc_extents(node).unwrap_or((0, 0, 0, 0));

    let role = role_from_atspi(&role_raw, editable);
    let value = if matches!(role, "Edit" | "Document" | "ComboBox" | "Spinner") {
        acc_text(node)
    } else {
        String::new()
    };
    // Action.Invoke availability — only queried for roles that can carry
    // actions so the dbus cost of huge trees stays flat. GetActions returns
    // a(sss): (name, description, keybinding) per action.
    let action_count = if matches!(
        role,
        "Button" | "MenuItem" | "Menu" | "CheckBox" | "RadioButton" | "ListItem" | "ComboBox"
            | "TabItem" | "Link" | "PageTab" | "ToggleButton" | "Image" | "TableCell"
            | "Row" | "ColumnHeader" | "Slider" | "ScrollBar" | "Hyperlink" | "TreeItem"
    ) {
        acc_call::<_, Vec<(String, String, String)>>(
            &node.service,
            &node.path,
            "org.a11y.atspi.Action",
            "GetActions",
            &(),
        )
        .map(|v| v.len() as i32)
        .unwrap_or(0)
    } else {
        0
    };

    ctx.count += 1;
    let my_id = ctx.count;
    let _ = ctx.conn.execute(
        "INSERT INTO elements(id,parent_id,depth,role,name,value,automation_id,enabled,offscreen,x,y,w,h,actions) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            my_id, parent_id, depth,
            role,
            if name.is_empty() { None } else { Some(name) },
            if value.is_empty() { None } else { Some(value) },
            Option::<String>::None,
            enabled as i32, offscreen,
            extent.0, extent.1, extent.2, extent.3,
            action_count
        ],
    );
    ctx.batch += 1;
    if ctx.batch >= STREAM_BATCH {
        let _ = ctx.conn.execute_batch("COMMIT; BEGIN TRANSACTION;");
        ctx.batch = 0;
    }

    for child in acc_children(node) {
        stream_elements(ctx, &child, walker, my_id, depth + 1, budget);
    }
}

/// Probe caption metrics from AT-SPI push-buttons near the top-right
fn probe_caption(root: &AccRef, win_right: i32, win_top: i32) {
    let mut leftmost_x = win_right;
    let mut max_bottom = win_top + top_h();
    let mut found = false;
    let level1 = acc_children(root);
    let level2: Vec<AccRef> = level1.iter().flat_map(|c| acc_children(c)).collect();
    for node in level1.iter().chain(level2.iter()) {
        if acc_role_name(node).to_lowercase().contains("button")
            && !acc_role_name(node).to_lowercase().contains("toggle")
        {
            if let Some((bx, by, _, bh)) = acc_extents(node) {
                if by < win_top + 80 && bx >= win_right - 500 {
                    leftmost_x = leftmost_x.min(bx);
                    max_bottom = max_bottom.max(by + bh);
                    found = true;
                }
            }
        }
    }
    if !found {
        return;
    }
    let off = win_right - leftmost_x;
    if off > 40 && off < 400 {
        BTN_OFF_X.store(off, SeqCst);
    }
    let th = (max_bottom - win_top + 4).clamp(DEFAULT_TOP_H, 60);
    DYN_TOP_H.store(th, SeqCst);
    log(&format!(
        "probe_caption: btn_offset={}, bar_height={}",
        BTN_OFF_X.load(SeqCst),
        DYN_TOP_H.load(SeqCst)
    ));
}

fn current_window_title(target: u32) -> String {
    lx!(|xs: &mut XState| -> Option<String> {
        let mut t = prop_string(xs, target, xs.atoms.net_wm_name);
        if t.trim().is_empty() {
            t = prop_string(xs, target, xs.atoms.wm_name);
        }
        Some(t)
    })
    .unwrap_or_default()
}


// ── Delta perception ─────────────────────────────────
/// Compare `elements` (just built) against `elements_prev` (previous cycle)
/// and append rows to `events` for anything that changed.  Then copy
/// `elements` → `elements_prev` so the next cycle has a baseline.
///
/// Stable element identity key: (name, role, depth)
///   — depth disambiguates same-named widgets at different tree levels
///   — more stable than AT-SPI object paths which change on recreation
///
/// Event types emitted:
///   appeared        — element in elements but not in elements_prev
///   disappeared     — element in elements_prev but not in elements
///   value_changed   — same key, value column differs
///   enabled_changed — same key, enabled column differs
///   state_changed   — same key, offscreen column differs
fn diff_and_emit_events(conn: &Connection) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // ── 1. appeared ─────────────────────────────────────────────────────────────
    let _ = conn.execute_batch(&format!(
        "INSERT INTO events(timestamp,event_type,element_name,element_role,detail,new_value,summary)
         SELECT {ts},'appeared',e.name,e.role,
                'element appeared in UI',
                e.value,
                e.role || ' \"' || e.name || '\" appeared'
         FROM elements e
         WHERE e.name IS NOT NULL AND e.name != ''
           AND e.offscreen = 0
           AND NOT EXISTS (
               SELECT 1 FROM elements_prev p
               WHERE p.name=e.name AND p.role=e.role AND p.depth=e.depth
           );"
    ));

    // ── 2. disappeared ──────────────────────────────────────────────────────────
    let _ = conn.execute_batch(&format!(
        "INSERT INTO events(timestamp,event_type,element_name,element_role,detail,new_value,summary)
         SELECT {ts},'disappeared',p.name,p.role,
                'element no longer in UI',
                NULL,
                p.role || ' \"' || p.name || '\" disappeared'
         FROM elements_prev p
         WHERE p.name IS NOT NULL AND p.name != ''
           AND p.offscreen = 0
           AND NOT EXISTS (
               SELECT 1 FROM elements e
               WHERE e.name=p.name AND e.role=p.role AND e.depth=p.depth
           );"
    ));

    // ── 3. value_changed ────────────────────────────────────────────────────────
    let _ = conn.execute_batch(&format!(
        "INSERT INTO events(timestamp,event_type,element_name,element_role,detail,new_value,summary)
         SELECT {ts},'value_changed',e.name,e.role,
                COALESCE(p.value,'(empty)') || ' → ' || COALESCE(e.value,'(empty)'),
                e.value,
                e.role || ' \"' || e.name || '\" value changed'
         FROM elements e
         JOIN elements_prev p ON p.name=e.name AND p.role=e.role AND p.depth=e.depth
         WHERE COALESCE(e.value,'') != COALESCE(p.value,'')
           AND e.name IS NOT NULL AND e.name != ''
           AND e.offscreen = 0
           AND e.role IN ('Edit','Document','ComboBox','Spinner','CheckBox','RadioButton');"
    ));

    // ── 4. enabled_changed ──────────────────────────────────────────────────────
    let _ = conn.execute_batch(&format!(
        "INSERT INTO events(timestamp,event_type,element_name,element_role,detail,new_value,summary)
         SELECT {ts},'enabled_changed',e.name,e.role,
                CASE WHEN e.enabled=1 THEN 'became enabled' ELSE 'became disabled' END,
                CAST(e.enabled AS TEXT),
                e.role || ' \"' || e.name || '\" ' ||
                    CASE WHEN e.enabled=1 THEN 'became enabled' ELSE 'became disabled' END
         FROM elements e
         JOIN elements_prev p ON p.name=e.name AND p.role=e.role AND p.depth=e.depth
         WHERE e.enabled != p.enabled
           AND e.name IS NOT NULL AND e.name != '';"
    ));

    // ── 5. state_changed (visible/hidden) ───────────────────────────────────────
    let _ = conn.execute_batch(&format!(
        "INSERT INTO events(timestamp,event_type,element_name,element_role,detail,new_value,summary)
         SELECT {ts},'state_changed',e.name,e.role,
                CASE WHEN e.offscreen=0 THEN 'became visible' ELSE 'became hidden' END,
                CASE WHEN e.offscreen=0 THEN 'visible' ELSE 'hidden' END,
                e.role || ' \"' || e.name || '\" ' ||
                    CASE WHEN e.offscreen=0 THEN 'became visible' ELSE 'became hidden' END
         FROM elements e
         JOIN elements_prev p ON p.name=e.name AND p.role=e.role AND p.depth=e.depth
         WHERE e.offscreen != p.offscreen
           AND e.name IS NOT NULL AND e.name != '';"
    ));

    // ── 6. Rotate: copy current elements → elements_prev ────────────────────────
    let _ = conn.execute_batch(
        "DELETE FROM elements_prev;
         INSERT INTO elements_prev
             SELECT id,parent_id,depth,role,name,value,automation_id,
                    enabled,offscreen,x,y,w,h,actions
             FROM elements;",
    );

    // Prune: keep newest 500 total, always keep unconsumed
    let _ = conn.execute_batch(
        "DELETE FROM events WHERE consumed=1
             AND id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT 200);
         DELETE FROM events WHERE id NOT IN
             (SELECT id FROM events ORDER BY id DESC LIMIT 500);",
    );
}

fn dump_tree() {
    if TREE_BUSY.compare_exchange(false, true, SeqCst, SeqCst).is_err() {
        return;
    }
    let target = tgt();
    if target == 0 {
        TREE_BUSY.store(false, SeqCst);
        return;
    }
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .name("tree-dump".into())
        .spawn(move || {
            let t0 = Instant::now();

            let title = current_window_title(target);
            if title.trim().is_empty() {
                TREE_BUSY.store(false, SeqCst);
                return;
            }
            let pid = TARGET_PID.load(SeqCst);
            let Some(app_root) = find_app_for_window(&title, pid) else {
                log(&format!(
                    "dump: no AT-SPI app for '{}' (pid {}) — enable toolkit accessibility?",
                    title, pid
                ));
                TREE_BUSY.store(false, SeqCst);
                return;
            };
            *LAST_APP_ROOT.lock().unwrap() = Some(app_root.clone());

            let db_path = get_db_path();
            if db_path.is_empty() {
                TREE_BUSY.store(false, SeqCst);
                return;
            }
            if let Some(conn) = init_db(&db_path) {
                let _ = conn.execute_batch(
                    "
                    DROP TABLE IF EXISTS elements;
                    DROP TABLE IF EXISTS meta;
                    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                    CREATE TABLE elements (
                        id INTEGER PRIMARY KEY, parent_id INTEGER, depth INTEGER,
                        role TEXT NOT NULL, name TEXT, value TEXT, automation_id TEXT,
                        enabled INTEGER DEFAULT 1, offscreen INTEGER DEFAULT 0,
                        x INTEGER, y INTEGER, w INTEGER, h INTEGER,
                        actions INTEGER DEFAULT 0
                    );
                ",
                );
                let (wx, wy, ww, wh) = target_geom();
                let ts =
                    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
                let _ = conn.execute(
                    "INSERT INTO meta(key,value) VALUES('window',?1),('hwnd',?2),('timestamp',?3),('x',?4),('y',?5),('w',?6),('h',?7)",
                    params![title, format!("0x{:X}", target), ts.to_string(), wx, wy, ww, wh],
                );
                let _ = conn.execute_batch("BEGIN TRANSACTION;");
                let mut ctx = StreamCtx { conn: &conn, count: 0, batch: 0 };
                let mut walker = HashSet::new();
                let mut budget = MAX_NODES;
                stream_elements(&mut ctx, &app_root, &mut walker, 0, 0, &mut budget);
                let _ = conn.execute_batch("COMMIT;");
                let total_ms = t0.elapsed().as_millis();
                log(&format!("dump: {} rows streamed, total={}ms", ctx.count, total_ms));
                log_a11y_hint_if_thin(ctx.count as usize);

                diff_and_emit_events(&conn);

                generate_snap(&db_path);
                generate_a11y(&db_path);
                generate_a11y_snap(&db_path);
                // do_unsnap may have cleared the path while we were dumping —
                // don't resurrect a stale "active" status over its "none".
                if get_db_path() == db_path {
                    write_active_status(&db_path);
                }
            }
            TREE_BUSY.store(false, SeqCst);
        })
        .expect("spawn tree-dump thread");
}

// ═════════════════════════════════════════════════════
// Snap / Unsnap
// ═════════════════════════════════════════════════════
fn do_snap(target: u32, title: &str) {
    log(&format!("do_snap: target=0x{:X} '{}'", target, title));
    TARGET_HW.store(target, SeqCst);
    let (x, y, w, h) = target_geom();
    if w == 0 || h == 0 {
        log("do_snap: target geometry invalid");
        TARGET_HW.store(0, SeqCst);
        return;
    }
    IS_SNAPPED.store(true, SeqCst);
    save(x, y, w, h);

    let db_path = db_name_from_title(title);
    let _ = fs::create_dir_all(prof(""));
    set_db_path(&db_path);
    // One-time purge of stale unprocessed inject rows from a previous session.
    if let Some(c) = Connection::open(&db_path).ok() {
        let _ = c.execute_batch("PRAGMA busy_timeout=500;");
        let _ = c.execute("DELETE FROM inject WHERE done=0", []);
    }
    log(&format!("do_snap: app db = {}", db_path));

    place_overlay(x, y, w, h);
    show_overlay(!AGENT_MODE.load(SeqCst));

    let title_owned = title.to_string();
    let pid = TARGET_PID.load(SeqCst);
    std::thread::spawn(move || {
        if a11y_connect() {
            if let Some(root) = find_app_for_window(&title_owned, pid) {
                let (_, _, fw, _) = target_geom();
                probe_caption(&root, target_geom().0 + fw, target_geom().1);
            }
        }
        place_overlay(target_geom().0, target_geom().1, target_geom().2, target_geom().3);
        dump_tree();
    });
    log("do_snap: COMPLETE");
}

// ── A11y hints: actionable advice when a target barely exposes a tree ──
fn a11y_hint_for(exe: &str) -> &'static str {
    let e = exe.to_lowercase();
    if e.contains("chrome") || e.contains("chromium") || matches!(e.as_str(),
        "code" | "discord" | "slack" | "signal" | "element" | "obsidian")
        || e.contains("electron")
    {
        "relaunch it with --force-renderer-accessibility"
    } else if e.contains("firefox") || e.contains("librewolf") || e.contains("thunderbird") {
        "set accessibility.force_disabled=0 in about:config, then restart the app"
    } else if e.contains("qt") || ["keepassxc", "qbittorrent", "vlc", "krita"].contains(&e.as_str())
    {
        "install qt-at-spi (or qt6-atspi) and launch with QT_ACCESSIBILITY=1"
    } else if e.contains("java") || e.ends_with("_wrap") {
        "install java-atk-wrapper and enable accessibility in its config"
    } else {
        "this app may not expose an accessibility tree on Linux"
    }
}

fn log_a11y_hint_if_thin(count: usize) {
    if count >= 15 {
        return;
    }
    let pid = TARGET_PID.load(SeqCst);
    let exe = fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    log(&format!(
        "dump: only {count} elements from pid {pid} ({exe}) — {}",
        a11y_hint_for(&exe)
    ));
}

fn do_unsnap() {
    log("do_unsnap: START");
    set_db_path("");
    write_active_status("");
    *LAST_APP_ROOT.lock().unwrap() = None;
    IS_SNAPPED.store(false, SeqCst);
    TARGET_HW.store(0, SeqCst);
    TARGET_PID.store(0, SeqCst);
    BTN_OFF_X.store(FALLBACK_BTN_X, SeqCst);
    DYN_TOP_H.store(DEFAULT_TOP_H, SeqCst);
    let (x, y, _, _) = overlay_geom();
    place_overlay(x, y, INIT_W, INIT_H);
    save(x, y, INIT_W, INIT_H);
    show_overlay(!AGENT_MODE.load(SeqCst));
    log("do_unsnap: COMPLETE");
}

// ── Sync Loop ────────────────────────────────────────
fn sync_tick() {
    if !snapped() {
        return;
    }
    let t = tgt();
    if t == 0 {
        return;
    }
    if !target_alive(t) {
        log("do_sync: target gone, unsnapping");
        do_unsnap();
        return;
    }
    let tp = target_geom();
    if tp.2 == 0 && tp.3 == 0 {
        return;
    }
    if AGENT_MODE.load(SeqCst) {
        show_overlay(false);
        return;
    }
    let hidden = lx!(|xs: &mut XState| -> Option<bool> { Some(is_hidden(xs, t)) }).unwrap_or(false);
    show_overlay(!hidden);
    if !hidden {
        let sp = saved();
        if tp != sp {
            place_overlay(tp.0, tp.1, tp.2, tp.3);
            save(tp.0, tp.1, tp.2, tp.3);
        }
    }
}

// ── Inject Pipeline ──────────────────────────────────
/// Fallback lookup by UIA-style role ("Edit", "Button", ...) — GTK apps expose
/// many interactive widgets with empty names.
fn find_menu_item(name: &str) -> Option<AccRef> {
    find_element_opts(name, false, false, false)
}

fn is_menu_role(a: &AccRef) -> bool {
    let r = acc_role_name(a);
    let r = r.to_lowercase();
    r.contains("menu item") || r.contains("menu bar") || r == "menu"
}

/// For text injection: exact name first, then role — both restricted to
/// EDITABLE elements so containers that merely share the label are skipped.
/// Lookup precedence used by inject/click:
/// 1. exact accessible name (skipping menus unless allow_menus),
/// 2. UIA-style role match,
/// 3. menu items (last resort — a menu named "Edit" must not shadow an
///    editable widget also called "Edit").
fn find_target(name: &str, require_editable: bool) -> Option<AccRef> {
    find_element_opts(name, false, require_editable, true)
        .or_else(|| find_element_opts(name, true, require_editable, true))
        .or_else(|| find_menu_item(name))
}

fn find_element_opts(
    name: &str,
    by_role: bool,
    require_editable: bool,
    skip_menus: bool,
) -> Option<AccRef> {
    let root = LAST_APP_ROOT.lock().unwrap().clone()?;
    let mut stack = vec![root.clone()];
    let mut visited = HashSet::new();
    let mut budget = 3000usize;
    let mut fallback: Option<AccRef> = None;
    let mut visible_hits: Vec<AccRef> = Vec::new();
    while let Some(node) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        if !visited.insert(node.path.clone()) {
            continue;
        }
        if skip_menus && !by_role && is_menu_role(&node) {
            continue;
        }
        let mut matches = if by_role {
            let editable = has_bit(acc_states(&node), ST_EDITABLE);
            role_from_atspi(&acc_role_name(&node), editable) == name
        } else {
            acc_name(&node) == name
        };
        if matches && require_editable {
            matches = has_bit(acc_states(&node), ST_EDITABLE);
        }
        if matches {
            if !by_role {
                return Some(node);
            }
            // Role matches can be ambiguous — prefer visible+enabled widgets.
            let s = acc_states(&node);
            let visible = has_bit(s, ST_SHOWING) && has_bit(s, ST_VISIBLE);
            let enabled = has_bit(s, ST_ENABLED) || has_bit(s, ST_SENSITIVE);
            if visible && enabled {
                visible_hits.push(node.clone());
            } else if fallback.is_none() {
                fallback = Some(node.clone());
            }
        }
        for c in acc_children(&node) {
            stack.push(c);
        }
    }
    if visible_hits.len() > 1 && matches!(name, "Edit" | "Document" | "Text" | "TextArea") {
        // Ambiguous text widgets: the content area is the biggest one.
        return visible_hits.into_iter().max_by_key(|n| {
            acc_extents(n).map(|e| (e.2.max(0) as i64) * (e.3.max(0) as i64)).unwrap_or(0)
        });
    }
    if let Some(first) = visible_hits.into_iter().next() {
        return Some(first);
    }
    fallback
}

/// The currently focused accessible inside the snapped app, if detectable.
fn focused_node() -> Option<AccRef> {
    let root = LAST_APP_ROOT.lock().unwrap().clone()?;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if has_bit(acc_states(&node), ST_FOCUSED) {
            return Some(node);
        }
        for c in acc_children(&node) {
            stack.push(c);
        }
    }
    None
}

/// URL-shaped text: raw keystrokes could trigger browser single-key
/// shortcuts (Quick Find on '/', etc.), so it must not be typed blind.
fn looks_like_url(text: &str) -> bool {
    text.contains("://") || text.starts_with("www.") || text.starts_with("http")
}

fn inject_text(text: &str, target_name: &str) -> bool {
    let elem = if target_name.is_empty() {
        None
    } else {
        find_target(target_name, true)
    };
    if let Some(el) = elem {
        let current = acc_text(&el);
        let combined = format!("{}{}", current, text);
        if acc_set_text(&el, &combined) {
            log(&format!("inject: EditableText OK len={}", combined.len()));
            return true;
        }
        log("inject: EditableText failed — falling back to focus+type");
        let _ = acc_grab_focus(&el);
        std::thread::sleep(Duration::from_millis(60));
    } else if !target_name.is_empty() {
        log(&format!(
            "inject: element '{}' not found — typing into focused widget",
            target_name
        ));
    }
    xtest_type_text(text);
    log("inject: typed via SendInput-style input");
    true
}

fn click_element(element_name: &str) -> bool {
    let el = match find_target(element_name, false) {
        Some(e) => e,
        None => {
            log(&format!("click: FindFirst FAIL ('{}')", element_name));
            return false;
        }
    };
    let (ex, ey, ew, eh) = match acc_extents(&el) {
        Some(e) => e,
        None => {
            log(&format!("click: rect FAIL ('{}')", element_name));
            return false;
        }
    };
    log(&format!(
        "click: picked {}{} ({},{}) {}x{}",
        el.service, el.path, ex, ey, ew, eh
    ));
    let cx = ex + ew / 2;
    let cy = ey + eh / 2;
    let t = tgt();
    if t != 0 {
        lx!(|xs: &mut XState| -> Option<()> {
            activate_target(xs, t);
            Some(())
        });
        std::thread::sleep(Duration::from_millis(50));
    }
    mouse_click(cx, cy);
    log(&format!("click: SendInput '{}' @ {},{} (persisted)", element_name, cx, cy));
    true
}

fn process_injections() {
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, SeqCst) {
        return;
    }
    let result = (|| {
        let db_path = get_db_path();
        if db_path.is_empty() {
            return;
        }
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=500;");
        let row: Option<(i64, String, String, String)> = conn
            .query_row(
                "SELECT id, COALESCE(action,'text'), text, COALESCE(target,'') \
                 FROM inject WHERE done=0 ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let Some((id, action, text, target_name)) = row else { return };
        if conn.execute("UPDATE inject SET done=1 WHERE id=?1", params![id]).is_err() {
            return;
        }
        log(&format!(
            "action: id={} type='{}' target='{}' text='{}'",
            id,
            action,
            target_name,
            if text.len() > 50 { &text[..50] } else { &text }
        ));

        let t = tgt();
        let ok = match action.as_str() {
            "text" => inject_text(&text, &target_name),
            "type" => {
                let lx = LAST_CLICK_X.load(SeqCst);
                let ly = LAST_CLICK_Y.load(SeqCst);
                if t != 0 {
                    lx!(|xs: &mut XState| -> Option<()> {
                        activate_target(xs, t);
                        Some(())
                    });
                    std::thread::sleep(Duration::from_millis(80));
                }
                // URL guard: elementless typing of URL-shaped text must not
                // land on page content — a '/' there opens browser Quick Find.
                let mut guarded_paste = false;
                if target_name.is_empty() && looks_like_url(&text) {
                    match focused_node() {
                        Some(f) => {
                            if has_bit(acc_states(&f), ST_EDITABLE) {
                                log("type: URL guard — editable focused, typing raw");
                            } else {
                                log(&format!(
                                    "type: URL guard — focused {} '{}' not editable, using clipboard paste",
                                    acc_role_name(&f),
                                    acc_name(&f)
                                ));
                                if set_clipboard(&text) {
                                    std::thread::sleep(Duration::from_millis(150));
                                    send_key_combo("ctrl+v");
                                    log("type: URL guard — pasted");
                                } else {
                                    log("type: URL guard — clipboard set failed");
                                }
                                guarded_paste = true;
                            }
                        }
                        None => log("type: URL guard — focus undetectable, typing raw"),
                    }
                }
                if guarded_paste {
                    true
                } else {
                if lx >= 0 && ly >= 0 {
                    let aw_before = active_window();
                    mouse_click(lx, ly);
                    std::thread::sleep(Duration::from_millis(50));
                    let dbg = lx!(|xs: &mut XState| -> Option<u32> {
                        Some(xs.conn.get_input_focus().ok()?.reply().ok()?.focus)
                    })
                    .unwrap_or(0);
                    log(&format!(
                        "type: re-focus @ {},{} ewmh_active=0x{:X} input_focus=0x{:X} target=0x{:X}",
                        lx, ly, aw_before, dbg, t
                    ));
                }
                log(&format!("type: BEGIN SendInput {} chars", text.len()));
                let mut aborted = false;
                for (i, ch) in text.chars().enumerate() {
                    let active = active_window();
                    if t != 0 && active != t {
                        log(&format!(
                            "type: ABORT at char[{}] — focus lost (fg=0x{:X} target=0x{:X})",
                            i, active, t
                        ));
                        aborted = true;
                        break;
                    }
                    xtest_type_text(&ch.to_string());
                    std::thread::sleep(Duration::from_millis(5));
                }
                if aborted {
                    log("type: ABORTED — focus lost mid-typing");
                } else {
                    log(&format!("type: ALL {} CHARS DONE", text.len()));
                }
                !aborted
                }
            }
            "key" => {
                if t != 0 {
                    lx!(|xs: &mut XState| -> Option<()> {
                        activate_target(xs, t);
                        Some(())
                    });
                    std::thread::sleep(Duration::from_millis(120));
                }
                send_key_combo(&text);
                true
            }
            "click" => {
                log(&format!("click: BEGIN '{}'", target_name));
                let r = click_element(&target_name);
                log(&format!("click: END '{}' result={}", target_name, r));
                r
            }
            "invoke" => {
                log(&format!("invoke: BEGIN '{}'", target_name));
                match find_target(&target_name, false) {
                    None => {
                        log("invoke: element not found");
                        false
                    }
                    Some(el) => {
                        let r: Option<bool> = acc_call::<_, bool>(
                            &el.service,
                            &el.path,
                            "org.a11y.atspi.Action",
                            "DoAction",
                            &(0i32),
                        );
                        log(&format!("invoke: DoAction -> {:?}", r));
                        r.unwrap_or(false)
                    }
                }
            }
            "clipset" => set_clipboard(&text),
            "clipget" => {
                match get_clipboard() {
                    Some(s) => {
                        let _ = fs::write(prof(CLIP_OUT_NAME), s);
                        true
                    }
                    None => {
                        log("clipget: no reply from selection owner");
                        let _ = fs::write(prof(CLIP_OUT_NAME), "");
                        false
                    }
                }
            }
            "paste" => {
                if !set_clipboard(&text) {
                    log("paste: clipboard set failed");
                    false
                } else {
                    if t != 0 {
                        lx!(|xs: &mut XState| -> Option<()> {
                            activate_target(xs, t);
                            Some(())
                        });
                    }
                    std::thread::sleep(Duration::from_millis(150));
                    send_key_combo("ctrl+v");
                    log("paste: ctrl+v sent");
                    true
                }
            }
            "scroll" => {
                if t != 0 {
                    lx!(|xs: &mut XState| -> Option<()> {
                        activate_target(xs, t);
                        Some(())
                    });
                    std::thread::sleep(Duration::from_millis(80));
                }
                mouse_scroll(&text);
                true
            }
            other => {
                log(&format!("action: unknown type '{}'", other));
                false
            }
        };

        if ok {
            log(&format!("action: done id={}", id));
        } else {
            let _ = conn.execute("UPDATE inject SET done=0 WHERE id=?1", params![id]);
            log(&format!("action: FAILED id={} — will retry", id));
        }
    })();
    let _ = result;
    BUSY.store(false, SeqCst);
}

// ── Daemon: Snap Requests & Overlay Mode ─────────────
fn check_snap_request() {
    let snap_req = prof(SNAP_REQUEST_NAME);
    let snap_res = prof(SNAP_RESULT_NAME);
    let content = match fs::read_to_string(&snap_req) {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = fs::remove_file(&snap_req);
    let requested = content.trim().to_lowercase();
    if requested.is_empty() {
        return;
    }
    if requested == "!unsnap" {
        if snapped() {
            do_unsnap();
        }
        log("snap_request: !unsnap");
        let _ = fs::write(&snap_res, r#"{"status":"ok","app":""}"#);
        return;
    }
    log(&format!("snap_request: looking for '{}'", requested));
    // Prefer the topmost matching window (EWMH stacking order ≈ Windows Z-order).
    let target = lx!(|xs: &mut XState| -> Option<WinInfo> {
        let wins = enumerate_windows(xs);
        let mut stacked = prop_u32s(xs, xs.root, xs.atoms.net_client_list_stacking);
        stacked.reverse(); // topmost first
        for hwnd in &stacked {
            if let Some(w) = wins.iter().find(|i| i.hwnd == *hwnd) {
                if w.app == requested {
                    return Some(WinInfo {
                        hwnd: w.hwnd,
                        title: w.title.clone(),
                        app: w.app.clone(),
                        exe: w.exe.clone(),
                        pid: w.pid,
                        geom: w.geom,
                    });
                }
            }
        }
        wins.into_iter().find(|w| w.app == requested)
    });

    match target {
        Some(w) => {
            log(&format!("snap_request: found '{}' at 0x{:X}", requested, w.hwnd));
            if snapped() && tgt() == w.hwnd {
                let _ = fs::write(
                    &snap_res,
                    format!(r#"{{"status":"ok","app":"{}"}}"#, requested),
                );
                return;
            }
            if snapped() {
                do_unsnap();
            }
            TARGET_PID.store(w.pid, SeqCst);
            do_snap(w.hwnd, &w.title);
            let status = if snapped() { "ok" } else { "error" };
            let _ = fs::write(
                &snap_res,
                format!(r#"{{"status":"{}","app":"{}"}}"#, status, requested),
            );
        }
        None => {
            log(&format!("snap_request: '{}' NOT FOUND", requested));
            let _ = fs::write(
                &snap_res,
                format!(
                    r#"{{"status":"error","reason":"No window matching '{}' found"}}"#,
                    requested
                ),
            );
        }
    }
}

fn check_overlay_mode() {
    let mode = match fs::read_to_string(prof(OVERLAY_MODE_NAME)) {
        Ok(m) => m,
        Err(_) => return,
    };
    let want_agent = mode.trim().eq_ignore_ascii_case("agent");
    let was_agent = AGENT_MODE.load(SeqCst);
    if want_agent != was_agent {
        AGENT_MODE.store(want_agent, SeqCst);
        if want_agent {
            log("overlay_mode: switching to AGENT (hidden)");
            show_overlay(false);
        } else {
            log("overlay_mode: switching to HUMAN (visible)");
            OVERLAY_SHOWN.store(false, SeqCst);
            show_overlay(true);
        }
    }
}

// ── Accessibility Activation ─────────────────────────
fn activate_accessibility() {
    log("activate_a11y: enabling toolkit accessibility...");
    let _ = std::process::Command::new("gsettings")
        .args(["set", "org.gnome.desktop.interface", "toolkit-accessibility", "true"])
        .output();
    if a11y_connect() {
        log("activate_a11y: AT-SPI2 registry reachable");
    } else {
        log("activate_a11y: WARNING — AT-SPI2 registry unreachable; will retry on demand");
    }
}

// ═════════════════════════════════════════════════════
// Main
// ═════════════════════════════════════════════════════
fn single_instance_check() -> bool {
    let _ = fs::create_dir_all(prof(""));
    if let Ok(existing) = fs::read_to_string(prof(LOCK_NAME)) {
        let pid: i32 = existing.trim().parse().unwrap_or(0);
        if pid > 0 && pid != std::process::id() as i32
            && std::path::Path::new(&format!("/proc/{}", pid)).exists()
        {
            eprintln!("DirectShell is already running (pid {}). Exiting.", pid);
            return false;
        }
    }
    let _ = fs::write(prof(LOCK_NAME), format!("{}", std::process::id()));
    true
}

extern "C" fn on_signal(_: libc::c_int) {
    EXIT_FLAG.store(true, SeqCst);
    write_active_status("");
    std::process::exit(0);
}

fn spawn_loop<F: Fn() + Send + 'static>(name: &str, ms: u64, f: F) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .name(name.to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(ms));
                if EXIT_FLAG.load(SeqCst) {
                    break;
                }
                f();
            }
        })
        .expect("spawn thread");
}

fn main() {
    if !single_instance_check() {
        std::process::exit(0);
    }
    write_active_status("");
    log("=== DirectShell START ===");

    if !x_connect() {
        eprintln!("FATAL: cannot connect to X11 display (is DISPLAY set?)");
        std::process::exit(1);
    }
    let _ = fs::create_dir_all(prof(""));

    unsafe {
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(libc::c_int) as usize);
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(libc::c_int) as usize);
    }

    if !create_overlay() {
        eprintln!("FATAL: cannot create overlay window");
        std::process::exit(1);
    }

    create_clip_window();

    activate_accessibility();

    let g = overlay_geom();
    save(g.0, g.1, g.2, g.3);

    enum_windows_to_json();

    spawn_loop("sync", TIMER_MS, sync_tick);
    spawn_loop("anim", ANIM_MS, || {
        if !snapped() && !AGENT_MODE.load(SeqCst) {
            lx!(|xs: &mut XState| -> Option<()> {
                repaint(xs);
                Some(())
            });
        }
    });
    spawn_loop("tree", TREE_MS, || {
        if snapped() {
            dump_tree();
        }
    });
    spawn_loop("inject", INJECT_MS, process_injections);
    spawn_loop("enum", ENUM_MS, enum_windows_to_json);
    spawn_loop("snapreq", SNAP_REQ_MS, || {
        check_snap_request();
        check_overlay_mode();
    });

    log("Daemon mode: SYNC + ANIM + TREE + INJECT + ENUM + SNAPREQ running");
    log("=== DirectShell READY ===");

    x_event_loop();
}
