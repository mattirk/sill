//! The drawer's tree and pins for the session in view: expanding, opening, the typed path, the
//! diff mode and the native folder picker.
use iced::Task;
use sill_client::files::{self, Request};
use sill_proto::sessions::HostPath;

use super::Action;
use super::edits::edit_source;
use crate::{App, Message, SessionView};

impl App {
    pub(super) fn pick_folder(&mut self) -> Task<Message> {
        if self.drawer.folder_picker.is_some() {
            return Task::none();
        }
        let Some(session) = self.selected.clone() else {
            return Task::none();
        };
        let Some(view) = self.views.get(&session) else {
            return Task::none();
        };
        if view.files.pin_pending.is_some() {
            return Task::none();
        }
        let token = view.media_token.clone();
        self.drawer.folder_picker = Some((session.clone(), token.clone()));
        Task::perform(
            async {
                rfd::AsyncFileDialog::new()
                    .set_title("Pin a folder")
                    .pick_folder()
                    .await
                    .map(|folder| folder.path().to_path_buf())
            },
            move |path| {
                Message::Files(Action::FolderPicked {
                    session,
                    token,
                    path,
                })
            },
        )
    }

    pub(super) fn folder_picked(
        &mut self,
        session: String,
        token: String,
        path: Option<std::path::PathBuf>,
    ) -> Task<Message> {
        if self.drawer.folder_picker.as_ref() != Some(&(session.clone(), token.clone()))
        {
            return Task::none();
        }
        // the Mac's own VM: the folder is the Mac's, put in the VM at its path before it is pinned
        #[cfg(target_os = "macos")]
        if let Some(picked) = path.clone()
            && crate::local_vm::phase().is_some()
        {
            return crate::blocking(
                move || crate::local_vm::expose(&picked),
                move |result| {
                    Message::Files(Action::MacFolderShared {
                        session,
                        token,
                        result,
                    })
                },
            );
        }
        self.drawer.folder_picker = None;
        let Some(view) = self
            .views
            .get_mut(&session)
            .filter(|v| v.media_token == token)
        else {
            return Task::none();
        };
        let requests = path
            .and_then(|path| view.files.pin(HostPath::from_path(&path), true))
            .into_iter()
            .collect();
        self.files_send(&session, requests);
        Task::none()
    }

    /// A picked Mac folder is in the VM at its path (or could not be put there): pin it as any.
    pub(super) fn mac_folder_shared(
        &mut self,
        session: String,
        token: String,
        result: Result<std::path::PathBuf, String>,
    ) -> Task<Message> {
        if self.drawer.folder_picker.as_ref() != Some(&(session.clone(), token.clone())) {
            return Task::none();
        }
        self.drawer.folder_picker = None;
        let Some(view) = self
            .views
            .get_mut(&session)
            .filter(|v| v.media_token == token)
        else {
            return Task::none();
        };
        let requests = match result {
            Ok(path) => view.files.pin(HostPath::from_path(&path), true).into_iter().collect(),
            Err(said) => {
                view.files.error = Some(said);
                Vec::new()
            }
        };
        self.files_send(&session, requests);
        Task::none()
    }

    /// Actions on the selected session's tree and reader, followed by its requests.
    pub(super) fn selected_files_action(&mut self, action: Action) -> Task<Message> {
        let Some(id) = self.selected.clone() else {
            return Task::none();
        };
        let Some(view) = self.views.get_mut(&id) else {
            return Task::none();
        };
        let mut restore = false;
        let requests: Vec<Request> = match action {
            Action::Refresh => {
                view.files.error = None;
                view.files.refresh()
            }
            Action::Collapse => {
                view.files.expanded.clear();
                vec![]
            }
            Action::Toggle(path) => {
                // a chip shows the drawer at its folder, without closing it
                let revealing = !self.drawer.shown;
                self.drawer.shown = true;
                if revealing && view.files.expanded.contains(files::key(&path)) {
                    vec![]
                } else {
                    view.files.toggle(path).into_iter().collect()
                }
            }
            Action::More(path) => view.files.more(path).into_iter().collect(),
            Action::Open { path, repo } => vec![view.files.open(path, repo)],
            Action::OpenAndHide { path, repo } => {
                self.drawer.shown = false;
                vec![view.files.open(path, repo)]
            }
            Action::CloseViewer => {
                view.files.close_reader();
                restore = true;
                vec![]
            }
            Action::RefreshFile => view.files.reload().into_iter().collect(),
            Action::Source => {
                view.files.toggle_source();
                vec![]
            }
            action @ (Action::Text(_) | Action::History(_)) => {
                if let Some(task) = edit_source(view, action) {
                    return task;
                }
                vec![]
            }
            Action::Path(value) => {
                view.files.set_path(value);
                vec![]
            }
            Action::BrowsePath => view.files.typed_path(false).into_iter().collect(),
            Action::PinPath => view.files.typed_path(true).into_iter().collect(),
            Action::Pin { path, on } => view.files.pin(path, on).into_iter().collect(),
            Action::DiffMode(on) => view.files.set_diff_mode(on),
            Action::DismissError => {
                view.files.error = None;
                vec![]
            }
            Action::Link { .. }
            | Action::Navigate { .. }
            | Action::Back(_)
            | Action::AnchorOffset { .. }
            | Action::SaveFile
            | Action::Saved { .. }
            | Action::DiscardEdits
            | Action::CancelDiscard
            | Action::PickFolder
            | Action::FolderPicked { .. }
            | Action::MacFolderShared { .. }
            | Action::ToggleDrawer
            | Action::Reply(_)
            | Action::Hover(_)
            | Action::Unhover(_) => unreachable!(),
        };
        view.sync_reader();
        self.files_send(&id, requests);
        if restore && self.current().is_some_and(SessionView::conversation_shown) {
            return self.restore_scroll();
        }
        Task::none()
    }
}
