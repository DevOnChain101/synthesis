//! The sidebar panels as controls: the workspaces known to the session store, the sessions of the
//! selected one (Running), and the Activity feed. Keyboard focus (Tab), mouse clicks and the wheel.
//! Selecting a workspace only changes what Running lists; opening a session restarts the app on it
//! (`ExitReason::Resume`), like `/resume`.
use super::*;

/// How long a second press may confirm a switch that cancels the running task.
const SWITCH_ARM_MS: i64 = 5000;
/// Rows PageUp / PageDown move in a focused panel.
const PANEL_PAGE: usize = 5;

/// One workspace: a folder with stored sessions (the live one is always the first).
pub(super) struct Group {
    /// `fold_dir` of the folder: what makes two paths the same workspace.
    pub key: String,
    /// Display form of the folder (the renderer matches rows to workspaces by it).
    pub cwd: String,
    /// The folder as stored: what `/new` opens.
    pub raw: String,
    /// Stored sessions other than the live one, newest first.
    pub sessions: Vec<SavedSession>,
}

/// The live workspace first, then the others by their newest session.
pub(super) fn group_sessions(cwd: &str, live_id: &str, saved: &[SavedSession]) -> Vec<Group> {
    let new = |raw: &str, key: String| Group {
        key,
        cwd: clean_line(raw),
        raw: raw.to_string(),
        sessions: vec![],
    };
    let mut groups = vec![new(cwd, fold_dir(cwd))];
    let mut canon: std::collections::HashMap<&str, String> = Default::default();
    for s in saved.iter().filter(|s| s.id != live_id) {
        let k = canon
            .entry(s.cwd.as_str())
            .or_insert_with(|| fold_dir(&s.cwd))
            .clone();
        match groups.iter_mut().find(|g| g.key == k) {
            Some(g) => g.sessions.push(s.clone()),
            None => {
                let mut g = new(&s.cwd, k);
                g.sessions.push(s.clone());
                groups.push(g);
            }
        }
    }
    for g in &mut groups {
        g.sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    }
    groups[1..].sort_by_key(|g| std::cmp::Reverse(g.sessions[0].mtime));
    groups
}

/// A prompt shown in Activity remembers which chat item holds it. `prompts` / `items` are the
/// counts before the event was applied to `activity` and `chat`.
pub(super) fn link_prompt(a: &mut Session, chat: &Chat, prompts: u32, items: usize) {
    if a.prompts == prompts {
        return;
    }
    let at = chat.agent.items[items.min(chat.agent.items.len())..]
        .iter()
        .position(|i| i.kind == "user");
    if let (Some(at), Some(f)) = (at, a.activity.last_mut()) {
        f.anchor = Some(items + at);
    }
}

fn contains(r: Rect, p: Point) -> bool {
    p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h
}

impl App {
    pub(super) fn live_group(&self) -> bool {
        self.ws_sel == 0
    }

    /// Ids of the rows of the Running panel: the live session (selected workspace = live one), then
    /// the stored ones.
    fn running_ids(&self) -> Vec<String> {
        let mut ids = vec![];
        if self.live_group() {
            ids.push(self.cfg.session_id.clone());
        }
        ids.extend(
            self.groups[self.ws_sel]
                .sessions
                .iter()
                .map(|s| s.id.clone()),
        );
        ids
    }

    /// Whether the sidebar (Workspace and Running) is on screen in the last frame.
    pub(super) fn sidebar_visible(&self) -> bool {
        self.frame
            .as_ref()
            .is_some_and(|f| f.rects.workspace_rect.is_some())
    }

    pub(super) fn select_ws(&mut self, i: usize) {
        let i = i.min(self.groups.len() - 1);
        if i != self.ws_sel {
            self.ws_sel = i;
            self.run_sel = 0;
        }
    }

    fn move_ws(&mut self, by: i64) {
        self.select_ws((self.ws_sel as i64 + by).clamp(0, self.groups.len() as i64 - 1) as usize);
    }

    fn move_run(&mut self, by: i64) {
        let n = self.running_ids().len() as i64;
        self.run_sel = (self.run_sel as i64 + by).clamp(0, (n - 1).max(0)) as usize;
    }

    /// A switch that cancels the running task needs a second press within `SWITCH_ARM_MS`.
    fn confirm_switch(&mut self, what: &str, now: i64) -> bool {
        if self.task.is_none()
            || self
                .switch_armed
                .as_ref()
                .is_some_and(|(w, t)| w == what && now - t < SWITCH_ARM_MS)
        {
            self.switch_armed = None;
            return true;
        }
        self.switch_armed = Some((what.to_string(), now));
        self.toast(
            "a task is running · do it again to cancel it and switch",
            now,
            true,
        );
        false
    }

    /// Enter / click on row `i` of the Running panel.
    fn open_row(&mut self, i: usize, now: i64) {
        self.run_sel = i;
        let Some(id) = self.running_ids().get(i).cloned() else {
            return;
        };
        if id == self.cfg.session_id {
            self.side = Focus::Agent;
        } else if self.confirm_switch(&id, now) {
            self.quit = Some(ExitReason::Resume(id));
        }
    }

    /// `/new`: a fresh session in the selected workspace.
    pub(super) fn new_session(&mut self, now: i64) {
        if self.confirm_switch("/new", now) {
            let dir = self.groups[self.ws_sel].raw.clone();
            self.quit = Some(ExitReason::OpenFolder(PathBuf::from(dir)));
        }
    }

