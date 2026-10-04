use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::TextArea;

use crate::event::{NodeId, NodeKind, NodeSpec, Status, UiEvent};

#[derive(Debug, Clone)]
pub struct Node {
    pub spec: NodeSpec,
    pub kind: NodeKind,
    pub status: Status,
    pub tokens: u64,
    pub cost_microusd: u64,
    pub remaining: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum Entry {
    User {
        node: NodeId,
        text: String,
    },
    Assistant {
        node: NodeId,
        text: String,
    },
    Tool {
        node: NodeId,
        id: String,
        name: String,
        args: String,
        result: Option<String>,
        status: Status,
        expanded: bool,
    },
}

impl Entry {
    pub fn node(&self) -> NodeId {
        match self {
            Self::User { node, .. } | Self::Assistant { node, .. } | Self::Tool { node, .. } => {
                *node
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    Conversation,
    Tree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(String),
    Cancel,
    Quit,
}

pub struct App {
    pub nodes: BTreeMap<NodeId, Node>,
    pub entries: Vec<Entry>,
    pub input: TextArea<'static>,
    pub focus: Focus,
    pub selected_node: NodeId,
    pub selected_tool: usize,
    /// Lines back from the live end of the selected transcript.
    pub scroll_back: u16,
    pub remaining: u64,
    pub help: bool,
    pub confirm_quit: bool,
    pub notice: String,
    pub no_color: bool,
}

impl App {
    pub fn new(no_color: bool) -> Self {
        let mut app = Self {
            nodes: BTreeMap::new(),
            entries: Vec::new(),
            input: TextArea::default(),
            focus: Focus::Input,
            selected_node: 0,
            selected_tool: 0,
            scroll_back: 0,
            remaining: 20_000,
            help: false,
            confirm_quit: false,
            notice: "Offline prototype. Send a prompt to play the scripted run.".into(),
            no_color,
        };
        app.start_node(
            NodeSpec {
                id: 0,
                parent: None,
                name: "root".into(),
                model: "fake/root".into(),
            },
            NodeKind::Agent,
        );
        app.nodes.get_mut(&0).unwrap().status = Status::Idle;
        app
    }

    fn start_node(&mut self, spec: NodeSpec, kind: NodeKind) {
        // Topology is immutable after admission. Repeated root admission starts
        // the next turn while preserving cumulative accounting and history.
        self.nodes
            .entry(spec.id)
            .and_modify(|node| {
                node.status = Status::Running;
                node.spec.name.clone_from(&spec.name);
                node.spec.model.clone_from(&spec.model);
            })
            .or_insert(Node {
                spec,
                kind,
                status: Status::Running,
                tokens: 0,
                cost_microusd: 0,
                remaining: None,
            });
    }

    fn finish_node(&mut self, id: NodeId, status: Status) {
        if let Some(node) = self.nodes.get_mut(&id) {
            node.status = status;
        }
    }

    pub fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::AgentSpawned(spec) => self.start_node(spec, NodeKind::Agent),
            UiEvent::ReplCellStarted { node, code } => {
                let id = node.id;
                self.start_node(node, NodeKind::Cell);
                self.entries.push(Entry::User { node: id, text: code });
            }
            UiEvent::LlmCall { node } => self.start_node(node, NodeKind::Llm),
            UiEvent::AgentFinished { node, status } | UiEvent::LlmCallFinished { node, status } => {
                self.finish_node(node, status);
            }
            UiEvent::ReplCellFinished { node, output, status } => {
                self.finish_node(node, status);
                self.entries.push(Entry::Assistant { node, text: output });
            }
            UiEvent::TextDelta { node, text } => {
                // Interleaved child streams append to their own latest block.
                match self.entries.iter_mut().rev().find(|entry| entry.node() == node) {
                    Some(Entry::Assistant { text: existing, .. }) => existing.push_str(&text),
                    _ => self.entries.push(Entry::Assistant { node, text }),
                }
            }
            UiEvent::ToolCallStarted { node, id, name, args } => {
                self.entries.push(Entry::Tool { node, id, name, args, result: None, status: Status::Running, expanded: false });
            }
            UiEvent::ToolCallFinished { node, id, result, status } => {
                if let Some(Entry::Tool { result: output, status: state, .. }) = self.entries.iter_mut().rev().find(|entry| {
                    matches!(entry, Entry::Tool { node: owner, id: tool, .. } if *owner == node && *tool == id)
                }) {
                    *output = Some(result);
                    *state = status;
                }
            }
            UiEvent::Usage { node, tokens, cost_microusd } => {
                if let Some(node) = self.nodes.get_mut(&node) {
                    node.tokens = tokens;
                    node.cost_microusd = cost_microusd;
                }
            }
            UiEvent::Budget { node, remaining } => match node {
                Some(id) => {
                    if let Some(node) = self.nodes.get_mut(&id) { node.remaining = Some(remaining); }
                }
                None => self.remaining = remaining,
            },
        }
    }

    pub fn active(&self) -> bool {
        self.nodes
            .values()
            .any(|node| node.status == Status::Running)
    }

    pub fn totals(&self) -> (u64, u64) {
        self.nodes
            .values()
            .filter(|node| node.kind != NodeKind::Cell)
            .fold((0, 0), |(tokens, cost), node| {
                (
                    tokens.saturating_add(node.tokens),
                    cost.saturating_add(node.cost_microusd),
                )
            })
    }

    /// Parent-first rows, also exposing children admitted before their parent.
    /// A visited set bounds traversal even for malformed cyclic topology.
    pub fn tree_rows(&self) -> Vec<(NodeId, usize)> {
        let mut rows = Vec::new();
        let mut visited = std::collections::BTreeSet::new();
        let roots = self
            .nodes
            .values()
            .filter(|node| {
                node.spec
                    .parent
                    .is_none_or(|id| !self.nodes.contains_key(&id))
            })
            .map(|node| node.spec.id);
        for root in roots.chain(self.nodes.keys().copied()) {
            let mut stack = vec![(root, 0)];
            while let Some((id, depth)) = stack.pop() {
                if !visited.insert(id) {
                    continue;
                }
                rows.push((id, depth));
                stack.extend(
                    self.nodes
                        .values()
                        .rev()
                        .filter(|node| node.spec.parent == Some(id))
                        .map(|node| (node.spec.id, depth + 1)),
                );
            }
        }
        rows
    }

    pub fn cancel_running(&mut self) {
        for node in self
            .nodes
            .values_mut()
            .filter(|node| node.status == Status::Running)
        {
            node.status = Status::Cancelled;
        }
        for entry in &mut self.entries {
            if let Entry::Tool { status, result, .. } = entry
                && *status == Status::Running
            {
                *status = Status::Cancelled;
                *result = Some("Cancelled by user.".into());
            }
        }
        self.notice = "Turn cancelled. Send another prompt to replay.".into();
    }

    pub fn begin_turn(&mut self, prompt: String) {
        self.entries.push(Entry::User {
            node: 0,
            text: prompt,
        });
        self.finish_node(0, Status::Running);
        self.selected_node = 0;
        self.scroll_back = 0;
        self.notice = "Scripted run in progress.".into();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if self.confirm_quit {
            return match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => Action::Quit,
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirm_quit = false;
                    Action::None
                }
                _ => Action::None,
            };
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if (control && key.code == KeyCode::Char('c'))
            || (key.code == KeyCode::Char('q') && self.focus != Focus::Input && !self.help)
        {
            if self.active() {
                self.confirm_quit = true;
                return Action::None;
            }
            return Action::Quit;
        }
        if key.code == KeyCode::Char('?') && (self.focus != Focus::Input || self.help) {
            self.help = !self.help;
            return Action::None;
        }
        if self.help {
            if key.code == KeyCode::Esc {
                self.help = false;
            }
            return Action::None;
        }
        match key.code {
            KeyCode::Char('j') if control && self.focus == Focus::Input => {
                self.input.insert_newline();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match (self.focus, key.code == KeyCode::BackTab) {
                    (Focus::Input, false) | (Focus::Tree, true) => Focus::Conversation,
                    (Focus::Conversation, false) | (Focus::Input, true) => Focus::Tree,
                    _ => Focus::Input,
                };
            }
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Enter
                if self.focus == Focus::Input && !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                let text = self.input.lines().join("\n");
                if !text.trim().is_empty() && !self.active() {
                    self.input = TextArea::default();
                    return Action::Submit(text);
                }
                if self.active() {
                    self.notice = "A turn is active. Esc cancels it.".into();
                }
            }
            _ if self.focus == Focus::Input => {
                self.input.input(key);
            }
            KeyCode::Up | KeyCode::Down if self.focus == Focus::Tree => {
                let rows = self.tree_rows();
                let current = rows
                    .iter()
                    .position(|(id, _)| *id == self.selected_node)
                    .unwrap_or(0);
                let next = if key.code == KeyCode::Up {
                    current.saturating_sub(1)
                } else {
                    (current + 1).min(rows.len().saturating_sub(1))
                };
                if let Some((id, _)) = rows.get(next) {
                    self.selected_node = *id;
                }
                self.selected_tool = 0;
                self.scroll_back = 0;
            }
            KeyCode::Up => self.selected_tool = self.selected_tool.saturating_sub(1),
            KeyCode::Down => {
                let count = self
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.node() == self.selected_node && matches!(entry, Entry::Tool { .. })
                    })
                    .count();
                self.selected_tool = (self.selected_tool + 1).min(count.saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(Entry::Tool { expanded, .. }) = self
                    .entries
                    .iter_mut()
                    .filter(|entry| {
                        entry.node() == self.selected_node && matches!(entry, Entry::Tool { .. })
                    })
                    .nth(self.selected_tool)
                {
                    *expanded = !*expanded;
                }
            }
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(8),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(8),
            KeyCode::End => self.scroll_back = 0,
            _ => {}
        }
        Action::None
    }
}
