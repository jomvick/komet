//! Git history pane: paged commit rows plus a topological lane graph.
//!
//! The pane is hosted by the current Changes surface for now, but owns its
//! data and rendering so it can move intact into the future right-panel tabs.

use std::collections::HashSet;
use std::time::Duration;

use chrono::DateTime;
use komet_proto::{GitHistoryCommit, GitHistoryPage, GitHistoryRef, GitHistoryRefKind};
use komet_rpc::methods;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, ListAlignment, ListState, PathBuilder, Render,
    SharedString, Subscription, Task, Window, canvas, div, list, point, prelude::*, px,
};

use crate::state::AppState;
use crate::theme::Theme;

const HISTORY_PAGE_SIZE: usize = 100;
const HISTORY_ROW_HEIGHT: f32 = 36.0;
const HISTORY_LANE_SPACING: f32 = 12.0;
const HISTORY_NODE_RADIUS: f32 = 3.25;
const HISTORY_STROKE_WIDTH: f32 = 1.5;
const HISTORY_GRAPH_SIDE_PADDING: f32 = 5.0;
const HISTORY_GRAPH_ROW_OVERLAP: f32 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentShape {
    Through,
    Incoming,
    Outgoing,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphSegment {
    from_lane: usize,
    to_lane: usize,
    color_id: usize,
    shape: SegmentShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphRow {
    sha: String,
    node_lane: usize,
    node_color_id: usize,
    segments: Vec<GraphSegment>,
    is_head: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GraphLayout {
    rows: Vec<GraphRow>,
    max_lane_count: usize,
}

#[derive(Debug, Clone)]
struct ActiveLane {
    id: usize,
    color_id: usize,
    target_sha: String,
}

fn lane_index(lanes: &[ActiveLane], id: usize) -> usize {
    lanes
        .iter()
        .position(|lane| lane.id == id)
        .expect("active Git history lane exists")
}

/// Commits arrive child-before-parent from `git log --topo-order`. Active
/// lanes point at the parent commit that will eventually resolve each path.
fn layout_graph(commits: &[GitHistoryCommit], head_sha: Option<&str>) -> GraphLayout {
    let mut active_lanes: Vec<ActiveLane> = Vec::new();
    let mut next_lane_id = 0usize;
    let mut next_color_id = 0usize;
    let mut max_lane_count = 0usize;
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let before = active_lanes.clone();
        let incoming: Vec<usize> = before
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| (lane.target_sha == commit.sha).then_some(index))
            .collect();
        let primary_incoming = incoming.first().copied();
        let node_lane = primary_incoming.unwrap_or(before.len());
        let primary_lane = primary_incoming.and_then(|index| before.get(index));
        let node_color_id = primary_lane.map(|lane| lane.color_id).unwrap_or_else(|| {
            let color = next_color_id;
            next_color_id += 1;
            color
        });
        let resolved_ids: HashSet<usize> = incoming.iter().map(|&index| before[index].id).collect();
        let mut next_lanes: Vec<ActiveLane> = before
            .iter()
            .filter(|lane| !resolved_ids.contains(&lane.id))
            .cloned()
            .collect();
        let mut outgoing: Vec<(usize, usize)> = Vec::new();

        let mut primary_outgoing_id = None;
        if let Some(first_parent) = commit.parent_shas.first() {
            let id = primary_lane.map(|lane| lane.id).unwrap_or_else(|| {
                let id = next_lane_id;
                next_lane_id += 1;
                id
            });
            let lane = ActiveLane {
                id,
                color_id: node_color_id,
                target_sha: first_parent.clone(),
            };
            next_lanes.insert(node_lane.min(next_lanes.len()), lane.clone());
            primary_outgoing_id = Some(id);
            outgoing.push((id, lane.color_id));
        }

        let mut parent_offset = 1usize;
        for parent_sha in commit.parent_shas.iter().skip(1) {
            if let Some(existing) = next_lanes
                .iter()
                .find(|lane| lane.target_sha == *parent_sha)
            {
                outgoing.push((existing.id, existing.color_id));
                continue;
            }
            let lane = ActiveLane {
                id: next_lane_id,
                color_id: next_color_id,
                target_sha: parent_sha.clone(),
            };
            next_lane_id += 1;
            next_color_id += 1;
            let primary_index = primary_outgoing_id
                .map(|id| lane_index(&next_lanes, id))
                .unwrap_or_else(|| node_lane.min(next_lanes.len()));
            next_lanes.insert(
                (primary_index + parent_offset).min(next_lanes.len()),
                lane.clone(),
            );
            parent_offset += 1;
            outgoing.push((lane.id, lane.color_id));
        }

        let mut segments: Vec<GraphSegment> = before
            .iter()
            .enumerate()
            .filter(|(_, lane)| !resolved_ids.contains(&lane.id))
            .map(|(from_lane, lane)| GraphSegment {
                from_lane,
                to_lane: lane_index(&next_lanes, lane.id),
                color_id: lane.color_id,
                shape: SegmentShape::Through,
            })
            .collect();
        segments.extend(incoming.iter().map(|&from_lane| GraphSegment {
            from_lane,
            to_lane: node_lane,
            color_id: before[from_lane].color_id,
            shape: SegmentShape::Incoming,
        }));
        segments.extend(outgoing.into_iter().map(|(id, color_id)| GraphSegment {
            from_lane: node_lane,
            to_lane: lane_index(&next_lanes, id),
            color_id,
            shape: SegmentShape::Outgoing,
        }));

        max_lane_count = max_lane_count
            .max(before.len())
            .max(next_lanes.len())
            .max(node_lane + 1);
        rows.push(GraphRow {
            sha: commit.sha.clone(),
            node_lane,
            node_color_id,
            segments,
            is_head: head_sha == Some(commit.sha.as_str()),
        });
        active_lanes = next_lanes;
    }

    GraphLayout {
        rows,
        max_lane_count,
    }
}

fn lane_x(lane: usize) -> f32 {
    HISTORY_GRAPH_SIDE_PADDING + HISTORY_NODE_RADIUS + lane as f32 * HISTORY_LANE_SPACING
}

fn graph_width(lane_count: usize) -> f32 {
    let count = lane_count.max(1);
    HISTORY_GRAPH_SIDE_PADDING * 2.0
        + HISTORY_NODE_RADIUS * 2.0
        + (count - 1) as f32 * HISTORY_LANE_SPACING
}

fn format_date(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.format("%b %-d, %Y").to_string())
        .unwrap_or_else(|_| "—".to_string())
}

