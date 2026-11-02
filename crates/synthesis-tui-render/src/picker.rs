//! Generic filterable picker state (port of `picker.js`, minus the `dynamic`/`navigate`/`back`
//! callbacks, which belong to the interactive layer).

use serde::Deserialize;

pub const DEFAULT_WINDOW: usize = 9;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub current: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Picker {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub items: Vec<PickerItem>,
    pub query: String,
    pub choice: usize,
    pub offset: usize,
    pub window_size: usize,
    pub loading: bool,
    pub loading_text: String,
    /// A yes/no style choice: no "filter:" line, typing does nothing.
    pub no_filter: bool,
}

/// Loose in-order character match, case-insensitive (the same rule "/" and "@" completion use).
pub fn matches(query: &str, text: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let tl: String = t.iter().collect();
    if tl.contains(&q.iter().collect::<String>()) {
        return true;
    }
    let mut i = 0;
    for ch in q {
        match t[i..].iter().position(|c| *c == ch) {
            Some(p) => i += p + 1,
            None => return false,
        }
    }
    true
}

impl Picker {
    pub fn new(id: &str, title: &str, subtitle: &str, items: Vec<PickerItem>) -> Self {
        let choice = items.iter().position(|i| i.current).unwrap_or(0);
        Picker {
            id: id.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            items,
            choice,
            window_size: DEFAULT_WINDOW,
            ..Default::default()
        }
    }

    pub fn filtered(&self) -> Vec<&PickerItem> {
        if self.query.is_empty() {
            return self.items.iter().collect();
        }
        self.items
            .iter()
            .filter(|it| matches(&self.query, &it.label) || matches(&self.query, &it.detail))
            .collect()
    }

    fn clamp_offset(&mut self) {
        let n = self.filtered().len();
        let size = self.window_size.max(1);
        let max_offset = n.saturating_sub(size);
        let mut offset = self.offset;
        if self.choice < offset {
            offset = self.choice;
        }
        if self.choice >= offset + size {
            offset = self.choice + 1 - size;
        }
        self.offset = offset.min(max_offset);
    }

    /// The window the renderer actually drew (it may be smaller than `DEFAULT_WINDOW`).
    pub fn set_window(&mut self, size: usize) {
        self.window_size = if size == 0 { DEFAULT_WINDOW } else { size };
        self.clamp_offset();
    }

    pub fn move_by(&mut self, delta: i64) {
        let n = self.filtered().len() as i64;
        if n == 0 {
            return;
        }
        self.choice = (self.choice as i64 + delta).rem_euclid(n) as usize;
        self.clamp_offset();
    }

    pub fn home(&mut self) {
        if !self.filtered().is_empty() {
            self.choice = 0;
            self.clamp_offset();
        }
    }

    pub fn end(&mut self) {
        let n = self.filtered().len();
        if n > 0 {
            self.choice = n - 1;
            self.clamp_offset();
        }
    }

    pub fn type_text(&mut self, text: &str) {
        self.query.push_str(text);
        self.choice = 0;
        self.offset = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.choice = 0;
        self.offset = 0;
    }
}
