//! Hidden developer diagnostics (`tomo dev …`). Not part of the stable CLI
//! surface; these exist to inspect build-time facts that the release tooling and
//! scenarios assert against.

use std::path::PathBuf;

use serde::Serialize;

use tomo_transport::{ResolvedEndpoint, ResolvedRoute, StrictHostKey};

use crate::error::CliError;
use crate::out::outln;

/// One embedded binary, as reported by `tomo dev embedded-binaries --json`.
#[derive(Debug, Serialize)]
struct EmbeddedEntry {
    /// The target triple the embedded binary runs on.
    triple: String,
    /// The exact version it was built at.
    version: String,
    /// The embedded payload size in bytes.
    bytes: usize,
}

/// Print the release binaries embedded into this build's bootstrap payload.
///
/// Empty in ordinary dev builds; populated only when compiled with
/// `--features embed-binaries`. `scripts/release.sh` runs this against the fat
/// binary to verify its embedded inventory (docs/RELEASING.md).
///
/// # Errors
/// [`CliError`] only if JSON serialization fails.
pub fn run_embedded_binaries(json: bool) -> Result<(), CliError> {
    let inventory = tomo_transport::embedded_inventory();

    if json {
        let entries: Vec<EmbeddedEntry> = inventory
            .iter()
            .map(|(triple, version, bytes)| EmbeddedEntry {
                triple: (*triple).to_owned(),
                version: (*version).to_owned(),
                bytes: *bytes,
            })
            .collect();
        let out = serde_json::to_string_pretty(&entries)
            .map_err(|e| CliError::msg(format!("could not serialize embedded inventory: {e}")))?;
        outln!("{out}");
    } else if inventory.is_empty() {
        outln!("no binaries embedded (dev build; rebuild with --features embed-binaries)");
    } else {
        outln!("embedded binaries ({}):", inventory.len());
        for (triple, version, bytes) in &inventory {
            outln!("  tomo {version}  {triple}  ({bytes} bytes)");
        }
    }
    Ok(())
}

/// One hop of a resolved SSH route, as rendered by `tomo dev ssh-route`.
#[derive(Debug, Serialize)]
struct RouteHop {
    /// `"jump"` for an intermediate hop, `"target"` for the destination.
    role: &'static str,
    alias: String,
    hostname: String,
    port: u16,
    /// The user the config/target named, if any (`null` → the login default).
    user: Option<String>,
    /// The user actually used (`user`, or the local login name).
    effective_user: String,
    /// Config-declared identity files for this hop (empty → agent + defaults).
    identity_files: Vec<String>,
    /// `IdentitiesOnly yes` — ssh-agent keys are not offered.
    agent_skipped: bool,
    strict_host_key_checking: String,
    /// User known-hosts files (also the only accept-new recording targets).
    user_known_hosts_files: Vec<String>,
    /// Global known-hosts files (lookup only).
    global_known_hosts_files: Vec<String>,
    /// Every file consulted for lookup (user then global), in order.
    known_hosts_consulted: Vec<String>,
}

/// A fully-resolved SSH route, the `ssh -G` analogue printed by `ssh-route`.
#[derive(Debug, Serialize)]
struct RouteView {
    target: String,
    description: String,
    /// The `ProxyJump` chain (aliases, in dial order); empty for a direct hop.
    proxy_jump_chain: Vec<String>,
    hops: Vec<RouteHop>,
    /// Names of config options Tomo does not act on (for awareness).
    ignored_options: Vec<String>,
}

/// The `StrictHostKeyChecking` policy name.
fn strict_str(policy: StrictHostKey) -> &'static str {
    match policy {
        StrictHostKey::Yes => "yes",
        StrictHostKey::No => "no",
        StrictHostKey::AcceptNew => "accept-new",
    }
}

fn paths_to_strings(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
}

fn hop_view(ep: &ResolvedEndpoint, role: &'static str, default_user: &str) -> RouteHop {
    RouteHop {
        role,
        alias: ep.alias.clone(),
        hostname: ep.host_name.clone(),
        port: ep.port,
        user: ep.user.clone(),
        effective_user: ep.user.clone().unwrap_or_else(|| default_user.to_owned()),
        identity_files: paths_to_strings(&ep.identity_files),
        agent_skipped: ep.identities_only,
        strict_host_key_checking: strict_str(ep.strict).to_owned(),
        user_known_hosts_files: paths_to_strings(&ep.known_hosts_files),
        global_known_hosts_files: paths_to_strings(&ep.global_known_hosts_files),
        known_hosts_consulted: paths_to_strings(&ep.lookup_known_hosts()),
    }
}

/// Build the pure, renderable view of a resolved route (unit-tested).
fn route_view(target: &str, route: &ResolvedRoute, default_user: &str) -> RouteView {
    let mut hops: Vec<RouteHop> = route
        .jumps
        .iter()
        .map(|j| hop_view(j, "jump", default_user))
        .collect();
    hops.push(hop_view(&route.target, "target", default_user));
    RouteView {
        target: target.to_owned(),
        description: route.describe(),
        proxy_jump_chain: route.jumps.iter().map(|j| j.alias.clone()).collect(),
        hops,
        ignored_options: route.unknown_options.clone(),
    }
}

