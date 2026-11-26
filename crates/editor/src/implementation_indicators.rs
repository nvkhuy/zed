use std::collections::BTreeMap;
use std::ops::Range;

use collections::{HashMap, HashSet};
use futures::stream::{self, StreamExt as _};
use gpui::{AppContext as _, ClickEvent, Context, MouseButton, SharedString, TaskExt as _, Window};
use language::BufferSnapshot;
use lsp::LanguageServerId;
use multi_buffer::{MultiBufferRow, ToPoint as _};
use text::{Bias, BufferId, PointUtf16};
use ui::{
    ButtonCommon as _, ButtonStyle, Clickable as _, Color, IconButton, IconButtonShape, IconName,
    IconSize, Tooltip,
};
use util::paths::UrlExt as _;

use crate::{
    Editor, GotoDefinitionKind, LSP_REQUEST_DEBOUNCE_TIMEOUT, LspDataKind,
    display_map::{DisplayPoint, DisplayRow, ToDisplayPoint as _},
    hover_links::HoverLink,
};

const MAX_CONCURRENT_IMPLEMENTATION_REQUESTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImplementationDirection {
    /// Interface or interface method: jump down to concrete implementation(s).
    Down,
    /// Concrete type or method: jump up to interface or interface method(s).
    Up,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImplementationIndicator {
    pub anchor: text::Anchor,
    pub direction: ImplementationDirection,
    pub symbol_name: String,
    pub locations: Vec<(LanguageServerId, lsp::Location)>,
}

#[derive(Default)]
pub(crate) struct ImplementationIndicatorData {
    indicators: HashMap<BufferId, Vec<ImplementationIndicator>>,
}

impl ImplementationIndicatorData {
    pub(crate) fn is_empty(&self) -> bool {
        self.indicators.values().all(Vec::is_empty)
    }

    pub(crate) fn clear_buffer(&mut self, buffer_id: BufferId) {
        self.indicators.remove(&buffer_id);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImplementationCandidate {
    anchor: text::Anchor,
    point_utf16: PointUtf16,
    row: u32,
    direction: ImplementationDirection,
    symbol_name: String,
}

fn is_go_buffer(snapshot: &BufferSnapshot) -> bool {
    snapshot
        .language()
        .is_some_and(|language| language.name().as_ref() == "Go")
}

fn record_candidate(
    snapshot: &BufferSnapshot,
    byte_range: Range<usize>,
    direction: ImplementationDirection,
    by_row: &mut BTreeMap<u32, ImplementationCandidate>,
) {
    let offset = byte_range.start;
    if offset > snapshot.len() || byte_range.end > snapshot.len() {
        return;
    }
    let point = snapshot.offset_to_point(offset);
    let replace = match by_row.get(&point.row) {
        None => true,
        Some(existing) => {
            existing.direction == ImplementationDirection::Up
                && direction == ImplementationDirection::Down
        }
    };
    if !replace {
        return;
    }

    let symbol_name = snapshot.text_for_range(byte_range).collect::<String>();
    if symbol_name.is_empty() {
        return;
    }

    by_row.insert(
        point.row,
        ImplementationCandidate {
            anchor: snapshot.anchor_before(offset),
            point_utf16: snapshot.offset_to_point_utf16(offset),
            row: point.row,
            direction,
            symbol_name,
        },
    );
}

fn extract_go_implementation_candidates(snapshot: &BufferSnapshot) -> Vec<ImplementationCandidate> {
    if !is_go_buffer(snapshot) {
        return Vec::new();
    }

    let mut by_row = BTreeMap::new();
    for layer in snapshot.syntax_layers() {
        if layer.language.name().as_ref() != "Go" {
            continue;
        }
        let root = layer.node();
        let mut cursor = root.walk();
        let mut visited_children = false;

        loop {
            if !visited_children {
                let node = cursor.node();
                match node.kind() {
                    "type_spec" => {
                        if let (Some(name_node), Some(type_node)) = (
                            node.child_by_field_name("name"),
                            node.child_by_field_name("type"),
                        ) {
                            let direction = if type_node.kind() == "interface_type" {
                                ImplementationDirection::Down
                            } else {
                                ImplementationDirection::Up
                            };
                            record_candidate(
                                snapshot,
                                name_node.byte_range(),
                                direction,
                                &mut by_row,
                            );
                        }
                    }
                    "method_elem" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            record_candidate(
                                snapshot,
                                name_node.byte_range(),
                                ImplementationDirection::Down,
                                &mut by_row,
                            );
                        }
                    }
                    "method_declaration" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            record_candidate(
                                snapshot,
                                name_node.byte_range(),
                                ImplementationDirection::Up,
                                &mut by_row,
                            );
                        }
                    }
                    _ => {}
                }

                if cursor.goto_first_child() {
                    continue;
                }
            }

            if cursor.goto_next_sibling() {
                visited_children = false;
            } else if cursor.goto_parent() {
                visited_children = true;
            } else {
                break;
            }
        }
    }

    by_row.into_values().collect()
}