    /// Tab from the chat: the Workspace panel, when the sidebar is on screen.
    pub(super) fn tab_into_sidebar(&mut self) {
        if self.sidebar_visible() {
            self.side = Focus::Workspace;
        }
    }

    /// A key while a panel has the focus. `false`: not for the panel, the chat takes it.
    pub(super) fn panel_key(&mut self, k: &Key, now: i64) -> bool {
        let ws = self.side == Focus::Workspace;
        let n = self.running_ids().len() as i64;
        let to = |s: &mut Self, first: bool| {
            if ws {
                s.select_ws(if first { 0 } else { s.groups.len() - 1 });
            } else {
                s.run_sel = if first { 0 } else { (n - 1).max(0) as usize };
            }
        };
        let step = |s: &mut Self, by: i64| if ws { s.move_ws(by) } else { s.move_run(by) };
        match k {
            Key::Named(Named::ShiftTab) => return false,
            Key::Named(nm) => match nm {
                Named::Tab => {
                    self.side = if ws { Focus::Dashboard } else { Focus::Agent };
                }
                Named::Escape => self.side = Focus::Agent,
                Named::Up => step(self, -1),
                Named::Down => step(self, 1),
                Named::PageUp => step(self, -(PANEL_PAGE as i64)),
                Named::PageDown => step(self, PANEL_PAGE as i64),
                Named::Home => to(self, true),
                Named::End => to(self, false),
                Named::Enter if ws => self.side = Focus::Dashboard,
                Named::Enter => self.open_row(self.run_sel, now),
                Named::Left | Named::Right => {}
                // anything else is for the chat (ctrl+c, ctrl+n, typing ...)
                _ => {
                    self.side = Focus::Agent;
                    return false;
                }
            },
            Key::Text(_) | Key::Paste(_) => {
                self.side = Focus::Agent;
                return false;
            }
            _ => return false,
        }
        true
    }

    /// Scrolls the chat so that chat item `item` (a prompt) is the first row of the view.
    fn jump_to_prompt(&mut self, item: usize) {
        let Some(b) = self.frame.as_ref().and_then(|f| f.chat_body.as_ref()) else {
            return;
        };
        let Some(&(_, row)) = b.prompts.iter().find(|(i, _)| *i == item) else {
            return;
        };
        let top = row.saturating_sub(b.skip);
        self.scroll = b.total.saturating_sub(b.h).saturating_sub(top);
    }

    /// Left click on a panel. `false`: not on one, the click goes on to the chat.
    pub(super) fn panel_click(&mut self, x: u32, y: u32, now: i64) -> bool {
        let p = Point {
            x: i64::from(x) - 1,
            y: i64::from(y) - 1,
        };
        let Some(f) = self.frame.as_ref() else {
            return false;
        };
        let r = &f.rects;
        let row_of =
            |rows: &[(i64, String)]| rows.iter().find(|(ry, _)| *ry == p.y).map(|r| r.1.clone());
        if let Some(rect) = r.workspace_rect.filter(|rc| contains(*rc, p)) {
            let key = row_of(&r.workspace_rows);
            let at = key.and_then(|k| self.groups.iter().position(|g| workspace_key(&g.cwd) == k));
            match at {
                Some(i) => {
                    self.select_ws(i);
                    self.side = Focus::Dashboard;
                }
                // the "↑ n more" / "↓ n more" rows
                None if p.y == rect.y + 1 => self.move_ws(-1),
                None if p.y == rect.y + rect.h - 2 => self.move_ws(1),
                None => {}
            }
            return true;
        }
        if let Some(rect) = r.session_rect.filter(|rc| contains(*rc, p)) {
            let id = row_of(&r.session_rows);
            let at = id.and_then(|id| self.running_ids().iter().position(|x| *x == id));
            match at {
                Some(i) => {
                    self.side = Focus::Dashboard;
                    self.open_row(i, now);
                }
                None if p.y == rect.y + 1 => self.move_run(-1),
                None if p.y == rect.y + rect.h - 2 => self.move_run(1),
                None => {}
            }
            return true;
        }
        if let Some(rect) = r.activity_rect.filter(|rc| contains(*rc, p)) {
            let hit = r
                .activity_hits
                .iter()
                .find(|(i, _)| rect.y + 1 + *i as i64 == p.y)
                .and_then(|(_, at)| self.activity.activity.get(*at))
                .and_then(|f| f.anchor);
            if let Some(item) = hit {
                self.jump_to_prompt(item);
            }
            return true;
        }
        false
    }

    /// The wheel over a panel scrolls that panel. `false`: not over one.
    pub(super) fn panel_wheel(&mut self, p: Point, up: bool) -> bool {
        let Some(f) = self.frame.as_ref() else {
            return false;
        };
        let r = &f.rects;
        let by = if up { -1 } else { 1 };
        if r.workspace_rect.is_some_and(|rc| contains(rc, p)) {
            self.move_ws(by);
        } else if r.session_rect.is_some_and(|rc| contains(rc, p)) {
            self.move_run(by);
        } else if r.activity_rect.is_some_and(|rc| contains(rc, p)) {
            // up = older rows
            let max = r.activity_max_scroll;
            self.act_scroll = if up {
                (self.act_scroll + WHEEL_LINES).min(max)
            } else {
                self.act_scroll.saturating_sub(WHEEL_LINES)
            };
        } else {
            return false;
        }
        true
    }
}