/// Render the human-readable route report (no trailing newline).
fn render_human(view: &RouteView) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "route to {}: {}", view.target, view.description);
    if !view.proxy_jump_chain.is_empty() {
        let _ = writeln!(s, "  proxy jump: {}", view.proxy_jump_chain.join(" -> "));
    }
    for hop in &view.hops {
        let _ = writeln!(s, "  [{}] {}", hop.role, hop.alias);
        let _ = writeln!(s, "      hostname               {}", hop.hostname);
        let _ = writeln!(s, "      port                   {}", hop.port);
        let _ = writeln!(s, "      user                   {}", hop.effective_user);
        if hop.identity_files.is_empty() {
            let _ = writeln!(
                s,
                "      identity files         (config: none; agent + built-in defaults)"
            );
        } else {
            let _ = writeln!(
                s,
                "      identity files         {}",
                hop.identity_files.join(", ")
            );
        }
        let _ = writeln!(
            s,
            "      identities only        {}",
            if hop.agent_skipped {
                "yes (ssh-agent skipped)"
            } else {
                "no"
            }
        );
        let _ = writeln!(
            s,
            "      stricthostkeychecking  {}",
            hop.strict_host_key_checking
        );
        let _ = writeln!(
            s,
            "      user known_hosts       {}",
            hop.user_known_hosts_files.join(", ")
        );
        let _ = writeln!(
            s,
            "      global known_hosts     {}",
            hop.global_known_hosts_files.join(", ")
        );
    }
    if !view.ignored_options.is_empty() {
        let _ = writeln!(s, "  ignored options: {}", view.ignored_options.join(", "));
    }
    s.trim_end().to_owned()
}

/// Resolve `target` through `~/.ssh/config` and print Tomo's route — the direct
/// analogue of `ssh -G <target>`. Pure resolution: no network. Honors
/// `TOMO_SSH_CONFIG`.
///
/// # Errors
/// [`CliError`] if `$HOME` is unset, the route cannot be resolved (a cyclic or
/// too-deep `ProxyJump`), or JSON serialization fails.
pub fn run_ssh_route(target: &str, json: bool) -> Result<(), CliError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CliError::msg("cannot resolve an ssh route: $HOME is unset"))?;
    let default_user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let opts = tomo_transport::SshOpts::new(&home, &default_user);
    let route = tomo_transport::resolve_route(target, &opts)
        .map_err(|e| CliError::msg(format!("cannot resolve ssh route for {target:?}: {e}")))?;
    let view = route_view(target, &route, &default_user);

    if json {
        let out = serde_json::to_string_pretty(&view)
            .map_err(|e| CliError::msg(format!("could not serialize ssh route: {e}")))?;
        outln!("{out}");
    } else {
        outln!("{}", render_human(&view));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    fn resolve(cfg: &str, target: &str) -> ResolvedRoute {
        tomo_transport::SshConfig::parse(cfg)
            .resolve_route(
                target,
                Path::new("/home/jake"),
                tomo_transport::DEFAULT_SSH_PORT,
            )
            .unwrap()
    }

    #[test]
    fn renders_port_and_known_hosts_for_configd_host() {
        // The p1 shape: non-22 port, default known-hosts set.
        let route = resolve("Host p1\n  Port 25601\n", "p1");
        let view = route_view("p1", &route, "jake");
        assert_eq!(view.hops.len(), 1);
        let hop = &view.hops[0];
        assert_eq!(hop.role, "target");
        assert_eq!(hop.port, 25601);
        assert_eq!(hop.effective_user, "jake");
        assert_eq!(
            hop.user_known_hosts_files,
            vec![
                "/home/jake/.ssh/known_hosts".to_owned(),
                "/home/jake/.ssh/known_hosts2".to_owned(),
            ]
        );
        assert!(hop
            .global_known_hosts_files
            .contains(&"/etc/ssh/ssh_known_hosts".to_owned()));
        assert_eq!(hop.known_hosts_consulted.len(), 4);

        let text = render_human(&view);
        assert!(text.contains("port                   25601"), "{text}");
        assert!(text.contains("/home/jake/.ssh/known_hosts2"), "{text}");
        assert!(text.contains("/etc/ssh/ssh_known_hosts"), "{text}");
    }

    #[test]
    fn renders_proxy_jump_chain_hops() {
        let route = resolve(
            "Host dst\n  HostName 10.0.0.9\n  ProxyJump gw\nHost gw\n  HostName 10.0.0.1\n  User bastion\n",
            "dst",
        );
        let view = route_view("dst", &route, "me");
        assert_eq!(view.proxy_jump_chain, vec!["gw".to_owned()]);
        assert_eq!(view.hops.len(), 2);
        assert_eq!(view.hops[0].role, "jump");
        assert_eq!(view.hops[0].effective_user, "bastion");
        assert_eq!(view.hops[1].role, "target");
        assert_eq!(view.hops[1].hostname, "10.0.0.9");
        let text = render_human(&view);
        assert!(text.contains("proxy jump: gw"), "{text}");
    }

    #[test]
    fn json_view_is_serializable() {
        let route = resolve("Host p1\n  Port 2222\n", "p1");
        let view = route_view("p1", &route, "jake");
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"port\":2222"));
        assert!(json.contains("known_hosts_consulted"));
    }
}
