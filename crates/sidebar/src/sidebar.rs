pub mod terminal_thread_metadata_store;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use chrono::Utc;
use editor::{Editor, EditorEvent};
use git_ui_core::worktree_picker::WorktreePicker;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyContext, Pixels, Render,
    SharedString, Subscription, WeakEntity, Window, actions, prelude::*, px,
};
use platform_title_bar::{SidebarWindowControls, apply_title_bar_insets};
use project::{ProjectGroupKey, WorktreePaths};
use remote::RemoteConnectionOptions;
use serde::{Deserialize, Serialize};
use settings::{SettingsStore, SidebarDockPosition};
use terminal::Event as TerminalEvent;
use terminal_view::TerminalView;
use ui::{
    ButtonStyle, ContextMenu, IconButton, IconName, IconSize, Label, LabelSize, ListItem,
    ListItemSpacing, Tooltip, prelude::*, right_click_menu,
    utils::{TRAFFIC_LIGHT_PADDING, platform_title_bar_height},
};
use util::path_list::PathList;
use workspace::{
    FocusWorkspaceSidebar, MoveProjectDown, MoveProjectUp, MultiWorkspace, MultiWorkspaceEvent,
    NextProject, NextThread, Open, OpenMode, PreviousProject, PreviousThread, ProjectGroup,
    Sidebar as WorkspaceSidebar, SidebarEvent, SidebarSide, ToggleWorkspaceSidebar, Workspace,
    sidebar_side_context_menu,
};
use zed_actions::agents_sidebar::FocusSidebarFilter;

pub use terminal_thread_metadata_store::{
    TerminalThreadMetadata, TerminalThreadMetadataStore, terminal_title_prefix,
};

pub const SIDEBAR_DEFAULT_WIDTH: Pixels = px(260.0);
pub const SIDEBAR_MIN_WIDTH: Pixels = px(180.0);
pub const SIDEBAR_MAX_WIDTH: Pixels = px(520.0);

actions!(
    sidebar,
    [
        /// Creates a new terminal thread in the active project.
        NewTerminalThread,
        /// Renames the currently selected terminal thread.
        RenameSelectedThread,
    ]
);

actions!(
    dev,
    [
        /// Dumps multi-workspace state (projects, worktrees, terminal threads) into a new buffer.
        DumpWorkspaceInfo,
    ]
);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct TerminalId(uuid::Uuid);

impl TerminalId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub fn to_key_string(&self) -> String {
        self.0.hyphenated().to_string()
    }

    pub fn from_key_string(value: &str) -> anyhow::Result<Self> {
        Ok(Self(uuid::Uuid::parse_str(value)?))
    }
}

impl Default for TerminalId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

