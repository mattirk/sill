//! The name of every method and notification of the daemon's protocol, each written once.
//!
//! Code names a method by its constant here, never by its string: a misspelt name does not
//! compile, and a search for the constant finds every caller and the handler. What a method
//! takes, answers and needs is in [`crate::schema::METHODS`], which a test keeps equal to [`ALL`].

macro_rules! names {
    ($($name:ident = $wire:literal;)*) => {
        $(pub const $name: &str = $wire;)*
        /// Every name above.
        pub const ALL: &[&str] = &[$($name),*];
    };
}

names! {
    // the handshake, before anything else on a connection
    INITIALIZE = "_sill/initialize";

    // storage
    STORAGE_SESSIONS = "_sill/storage.sessions";
    STORAGE_RECLAIM = "_sill/storage.reclaim";
    STORAGE_SPACE = "_sill/storage.space";
    STORAGE_CACHES = "_sill/storage.caches";
    STORAGE_CLEAR_BUILD_CACHE = "_sill/storage.clear_build_cache";

    // crosscheck
    CROSSCHECK_REVIEWERS = "_sill/crosscheck.reviewers";
    CROSSCHECK_SET_REVIEWERS = "_sill/crosscheck.set_reviewers";
    CROSSCHECK_GET = "_sill/crosscheck.get";
    CROSSCHECK_START = "_sill/crosscheck.start";
    CROSSCHECK_REMOVE = "_sill/crosscheck.remove";
    CROSSCHECK_CHANGED = "_sill/crosscheck.changed";

    // memory
    MEMORY_TEXT_CLOSE = "_sill/memory.text.close";
    MEMORY_COMPACT = "_sill/memory.compact";
    MEMORY_FORGET = "_sill/memory.forget";
    MEMORY_LIST = "_sill/memory.list";
    MEMORY_TEXT = "_sill/memory.text";
    MEMORY_SEARCH = "_sill/memory.search";
    MEMORY_NOTES_LIST = "_sill/memory.notes.list";
    MEMORY_NOTES_ADD = "_sill/memory.notes.add";
    MEMORY_NOTES_REMOVE = "_sill/memory.notes.remove";
    MEMORY_REFRESH = "_sill/memory.refresh";

    // sessions
    SESSIONS_DELETE = "_sill/sessions.delete";
    SESSIONS_RESET = "_sill/sessions.reset";
    SESSIONS_LOG = "_sill/sessions.log";
    SESSIONS_PIN = "_sill/sessions.pin";
    SESSIONS_UNPIN = "_sill/sessions.unpin";
    SESSIONS_LIST = "_sill/sessions.list";
    SESSIONS_GET = "_sill/sessions.get";
    SESSIONS_CREATE = "_sill/sessions.create";
    SESSIONS_UPDATE = "_sill/sessions.update";
    SESSIONS_START = "_sill/sessions.start";
    SESSIONS_STOP = "_sill/sessions.stop";
    SESSIONS_END = "_sill/sessions.end";
    SESSIONS_CHECKPOINT = "_sill/sessions.checkpoint";
    SESSIONS_CONTROL = "_sill/sessions.control";
    SESSIONS_CANCEL = "_sill/sessions.cancel";
    SESSIONS_ATTACH = "_sill/sessions.attach";
    SESSIONS_SNAPSHOT = "_sill/sessions.snapshot";
    SESSIONS_DETACH = "_sill/sessions.detach";
    SESSIONS_SUBMIT = "_sill/sessions.submit";

    // files
    FILES_FIND = "_sill/files.find";
    FILES_IMAGE = "_sill/files.image";
    FILES_HOME = "_sill/files.home";
    FILES_METADATA = "_sill/files.metadata";
    FILES_LIST = "_sill/files.list";
    FILES_READ = "_sill/files.read";
    FILES_WRITE = "_sill/files.write";

    // repos
    REPOS_ROOT = "_sill/repos.root";
    REPOS_HEAD = "_sill/repos.head";
    REPOS_SUMMARY = "_sill/repos.summary";
    REPOS_CHANGED_LINES = "_sill/repos.changed_lines";
    REPOS_STATUS = "_sill/repos.status";

    // policy
    POLICY_DENIED = "_sill/policy.denied";
    POLICY_QUIET_HOST = "_sill/policy.quiet_host";
    POLICY_REMEMBER_DENIED = "_sill/policy.remember_denied";
    POLICY_ALLOW_HOST = "_sill/policy.allow_host";
    POLICY_SAVE_HOST = "_sill/policy.save_host";
    POLICY_LIST = "_sill/policy.list";
    POLICY_GET = "_sill/policy.get";
    POLICY_PREVIEW = "_sill/policy.preview";
    POLICY_SNAPSHOT = "_sill/policy.snapshot";
    POLICY_CHECK = "_sill/policy.check";
    POLICY_IMPORT = "_sill/policy.import";

    // desktop
    DESKTOP_CAPTURE = "_sill/desktop.capture";
    DESKTOP_READ = "_sill/desktop.read";

    // permissions
    PERMISSIONS_SET = "_sill/permissions.set";
    PERMISSIONS_APPLY_DEVELOPMENT = "_sill/permissions.apply_development";
    PERMISSIONS_RESTORE_DEFAULTS = "_sill/permissions.restore_defaults";

    // host_control
    HOST_CONTROL_ACTIVATE = "_sill/host_control.activate";
    HOST_CONTROL_REVOKE = "_sill/host_control.revoke";
    HOST_CONTROL_KEEP = "_sill/host_control.keep";
    HOST_CONTROL_STATE = "_sill/host_control.state";

    // changes
    CHANGES_CHANGED = "_sill/changes.changed";
    CHANGES_LIST = "_sill/changes.list";
    CHANGES_DIFF = "_sill/changes.diff";
    CHANGES_KEEP = "_sill/changes.keep";
    CHANGES_REVERT = "_sill/changes.revert";
    CHANGES_UNDO = "_sill/changes.undo";
    CHANGES_GIT_REVERT = "_sill/changes.git-revert";
    CHANGES_HISTORY = "_sill/changes.history";

    // workspaces
    WORKSPACES_LIST = "_sill/workspaces.list";
    WORKSPACES_SESSION = "_sill/workspaces.session";
    WORKSPACES_GET = "_sill/workspaces.get";
    WORKSPACES_DIFF = "_sill/workspaces.diff";
    WORKSPACES_PREPARE = "_sill/workspaces.prepare";
    WORKSPACES_APPLY = "_sill/workspaces.apply";
    WORKSPACES_COPY = "_sill/workspaces.copy";
    WORKSPACES_BRANCH = "_sill/workspaces.branch";
    WORKSPACES_DISCARD = "_sill/workspaces.discard";
    WORKSPACES_UNDO = "_sill/workspaces.undo";
    WORKSPACES_REMOVE = "_sill/workspaces.remove";
    WORKSPACES_ABORT = "_sill/workspaces.abort";
    WORKSPACES_CHANGED = "_sill/workspaces.changed";

    // agents
    AGENTS_LIST = "_sill/agents.list";
    AGENTS_GET = "_sill/agents.get";
    AGENTS_SAVE = "_sill/agents.save";
    AGENTS_REMOVE = "_sill/agents.remove";
    AGENTS_AVAILABLE = "_sill/agents.available";
    AGENTS_DEFAULTS_GET = "_sill/agents.defaults.get";
    AGENTS_DEFAULTS_SET = "_sill/agents.defaults.set";
    AGENTS_DISCOVERED = "_sill/agents.discovered";
    AGENTS_LOGIN = "_sill/agents.login";
    AGENTS_DISCOVER = "_sill/agents.discover";
    AGENTS_RESET = "_sill/agents.reset";

    // registry
    REGISTRY_REFRESH = "_sill/registry.refresh";
    REGISTRY_INSTALLED = "_sill/registry.installed";
    REGISTRY_UNINSTALL = "_sill/registry.uninstall";
    REGISTRY_ICON = "_sill/registry.icon";
    REGISTRY_INSTALL = "_sill/registry.install";
    REGISTRY_LIST = "_sill/registry.list";

    // providers
    PROVIDERS_LIST = "_sill/providers.list";
    PROVIDERS_SAVE = "_sill/providers.save";
    PROVIDERS_REMOVE = "_sill/providers.remove";
    PROVIDERS_TEST = "_sill/providers.test";
    PROVIDERS_DETECT_LIMITS = "_sill/providers.detect_limits";

    // pins
    PINS_LIST = "_sill/pins.list";
    PINS_ADD = "_sill/pins.add";
    PINS_REMOVE = "_sill/pins.remove";

    // operations
    OPERATIONS_GET = "_sill/operations.get";
    OPERATIONS_CANCEL = "_sill/operations.cancel";

    // settings
    SETTINGS_GET = "_sill/settings.get";
    SETTINGS_PATCH = "_sill/settings.patch";
    SETTINGS_INSTRUCTIONS_GET = "_sill/settings.instructions.get";
    SETTINGS_INSTRUCTIONS_SET = "_sill/settings.instructions.set";

    // usage
    USAGE_ACTIVE_TIME = "_sill/usage.active_time";

    // procs
    PROCS_LIST = "_sill/procs.list";
    PROCS_TAIL = "_sill/procs.tail";
    PROCS_STOP = "_sill/procs.stop";
    PROCS_RUNNING = "_sill/procs.running";
    PROCS_STOP_COMMAND = "_sill/procs.stop_command";
    PROCS_DISMISS = "_sill/procs.dismiss";

    // streams
    STREAMS_PORT = "_sill/streams.port";
    STREAMS_LOGIN_CALLBACK = "_sill/streams.login_callback";

    // terminals
    TERMINALS_OPEN = "_sill/terminals.open";
    TERMINALS_LIST = "_sill/terminals.list";
    TERMINALS_GET = "_sill/terminals.get";
    TERMINALS_ATTACH = "_sill/terminals.attach";
    TERMINALS_RESIZE = "_sill/terminals.resize";
    TERMINALS_CWD = "_sill/terminals.cwd";
    TERMINALS_CLOSE = "_sill/terminals.close";

    // uploads
    UPLOADS_CREATE = "_sill/uploads.create";
    UPLOADS_GET = "_sill/uploads.get";

    // submissions
    SUBMISSIONS_GET = "_sill/submissions.get";
    SUBMISSIONS_CANCEL = "_sill/submissions.cancel";
    SUBMISSIONS_RESUME = "_sill/submissions.resume";
    SUBMISSIONS_SEND_NOW = "_sill/submissions.send_now";

    // questions
    QUESTIONS_ANSWER = "_sill/questions.answer";

    // session
    SESSION_EVENT = "_sill/session.event";

    // daemon
    DAEMON_SCHEMA = "_sill/daemon.schema";
    DAEMON_STATUS = "_sill/daemon.status";
    DAEMON_SHUTDOWN = "_sill/daemon.shutdown";
}
