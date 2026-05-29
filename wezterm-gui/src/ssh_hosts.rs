//! Runtime SSH host -> mux domain plumbing for the ThinkTerm host manager.
//!
//! ThinkTerm stores SSH hosts as "remote projects" (see
//! [`crate::project_sessions::SshHostSpec`]). Unlike WezTerm's Lua
//! `ssh_domains`, these are registered with the mux **at runtime** via
//! [`mux::Mux::add_domain`]. No engine changes are required: building a domain
//! from a struct (`RemoteSshDomain::with_ssh_domain`) and adding it is exactly
//! what `wezterm ssh` already does in `main.rs`.

use crate::project_sessions::{self, SshHostSpec};
use anyhow::Result;
use config::{SshDomain, SshMultiplexing};
use mux::domain::Domain;
use mux::ssh::RemoteSshDomain;
use mux::Mux;
use std::collections::HashMap;
use std::sync::Arc;

/// `user@host` (or `host`, with `:port` when non-default) used for the
/// human-readable domain / workspace identifiers.
fn endpoint(spec: &SshHostSpec) -> String {
    let host = match spec.port {
        Some(port) if port != 22 => format!("{}:{port}", spec.host),
        _ => spec.host.clone(),
    };
    match &spec.username {
        Some(user) if !user.is_empty() => format!("{user}@{host}"),
        _ => host,
    }
}

/// Deterministic mux domain name for a host. Stable across restarts so that a
/// snapshotted SSH layout (which records the pane's domain name) can be
/// re-materialized after [`register_saved_hosts`] re-registers the domain.
pub fn ssh_domain_name(spec: &SshHostSpec) -> String {
    format!("ssh:{}", endpoint(spec))
}

/// Default workspace a connection lands in: `ssh:<endpoint>`, unless the host
/// overrides it.
pub fn default_workspace_name(spec: &SshHostSpec) -> String {
    spec.default_workspace
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("ssh:{}", endpoint(spec)))
}

/// Translate a stored host spec into a `config::SshDomain`.
pub fn build_ssh_domain(spec: &SshHostSpec) -> SshDomain {
    let mut ssh_option = HashMap::new();
    for (key, value) in &spec.ssh_options {
        ssh_option.insert(key.to_lowercase(), value.clone());
    }
    if let Some(identity) = &spec.identity_file {
        if !identity.trim().is_empty() {
            ssh_option.insert("identityfile".to_string(), identity.clone());
        }
    }

    let remote_address = match spec.port {
        Some(port) => format!("{}:{port}", spec.host),
        None => spec.host.clone(),
    };

    SshDomain {
        name: ssh_domain_name(spec),
        remote_address,
        username: spec.username.clone(),
        multiplexing: if spec.multiplexing {
            SshMultiplexing::WezTerm
        } else {
            SshMultiplexing::None
        },
        ssh_option,
        ..Default::default()
    }
}

/// Ensure the host's SSH domain is registered with the mux, building and adding
/// it on first use. Returns the domain name to spawn into. The actual SSH
/// connection is established lazily by the domain's `spawn_pane`, so this is
/// cheap and does not block on the network.
pub fn ensure_ssh_domain_registered(spec: &SshHostSpec) -> Result<String> {
    let name = ssh_domain_name(spec);
    let mux = Mux::get();
    if mux.get_domain_by_name(&name).is_none() {
        let dom = build_ssh_domain(spec);
        let domain: Arc<dyn Domain> = Arc::new(RemoteSshDomain::with_ssh_domain(&dom)?);
        mux.add_domain(&domain);
    }
    Ok(name)
}

/// Register every saved SSH host as a mux domain. Called once at startup so
/// that re-activating a previously-connected remote session (whose saved layout
/// references the domain by name) works without re-opening the host manager.
pub fn register_saved_hosts() {
    for (_project_id, spec) in project_sessions::list_hosts() {
        if let Err(err) = ensure_ssh_domain_registered(&spec) {
            log::warn!(
                "failed to register saved SSH host {:?}: {err:#}",
                spec.label
            );
        }
    }
}