pub fn init(cx: &mut App) {
    terminal_thread_metadata_store::init(cx);
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedSidebarState {
    width: Option<f32>,
}

struct LiveTerminalEntry {
    workspace: WeakEntity<Workspace>,
    terminal_view: WeakEntity<TerminalView>,
    has_bell: bool,
    _subscriptions: Vec<Subscription>,
}

struct SidebarTerminalRow {
    metadata: TerminalThreadMetadata,
    is_live: bool,
    has_bell: bool,
    worktree_label: Option<SharedString>,
}

struct SidebarProjectSection {
    group: ProjectGroup,
    display_name: SharedString,
    branch_name: Option<SharedString>,
    is_active: bool,
    terminals: Vec<SidebarTerminalRow>,
}

pub struct Sidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    rename_editor: Entity<Editor>,
    renaming_terminal: Option<TerminalId>,
    active_terminal: Option<TerminalId>,
    live_terminals: HashMap<TerminalId, LiveTerminalEntry>,
    width: Pixels,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Sidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter workspaces & threads…", window, cx);
            editor
        });

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));

        let mut subscriptions = Vec::new();

        subscriptions.push(cx.subscribe_in(
            &multi_workspace,
            window,
            |this, _mw, event: &MultiWorkspaceEvent, _window, cx| match event {
                MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                | MultiWorkspaceEvent::WorkspaceAdded(_)
                | MultiWorkspaceEvent::WorkspaceRemoved(_)
                | MultiWorkspaceEvent::ProjectGroupsChanged => {
                    this.prune_dead_terminals();
                    cx.notify();
                }
            },
        ));

        subscriptions.push(cx.subscribe(&filter_editor, |_this, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.notify();
            }
        }));

        subscriptions.push(cx.subscribe_in(
            &rename_editor,
            window,
            |this, editor, event, window, cx| match event {
                EditorEvent::Blurred => {
                    this.commit_rename(editor.clone(), cx);
                }
                _ => {
                    let _ = window;
                }
            },
        ));

        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
            subscriptions.push(cx.observe(&store, |_this, _, cx| {
                cx.notify();
            }));
        }

        subscriptions.push(cx.observe_global::<SettingsStore>(|_this, cx| {
            cx.notify();
        }));

        Self {
            multi_workspace: multi_workspace.downgrade(),
            focus_handle,
            filter_editor,
            rename_editor,
            renaming_terminal: None,
            active_terminal: None,
            live_terminals: HashMap::new(),
            width: SIDEBAR_DEFAULT_WIDTH,
            _subscriptions: subscriptions,
        }
    }

    fn prune_dead_terminals(&mut self) {
        self.live_terminals
            .retain(|_, entry| entry.terminal_view.upgrade().is_some());
    }

    fn sidebar_dock_position(cx: &App) -> SidebarDockPosition {
        cx.global::<SettingsStore>()
            .raw_user_settings()
            .and_then(|u| u.content.agent.as_ref())
            .and_then(|a| a.threads_sidebar.as_ref())
            .and_then(|s| s.position)
            .unwrap_or(SidebarDockPosition::Left)
    }

    fn project_display_name(group: &ProjectGroup, cx: &App) -> SharedString {
        if let Some(workspace) = group.workspaces.first() {
            let project = workspace.read(cx).project().read(cx);
            let names: Vec<String> = project
                .visible_worktrees(cx)
                .map(|wt| wt.read(cx).root_name_str().to_string())
                .collect();
            if !names.is_empty() {
                return SharedString::from(names.join(", "));
            }
        }
        let path_names: Vec<String> = group
            .key
            .path_list()
            .paths()
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        if path_names.is_empty() {
            SharedString::from("Untitled Workspace")
        } else {
            SharedString::from(path_names.join(", "))
        }
    }

    fn project_branch_name(group: &ProjectGroup, cx: &App) -> Option<SharedString> {
        let workspace = group.workspaces.first()?;
        let repo = workspace.read(cx).project().read(cx).active_repository(cx)?;
        let branch = repo.read(cx).branch.as_ref()?;
        Some(branch.name().to_string().into())
    }

    fn sections(&self, cx: &App) -> Vec<SidebarProjectSection> {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return Vec::new();
        };
        let mw = multi_workspace.read(cx);
        let active_workspace_id = mw.workspace().entity_id();
        let groups = mw.project_groups(cx);
        let filter = self.filter_editor.read(cx).text(cx).trim().to_lowercase();
        let store = TerminalThreadMetadataStore::try_global(cx);

        let mut sections = Vec::with_capacity(groups.len());
        for group in groups {
            let display_name = Self::project_display_name(&group, cx);
            let branch_name = Self::project_branch_name(&group, cx);
            let is_active = group
                .workspaces
                .iter()
                .any(|w| w.entity_id() == active_workspace_id);

            let mut terminals: Vec<SidebarTerminalRow> = Vec::new();
            if let Some(store) = &store {
                let store = store.read(cx);
                let host = group.key.host();
                let mut seen = std::collections::HashSet::new();
                let mut collected: Vec<TerminalThreadMetadata> = store
                    .entries_for_main_worktree_path(group.key.path_list(), host)
                    .chain(store.entries_for_path(group.key.path_list(), host))
                    .filter(|meta| seen.insert(meta.terminal_id))
                    .cloned()
                    .collect();
                collected.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));

                for meta in collected {
                    let title = meta.display_title();
                    if !filter.is_empty()
                        && !title.to_lowercase().contains(&filter)
                        && !display_name.to_lowercase().contains(&filter)
                    {
                        continue;
                    }
                    let live_entry = self.live_terminals.get(&meta.terminal_id);
                    let is_live = live_entry
                        .and_then(|e| e.terminal_view.upgrade())
                        .is_some();
                    let has_bell = live_entry.is_some_and(|e| e.has_bell);
                    let worktree_label = meta
                        .worktree_paths
                        .ordered_pairs()
                        .find(|(main_path, folder_path)| main_path != folder_path)
                        .and_then(|(_, folder_path)| {
                            folder_path
                                .file_name()
                                .map(|n| SharedString::from(n.to_string_lossy().into_owned()))
                        });
                    terminals.push(SidebarTerminalRow {
                        metadata: meta,
                        is_live,
                        has_bell,
                        worktree_label,
                    });
                }
            }

            if !filter.is_empty()
                && terminals.is_empty()
                && !display_name.to_lowercase().contains(&filter)
            {
                continue;
            }

            sections.push(SidebarProjectSection {
                group,
                display_name,
                branch_name,
                is_active,
                terminals,
            });
        }

        sections
    }

    fn open_or_activate_workspace(
        &mut self,
        path_list: PathList,
        host: Option<RemoteConnectionOptions>,
        provisional_key: Option<ProjectGroupKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_ready: impl FnOnce(&mut Self, Entity<Workspace>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let existing = multi_workspace
            .read(cx)
            .workspace_for_paths(&path_list, host.as_ref(), cx)
            .or_else(|| {
                provisional_key.as_ref().and_then(|key| {
                    multi_workspace
                        .read(cx)
                        .last_active_workspace_for_group(key, cx)
                })
            });

        if let Some(workspace) = existing {
            multi_workspace.update(cx, |mw, cx| {
                mw.activate(workspace.clone(), None, window, cx);
            });
            on_ready(self, workspace, window, cx);
            cx.notify();
            return;
        }

        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let task = multi_workspace.update(cx, |mw, cx| {
            mw.find_or_create_workspace(
                path_list,
                host,
                provisional_key,
                move |options, window, cx| {
                    remote_connection::connect_with_modal(&active_workspace, options, window, cx)
                },
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            let workspace = result?;
            this.update_in(cx, |this, window, cx| {
                on_ready(this, workspace, window, cx);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn activate_group(
        &mut self,
        key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_list = key.path_list().clone();
        let host = key.host().cloned();
        self.open_or_activate_workspace(
            path_list,
            host,
            Some(key),
            window,
            cx,
            |_this, _workspace, _window, _cx| {},
        );
    }

    fn toggle_group_expanded(&mut self, key: ProjectGroupKey, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        multi_workspace.update(cx, |mw, cx| {
            if let Some(state) = mw.group_state_by_key_mut(&key) {
                state.expanded = !state.expanded;
            }
            mw.serialize(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn spawn_terminal_thread_for_group(
        &mut self,
        key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_list = key.path_list().clone();
        let host = key.host().cloned();
        let group_key = key.clone();
        self.open_or_activate_workspace(
            path_list,
            host,
            Some(key),
            window,
            cx,
            move |this, workspace, window, cx| {
                if let Some(multi_workspace) = this.multi_workspace.upgrade() {
                    multi_workspace.update(cx, |mw, cx| {
                        if let Some(state) = mw.group_state_by_key_mut(&group_key) {
                            state.expanded = true;
                        }
                        mw.retain_active_workspace(cx);
                    });
                }

                let (worktree_paths, remote_connection, working_directory) = {
                    let ws = workspace.read(cx);
                    let project = ws.project().read(cx);
                    let worktree_paths = project.worktree_paths(cx);
                    let remote_connection = project.remote_connection_options(cx).cloned();
                    let working_directory =
                        terminal_view::default_working_directory(ws, cx).or_else(|| {
                            worktree_paths
                                .folder_path_list()
                                .paths()
                                .first()
                                .cloned()
                        });
                    (worktree_paths, remote_connection, working_directory)
                };

                let terminal_id = TerminalId::new();
                let initial_title: SharedString = working_directory
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| SharedString::from(n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| SharedString::from("Terminal"));

                let metadata = TerminalThreadMetadata {
                    terminal_id,
                    title: initial_title,
                    custom_title: None,
                    created_at: Utc::now(),
                    worktree_paths,
                    remote_connection,
                    working_directory: working_directory.clone(),
                };

                if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
                    store.update(cx, |store, cx| {
                        store.save(metadata.clone(), cx);
                    });
                }

                this.spawn_terminal_view_in_workspace(
                    terminal_id,
                    workspace,
                    working_directory,
                    window,
                    cx,
                );
            },
        );
    }

    fn open_or_activate_terminal_thread(
        &mut self,
        metadata: TerminalThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        // If already live, activate its workspace and focus its pane item.
        if let Some(entry) = self.live_terminals.get_mut(&metadata.terminal_id) {
            entry.has_bell = false;
            if let (Some(workspace), Some(terminal_view)) =
                (entry.workspace.upgrade(), entry.terminal_view.upgrade())
            {
                multi_workspace.update(cx, |mw, cx| {
                    mw.activate(workspace.clone(), None, window, cx);
                });
                workspace.update(cx, |ws, cx| {
                    let item_id = terminal_view.entity_id();
                    for pane in ws.panes() {
                        let idx = pane
                            .read(cx)
                            .items()
                            .position(|item| item.item_id() == item_id);
                        if let Some(idx) = idx {
                            pane.update(cx, |pane, cx| {
                                pane.activate_item(idx, true, true, window, cx);
                            });
                            break;
                        }
                    }
                });
                self.active_terminal = Some(metadata.terminal_id);
                cx.notify();
                return;
            }
        }

        // Otherwise resolve or open the workspace for this thread's paths and spawn a fresh terminal.
        let group_key = ProjectGroupKey::from_worktree_paths(
            &metadata.worktree_paths,
            metadata.remote_connection.clone(),
        );
        let folder_paths = metadata.folder_paths().clone();
        let remote_connection = metadata.remote_connection.clone();
        let terminal_id = metadata.terminal_id;
        let working_directory = metadata.working_directory.clone();

        self.open_or_activate_workspace(
            folder_paths,
            remote_connection,
            Some(group_key),
            window,
            cx,
            move |this, workspace, window, cx| {
                this.spawn_terminal_view_in_workspace(
                    terminal_id,
                    workspace,
                    working_directory,
                    window,
                    cx,
                );
            },
        );
    }

    fn spawn_terminal_view_in_workspace(
        &mut self,
        terminal_id: TerminalId,
        workspace: Entity<Workspace>,
        working_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_terminal = Some(terminal_id);
        let project = workspace.read(cx).project().clone();
        let create_task =
            project.update(cx, |project, cx| project.create_terminal_shell(working_directory, cx));

        cx.spawn_in(window, async move |this, cx| {
            let terminal = create_task.await?;
            let terminal_view = workspace.update_in(cx, |ws, window, cx| {
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        ws.weak_handle(),
                        ws.database_id(),
                        ws.project().downgrade(),
                        window,
                        cx,
                    )
                });
                ws.add_item_to_active_pane(Box::new(terminal_view.clone()), None, true, window, cx);
                terminal_view
            })?;

            this.update(cx, |this, cx| {
                let title_sub = cx.subscribe(&terminal, move |this, term, event, cx| match event {
                    TerminalEvent::TitleChanged => {
                        let new_title: SharedString = term.read(cx).title(false).into();
                        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
                            store.update(cx, |store, cx| {
                                if let Some(mut meta) = store.entry(terminal_id).cloned() {
                                    if meta.title != new_title && !new_title.trim().is_empty() {
                                        meta.title = new_title;
                                        store.save(meta, cx);
                                    }
                                }
                            });
                        }
                        cx.notify();
                    }
                    TerminalEvent::Bell => {
                        if this.active_terminal != Some(terminal_id) {
                            if let Some(entry) = this.live_terminals.get_mut(&terminal_id) {
                                entry.has_bell = true;
                                cx.notify();
                            }
                        }
                    }
                    TerminalEvent::CloseTerminal => {
                        this.live_terminals.remove(&terminal_id);
                        if this.active_terminal == Some(terminal_id) {
                            this.active_terminal = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                });

                this.live_terminals.insert(
                    terminal_id,
                    LiveTerminalEntry {
                        workspace: workspace.downgrade(),
                        terminal_view: terminal_view.downgrade(),
                        has_bell: false,
                        _subscriptions: vec![title_sub],
                    },
                );
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn delete_terminal_thread(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.live_terminals.remove(&terminal_id) {
            if let (Some(workspace), Some(terminal_view)) =
                (entry.workspace.upgrade(), entry.terminal_view.upgrade())
            {
                let item_id = terminal_view.entity_id();
                workspace.update(cx, |ws, cx| {
                    for pane in ws.panes().iter().cloned().collect::<Vec<_>>() {
                        if pane
                            .read(cx)
                            .items()
                            .any(|item| item.item_id() == item_id)
                        {
                            pane.update(cx, |pane, cx| {
                                pane.remove_item(item_id, false, false, window, cx);
                            });
                            break;
                        }
                    }
                });
            }
        }
        if self.active_terminal == Some(terminal_id) {
            self.active_terminal = None;
        }
        if self.renaming_terminal == Some(terminal_id) {
            self.renaming_terminal = None;
        }
        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.delete(terminal_id, cx);
            });
        }
        cx.notify();
    }

    fn start_rename(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial_text = TerminalThreadMetadataStore::try_global(cx)
            .and_then(|s| s.read(cx).entry(terminal_id).map(|m| m.editable_title()))
            .unwrap_or_default();
        self.renaming_terminal = Some(terminal_id);
        self.rename_editor.update(cx, |editor, cx| {
            editor.set_text(initial_text, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        self.rename_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let Some(terminal_id) = self.renaming_terminal.take() else {
            return;
        };
        let new_title: SharedString = editor.read(cx).text(cx).trim().to_string().into();
        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.rename_terminal(terminal_id, new_title, cx);
            });
        }
        cx.notify();
    }

    fn open_worktree_picker_for_group(
        &mut self,
        key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_list = key.path_list().clone();
        let host = key.host().cloned();
        self.open_or_activate_workspace(
            path_list,
            host,
            Some(key),
            window,
            cx,
            |_this, workspace, window, cx| {
                workspace.update(cx, |ws, cx| {
                    let project = ws.project().clone();
                    let weak_ws = ws.weak_handle();
                    ws.toggle_modal(window, cx, move |window, cx| {
                        WorktreePicker::new_modal(project, weak_ws, None, window, cx)
                    });
                });
            },
        );
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let side = WorkspaceSidebar::side(self, cx);
        let window_controls = SidebarWindowControls::new(side, window, cx);
        let header_height = platform_title_bar_height(window);

        let header = h_flex()
            .id("sidebar-header")
            .h(header_height)
            .w_full()
            .px_2()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when(window_controls.traffic_lights, |this| {
                this.pl(px(TRAFFIC_LIGHT_PADDING))
            })
            .child(
                Label::new("Workspaces")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("sidebar-open-project", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Open Project…"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Open::default()), cx);
                            }),
                    )
                    .child(
                        IconButton::new("sidebar-collapse", IconName::ChevronLeft)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::for_action_title(
                                "Toggle Sidebar",
                                &ToggleWorkspaceSidebar,
                            ))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleWorkspaceSidebar), cx);
                            }),
                    ),
            );

        sidebar_side_context_menu(
            "sidebar-header-context-menu",
            cx,
        )
        .trigger(move |_, _, _| {
            apply_title_bar_insets(
                header,
                window_controls.left,
                window_controls.right,
                false,
            )
        })
    }

    fn render_section(
        &self,
        section: SidebarProjectSection,
        section_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = section.group.key.clone();
        let expanded = section.group.expanded;
        let chevron = if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };

        let header_row = h_flex()
            .id(("project-group", section_ix))
            .group("project-group-row")
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .rounded_sm()
            .cursor_pointer()
            .when(section.is_active, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener({
                let key = key.clone();
                move |this, _, window, cx| {
                    this.activate_group(key.clone(), window, cx);
                }
            }))
            .child(
                IconButton::new(("toggle-group", section_ix), chevron)
                    .icon_size(IconSize::XSmall)
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener({
                        let key = key.clone();
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_group_expanded(key.clone(), cx);
                        }
                    })),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new(section.display_name.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .when_some(section.branch_name.clone(), |this, branch| {
                        this.child(
                            Label::new(branch)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new(("worktree-picker", section_ix), IconName::GitBranch)
                            .icon_size(IconSize::XSmall)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Git Worktrees…"))
                            .on_click(cx.listener({
                                let key = key.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_worktree_picker_for_group(key.clone(), window, cx);
                                }
                            })),
                    )
                    .child(
                        IconButton::new(("new-terminal-thread", section_ix), IconName::Plus)
                            .icon_size(IconSize::XSmall)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("New Terminal Thread"))
                            .on_click(cx.listener({
                                let key = key.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.spawn_terminal_thread_for_group(key.clone(), window, cx);
                                }
                            })),
                    ),
            );

        let group_with_menu = right_click_menu(("project-group-menu", section_ix))
            .trigger(move |_, _, _| header_row)
            .menu({
                let sidebar = cx.entity().downgrade();
                let mw = self.multi_workspace.clone();
                let key = key.clone();
                move |window, cx| {
                    let sidebar = sidebar.clone();
                    let mw = mw.clone();
                    let key = key.clone();
                    ContextMenu::build(window, cx, move |menu, _, _| {
                        menu.entry("New Terminal Thread", None, {
                            let sidebar = sidebar.clone();
                            let key = key.clone();
                            move |window, cx| {
                                sidebar
                                    .update(cx, |this, cx| {
                                        this.spawn_terminal_thread_for_group(
                                            key.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            }
                        })
                        .entry("Git Worktrees…", None, {
                            let sidebar = sidebar.clone();
                            let key = key.clone();
                            move |window, cx| {
                                sidebar
                                    .update(cx, |this, cx| {
                                        this.open_worktree_picker_for_group(
                                            key.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            }
                        })
                        .separator()
                        .entry("Move Project Up", None, {
                            let mw = mw.clone();
                            let key = key.clone();
                            move |_, cx| {
                                mw.update(cx, |mw, cx| {
                                    mw.move_project_group_up(&key, cx);
                                })
                                .ok();
                            }
                        })
                        .entry("Move Project Down", None, {
                            let mw = mw.clone();
                            let key = key.clone();
                            move |_, cx| {
                                mw.update(cx, |mw, cx| {
                                    mw.move_project_group_down(&key, cx);
                                })
                                .ok();
                            }
                        })
                        .entry("Move to New Window", None, {
                            let mw = mw.clone();
                            let key = key.clone();
                            move |window, cx| {
                                mw.update(cx, |mw, cx| {
                                    mw.open_project_group_in_new_window(&key, window, cx)
                                        .detach_and_log_err(cx);
                                })
                                .ok();
                            }
                        })
                        .separator()
                        .entry("Close Project", None, {
                            let mw = mw.clone();
                            let key = key.clone();
                            move |window, cx| {
                                mw.update(cx, |mw, cx| {
                                    mw.remove_project_group(&key, window, cx)
                                        .detach_and_log_err(cx);
                                })
                                .ok();
                            }
                        })
                    })
                }
            });

        let mut container = v_flex().w_full().gap_0p5().child(group_with_menu);

        if expanded {
            if section.terminals.is_empty() {
                container = container.child(
                    div().pl_6().py_0p5().child(
                        Label::new("No terminal threads")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                );
            } else {
                for (row_ix, row) in section.terminals.into_iter().enumerate() {
                    let terminal_id = row.metadata.terminal_id;
                    let is_selected = self.active_terminal == Some(terminal_id);
                    let is_renaming = self.renaming_terminal == Some(terminal_id);
                    let meta = row.metadata;
                    let title = meta.display_title();
                    let icon_color = if row.has_bell {
                        Color::Accent
                    } else if row.is_live {
                        Color::Success
                    } else {
                        Color::Muted
                    };

                    let row_el = ListItem::new(("terminal-thread", section_ix * 10000 + row_ix))
                        .indent_level(1)
                        .spacing(ListItemSpacing::Dense)
                        .toggle_state(is_selected)
                        .start_slot(
                            Icon::new(IconName::Terminal)
                                .size(IconSize::XSmall)
                                .color(icon_color),
                        )
                        .child(if is_renaming {
                            div()
                                .flex_1()
                                .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| {
                                    let editor = this.rename_editor.clone();
                                    this.commit_rename(editor, cx);
                                }))
                                .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                                    this.renaming_terminal = None;
                                    cx.notify();
                                }))
                                .child(self.rename_editor.clone())
                                .into_any_element()
                        } else {
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .items_center()
                                .child(Label::new(title).size(LabelSize::Small).truncate())
                                .when_some(row.worktree_label, |this, wt| {
                                    this.child(
                                        Label::new(wt)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                })
                                .into_any_element()
                        })
                        .end_slot(
                            IconButton::new(
                                ("delete-terminal", section_ix * 10000 + row_ix),
                                IconName::Close,
                            )
                            .icon_size(IconSize::XSmall)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Close Terminal Thread"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.delete_terminal_thread(terminal_id, window, cx);
                            })),
                        )
                        .on_click(cx.listener({
                            let meta = meta.clone();
                            move |this, _, window, cx| {
                                if this.renaming_terminal != Some(terminal_id) {
                                    this.open_or_activate_terminal_thread(
                                        meta.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            }
                        }));

                    let wrapped = right_click_menu(("terminal-menu", section_ix * 10000 + row_ix))
                        .trigger(move |_, _, _| row_el)
                        .menu({
                            let sidebar = cx.entity().downgrade();
                            move |window, cx| {
                                let sidebar = sidebar.clone();
                                ContextMenu::build(window, cx, move |menu, _, _| {
                                    menu.entry("Rename Thread", None, {
                                        let sidebar = sidebar.clone();
                                        move |window, cx| {
                                            sidebar
                                                .update(cx, |this, cx| {
                                                    this.start_rename(terminal_id, window, cx);
                                                })
                                                .ok();
                                        }
                                    })
                                    .entry("Close Thread", None, {
                                        let sidebar = sidebar.clone();
                                        move |window, cx| {
                                            sidebar
                                                .update(cx, |this, cx| {
                                                    this.delete_terminal_thread(
                                                        terminal_id,
                                                        window,
                                                        cx,
                                                    );
                                                })
                                                .ok();
                                        }
                                    })
                                })
                            }
                        });

                    container = container.child(wrapped);
                }
            }
        }

        container
    }
}

impl workspace::Sidebar for Sidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width
            .unwrap_or(SIDEBAR_DEFAULT_WIDTH)
            .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        cx.emit(SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn has_notifications(&self, _cx: &App) -> bool {
        self.live_terminals.values().any(|entry| entry.has_bell)
    }

    fn side(&self, cx: &App) -> SidebarSide {
        match Self::sidebar_dock_position(cx) {
            SidebarDockPosition::Left => SidebarSide::Left,
            SidebarDockPosition::Right => SidebarSide::Right,
        }
    }

    fn retains_project_group(
        &self,
        old_worktree_paths: &WorktreePaths,
        _new_git_worktree_paths: &WorktreePaths,
        cx: &App,
    ) -> bool {
        TerminalThreadMetadataStore::try_global(cx).is_some_and(|store| {
            store
                .read(cx)
                .entries_for_main_worktree_path(old_worktree_paths.main_worktree_path_list(), None)
                .next()
                .is_some()
        })
    }

    fn prepare_for_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_editor.focus_handle(cx).focus(window, cx);
    }

    fn cycle_project(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let sections = self.sections(cx);
        if sections.len() < 2 {
            return;
        }
        let current_ix = sections.iter().position(|s| s.is_active).unwrap_or(0);
        let next_ix = if forward {
            (current_ix + 1) % sections.len()
        } else {
            (current_ix + sections.len() - 1) % sections.len()
        };
        let key = sections[next_ix].group.key.clone();
        self.activate_group(key, window, cx);
    }

    fn cycle_thread(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let all_threads: Vec<TerminalThreadMetadata> = self
            .sections(cx)
            .into_iter()
            .flat_map(|s| s.terminals.into_iter().map(|t| t.metadata))
            .collect();
        if all_threads.is_empty() {
            return;
        }
        let current_ix = self
            .active_terminal
            .and_then(|active| all_threads.iter().position(|t| t.terminal_id == active))
            .unwrap_or(0);
        let next_ix = if forward {
            (current_ix + 1) % all_threads.len()
        } else {
            (current_ix + all_threads.len() - 1) % all_threads.len()
        };
        self.open_or_activate_terminal_thread(all_threads[next_ix].clone(), window, cx);
    }

    fn serialized_state(&self, _cx: &App) -> Option<String> {
        serde_json::to_string(&SerializedSidebarState {
            width: Some(f32::from(self.width)),
        })
        .ok()
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Ok(parsed) = serde_json::from_str::<SerializedSidebarState>(state) {
            if let Some(width) = parsed.width {
                self.width = px(width).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                cx.notify();
            }
        }
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::default();
        key_context.add("Sidebar");

        let sections = self.sections(cx);
        let section_elements: Vec<_> = sections
            .into_iter()
            .enumerate()
            .map(|(ix, section)| self.render_section(section, ix, cx).into_any_element())
            .collect();

        v_flex()
            .id("multi-workspace-sidebar")
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &FocusSidebarFilter, window, cx| {
                this.filter_editor.focus_handle(cx).focus(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTerminalThread, window, cx| {
                if let Some(mw) = this.multi_workspace.upgrade() {
                    let active_ws = mw.read(cx).workspace().clone();
                    let key = active_ws.read(cx).project_group_key(cx);
                    this.spawn_terminal_thread_for_group(key, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &workspace::NewThread, window, cx| {
                if let Some(mw) = this.multi_workspace.upgrade() {
                    let active_ws = mw.read(cx).workspace().clone();
                    let key = active_ws.read(cx).project_group_key(cx);
                    this.spawn_terminal_thread_for_group(key, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextProject, window, cx| {
                workspace::Sidebar::cycle_project(this, true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousProject, window, cx| {
                workspace::Sidebar::cycle_project(this, false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextThread, window, cx| {
                workspace::Sidebar::cycle_thread(this, true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousThread, window, cx| {
                workspace::Sidebar::cycle_thread(this, false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveProjectUp, _, cx| {
                if let Some(mw) = this.multi_workspace.upgrade() {
                    let key = mw.read(cx).workspace().read(cx).project_group_key(cx);
                    mw.update(cx, |mw, cx| {
                        mw.move_project_group_up(&key, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &MoveProjectDown, _, cx| {
                if let Some(mw) = this.multi_workspace.upgrade() {
                    let key = mw.read(cx).workspace().read(cx).project_group_key(cx);
                    mw.update(cx, |mw, cx| {
                        mw.move_project_group_down(&key, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &FocusWorkspaceSidebar, window, cx| {
                this.filter_editor.focus_handle(cx).focus(window, cx);
            }))
            .size_full()
            .bg(cx.theme().colors().surface_background)
            .child(self.render_header(window, cx))
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.filter_editor.clone()),
            )
            .child(
                v_flex()
                    .id("sidebar-projects-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_1()
                    .gap_1()
                    .children(section_elements),
            )
    }
}

pub fn dump_workspace_info(
    workspace: &mut Workspace,
    _: &DumpWorkspaceInfo,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let mut output = String::from("# Multi-Workspace Info\n\n");
    if let Some(mw) = window.window_handle().downcast::<MultiWorkspace>() {
        if let Ok(groups) = mw.read_with(cx, |mw, cx| mw.project_groups(cx)) {
            for (i, group) in groups.iter().enumerate() {
                output.push_str(&format!(
                    "{}. {:?} ({} workspaces)\n",
                    i + 1,
                    group.key.path_list().paths(),
                    group.workspaces.len()
                ));
            }
        }
    }
    let project = workspace.project().clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(None, true, cx))
            .await?;
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(output, cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}
