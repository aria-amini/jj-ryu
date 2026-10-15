//! `JjWorkspace` - wrapper around jj-lib for repository operations

use crate::error::{Error, Result};
use crate::types::{Bookmark, GitRemote, LogEntry};
use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt as _;
use jj_lib::backend::{CommitId, Timestamp};
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::fileset::FilesetAliasesMap;
use jj_lib::git::{
    self, GitFetch, GitFetchRefExpression, GitImportOptions, GitProgress, GitPushOptions,
    GitPushStats, GitRefUpdate, GitSettings, GitSidebandLineTerminator, GitSubprocessCallback,
    expand_fetch_refspecs,
};
use jj_lib::merge::Diff;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteName};
use jj_lib::repo::Repo;
use jj_lib::revset::{
    self, RevsetExtensions, RevsetParseContext, RevsetWorkspaceContext, SymbolResolver,
};
use jj_lib::rewrite::rebase_commit;
use jj_lib::settings::UserSettings;
use jj_lib::str_util::{StringExpression, StringMatcher, StringPattern};
use jj_lib::ui_path::RepoPathUiConverter;
use jj_lib::workspace::Workspace;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Wrapper around jj-lib workspace and repository
pub struct JjWorkspace {
    workspace: Workspace,
    settings: UserSettings,
}

/// Discards Git subprocess progress while retaining command errors.
#[derive(Debug)]
struct NoopGitSubprocessCallback;

impl GitSubprocessCallback for NoopGitSubprocessCallback {
    fn needs_progress(&self) -> bool {
        false
    }

    fn progress(&mut self, _progress: &GitProgress) -> io::Result<()> {
        Ok(())
    }

    fn local_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> io::Result<()> {
        Ok(())
    }
}

/// Formats every rejected Git push result for a caller-visible error.
fn format_git_push_rejections(stats: &GitPushStats) -> String {
    let mut rejections = Vec::new();

    for (ref_name, reason) in &stats.rejected {
        rejections.push(format!(
            "lease rejection for {} ({})",
            ref_name.as_str(),
            reason.as_deref().unwrap_or("no reason provided")
        ));
    }
    for (ref_name, reason) in &stats.remote_rejected {
        rejections.push(format!(
            "remote rejection for {} ({})",
            ref_name.as_str(),
            reason.as_deref().unwrap_or("no reason provided")
        ));
    }
    for (symbol, reason) in &stats.unexported_bookmarks {
        rejections.push(format!("unexported bookmark {symbol} ({reason})"));
    }

    rejections.join("; ")
}

/// Create `UserSettings` with defaults for read operations
fn create_user_settings() -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();

    // Add minimal user config - required by UserSettings::from_config
    let mut user_layer = ConfigLayer::empty(ConfigSource::User);
    user_layer
        .set_value("user.name", "jj-ryu")
        .map_err(|e| Error::Config(format!("Failed to set user.name: {e}")))?;
    user_layer
        .set_value("user.email", "jj-ryu@localhost")
        .map_err(|e| Error::Config(format!("Failed to set user.email: {e}")))?;
    config.add_layer(user_layer);

    // Try to load actual user config file if it exists
    let home = dirs::home_dir();
    if let Some(ref home_dir) = home {
        let jj_config = home_dir.join(".config").join("jj").join("config.toml");
        if jj_config.exists() {
            let _ = config.load_file(ConfigSource::User, &jj_config);
        }
    }

    UserSettings::from_config(config)
        .map_err(|e| Error::Config(format!("Failed to create settings: {e}")))
}

/// Finds the nearest workspace root by walking up the directory tree.
///
/// Canonicalizes the path first so relative paths like "." correctly
/// resolve to their absolute positions before walking up ancestors.
fn find_workspace_dir(path: &Path) -> PathBuf {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    absolute
        .ancestors()
        .find(|p| p.join(".jj").is_dir())
        .map(Path::to_path_buf)
        .unwrap_or(absolute)
}