fn short_location_label(location: &lsp::Location) -> Option<String> {
    let path = location
        .uri
        .to_file_path_ext(util::paths::PathStyle::local())
        .ok()?;
    let file_name = path.file_name()?.to_string_lossy();
    let line = location.range.start.line + 1;
    Some(format!("{file_name}:{line}"))
}

impl Editor {
    pub(super) fn refresh_implementation_indicators(
        &mut self,
        for_buffer: Option<BufferId>,
        cx: &mut Context<Self>,
    ) {
        if !self.lsp_data_enabled() {
            return;
        }
        let Some(project) = self.project.as_ref().map(|project| project.downgrade()) else {
            return;
        };

        let buffers_to_query: Vec<_> = self
            .lsp_data_buffers(for_buffer, cx)
            .into_iter()
            .filter(|buffer| is_go_buffer(&buffer.read(cx).snapshot()))
            .collect();

        if buffers_to_query.is_empty() {
            return;
        }

        self.spawn_lsp_data_tasks(
            LspDataKind::ImplementationIndicators,
            for_buffer,
            buffers_to_query,
            cx,
            |buffer, cx| {
                let project = project.clone();
                let buffer_id = buffer.read(cx).remote_id();
                cx.spawn(async move |editor, cx| {
                    cx.background_executor()
                        .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                        .await;

                    let (snapshot, buffer_uri) = buffer.read_with(cx, |buffer, cx| {
                        let uri = buffer
                            .file()
                            .and_then(|file| file.as_local())
                            .and_then(|local| lsp::Uri::from_file_path(local.abs_path(cx)).ok());
                        (buffer.snapshot(), uri)
                    });

                    let candidates = cx
                        .background_spawn(async move {
                            extract_go_implementation_candidates(&snapshot)
                        })
                        .await;

                    if candidates.is_empty() {
                        editor
                            .update(cx, |editor, cx| {
                                if editor
                                    .implementation_indicators
                                    .indicators
                                    .remove(&buffer_id)
                                    .is_some()
                                {
                                    cx.notify();
                                }
                            })
                            .ok();
                        return;
                    }

                    let mut tasks = Vec::with_capacity(candidates.len());
                    for candidate in candidates {
                        let Ok(task) = project.update(cx, |project, cx| {
                            project.implementation_locations(&buffer, candidate.point_utf16, cx)
                        }) else {
                            return;
                        };
                        tasks.push((candidate, task));
                    }

                    let resolved: Vec<Option<ImplementationIndicator>> = stream::iter(tasks)
                        .map(|(candidate, task)| {
                            let buffer_uri = buffer_uri.clone();
                            async move {
                                let responses = task.await;
                                let mut seen = HashSet::default();
                                let mut locations = Vec::new();

                                for (server_id, server_locations) in responses {
                                    for location in server_locations {
                                        let is_self = buffer_uri.as_ref().is_some_and(|uri| {
                                            uri == &location.uri
                                                && location.range.start.line == candidate.row
                                        });
                                        if is_self {
                                            continue;
                                        }

                                        let dedup_key = (
                                            location.uri.as_str().to_owned(),
                                            location.range.start.line,
                                            location.range.start.character,
                                            location.range.end.line,
                                            location.range.end.character,
                                        );
                                        if seen.insert(dedup_key) {
                                            locations.push((server_id, location));
                                        }
                                    }
                                }

                                if locations.is_empty() {
                                    None
                                } else {
                                    Some(ImplementationIndicator {
                                        anchor: candidate.anchor,
                                        direction: candidate.direction,
                                        symbol_name: candidate.symbol_name,
                                        locations,
                                    })
                                }
                            }
                        })
                        .buffer_unordered(MAX_CONCURRENT_IMPLEMENTATION_REQUESTS)
                        .collect()
                        .await;

                    let indicators: Vec<ImplementationIndicator> =
                        resolved.into_iter().flatten().collect();

                    editor
                        .update(cx, |editor, cx| {
                            let changed = editor
                                .implementation_indicators
                                .indicators
                                .get(&buffer_id)
                                != Some(&indicators);
                            if indicators.is_empty() {
                                editor
                                    .implementation_indicators
                                    .indicators
                                    .remove(&buffer_id);
                            } else {
                                editor
                                    .implementation_indicators
                                    .indicators
                                    .insert(buffer_id, indicators);
                            }
                            if changed {
                                cx.notify();
                            }
                        })
                        .ok();
                })
            },
        );
    }

