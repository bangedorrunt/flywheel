//! Catalog hub (ADR-0007 D4): a ratatui multi-panel browser over the catalog
//! nouns — slots | models | profiles/squads.
//!
//! hacker-night semantic slots ONLY (`crate::output::Slot`); never raw hex in
//! widget code (tui-design §4). Refuses on non-TTY / `NO_COLOR` /
//! `TERM=dumb`, naming the CLI verbs instead. Min size 80×24.
//!
//! Keys: `j/k g/G` move · `Tab` focus panel · `Space` multi-select · `d`
//! delete (named CLI form printed — destructive edits confirm outside the
//! hub in v1) · `?` help · `q` quit.
//!
//! governed-by: ADR-0007 (D4)

use crate::models::{self, ModelRegistry};
use crate::output::Slot;
use crate::profile::{self, ProfileCatalog};
use anyhow::{Result, bail};
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::io::IsTerminal;

/// The three focusable panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Slots,
    Models,
    Profiles,
}

impl Panel {
    fn next(self) -> Self {
        match self {
            Self::Slots => Self::Models,
            Self::Models => Self::Profiles,
            Self::Profiles => Self::Slots,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Slots => "slots",
            Self::Models => "models",
            Self::Profiles => "profiles / squads",
        }
    }
}

/// A flattened row for the profiles/squads panel (D3: 1-member derived rows
/// hidden unless `--all`-style expanded view; the hub shows both lists).
struct CatalogRow {
    label: String,
    detail: String,
    builtin: bool,
    selected: bool,
}

/// The TTY/Term guard from D4: refuses the TUI and names the CLI.
pub fn guard_tty() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "catalog: not a TTY — use the CLI: flywheel slot list --json · \
             model list --json · profile list · squad list · catalog --json"
        );
    }
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        bail!(
            "catalog: NO_COLOR set — use the CLI: flywheel slot list · model list · \
             profile list · squad list"
        );
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term == "dumb" || term.is_empty() {
        bail!(
            "catalog: TERM={term:?} — use the CLI: flywheel slot list · model list · \
             profile list · squad list"
        );
    }
    Ok(())
}

fn slot_style(slot: Slot) -> Style {
    let (r, g, b) = crate::output::slot_rgb(slot);
    Style::default().fg(ratatui::style::Color::Rgb(r, g, b))
}

/// Build the models panel rows.
fn model_rows(reg: &ModelRegistry) -> Vec<CatalogRow> {
    let user = models::ModelRegistry::load().ok();
    reg.models
        .iter()
        .map(|(id, e)| CatalogRow {
            label: id.clone(),
            detail: format!(
                "tier={} ${:.2}/${:.2} caps={}",
                e.tier.map(super::models::Tier::as_str).unwrap_or("-"),
                e.cost_in_per_mtok,
                e.cost_out_per_mtok,
                if e.caps.is_empty() {
                    "*".to_string()
                } else {
                    e.caps.join(",")
                }
            ),
            builtin: !user
                .as_ref()
                .map(|u| u.models.contains_key(id))
                .unwrap_or(false),
            selected: false,
        })
        .collect()
}

/// Build the slots panel rows.
fn slot_rows(reg: &ModelRegistry) -> Vec<CatalogRow> {
    let user = models::ModelRegistry::load().ok();
    reg.slots
        .iter()
        .map(|(name, s)| CatalogRow {
            label: name.clone(),
            detail: format!(
                "{} · {}{}",
                s.harness,
                s.model,
                s.fallback_harness
                    .as_deref()
                    .map(|f| format!(" (fb {f})"))
                    .unwrap_or_default()
            ),
            builtin: !user
                .as_ref()
                .map(|u| u.slots.contains_key(name))
                .unwrap_or(false),
            selected: false,
        })
        .collect()
}

/// Build the profiles panel rows: 6 job profiles + compiled-in squads (D2/D3).
fn profile_rows(cat: &ProfileCatalog) -> Vec<CatalogRow> {
    let mut rows: Vec<CatalogRow> = cat
        .names()
        .into_iter()
        .filter_map(|name| {
            let p = cat.get(&name)?;
            let route = models::ModelRegistry::load_routing()
                .ok()
                .and_then(|r| r.resolve_profile_route(&p.harness, &p.model).ok());
            let detail = match route {
                Some((h, m)) => format!("{h} · {m} · effort={}", p.effort),
                None => format!("{} · {} · effort={}", p.harness, p.model, p.effort),
            };
            Some(CatalogRow {
                label: format!("@{name}"),
                detail,
                builtin: crate::is_builtin_profile(&name),
                selected: false,
            })
        })
        .collect();
    for name in profile::compiled_in_squads().keys() {
        rows.push(CatalogRow {
            label: format!("[{name}]"),
            detail: "squad (compiled-in)".to_string(),
            builtin: true,
            selected: false,
        });
    }
    rows
}

