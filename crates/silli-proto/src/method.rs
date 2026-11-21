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
    INITIALIZE = "_silli/initialize";

    // storage
    STORAGE_SESSIONS = "_silli/storage.sessions";
    STORAGE_RECLAIM = "_silli/storage.reclaim";
    STORAGE_SPACE = "_silli/storage.space";
    STORAGE_CACHES = "_silli/storage.caches";
    STORAGE_CLEAR_BUILD_CACHE = "_silli/storage.clear_build_cache";

    // crosscheck
    CROSSCHECK_REVIEWERS = "_silli/crosscheck.reviewers";
    CROSSCHECK_SET_REVIEWERS = "_silli/crosscheck.set_reviewers";
    CROSSCHECK_GET = "_silli/crosscheck.get";
    CROSSCHECK_START = "_silli/crosscheck.start";
    CROSSCHECK_REMOVE = "_silli/crosscheck.remove";
    CROSSCHECK_CHANGED = "_silli/crosscheck.changed";

    // memory
    MEMORY_TEXT_CLOSE = "_silli/memory.text.close";
    MEMORY_COMPACT = "_silli/memory.compact";
    MEMORY_FORGET = "_silli/memory.forget";
    MEMORY_LIST = "_silli/memory.list";
    MEMORY_TEXT = "_silli/memory.text";
    MEMORY_SEARCH = "_silli/memory.search";
    MEMORY_NOTES_LIST = "_silli/memory.notes.list";
    MEMORY_NOTES_ADD = "_silli/memory.notes.add";
    MEMORY_NOTES_REMOVE = "_silli/memory.notes.remove";
    MEMORY_REFRESH = "_silli/memory.refresh";

    // sessions
    SESSIONS_DELETE = "_silli/sessions.delete";
    SESSIONS_RESET = "_silli/sessions.reset";
    SESSIONS_LOG = "_silli/sessions.log";
    SESSIONS_PIN = "_silli/sessions.pin";
    SESSIONS_UNPIN = "_silli/sessions.unpin";
    SESSIONS_LIST = "_silli/sessions.list";
    SESSIONS_GET = "_silli/sessions.get";
    SESSIONS_CREATE = "_silli/sessions.create";
    SESSIONS_UPDATE = "_silli/sessions.update";
    SESSIONS_START = "_silli/sessions.start";
    SESSIONS_STOP = "_silli/sessions.stop";
    SESSIONS_END = "_silli/sessions.end";
    SESSIONS_CHECKPOINT = "_silli/sessions.checkpoint";
    SESSIONS_CONTROL = "_silli/sessions.control";
    SESSIONS_CANCEL = "_silli/sessions.cancel";
    SESSIONS_ATTACH = "_silli/sessions.attach";
    SESSIONS_SNAPSHOT = "_silli/sessions.snapshot";
    SESSIONS_DETACH = "_silli/sessions.detach";
    SESSIONS_SUBMIT = "_silli/sessions.submit";

    // files
    FILES_FIND = "_silli/files.find";
    FILES_IMAGE = "_silli/files.image";
    FILES_HOME = "_silli/files.home";
    FILES_METADATA = "_silli/files.metadata";
    FILES_LIST = "_silli/files.list";
    FILES_READ = "_silli/files.read";
    FILES_WRITE = "_silli/files.write";

    // repos
    REPOS_ROOT = "_silli/repos.root";
    REPOS_HEAD = "_silli/repos.head";
    REPOS_SUMMARY = "_silli/repos.summary";
    REPOS_CHANGED_LINES = "_silli/repos.changed_lines";
    REPOS_STATUS = "_silli/repos.status";

    // policy
    POLICY_DENIED = "_silli/policy.denied";
    POLICY_QUIET_HOST = "_silli/policy.quiet_host";
    POLICY_REMEMBER_DENIED = "_silli/policy.remember_denied";
    POLICY_ALLOW_HOST = "_silli/policy.allow_host";
    POLICY_SAVE_HOST = "_silli/policy.save_host";
    POLICY_LIST = "_silli/policy.list";
    POLICY_GET = "_silli/policy.get";
    POLICY_PREVIEW = "_silli/policy.preview";
    POLICY_SNAPSHOT = "_silli/policy.snapshot";
    POLICY_CHECK = "_silli/policy.check";
    POLICY_IMPORT = "_silli/policy.import";

    // desktop
    DESKTOP_CAPTURE = "_silli/desktop.capture";
    DESKTOP_READ = "_silli/desktop.read";

    // permissions
    PERMISSIONS_SET = "_silli/permissions.set";
    PERMISSIONS_APPLY_DEVELOPMENT = "_silli/permissions.apply_development";
    PERMISSIONS_RESTORE_DEFAULTS = "_silli/permissions.restore_defaults";

    // host_control
    HOST_CONTROL_ACTIVATE = "_silli/host_control.activate";
    HOST_CONTROL_REVOKE = "_silli/host_control.revoke";
    HOST_CONTROL_KEEP = "_silli/host_control.keep";
    HOST_CONTROL_STATE = "_silli/host_control.state";

    // changes
    CHANGES_CHANGED = "_silli/changes.changed";
    CHANGES_LIST = "_silli/changes.list";
    CHANGES_DIFF = "_silli/changes.diff";
    CHANGES_KEEP = "_silli/changes.keep";
    CHANGES_REVERT = "_silli/changes.revert";
    CHANGES_UNDO = "_silli/changes.undo";
    CHANGES_GIT_REVERT = "_silli/changes.git-revert";
    CHANGES_HISTORY = "_silli/changes.history";

    // workspaces
    WORKSPACES_LIST = "_silli/workspaces.list";
    WORKSPACES_SESSION = "_silli/workspaces.session";
    WORKSPACES_GET = "_silli/workspaces.get";
    WORKSPACES_DIFF = "_silli/workspaces.diff";
    WORKSPACES_PREPARE = "_silli/workspaces.prepare";
    WORKSPACES_APPLY = "_silli/workspaces.apply";
    WORKSPACES_COPY = "_silli/workspaces.copy";
    WORKSPACES_BRANCH = "_silli/workspaces.branch";
    WORKSPACES_DISCARD = "_silli/workspaces.discard";
    WORKSPACES_UNDO = "_silli/workspaces.undo";
    WORKSPACES_REMOVE = "_silli/workspaces.remove";
    WORKSPACES_ABORT = "_silli/workspaces.abort";
    WORKSPACES_CHANGED = "_silli/workspaces.changed";

    // agents
    AGENTS_LIST = "_silli/agents.list";
    AGENTS_GET = "_silli/agents.get";
    AGENTS_SAVE = "_silli/agents.save";
    AGENTS_REMOVE = "_silli/agents.remove";
    AGENTS_AVAILABLE = "_silli/agents.available";
    AGENTS_DEFAULTS_GET = "_silli/agents.defaults.get";
    AGENTS_DEFAULTS_SET = "_silli/agents.defaults.set";
    AGENTS_DISCOVERED = "_silli/agents.discovered";
    AGENTS_LOGIN = "_silli/agents.login";
    AGENTS_DISCOVER = "_silli/agents.discover";
    AGENTS_RESET = "_silli/agents.reset";

    // registry
    REGISTRY_REFRESH = "_silli/registry.refresh";
    REGISTRY_INSTALLED = "_silli/registry.installed";
    REGISTRY_UNINSTALL = "_silli/registry.uninstall";
    REGISTRY_ICON = "_silli/registry.icon";
    REGISTRY_INSTALL = "_silli/registry.install";
    REGISTRY_LIST = "_silli/registry.list";

    // providers
    PROVIDERS_LIST = "_silli/providers.list";
    PROVIDERS_SAVE = "_silli/providers.save";
    PROVIDERS_REMOVE = "_silli/providers.remove";
    PROVIDERS_TEST = "_silli/providers.test";
    PROVIDERS_DETECT_LIMITS = "_silli/providers.detect_limits";

    // pins
    PINS_LIST = "_silli/pins.list";
    PINS_ADD = "_silli/pins.add";
    PINS_REMOVE = "_silli/pins.remove";

    // operations
    OPERATIONS_GET = "_silli/operations.get";
    OPERATIONS_CANCEL = "_silli/operations.cancel";

    // settings
    SETTINGS_GET = "_silli/settings.get";
    SETTINGS_PATCH = "_silli/settings.patch";
    SETTINGS_INSTRUCTIONS_GET = "_silli/settings.instructions.get";
    SETTINGS_INSTRUCTIONS_SET = "_silli/settings.instructions.set";

    // usage
    USAGE_ACTIVE_TIME = "_silli/usage.active_time";

    // procs
    PROCS_LIST = "_silli/procs.list";
    PROCS_TAIL = "_silli/procs.tail";
    PROCS_STOP = "_silli/procs.stop";
    PROCS_RUNNING = "_silli/procs.running";
    PROCS_STOP_COMMAND = "_silli/procs.stop_command";
    PROCS_DISMISS = "_silli/procs.dismiss";

    // streams
    STREAMS_PORT = "_silli/streams.port";
    STREAMS_LOGIN_CALLBACK = "_silli/streams.login_callback";

    // terminals
    TERMINALS_OPEN = "_silli/terminals.open";
    TERMINALS_LIST = "_silli/terminals.list";
    TERMINALS_GET = "_silli/terminals.get";
    TERMINALS_ATTACH = "_silli/terminals.attach";
    TERMINALS_RESIZE = "_silli/terminals.resize";
    TERMINALS_CWD = "_silli/terminals.cwd";
    TERMINALS_CLOSE = "_silli/terminals.close";

    // uploads
    UPLOADS_CREATE = "_silli/uploads.create";
    UPLOADS_GET = "_silli/uploads.get";

    // submissions
    SUBMISSIONS_GET = "_silli/submissions.get";
    SUBMISSIONS_CANCEL = "_silli/submissions.cancel";
    SUBMISSIONS_RESUME = "_silli/submissions.resume";
    SUBMISSIONS_SEND_NOW = "_silli/submissions.send_now";

    // questions
    QUESTIONS_ANSWER = "_silli/questions.answer";

    // session
    SESSION_EVENT = "_silli/session.event";

    // daemon
    DAEMON_SCHEMA = "_silli/daemon.schema";
    DAEMON_STATUS = "_silli/daemon.status";
    DAEMON_SHUTDOWN = "_silli/daemon.shutdown";
}