    pub(super) fn remove_invalid_implementation_indicators(
        &mut self,
        buffer_ids: &[BufferId],
        snapshot: &multi_buffer::MultiBufferSnapshot,
    ) {
        for buffer_id in buffer_ids {
            if snapshot.buffer_for_id(*buffer_id).is_none() {
                self.implementation_indicators.clear_buffer(*buffer_id);
            }
        }
    }

    pub(crate) fn active_implementation_indicators(
        &self,
        range: Range<DisplayRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> HashMap<DisplayRow, ImplementationIndicator> {
        let mut active = HashMap::default();
        if self.implementation_indicators.is_empty() {
            return active;
        }

        let snapshot = self.snapshot(window, cx);
        let multi_buffer_snapshot = snapshot.buffer_snapshot();

        let offset_range_start =
            snapshot.display_point_to_point(DisplayPoint::new(range.start, 0), Bias::Left);
        let offset_range_end =
            snapshot.display_point_to_point(DisplayPoint::new(range.end, 0), Bias::Right);

        for (buffer_id, indicators) in &self.implementation_indicators.indicators {
            if self.is_buffer_folded(*buffer_id, cx) {
                continue;
            }
            for indicator in indicators {
                let Some(multi_buffer_anchor) =
                    multi_buffer_snapshot.anchor_in_buffer(indicator.anchor)
                else {
                    continue;
                };
                let multibuffer_point = multi_buffer_anchor.to_point(multi_buffer_snapshot);
                if multibuffer_point < offset_range_start || multibuffer_point > offset_range_end {
                    continue;
                }

                let multibuffer_row = MultiBufferRow(multibuffer_point.row);
                if snapshot.is_line_folded(multibuffer_row)
                    && multibuffer_row
                        .0
                        .checked_sub(1)
                        .is_some_and(|prev| snapshot.is_line_folded(MultiBufferRow(prev)))
                {
                    continue;
                }

                let display_row = multibuffer_point.to_display_point(&snapshot).row();
                active.entry(display_row).or_insert_with(|| indicator.clone());
            }
        }

        active
    }

    pub(crate) fn render_implementation_indicator(
        &self,
        row: DisplayRow,
        indicator: ImplementationIndicator,
        cx: &mut Context<Self>,
    ) -> IconButton {
        let icon = match indicator.direction {
            ImplementationDirection::Down => IconName::ArrowDown,
            ImplementationDirection::Up => IconName::ArrowUp,
        };

        let count = indicator.locations.len();
        let title: SharedString = match (indicator.direction, count) {
            (ImplementationDirection::Down, 1) => "Go to implementation".into(),
            (ImplementationDirection::Down, n) => format!("Go to {n} implementations").into(),
            (ImplementationDirection::Up, 1) => "Implements interface".into(),
            (ImplementationDirection::Up, n) => format!("Implements {n} interfaces").into(),
        };

        let meta: SharedString = if count == 1 {
            if let Some(target_label) = short_location_label(&indicator.locations[0].1) {
                format!("{target_label} — Click to open, ⌘-click to split").into()
            } else {
                "Click to open, ⌘-click to split".into()
            }
        } else {
            "Click to view all, ⌘-click to split".into()
        };

        let locations = indicator.locations;
        IconButton::new(("implementation_indicator", row.0 as usize), icon)
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::XSmall)
            .icon_color(Color::Success)
            .style(ButtonStyle::Transparent)
            .on_click(cx.listener(move |editor, event: &ClickEvent, window, cx| {
                let split = match event {
                    ClickEvent::Mouse(mouse_event) => {
                        mouse_event.down.button == MouseButton::Middle
                            || mouse_event.down.modifiers.secondary(cx)
                            || mouse_event.down.modifiers.alt
                    }
                    _ => false,
                };
                let links = locations
                    .iter()
                    .map(|(server_id, location)| {
                        HoverLink::LspLocation(location.clone(), *server_id)
                    })
                    .collect();
                let nav_entry =
                    editor.navigation_entry(editor.selections.newest_anchor().head(), cx);
                editor
                    .navigate_to_hover_links(
                        Some(GotoDefinitionKind::Implementation),
                        links,
                        nav_entry,
                        split,
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
            }))
            .on_right_click(cx.listener(move |editor, event: &ClickEvent, window, cx| {
                editor.set_gutter_context_menu(row, None, event.position(), window, cx);
            }))
            .tooltip(move |_window, cx| Tooltip::with_meta(title.clone(), None, meta.clone(), cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::editor_lsp_test_context::EditorLspTestContext;
    use gpui::TestAppContext;
    use language::{Buffer, Language, LanguageConfig, LanguageMatcher};

    #[gpui::test]
    async fn test_extract_go_implementation_candidates(cx: &mut TestAppContext) {
        let go_lang = languages::language("go", tree_sitter_go::LANGUAGE.into());
        let source = indoc::indoc! {r#"
            package db

            import "context"

            // HealthChecker checks database connectivity.
            type HealthChecker interface {
                Ping(ctx context.Context) error
                Close() error
            }

            type Store[T any] interface {
                Get(id string) (T, error)
            }

            type DBHealthChecker struct{}

            func (d *DBHealthChecker) Ping(ctx context.Context) error {
                return nil
            }
        "#};

        let buffer = cx.new(|cx| Buffer::local(source, cx).with_language(go_lang, cx));
        cx.background_executor.run_until_parked();

        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let candidates = extract_go_implementation_candidates(&snapshot);

        let summary: Vec<(u32, &str, ImplementationDirection)> = candidates
            .iter()
            .map(|c| (c.row, c.symbol_name.as_str(), c.direction))
            .collect();

        assert_eq!(
            summary,
            vec![
                (5, "HealthChecker", ImplementationDirection::Down),
                (6, "Ping", ImplementationDirection::Down),
                (7, "Close", ImplementationDirection::Down),
                (10, "Store", ImplementationDirection::Down),
                (11, "Get", ImplementationDirection::Down),
                (14, "DBHealthChecker", ImplementationDirection::Up),
                (16, "Ping", ImplementationDirection::Up),
            ]
        );
    }

    #[gpui::test]
    async fn test_go_implementation_indicators_from_lsp(cx: &mut TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});

        let go_lang = Language::new(
            LanguageConfig {
                name: "Go".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["go".into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            Some(tree_sitter_go::LANGUAGE.into()),
        );

        let mut cx = EditorLspTestContext::new(
            go_lang,
            lsp::ServerCapabilities {
                implementation_provider: Some(lsp::ImplementationProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;

        let impl_uri = lsp::Uri::from_file_path(util::path!("/root/dir/postgres.go")).unwrap();
        let impl_uri_clone = impl_uri.clone();

        cx.set_request_handler::<lsp::request::GotoImplementation, _, _>(
            move |_, params, _| {
                let impl_uri = impl_uri_clone.clone();
                let line = params.text_document_position_params.position.line;
                let req_uri = params.text_document_position_params.text_document.uri;
                async move {
                    let locations = match line {
                        // HealthChecker interface -> implemented in postgres.go + self echo (filtered)
                        3 => vec![
                            lsp::Location {
                                uri: req_uri,
                                range: lsp::Range::new(
                                    lsp::Position::new(3, 5),
                                    lsp::Position::new(3, 18),
                                ),
                            },
                            lsp::Location {
                                uri: impl_uri,
                                range: lsp::Range::new(
                                    lsp::Position::new(10, 5),
                                    lsp::Position::new(10, 18),
                                ),
                            },
                        ],
                        // Ping interface method -> implemented in postgres.go
                        4 => vec![lsp::Location {
                            uri: impl_uri,
                            range: lsp::Range::new(
                                lsp::Position::new(15, 20),
                                lsp::Position::new(15, 24),
                            ),
                        }],
                        // Unused interface method -> no implementations
                        _ => vec![],
                    };
                    Ok(Some(lsp::GotoImplementationResponse::Array(locations)))
                }
            },
        );

        cx.set_state(indoc::indoc! {r#"
            package db

            // HealthChecker checks database connectivity.
            type HealthChecker interface {ˇ
                Ping(ctx context.Context) error
                Unused() error
            }
        "#});

        cx.background_executor
            .advance_clock(LSP_REQUEST_DEBOUNCE_TIMEOUT + std::time::Duration::from_millis(50));
        cx.run_until_parked();

        cx.update_editor(|editor, window, cx| {
            let active =
                editor.active_implementation_indicators(DisplayRow(0)..DisplayRow(10), window, cx);
            assert_eq!(active.len(), 2);

            let iface = active.get(&DisplayRow(3)).expect("HealthChecker indicator");
            assert_eq!(iface.direction, ImplementationDirection::Down);
            assert_eq!(iface.symbol_name, "HealthChecker");
            assert_eq!(iface.locations.len(), 1);
            assert_eq!(iface.locations[0].1.uri, impl_uri);

            let method = active.get(&DisplayRow(4)).expect("Ping indicator");
            assert_eq!(method.direction, ImplementationDirection::Down);
            assert_eq!(method.symbol_name, "Ping");
            assert_eq!(method.locations.len(), 1);
            assert_eq!(method.locations[0].1.range.start.line, 15);

            assert!(
                !active.contains_key(&DisplayRow(5)),
                "unimplemented method should not show an arrow"
            );
        });
    }
}
