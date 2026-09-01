use super::{
    join_remote_path, machines, parent_remote_path, Arc, Engine, MachineId, RemoteDirEntry,
    RequestToken, RuntimeEvent, WorkspaceBrowser, WorkspaceBrowserId, WorkspaceBrowserStatus,
    WorkspaceId,
};

#[derive(Debug, Clone)]
pub(super) enum BrowserRequest {
    Home,
    Listing(String),
    Validation(String),
}

impl Engine {
    pub(super) fn begin_workspace_browser(&mut self, id: WorkspaceBrowserId, machine: MachineId) {
        if !self.machine_is_available(&machine) {
            self.reject(format!("machine {machine} is not available"));

            return;
        }

        let machine_label = self.state.machine_label(&machine);
        let browser = WorkspaceBrowser {
            id,
            machine,
            machine_label,
            path: String::new(),
            entries: Vec::new(),
            status: WorkspaceBrowserStatus::LoadingHome,
            error: None,
        };

        Arc::make_mut(&mut self.state.browsers).insert(id, browser);
        self.send_browser_request(id, BrowserRequest::Home);
        self.publish();
    }

    pub(super) fn open_browser_directory(&mut self, id: WorkspaceBrowserId, name: &str) {
        let Some(browser) = self.state.browsers.get(&id) else {
            return;
        };

        if browser.busy()
            || !browser
                .entries
                .iter()
                .any(|entry| entry.is_dir && entry.name == name)
        {
            self.reject("folder is not in the current directory listing".to_string());

            return;
        }

        self.send_browser_request(
            id,
            BrowserRequest::Listing(join_remote_path(&browser.path, name)),
        );
        self.publish();
    }

    pub(super) fn browser_up(&mut self, id: WorkspaceBrowserId) {
        let Some(browser) = self.state.browsers.get(&id) else {
            return;
        };

        if browser.busy() {
            return;
        }

        self.send_browser_request(
            id,
            BrowserRequest::Listing(parent_remote_path(&browser.path)),
        );
        self.publish();
    }

    pub(super) fn confirm_browser(&mut self, id: WorkspaceBrowserId) {
        let Some(browser) = self.state.browsers.get(&id) else {
            return;
        };

        if !browser.can_submit() {
            return;
        }

        self.send_browser_request(id, BrowserRequest::Validation(browser.path.clone()));
        self.publish();
    }

    pub(super) fn cancel_browser(&mut self, id: WorkspaceBrowserId) {
        self.latest_browser.remove(&id);
        self.browser_requests
            .retain(|_, (browser, _)| *browser != id);
        Arc::make_mut(&mut self.state.browsers).remove(&id);
        self.publish();
    }

    fn send_browser_request(&mut self, id: WorkspaceBrowserId, request: BrowserRequest) {
        let Some(browser) = Arc::make_mut(&mut self.state.browsers).get_mut(&id) else {
            return;
        };
        let host = match self.fleet.host_for(browser.machine.as_str()) {
            Ok(host) => host,
            Err(error) => {
                browser.status = WorkspaceBrowserStatus::Idle;
                browser.error = Some(error.to_string());

                return;
            }
        };
        let token = self.tokens.mint();
        let command = match &request {
            BrowserRequest::Home => {
                browser.status = WorkspaceBrowserStatus::LoadingHome;
                machines::Command::ResolveHome { host, token }
            }
            BrowserRequest::Listing(path) => {
                browser.status = WorkspaceBrowserStatus::LoadingDirectory;
                machines::Command::ListDir {
                    host,
                    path: path.clone(),
                    token,
                }
            }
            BrowserRequest::Validation(path) => {
                browser.status = WorkspaceBrowserStatus::Validating;
                machines::Command::ValidateWorkspace {
                    host,
                    path: path.clone(),
                    token,
                }
            }
        };

        browser.error = None;
        self.latest_browser.insert(id, token);
        self.browser_requests.insert(token, (id, request));

        if let Err(error) = self.send_host_command(command) {
            self.latest_browser.remove(&id);
            self.browser_requests.remove(&token);
            self.fail_browser(id, error.to_string());
        }
    }

    pub(super) fn handle_browser_home(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
    ) {
        let Some((id, BrowserRequest::Home)) = self.take_browser_request(token) else {
            return;
        };

        match result {
            Ok(home) => self.send_browser_request(id, BrowserRequest::Listing(home)),
            Err(detail) => self.fail_browser(id, detail),
        }
    }

    pub(super) fn handle_browser_listing(
        &mut self,
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
    ) {
        let Some((id, BrowserRequest::Listing(path))) = self.take_browser_request(token) else {
            return;
        };
        let Some(browser) = Arc::make_mut(&mut self.state.browsers).get_mut(&id) else {
            return;
        };

        match result {
            Ok(entries) => {
                browser.path = path;
                browser.entries = entries;
                browser.status = WorkspaceBrowserStatus::Idle;
                browser.error = None;
            }
            Err(detail) => {
                browser.status = WorkspaceBrowserStatus::Idle;
                browser.error = Some(detail);
            }
        }
    }

    pub(super) fn handle_browser_validation(
        &mut self,
        token: RequestToken,
        result: Result<(), String>,
    ) {
        let Some((id, BrowserRequest::Validation(path))) = self.take_browser_request(token) else {
            return;
        };
        if let Err(detail) = result {
            self.fail_browser(id, detail);

            return;
        }

        let Some(machine) = self
            .state
            .browsers
            .get(&id)
            .map(|browser| browser.machine.clone())
        else {
            return;
        };
        let Some(workspace) = WorkspaceId::new(machine, path) else {
            self.fail_browser(id, "workspace path is not absolute".to_string());

            return;
        };
        let mut next = self.state.workspaces.as_ref().clone();

        if !next.contains(&workspace) {
            next.push(workspace.clone());

            if let Err(error) = self.workspace_store.save(&next) {
                self.fail_browser(id, format!("save workspaces: {error}"));

                return;
            }

            self.state.workspaces = Arc::new(next);
            self.track_machines();
            self.sync_sessions();
        }

        Arc::make_mut(&mut self.state.browsers).remove(&id);
        self.emit(RuntimeEvent::WorkspaceAdded {
            browser: id,
            workspace,
        });
    }

    fn take_browser_request(
        &mut self,
        token: RequestToken,
    ) -> Option<(WorkspaceBrowserId, BrowserRequest)> {
        let (id, request) = self.browser_requests.remove(&token)?;

        if self.latest_browser.get(&id) != Some(&token) {
            return None;
        }

        self.latest_browser.remove(&id);

        Some((id, request))
    }

    fn fail_browser(&mut self, id: WorkspaceBrowserId, detail: String) {
        if let Some(browser) = Arc::make_mut(&mut self.state.browsers).get_mut(&id) {
            browser.status = WorkspaceBrowserStatus::Idle;
            browser.error = Some(detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_path_navigation_handles_root_and_nested_directories() {
        assert_eq!(join_remote_path("/", "repo"), "/repo");
        assert_eq!(join_remote_path("/work", "repo"), "/work/repo");
        assert_eq!(parent_remote_path("/work/repo"), "/work");
        assert_eq!(parent_remote_path("/work"), "/");
        assert_eq!(parent_remote_path("/"), "/");
    }
}