/// Run the user's `jj` binary in the workspace
///
/// ryu links its own jj-lib, which can differ from the user's `jj`. The `jj`
/// binary owns the on-disk working copy so that its format, ignore rules, and
/// snapshot limits stay consistent with the user's version and config.
fn run_jj(workspace_root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("jj")
        .args(args)
        .current_dir(workspace_root)
        .output()
        .map_err(|e| Error::Workspace(format!("Failed to run jj {}: {e}", args.join(" "))))?;
    if !output.status.success() {
        return Err(Error::Workspace(format!(
            "jj {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Local bookmarks that match their remote bookmark, with their target
fn synced_bookmark_targets(
    repo: &jj_lib::repo::ReadonlyRepo,
    remote: &RemoteName,
) -> Vec<(RefNameBuf, CommitId)> {
    let view = repo.view();
    view.local_bookmarks()
        .filter_map(|(name, target)| {
            let local = target.as_normal()?;
            let remote_ref = view.get_remote_bookmark(name.to_remote_symbol(remote));
            (remote_ref.target.as_normal() == Some(local)).then(|| (name.to_owned(), local.clone()))
        })
        .collect()
}

/// Record server-side rewrites of synced bookmarks as commit rewrites
///
/// A forge can rewrite a branch, for example when GitHub rebases the rest of
/// a stack after a merge. The import then abandons the old commit as
/// unreachable, and jj rebases its descendants, including the working copy,
/// onto the old parent. Recording old -> new instead keeps descendants on top
/// of the bookmark.
fn record_remote_rewrites(
    mut_repo: &mut jj_lib::repo::MutableRepo,
    synced_before: &[(RefNameBuf, CommitId)],
) -> Result<()> {
    for (name, old_id) in synced_before {
        let new_target = mut_repo.view().get_local_bookmark(name);
        let Some(new_id) = new_target.as_normal().cloned() else {
            continue;
        };
        if &new_id == old_id
            || pollster::block_on(mut_repo.index().is_ancestor(old_id, &new_id))
                .map_err(|e| Error::Git(format!("Failed to query index: {e}")))?
        {
            continue;
        }
        mut_repo.set_rewritten_commit(old_id.clone(), new_id);
    }
    Ok(())
}

impl JjWorkspace {
    /// Open a jj workspace, snapshotting the working copy first
    ///
    /// Without the snapshot, a later rewrite of the working-copy commit
    /// orphans the unsnapshotted edits in a divergent commit.
    pub fn open(path: &Path) -> Result<Self> {
        let settings = create_user_settings()?;
        let workspace_root = find_workspace_dir(path);

        let workspace = Workspace::load(
            &settings,
            &workspace_root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .map_err(|e| Error::Workspace(format!("Failed to open workspace: {e}")))?;
        run_jj(&workspace_root, &["util", "snapshot"])?;

        Ok(Self {
            workspace,
            settings,
        })
    }

    /// Get the readonly repo at head operation
    fn repo(&self) -> Result<Arc<jj_lib::repo::ReadonlyRepo>> {
        pollster::block_on(self.workspace.repo_loader().load_at_head())
            .map_err(|e| Error::Workspace(format!("Failed to load repo: {e}")))
    }

    /// Get git settings from user settings
    fn git_settings(&self) -> Result<GitSettings> {
        GitSettings::from_settings(&self.settings)
            .map_err(|e| Error::Config(format!("Invalid git settings: {e}")))
    }

    /// Get all local bookmarks
    pub fn local_bookmarks(&self) -> Result<Vec<Bookmark>> {
        let repo = self.repo()?;
        let view = repo.view();

        let mut bookmarks = Vec::new();
        for (name, target) in view.local_bookmarks() {
            if let Some(commit_id) = target.as_normal() {
                let commit = repo
                    .store()
                    .get_commit(commit_id)
                    .map_err(|e| Error::Workspace(format!("Failed to get commit: {e}")))?;

                // Check if bookmark has remote tracking (excluding @git pseudo-remote)
                let name_matcher = StringPattern::exact(name.as_str()).to_matcher();
                let remote_matcher = StringMatcher::All;
                let has_remote = view
                    .remote_bookmarks_matching(&name_matcher, &remote_matcher)
                    .any(|(symbol, _)| symbol.remote.as_str() != "git");

                // Check if synced with remote (excluding @git pseudo-remote)
                let is_synced = view
                    .remote_bookmarks_matching(&name_matcher, &remote_matcher)
                    .filter(|(symbol, _)| symbol.remote.as_str() != "git")
                    .any(|(_, remote_ref)| {
                        remote_ref
                            .target
                            .as_normal()
                            .is_some_and(|id| id == commit_id)
                    });

                bookmarks.push(Bookmark {
                    name: name.as_str().to_string(),
                    commit_id: commit_id.hex(),
                    change_id: commit.change_id().hex(),
                    has_remote,
                    is_synced,
                });
            }
        }

        Ok(bookmarks)
    }

    /// Get a specific local bookmark
    pub fn get_local_bookmark(&self, name: &str) -> Result<Option<Bookmark>> {
        let repo = self.repo()?;
        let view = repo.view();

        let ref_name = RefName::new(name);
        let target = view.get_local_bookmark(ref_name);

        if !target.is_present() {
            return Ok(None);
        }

        let Some(commit_id) = target.as_normal() else {
            return Ok(None);
        };

        let commit = repo
            .store()
            .get_commit(commit_id)
            .map_err(|e| Error::Workspace(format!("Failed to get commit: {e}")))?;

        // Check if bookmark has remote tracking (excluding @git pseudo-remote)
        let name_matcher = StringPattern::exact(name).to_matcher();
        let remote_matcher = StringMatcher::All;
        let has_remote = view
            .remote_bookmarks_matching(&name_matcher, &remote_matcher)
            .any(|(symbol, _)| symbol.remote.as_str() != "git");

        // Check if synced with remote (excluding @git pseudo-remote)
        let is_synced = view
            .remote_bookmarks_matching(&name_matcher, &remote_matcher)
            .filter(|(symbol, _)| symbol.remote.as_str() != "git")
            .any(|(_, remote_ref)| {
                remote_ref
                    .target
                    .as_normal()
                    .is_some_and(|id| id == commit_id)
            });

        Ok(Some(Bookmark {
            name: name.to_string(),
            commit_id: commit_id.hex(),
            change_id: commit.change_id().hex(),
            has_remote,
            is_synced,
        }))
    }

    /// Get a remote bookmark
    pub fn get_remote_bookmark(&self, name: &str, remote: &str) -> Result<Option<Bookmark>> {
        let repo = self.repo()?;
        let view = repo.view();

        let ref_name = RefName::new(name);
        let remote_name = RemoteName::new(remote);
        let symbol = ref_name.to_remote_symbol(remote_name);
        let remote_ref = view.get_remote_bookmark(symbol);

        if !remote_ref.target.is_present() {
            return Ok(None);
        }

        let Some(commit_id) = remote_ref.target.as_normal() else {
            return Ok(None);
        };

        let commit = repo
            .store()
            .get_commit(commit_id)
            .map_err(|e| Error::Workspace(format!("Failed to get commit: {e}")))?;

        Ok(Some(Bookmark {
            name: name.to_string(),
            commit_id: commit_id.hex(),
            change_id: commit.change_id().hex(),
            has_remote: true,
            is_synced: true,
        }))
    }

    /// Get the change ID for a bookmark.
    ///
    /// Used for rename detection in tracking.
    pub fn get_change_id(&self, bookmark: &str) -> Result<Option<String>> {
        self.get_local_bookmark(bookmark)
            .map(|opt| opt.map(|b| b.change_id))
    }

    /// Find the bookmark name that points to a given change ID.
    ///
    /// Used for rename detection - if a tracked bookmark's name no longer
    /// matches its stored `change_id`, we search for what bookmark now points
    /// to that `change_id`.
    pub fn get_bookmark_for_change_id(&self, change_id: &str) -> Result<Option<String>> {
        let bookmarks = self.local_bookmarks()?;
        Ok(bookmarks
            .into_iter()
            .find(|b| b.change_id == change_id)
            .map(|b| b.name))
    }

    /// Preferred remote order for detecting default branch
    const REMOTE_PREFERENCE: &[&str] = &["origin", "upstream"];

    /// Default `trunk()` revset alias - matches jj CLI's built-in default
    ///
    /// Uses `latest()` to pick the newest commit if multiple trunk candidates exist,
    /// checking main/master/trunk on origin and upstream remotes, falling back to `root()`.
    const DEFAULT_TRUNK_ALIAS: &str = r#"latest(
        remote_bookmarks(exact:"main", exact:"origin") |
        remote_bookmarks(exact:"master", exact:"origin") |
        remote_bookmarks(exact:"trunk", exact:"origin") |
        remote_bookmarks(exact:"main", exact:"upstream") |
        remote_bookmarks(exact:"master", exact:"upstream") |
        remote_bookmarks(exact:"trunk", exact:"upstream") |
        root()
    )"#;

    /// Detect default branch from git remote HEAD (e.g., refs/remotes/origin/HEAD)
    ///
    /// Returns `(branch_name, remote_name)` if found.
    fn detect_default_branch_from_remote(
        git_repo: &gix::Repository,
    ) -> Option<(String, &'static str)> {
        for &remote in Self::REMOTE_PREFERENCE {
            let ref_name = format!("refs/remotes/{remote}/HEAD");
            if let Some(reference) = git_repo.try_find_reference(&ref_name).ok().flatten()
                && let Some(target_name) = reference.target().try_name()
            {
                let target_str = target_name.to_string();
                let prefix = format!("refs/remotes/{remote}/");
                if let Some(branch) = target_str.strip_prefix(&prefix) {
                    return Some((branch.to_string(), remote));
                }
            }
        }
        None
    }

    /// Compute `trunk()` alias by checking remote HEAD first, then falling back to default
    fn compute_trunk_alias(repo: &Arc<jj_lib::repo::ReadonlyRepo>) -> String {
        if let Ok(git_repo) = git::get_git_repo(repo.store())
            && let Some((branch, remote)) = Self::detect_default_branch_from_remote(&git_repo)
        {
            return format!(r#"remote_bookmarks(exact:"{branch}", exact:"{remote}")"#);
        }
        Self::DEFAULT_TRUNK_ALIAS.to_string()
    }

    /// Resolve a revset expression to commits
    pub fn resolve_revset(&self, expr: &str) -> Result<Vec<LogEntry>> {
        let repo = self.repo()?;
        let commit_ids = self.evaluate_revset(&repo, expr)?;

        let mut entries = Vec::new();
        for commit_id in commit_ids {
            let commit = repo
                .store()
                .get_commit(&commit_id)
                .map_err(|e| Error::Workspace(format!("Failed to get commit: {e}")))?;

            entries.push(Self::commit_to_log_entry(&repo, &commit));
        }

        Ok(entries)
    }

    /// Parse and evaluate a revset expression, returning matching commit IDs
    fn evaluate_revset(
        &self,
        repo: &Arc<jj_lib::repo::ReadonlyRepo>,
        expr: &str,
    ) -> Result<Vec<CommitId>> {
        // Parse and evaluate the revset
        let extensions = RevsetExtensions::default();
        let mut aliases = revset::RevsetAliasesMap::default();

        // Define trunk() alias - checks remote HEAD first, then falls back to jj's default
        let trunk_alias = Self::compute_trunk_alias(repo);
        aliases
            .insert("trunk()", trunk_alias, None)
            .expect("trunk() alias declaration is valid");

        let date_context = jj_lib::time_util::DatePatternContext::Local(chrono::Local::now());

        // Create workspace context for trunk() resolution
        let workspace_root = self.workspace.workspace_root().to_path_buf();
        let path_converter = RepoPathUiConverter::Fs {
            cwd: workspace_root.clone(),
            base: workspace_root,
        };
        let workspace_name = self.workspace.workspace_name();
        let workspace_ctx = RevsetWorkspaceContext {
            path_converter: &path_converter,
            workspace_name,
        };

        let context = RevsetParseContext {
            aliases_map: &aliases,
            local_variables: std::collections::HashMap::new(),
            user_email: self.settings.user_email(),
            date_pattern_context: date_context,
            default_ignored_remote: Some(git::REMOTE_NAME_FOR_LOCAL_GIT_REPO),
            fileset_aliases_map: &FilesetAliasesMap::default(),
            extensions: &extensions,
            workspace: Some(workspace_ctx),
        };

        let mut diagnostics = revset::RevsetDiagnostics::new();
        let expression = revset::parse(&mut diagnostics, expr, &context)
            .map_err(|e| Error::Parse(format!("Failed to parse revset: {e}")))?;

        let empty_extensions: &[Box<dyn jj_lib::revset::SymbolResolverExtension>] = &[];
        let symbol_resolver = SymbolResolver::new(repo.as_ref(), empty_extensions);
        let resolved = expression
            .resolve_user_expression(repo.as_ref(), &symbol_resolver)
            .map_err(|e| Error::Revset(format!("Failed to resolve revset: {e}")))?;

        let revset = resolved
            .evaluate(repo.as_ref())
            .map_err(|e| Error::Revset(format!("Failed to evaluate revset: {e}")))?;

        pollster::block_on(async {
            let mut commit_ids = Vec::new();
            let mut stream = revset.stream();
            while let Some(commit_id) = stream.next().await {
                commit_ids.push(
                    commit_id
                        .map_err(|e| Error::Revset(format!("Failed to iterate revset: {e}")))?,
                );
            }
            Ok(commit_ids)
        })
    }

    /// Convert a jj commit to a `LogEntry`
    fn commit_to_log_entry(repo: &Arc<jj_lib::repo::ReadonlyRepo>, commit: &Commit) -> LogEntry {
        let view = repo.view();

        // Get bookmarks pointing to this commit
        let local_bookmarks: Vec<String> = view
            .local_bookmarks_for_commit(commit.id())
            .map(|(name, _)| name.as_str().to_string())
            .collect();

        let remote_bookmarks: Vec<String> = view
            .all_remote_bookmarks()
            .filter(|(_, remote_ref)| {
                remote_ref
                    .target
                    .as_normal()
                    .is_some_and(|id| id == commit.id())
            })
            .map(|(symbol, _)| format!("{}@{}", symbol.name.as_str(), symbol.remote.as_str()))
            .collect();

        // Get parents
        let parents: Vec<String> = commit.parent_ids().iter().map(ObjectId::hex).collect();

        // Get description first line
        let description = commit.description();
        let description_first_line = description.lines().next().unwrap_or("").to_string();

        // Get timestamps
        let author = commit.author();
        let committer = commit.committer();

        let authored_at = timestamp_to_datetime(&author.timestamp);
        let committed_at = timestamp_to_datetime(&committer.timestamp);

        // Check if this is the working copy
        let is_working_copy = repo
            .view()
            .wc_commit_ids()
            .values()
            .any(|id| id == commit.id());

        LogEntry {
            commit_id: commit.id().hex(),
            change_id: commit.change_id().hex(),
            author_name: author.name.clone(),
            author_email: author.email.clone(),
            description_first_line,
            parents,
            local_bookmarks,
            remote_bookmarks,
            is_working_copy,
            authored_at,
            committed_at,
        }
    }

    /// Get all git remotes
    pub fn git_remotes(&self) -> Result<Vec<GitRemote>> {
        let repo = self.repo()?;

        let remote_names = git::get_all_remote_names(repo.store())
            .map_err(|_| Error::Git("Not a git-backed repo".to_string()))?;

        // Get the git repo for URL lookup
        let git_repo = git::get_git_repo(repo.store())
            .map_err(|_| Error::Git("Not a git-backed repo".to_string()))?;

        let mut remotes = Vec::new();
        for name in remote_names {
            // Get the URL - try_find_remote returns Option<Result<Remote, Error>>
            let url = git_repo
                .try_find_remote(name.as_str())
                .and_then(std::result::Result::ok)
                .and_then(|remote| {
                    remote
                        .url(gix::remote::Direction::Push)
                        .map(|u| u.to_bstring().to_string())
                })
                .unwrap_or_default();

            remotes.push(GitRemote {
                name: name.as_str().to_string(),
                url,
            });
        }

        Ok(remotes)
    }

    /// Fetch from a git remote
    pub fn git_fetch(&mut self, remote: &str) -> Result<()> {
        let repo = self.repo()?;
        let git_settings = self.git_settings()?;

        let synced_before = synced_bookmark_targets(&repo, RemoteName::new(remote));

        // Start a transaction for the fetch
        let mut tx = repo.start_transaction();

        let import_options = GitImportOptions {
            abandon_unreachable_commits: git_settings.abandon_unreachable_commits,
            record_synthetic_predecessors: git_settings.record_synthetic_predecessors,
            remote_auto_track_bookmarks: std::iter::once((
                RemoteName::new(remote).to_owned(),
                StringMatcher::all(),
            ))
            .collect(),
        };
        let mut fetch = GitFetch::new(
            tx.repo_mut(),
            git_settings.to_subprocess_options(),
            &import_options,
        )
        .map_err(|e| Error::Git(format!("Failed to create fetch: {e}")))?;

        let remote_name = RemoteName::new(remote);
        let refspecs = expand_fetch_refspecs(
            remote_name,
            GitFetchRefExpression {
                bookmark: StringExpression::all(),
                // jj-lib 0.45 forces `--no-tags` for this choice. Ryu models bookmark
                // stacks only, so avoid fetching unrelated tags as part of stack sync.
                tag: StringExpression::none(),
            },
        )
        .map_err(|e| Error::Git(format!("Failed to expand refspecs: {e}")))?;
        fetch
            .fetch(remote_name, refspecs, &mut NoopGitSubprocessCallback, None)
            .map_err(|e| Error::Git(format!("Failed to fetch: {e}")))?;

        // Import the fetched refs
        pollster::block_on(fetch.import_refs())
            .map_err(|e| Error::Git(format!("Failed to import refs: {e}")))?;

        record_remote_rewrites(tx.repo_mut(), &synced_before)?;

        // Rebase descendants if there were any rewrites from the import
        // This is required before committing the transaction - see issue #8
        // Without this, jj-lib panics with "BUG: Descendants have not been rebased"
        if tx.repo().has_rewrites() {
            pollster::block_on(tx.repo_mut().rebase_descendants())
                .map_err(|e| Error::Git(format!("Failed to rebase descendants: {e}")))?;
        }

        // Commit the transaction
        pollster::block_on(tx.commit(format!("fetch from {remote}")))
            .map_err(|e| Error::Git(format!("Failed to commit fetch: {e}")))?;

        Ok(())
    }

    /// Push a bookmark to a remote
    pub fn git_push(&mut self, bookmark: &str, remote: &str) -> Result<()> {
        let repo = self.repo()?;
        let git_settings = self.git_settings()?;

        // Get the local bookmark target
        let view = repo.view();
        let ref_name = RefName::new(bookmark);
        let target = view.get_local_bookmark(ref_name);

        if !target.is_present() {
            return Err(Error::BookmarkNotFound(bookmark.to_string()));
        }

        let new_target = target.as_normal().cloned();

        // Get expected current target from remote tracking
        let remote_name = RemoteName::new(remote);
        let remote_symbol = ref_name.to_remote_symbol(remote_name);
        let remote_ref = view.get_remote_bookmark(remote_symbol);
        let expected_current_target = remote_ref.target.as_normal().cloned();

        // Start a transaction first - needed for export_refs
        let mut tx = repo.start_transaction();

        // Export refs to underlying git repo before pushing
        // This is essential for new bookmarks that don't exist in .git/refs/heads/ yet
        let export_stats = git::export_refs(tx.repo_mut())
            .map_err(|e| Error::Git(format!("Failed to export refs: {e}")))?;

        // Check if our bookmark failed to export
        if export_stats
            .failed_bookmarks
            .iter()
            .any(|(symbol, _)| symbol.name.as_str() == bookmark)
        {
            return Err(Error::Git(format!(
                "Failed to export bookmark '{bookmark}' to git"
            )));
        }

        // Build the update for pushing
        let update = GitRefUpdate {
            qualified_name: format!("refs/heads/{bookmark}").into(),
            targets: Diff::new(expected_current_target, new_target).map(|target| {
                target.map(|commit_id| gix::ObjectId::from_bytes_or_panic(commit_id.as_bytes()))
            }),
        };

        let push_stats = git::push_updates(
            tx.repo_mut().base_repo().as_ref(),
            git_settings.to_subprocess_options(),
            remote_name,
            &[update],
            &mut NoopGitSubprocessCallback,
            &GitPushOptions::default(),
        )
        .map_err(|e| Error::Git(format!("Failed to push: {e}")))?;

        if !push_stats.all_ok() {
            return Err(Error::Git(format!(
                "Push rejected for bookmark '{bookmark}': {}",
                format_git_push_rejections(&push_stats)
            )));
        }

        // Update the remote tracking ref to match what we just pushed
        // This ensures the bookmark shows as "synced" after push
        let remote_ref = RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracked,
        };
        tx.repo_mut().set_remote_bookmark(remote_symbol, remote_ref);

        pollster::block_on(tx.commit(format!("push {bookmark} to {remote}")))
            .map_err(|e| Error::Git(format!("Failed to commit push: {e}")))?;

        Ok(())
    }

    /// Delete a local bookmark (no-op when it doesn't exist)
    pub fn delete_local_bookmark(&mut self, name: &str) -> Result<()> {
        let repo = self.repo()?;
        let ref_name = RefName::new(name);

        if !repo.view().get_local_bookmark(ref_name).is_present() {
            return Ok(());
        }

        let mut tx = repo.start_transaction();
        tx.repo_mut()
            .set_local_bookmark_target(ref_name, RefTarget::absent());
        pollster::block_on(tx.commit(format!("delete bookmark {name}")))
            .map_err(|e| Error::Workspace(format!("Failed to commit bookmark deletion: {e}")))?;

        Ok(())
    }

    /// Check whether any commit in the revset has unresolved conflicts
    pub fn has_conflicts(&self, revset: &str) -> Result<bool> {
        Ok(!self
            .resolve_revset(&format!("({revset}) & conflicts()"))?
            .is_empty())
    }

    /// Check out the current working-copy commit on disk
    ///
    /// Operations that rewrite the working-copy commit leave the on-disk
    /// working copy stale. Call this after them.
    pub fn update_working_copy(&self) -> Result<()> {
        run_jj(self.workspace_root(), &["workspace", "update-stale"])
    }

    /// Abandon commits, reparenting their children onto the commits' parents
    pub fn abandon_commits(&mut self, commit_ids: &[String]) -> Result<()> {
        if commit_ids.is_empty() {
            return Ok(());
        }

        let repo = self.repo()?;
        let mut tx = repo.start_transaction();
        for hex in commit_ids {
            let id = CommitId::try_from_hex(hex)
                .ok_or_else(|| Error::Workspace(format!("Invalid commit id: {hex}")))?;
            let commit = repo
                .store()
                .get_commit(&id)
                .map_err(|e| Error::Workspace(format!("Failed to get commit: {e}")))?;
            tx.repo_mut().record_abandoned_commit(&commit);
        }

        pollster::block_on(tx.repo_mut().rebase_descendants())
            .map_err(|e| Error::Workspace(format!("Failed to rebase descendants: {e}")))?;

        pollster::block_on(tx.commit("abandon merged commits"))
            .map_err(|e| Error::Workspace(format!("Failed to commit abandon: {e}")))?;

        Ok(())
    }

    /// Rebase the bottom of the current stack onto the trunk tip
    ///
    /// Reparents each commit in `roots(trunk()..@)` onto `trunk()` and rebases
    /// descendants. Returns the number of root commits reparented (0 when the
    /// stack is already based on trunk).
    pub fn rebase_stack_onto_trunk(&mut self) -> Result<usize> {
        let repo = self.repo()?;

        let root_ids = self.evaluate_revset(&repo, "roots(trunk()..@)")?;
        if root_ids.is_empty() {
            return Ok(0);
        }
        let trunk_ids = self.evaluate_revset(&repo, "trunk()")?;
        let Some(trunk_id) = trunk_ids.first() else {
            return Err(Error::Workspace("Could not resolve trunk()".to_string()));
        };
        let trunk_commit = repo
            .store()
            .get_commit(trunk_id)
            .map_err(|e| Error::Workspace(format!("Failed to get trunk commit: {e}")))?;

        let mut tx = repo.start_transaction();
        let mut reparented = 0;
        for root_id in &root_ids {
            let commit = repo
                .store()
                .get_commit(root_id)
                .map_err(|e| Error::Workspace(format!("Failed to get commit: {e}")))?;

            if commit.parent_ids().contains(&trunk_commit.id().clone()) {
                continue;
            }

            pollster::block_on(rebase_commit(
                tx.repo_mut(),
                commit,
                vec![trunk_commit.id().clone()],
            ))
            .map_err(|e| Error::Workspace(format!("Failed to rebase commit: {e}")))?;
            reparented += 1;
        }

        if reparented == 0 {
            return Ok(0);
        }

        pollster::block_on(tx.repo_mut().rebase_descendants())
            .map_err(|e| Error::Workspace(format!("Failed to rebase descendants: {e}")))?;

        pollster::block_on(tx.commit("rebase stack onto trunk"))
            .map_err(|e| Error::Workspace(format!("Failed to commit rebase: {e}")))?;

        Ok(reparented)
    }

    /// Get the default branch name by checking remote HEAD first, then common names
    pub fn default_branch(&self) -> Result<String> {
        let repo = self.repo()?;

        // Try to detect from git remote HEAD (handles custom default branches like "develop")
        if let Ok(git_repo) = git::get_git_repo(repo.store())
            && let Some((branch, _)) = Self::detect_default_branch_from_remote(&git_repo)
        {
            return Ok(branch);
        }

        // Fall back to checking local bookmarks for common names
        let view = repo.view();
        for name in &["main", "master", "trunk"] {
            let target = view.get_local_bookmark(RefName::new(name));
            if target.is_present() {
                return Ok((*name).to_string());
            }
        }

        // Final fallback
        Ok("main".to_string())
    }

    /// Get the workspace root path
    pub fn workspace_root(&self) -> &Path {
        self.workspace.workspace_root()
    }
}

/// Select a remote from a list of available remotes
///
/// - If `specified` is provided and exists, use it
/// - If only one remote exists, use it
/// - If multiple remotes exist, prefer "origin", else use first
pub fn select_remote(remotes: &[GitRemote], specified: Option<&str>) -> Result<String> {
    if remotes.is_empty() {
        return Err(Error::NoSupportedRemotes);
    }

    if let Some(name) = specified {
        if !remotes.iter().any(|r| r.name == name) {
            return Err(Error::RemoteNotFound(name.to_string()));
        }
        return Ok(name.to_string());
    }

    if remotes.len() == 1 {
        return Ok(remotes[0].name.clone());
    }

    // Multiple remotes: prefer "origin", else first
    Ok(remotes
        .iter()
        .find(|r| r.name == "origin")
        .map_or_else(|| remotes[0].name.clone(), |r| r.name.clone()))
}

/// Convert jj timestamp to chrono `DateTime`
fn timestamp_to_datetime(ts: &Timestamp) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ts.timestamp.0)
        .single()
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_to_datetime() {
        let ts = Timestamp {
            timestamp: jj_lib::backend::MillisSinceEpoch(1_700_000_000_000),
            tz_offset: 0,
        };
        let dt = timestamp_to_datetime(&ts);
        assert_eq!(dt.timestamp_millis(), 1_700_000_000_000);
    }

    #[test]
    fn test_create_user_settings() {
        // Should not panic even without user config
        let settings = create_user_settings();
        assert!(settings.is_ok());
    }

    #[test]
    fn test_format_git_push_rejections_lists_every_rejection_category() {
        let remote_symbol = RefName::new("unexported")
            .to_remote_symbol(RemoteName::new("origin"))
            .to_owned();
        let stats = GitPushStats {
            rejected: vec![(
                "refs/heads/lease-failed".into(),
                Some("stale info".to_string()),
            )],
            remote_rejected: vec![(
                "refs/heads/hook-failed".into(),
                Some("hook declined".to_string()),
            )],
            unexported_bookmarks: vec![(
                remote_symbol,
                jj_lib::git::FailedRefExportReason::InvalidGitName,
            )],
            ..GitPushStats::default()
        };

        assert_eq!(
            format_git_push_rejections(&stats),
            "lease rejection for refs/heads/lease-failed (stale info); remote rejection for refs/heads/hook-failed (hook declined); unexported bookmark unexported@origin (Name is not allowed in Git)"
        );
    }
}