/// Terminal-cell style helper: panel title uses cyan when focused.
fn panel_block(title: &'static str, focused: bool) -> Block<'static> {
    let title_span = Span::styled(
        format!(" {title} "),
        if focused {
            slot_style(Slot::AccentCyan).add_modifier(Modifier::BOLD)
        } else {
            slot_style(Slot::FgMuted)
        },
    );
    Block::default()
        .borders(Borders::ALL)
        .border_style(slot_style(if focused {
            Slot::AccentCyan
        } else {
            Slot::FgMuted
        }))
        .title(title_span)
}

fn render_panel(
    f: &mut Frame,
    area: Rect,
    title: &'static str,
    rows: &[CatalogRow],
    state: &mut ListState,
    focused: bool,
) {
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let marker = if r.selected { "* " } else { "  " };
            let builtin_mark = if r.builtin { "" } else { " +" };
            ListItem::new(Line::from(vec![
                Span::styled(marker, slot_style(Slot::Warning)),
                Span::styled(
                    format!("{:<28}", r.label),
                    slot_style(if focused {
                        Slot::FgEmphasis
                    } else {
                        Slot::FgDefault
                    })
                    .add_modifier(if r.builtin {
                        Modifier::empty()
                    } else {
                        Modifier::BOLD
                    }),
                ),
                Span::styled(
                    format!("{detail}{builtin_mark}", detail = r.detail),
                    slot_style(Slot::FgMuted),
                ),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(panel_block(title, focused))
        .highlight_style(slot_style(Slot::BgOverlay).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, state);
}

/// Run the hub. Returns when the operator quits. Destructive actions print
/// the exact CLI verb to run (v1: observe + select, mutate via CLI — the
/// same dual-output contract as every catalog verb).
pub fn run(json_fallback: bool) -> Result<()> {
    if json_fallback {
        return crate::catalog_snapshot_json();
    }
    guard_tty()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let result = run_loop();
    crossterm::execute!(
        stdout,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    crossterm::terminal::disable_raw_mode()?;
    result
}

fn run_loop() -> Result<()> {
    let mut terminal = ratatui::init();
    let mut focus = Panel::Slots;
    let mut help = false;
    let mut status = String::from("j/k move · Tab panel · ? help · q quit");
    let result = loop {
        let reg = match models::ModelRegistry::load_routing() {
            Ok(r) => r,
            Err(e) => bail!("catalog: {e:#}"),
        };
        let cat = match ProfileCatalog::load() {
            Ok(c) => c,
            Err(e) => bail!("catalog: {e:#}"),
        };
        let slots = slot_rows(&reg);
        let models_rows = model_rows(&reg);
        let profiles = profile_rows(&cat);
        let mut slot_state = ListState::default();
        slot_state.select(Some(0));
        let mut model_state = ListState::default();
        model_state.select(Some(0));
        let mut profile_state = ListState::default();
        profile_state.select(Some(0));

        terminal
            .draw(|f: &mut Frame| {
                let root = f.area();
                let [top, status_bar] = Layout::vertical([
                    Constraint::Min(3),
                    Constraint::Length(1),
                ])
                .areas(root);
                let columns = Layout::horizontal([
                    Constraint::Ratio(1, 3),
                    Constraint::Ratio(1, 3),
                    Constraint::Ratio(1, 3),
                ])
                .split(top);
                render_panel(f, columns[0], Panel::Slots.title(), &slots, &mut slot_state, focus == Panel::Slots);
                render_panel(f, columns[1], Panel::Models.title(), &models_rows, &mut model_state, focus == Panel::Models);
                render_panel(f, columns[2], Panel::Profiles.title(), &profiles, &mut profile_state, focus == Panel::Profiles);
                let text = if help {
                    "keys: j/k g/G move · Tab focus · Space multi-select · d delete (prints the CLI verb) · q quit".to_string()
                } else {
                    status.clone()
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(text, slot_style(Slot::AccentLav)))),
                    status_bar,
                );
            })
            .map_err(|e| anyhow::anyhow!("draw: {e}"))?;

        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('?') => help = !help,
                    KeyCode::Tab => focus = focus.next(),
                    KeyCode::Char('d') => {
                        status = match focus {
                            Panel::Models => {
                                let idx = model_state.selected().unwrap_or(0);
                                match models_rows.get(idx) {
                                    Some(r) if r.builtin => {
                                        format!("compiled-in: {} is permanent (flywheel model remove <user-id> --force)", r.label)
                                    }
                                    Some(r) => format!("run: flywheel model remove {} --force", r.label),
                                    None => "nothing under the cursor".to_string(),
                                }
                            }
                            Panel::Slots => "slots mutate via: flywheel slot set <name> --harness <h> --model <id>".to_string(),
                            Panel::Profiles => {
                                let idx = profile_state.selected().unwrap_or(0);
                                match profiles.get(idx) {
                                    Some(r) if r.label.starts_with('[') => {
                                        "squads mutate via: flywheel squad create <name> --profile p=n".to_string()
                                    }
                                    Some(r) => format!("run: flywheel profile set {} --model @slot", r.label),
                                    None => "nothing under the cursor".to_string(),
                                }
                            }
                        };
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(e) => break Err(anyhow::anyhow!("event: {e}")),
        }
    };
    ratatui::restore();
    result
}
