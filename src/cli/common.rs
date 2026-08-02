//! Shared CLI helpers

use jj_ryu::error::{Error, Result};
use jj_ryu::platform::{PlatformService, create_platform_service, parse_repo_info};
use jj_ryu::repo::{JjWorkspace, select_remote};
use std::path::{Path, PathBuf};

/// Open workspace with the platform service for the selected remote
pub struct PlatformContext {
    pub workspace_root: PathBuf,
    pub remote_name: String,
    pub platform: Box<dyn PlatformService>,
}

/// Open the jj workspace, select a remote, and build its platform service
pub async fn open_platform(path: &Path, remote: Option<&str>) -> Result<PlatformContext> {
    let workspace = JjWorkspace::open(path)?;
    let workspace_root = workspace.workspace_root().to_path_buf();

    let remotes = workspace.git_remotes()?;
    let remote_name = select_remote(&remotes, remote)?;
    let remote_info = remotes
        .iter()
        .find(|r| r.name == remote_name)
        .ok_or_else(|| Error::RemoteNotFound(remote_name.clone()))?;

    let platform_config = parse_repo_info(&remote_info.url)?;
    let platform = create_platform_service(&platform_config).await?;

    Ok(PlatformContext {
        workspace_root,
        remote_name,
        platform,
    })
}