fn ref_color(reference: &GitHistoryRef, theme: &Theme) -> gpui::Hsla {
    match reference.kind {
        GitHistoryRefKind::Branch => theme.accent,
        GitHistoryRefKind::Remote => theme.busy,
        GitHistoryRefKind::Tag => theme.warning,
    }
}

pub struct GitHistory {
    state: Entity<AppState>,
    started: bool,
    target_key: Option<String>,
    commits: Vec<GitHistoryCommit>,
    head_sha: Option<String>,
    next_cursor: Option<usize>,
    total_count: Option<usize>,
    loading: bool,
    error: Option<SharedString>,
    graph: GraphLayout,
    list: ListState,
    copied_sha: Option<String>,
    request_task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    _observe: Subscription,
}

impl GitHistory {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this, _, cx| {
            if this.started {
                this.ensure_loaded(cx);
            }
        });
        Self {
            state,
            started: false,
            target_key: None,
            commits: Vec::new(),
            head_sha: None,
            next_cursor: None,
            total_count: None,
            loading: false,
            error: None,
            graph: GraphLayout::default(),
            list: ListState::new(0, ListAlignment::Top, px(HISTORY_ROW_HEIGHT * 5.0)),
            copied_sha: None,
            request_task: None,
            copy_task: None,
            _observe: observe,
        }
    }

    fn context(&self, cx: &App) -> Option<(String, String, Option<String>)> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let cwd = chat.cwd.clone()?;
        let target = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        let key = format!("{}|{cwd}", target.as_deref().unwrap_or("local"));
        Some((key, cwd, target))
    }

    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        self.started = true;
        let Some((key, cwd, target)) = self.context(cx) else {
            self.target_key = None;
            self.commits.clear();
            self.graph = GraphLayout::default();
            self.list.reset(0);
            self.loading = false;
            return;
        };
        if self.target_key.as_deref() == Some(key.as_str()) {
            if !self.loading && self.commits.is_empty() && self.error.is_none() {
                self.fetch_page(key, cwd, target, 0, true, cx);
            }
            return;
        }
        self.request_task = None;
        self.loading = false;
        self.target_key = Some(key.clone());
        self.commits.clear();
        self.head_sha = None;
        self.next_cursor = None;
        self.total_count = None;
        self.error = None;
        self.graph = GraphLayout::default();
        self.list.reset(0);
        self.fetch_page(key, cwd, target, 0, true, cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        self.target_key = Some(key.clone());
        self.fetch_page(key, cwd, target, 0, true, cx);
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        let Some(cursor) = self.next_cursor else {
            return;
        };
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        self.fetch_page(key, cwd, target, cursor, false, cx);
    }

    fn fetch_page(
        &mut self,
        key: String,
        cwd: String,
        target: Option<String>,
        cursor: usize,
        reset: bool,
        cx: &mut Context<Self>,
    ) {
        if self.loading {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.loading = true;
        self.error = None;
        cx.notify();
        self.request_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd));
            params.insert("cursor".into(), serde_json::json!(cursor));
            params.insert("limit".into(), serde_json::json!(HISTORY_PAGE_SIZE));
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(methods::LIST_GIT_HISTORY, serde_json::Value::Object(params))
                .await;
            this.update(cx, |history, cx| {
                if history.target_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                history.loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<GitHistoryPage>(value)
                        .map_err(|error| komet_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(page) => {
                        let old_commit_count = history.commits.len();
                        let old_item_count =
                            old_commit_count + usize::from(history.next_cursor.is_some());
                        if reset {
                            history.commits = page.commits;
                            history.total_count = page.total_count;
                        } else {
                            let mut seen: HashSet<String> = history
                                .commits
                                .iter()
                                .map(|commit| commit.sha.clone())
                                .collect();
                            history.commits.extend(
                                page.commits
                                    .into_iter()
                                    .filter(|commit| seen.insert(commit.sha.clone())),
                            );
                            if page.total_count.is_some() {
                                history.total_count = page.total_count;
                            }
                        }
                        history.head_sha = page.head_sha;
                        history.next_cursor = page.next_cursor;
                        history.graph = layout_graph(&history.commits, history.head_sha.as_deref());
                        let new_item_count =
                            history.commits.len() + usize::from(history.next_cursor.is_some());
                        if reset {
                            history
                                .list
                                .reset_with_uniform_height(new_item_count, px(HISTORY_ROW_HEIGHT));
                        } else {
                            history.list.splice(
                                old_commit_count..old_item_count,
                                new_item_count - old_commit_count,
                            );
                        }
                    }
                    Err(error) => history.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn copy_sha(&mut self, sha: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()));
        self.copied_sha = Some(sha);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1_200))
                .await;
            this.update(cx, |history, cx| {
                history.copied_sha = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn graph_cell(row: GraphRow, lane_count: usize, theme: &Theme) -> AnyElement {
        let width = graph_width(lane_count);
        let palette = [
            theme.accent,
            theme.busy,
            theme.success,
            theme.warning,
            theme.danger,
            theme.text_muted,
        ];
        let segments = row.segments.clone();
        let paths = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let middle = HISTORY_ROW_HEIGHT / 2.0;
                for segment in &segments {
                    let from_x = lane_x(segment.from_lane);
                    let to_x = lane_x(segment.to_lane);
                    let mut builder = PathBuilder::stroke(px(HISTORY_STROKE_WIDTH));
                    let origin = bounds.origin;
                    match segment.shape {
                        SegmentShape::Incoming => {
                            builder.move_to(point(
                                origin.x + px(from_x),
                                origin.y - px(HISTORY_GRAPH_ROW_OVERLAP),
                            ));
                            builder.cubic_bezier_to(
                                point(origin.x + px(to_x), origin.y + px(middle)),
                                point(origin.x + px(from_x), origin.y + px(middle * 0.55)),
                                point(origin.x + px(to_x), origin.y + px(middle * 0.55)),
                            );
                        }
                        SegmentShape::Outgoing => {
                            builder.move_to(point(origin.x + px(from_x), origin.y + px(middle)));
                            builder.cubic_bezier_to(
                                point(
                                    origin.x + px(to_x),
                                    origin.y + px(HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP),
                                ),
                                point(origin.x + px(from_x), origin.y + px(middle * 1.45)),
                                point(origin.x + px(to_x), origin.y + px(middle * 1.45)),
                            );
                        }
                        SegmentShape::Through => {
                            builder.move_to(point(
                                origin.x + px(from_x),
                                origin.y - px(HISTORY_GRAPH_ROW_OVERLAP),
                            ));
                            builder.cubic_bezier_to(
                                point(
                                    origin.x + px(to_x),
                                    origin.y + px(HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP),
                                ),
                                point(origin.x + px(from_x), origin.y + px(middle)),
                                point(origin.x + px(to_x), origin.y + px(middle)),
                            );
                        }
                    }
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, palette[segment.color_id % palette.len()]);
                    }
                }
            },
        )
        .absolute()
        .inset_0();
        let color = palette[row.node_color_id % palette.len()];
        let node_x = lane_x(row.node_lane);
        let node = div()
            .absolute()
            .left(px(node_x - HISTORY_NODE_RADIUS))
            .top(px(HISTORY_ROW_HEIGHT / 2.0 - HISTORY_NODE_RADIUS))
            .size(px(HISTORY_NODE_RADIUS * 2.0))
            .rounded_full()
            .bg(color);
        div()
            .relative()
            .w(px(width))
            .h(px(HISTORY_ROW_HEIGHT))
            .flex_none()
            .child(paths)
            .when(row.is_head, |element| {
                element.child(
                    div()
                        .absolute()
                        .left(px(node_x - HISTORY_NODE_RADIUS - 2.25))
                        .top(px(HISTORY_ROW_HEIGHT / 2.0 - HISTORY_NODE_RADIUS - 2.25))
                        .size(px((HISTORY_NODE_RADIUS + 2.25) * 2.0))
                        .rounded_full()
                        .border_1()
                        .border_color(color)
                        .bg(theme.bg),
                )
            })
            .child(node)
            .into_any_element()
    }

    fn render_ref(reference: GitHistoryRef, theme: &Theme) -> AnyElement {
        let color = ref_color(&reference, theme);
        div()
            .h(px(16.0))
            .max_w(px(112.0))
            .px(px(5.0))
            .flex_none()
            .flex()
            .items_center()
            .rounded(px(4.0))
            .border_1()
            .border_color(color.opacity(0.22))
            .bg(color.opacity(0.07))
            .text_size(px(10.0))
            .text_color(color.opacity(0.9))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(reference.label)),
            )
            .into_any_element()
    }

    fn render_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if index >= self.commits.len() {
            let label = if self.loading {
                "Loading older commits…"
            } else if self.error.is_some() {
                "Retry loading older commits"
            } else {
                "Load older commits"
            };
            return div()
                .id("history-load-older")
                .h(px(48.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.0))
                .text_color(Theme::of(cx).text_muted)
                .when(!self.loading, |element| {
                    element
                        .cursor_pointer()
                        .hover(|style| style.bg(crate::theme::ink(0.04)))
                        .on_click(cx.listener(|this, _, _, cx| this.load_older(cx)))
                })
                .child(SharedString::from(label))
                .into_any_element();
        }
        let Some(commit) = self.commits.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let Some(graph_row) = self.graph.rows.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let sha = commit.sha.clone();
        let copied = self.copied_sha.as_deref() == Some(sha.as_str());
        let visible_refs: Vec<_> = commit.refs.iter().take(2).cloned().collect();
        let hidden_refs = commit.refs.len().saturating_sub(visible_refs.len());

        div()
            .h(px(HISTORY_ROW_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .border_b_1()
            .border_color(crate::theme::hairline(0.04))
            .text_size(px(11.0))
            .hover(|style| style.bg(crate::theme::ink(0.025)))
            .child(Self::graph_cell(
                graph_row,
                self.graph.max_lane_count,
                &theme,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(80.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pr(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(SharedString::from(if commit.subject.is_empty() {
                                "(no subject)".to_string()
                            } else {
                                commit.subject
                            })),
                    )
                    .children(
                        visible_refs
                            .into_iter()
                            .map(|reference| Self::render_ref(reference, &theme)),
                    )
                    .when(hidden_refs > 0, |element| {
                        element.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!("+{hidden_refs}"))),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_none()
                    .truncate()
                    .pr(px(8.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(if commit.author_name.is_empty() {
                        "Unknown".to_string()
                    } else {
                        commit.author_name
                    })),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_none()
                    .truncate()
                    .pr(px(8.0))
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format_date(&commit.authored_at))),
            )
            .child(
                div()
                    .id(SharedString::from(format!("history-sha-{index}")))
                    .w(px(68.0))
                    .h(px(24.0))
                    .mr(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::ink(0.07)))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.5))
                    .text_color(if copied {
                        theme.accent
                    } else {
                        theme.text_muted
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.copy_sha(sha.clone(), cx)))
                    .child(SharedString::from(if copied {
                        "Copied".to_string()
                    } else {
                        commit.sha.chars().take(7).collect()
                    })),
            )
            .into_any_element()
    }
}

impl Render for GitHistory {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        let theme = Theme::of(cx).clone();
        let count = self.total_count.unwrap_or(self.commits.len());
        let count_label = format!("{count} commit{}", if count == 1 { "" } else { "s" });
        let graph_column = graph_width(self.graph.max_lane_count);

        let body: AnyElement = if self.target_key.is_none() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child("No repository selected")
                .into_any_element()
        } else if self.loading && self.commits.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(crate::loaders::gradient_spinner(
                    "history-loading",
                    &theme,
                    3.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("Loading history…"),
                )
                .into_any_element()
        } else if self.commits.is_empty() {
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| SharedString::from("No commits found"));
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.0))
                .text_size(px(12.0))
                .text_color(if self.error.is_some() {
                    theme.warning
                } else {
                    theme.text_faint
                })
                .child(message)
                .into_any_element()
        } else {
            list(self.list.clone(), cx.processor(Self::render_row))
                .flex_1()
                .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(32.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .border_b_1()
                    .border_color(crate::theme::hairline(0.06))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(count_label))
                    .when(self.loading && !self.commits.is_empty(), |element| {
                        element.child(
                            div()
                                .ml(px(8.0))
                                .text_size(px(10.0))
                                .text_color(theme.text_faint)
                                .child("Reloading…"),
                        )
                    }),
            )
            .when_some(
                self.error.clone().filter(|_| !self.commits.is_empty()),
                |element, error| {
                    element.child(
                        div()
                            .h(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(px(8.0))
                            .border_b_1()
                            .border_color(theme.danger.opacity(0.16))
                            .bg(theme.danger.opacity(0.05))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.danger_muted)
                            .child(error),
                    )
                },
            )
            .when(!self.commits.is_empty(), |element| {
                element.child(
                    div()
                        .h(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .border_b_1()
                        .border_color(crate::theme::hairline(0.06))
                        .text_size(px(9.5))
                        .text_color(theme.text_faint)
                        .child(div().w(px(graph_column)).flex_none())
                        .child(div().flex_1().min_w(px(80.0)).child("Commit"))
                        .child(div().w(px(88.0)).flex_none().child("Author"))
                        .child(div().w(px(88.0)).flex_none().child("Date"))
                        .child(div().w(px(74.0)).flex_none().child("SHA")),
                )
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> GitHistoryCommit {
        GitHistoryCommit {
            sha: sha.into(),
            parent_shas: parents.iter().map(|value| (*value).into()).collect(),
            subject: sha.into(),
            author_name: "Test".into(),
            author_email: "test@example.com".into(),
            authored_at: "2026-08-12T12:00:00Z".into(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn graph_splits_and_rejoins_merge_lanes() {
        let commits = vec![
            commit("merge", &["main", "feature"]),
            commit("main", &["base"]),
            commit("feature", &["base"]),
            commit("base", &[]),
        ];
        let graph = layout_graph(&commits, Some("merge"));
        assert_eq!(graph.rows.len(), commits.len());
        assert!(graph.max_lane_count >= 2);
        assert!(graph.rows[0].is_head);
        assert_eq!(graph.rows[0].segments.len(), 2);
        assert_eq!(graph.rows[3].segments.len(), 2);
        assert_eq!(graph.rows[3].node_lane, 0);
    }

    #[test]
    fn appending_older_commits_preserves_the_loaded_prefix_layout() {
        let prefix = vec![commit("tip", &["parent"]), commit("parent", &["root"])];
        let before = layout_graph(&prefix, Some("tip"));
        let mut all = prefix.clone();
        all.push(commit("root", &[]));
        let after = layout_graph(&all, Some("tip"));
        assert_eq!(before.rows, after.rows[..before.rows.len()]);
    }
}
