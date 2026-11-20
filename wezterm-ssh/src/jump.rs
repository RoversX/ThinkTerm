//! Reaching a host through others, as `ssh -J` does: a session to the
//! first hop, a direct-tcpip channel through it to the next, and so on
//! until one carries the connection to the host itself. Each hop signs in
//! with its own configuration; the hops in between only pass bytes along,
//! which the session to the host keeps encrypted end to end.
//!
//! The hops are kept in the host's own [`ConfigMap`], so that everything
//! that already carries one there carries them too: hop `i`'s option `key`
//! is held as `wezterm_jump.<i>.<key>`. [`Config::resolve_jump_hosts`]
//! puts them there from the `proxyjump` option, and whoever has them some
//! other way can with [`set_jump_hosts`].

use crate::config::{Config, ConfigMap};
use anyhow::{anyhow, bail, Context};

/// The most hosts a connection may go through. A chain longer than this
/// is all but certainly a ProxyJump that leads back to itself.
pub const MAX_JUMP_HOSTS: usize = 8;

const PREFIX: &str = "wezterm_jump.";

/// A hop's option for whoever named it, given back with the hop's prompts
/// (see [`JumpHop::tag`]) so they can be told apart from other hops'.
pub const JUMP_TAG: &str = "wezterm_jump_tag";

/// Options that a hop which does not set them goes with as the host it
/// leads to does: how this crate connects, and the agent to sign in with,
/// which `ssh -J` would find for every hop where it finds it for the host.
/// Not the network: a host to be reached over the embedded tailnet is not
/// reached through jump hosts at all (see `connect_to_host`).
const INHERITED: &[&str] = &[
    "wezterm_ssh_backend",
    "wezterm_ssh_verbose",
    "identityagent",
];

/// A host the connection goes through on its way, as the prompts it
/// raises name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpHop {
    /// Its place on the way: 0 is the host reached first.
    pub index: usize,
    /// `user@host:port`, to show people.
    pub name: String,
    /// The hop's [`JUMP_TAG`] option, if whoever named it gave it one.
    pub tag: Option<String>,
}

impl JumpHop {
    pub(crate) fn new(index: usize, config: &ConfigMap) -> Self {
        let get = |key: &str| config.get(key).map(String::as_str).unwrap_or("");
        Self {
            index,
            name: format!("{}@{}:{}", get("user"), get("hostname"), get("port")),
            tag: config.get(JUMP_TAG).cloned(),
        }
    }
}

impl std::fmt::Display for JumpHop {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// One host named in a ProxyJump value: `[user@]host[:port]`, or the same
/// as an `ssh://` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpSpec {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl std::fmt::Display for JumpSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// The hosts a ProxyJump value names, in the order they are gone through.
/// `none` names none.
pub fn parse_proxy_jump(value: &str) -> anyhow::Result<Vec<JumpSpec>> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Ok(vec![]);
    }
    value.split(',').map(parse_jump_spec).collect()
}

fn parse_jump_spec(item: &str) -> anyhow::Result<JumpSpec> {
    let item = item.trim();
    let (spec, is_uri) = match item.strip_prefix("ssh://") {
        Some(rest) => (rest.strip_suffix('/').unwrap_or(rest), true),
        None => (item, false),
    };
    let (user, host_port) = match spec.rsplit_once('@') {
        Some((user, rest)) => (Some(user), rest),
        None => (None, spec),
    };
    if user.is_some_and(str::is_empty) {
        bail!("jump host {item:?} has an empty user name");
    }
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, rest) = bracketed
            .split_once(']')
            .ok_or_else(|| anyhow!("jump host {item:?} has an unclosed '['"))?;
        match rest {
            "" => (host, None),
            rest => match rest.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None => bail!("jump host {item:?} has something after ']' that is not a port"),
            },
        }
    } else {
        match host_port.split_once(':') {
            // More than one colon and no brackets: an IPv6 address alone.
            Some((_, rest)) if rest.contains(':') && !is_uri => (host_port, None),
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        bail!("jump host {item:?} names no host");
    }
    if host.contains(char::is_whitespace) {
        bail!("jump host {item:?} has a space in its host name");
    }
    let port = port
        .map(|port| {
            port.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| anyhow!("jump host {item:?} has an invalid port {port:?}"))
        })
        .transpose()?;
    Ok(JumpSpec {
        user: user.map(str::to_string),
        host: host.to_string(),
        port,
    })
}

/// Puts `hops` into `config` as the hosts it is reached through, nearest
/// first, in place of any it had. Each is a host's whole configuration, as
/// [`Config::for_host`] gives it; how it is itself reached is no longer
/// up to it, so its own `proxyjump` and `proxycommand` are left out, but
/// for the first hop's `proxycommand`, which still reaches it.
pub fn set_jump_hosts(config: &mut ConfigMap, hops: &[ConfigMap]) {
    config.retain(|key, _| !key.starts_with(PREFIX));
    for (index, hop) in hops.iter().enumerate() {
        for (key, value) in hop {
            let is_route = key == "proxyjump" || (key == "proxycommand" && index > 0);
            if key.starts_with(PREFIX) || is_route {
                continue;
            }
            config.insert(format!("{PREFIX}{index}.{key}"), value.clone());
        }
    }
}

/// The hosts `config` is reached through, nearest first, as
/// [`set_jump_hosts`] or [`Config::resolve_jump_hosts`] put them there.
pub fn jump_hosts(config: &ConfigMap) -> anyhow::Result<Vec<ConfigMap>> {
    let mut hops: Vec<ConfigMap> = vec![];
    for (key, value) in config.range(PREFIX.to_string()..) {
        let Some(rest) = key.strip_prefix(PREFIX) else {
            break;
        };
        let (index, option) = rest
            .split_once('.')
            .ok_or_else(|| anyhow!("malformed jump host option {key:?}"))?;
        let index: usize = index
            .parse()
            .with_context(|| format!("malformed jump host option {key:?}"))?;
        if index >= MAX_JUMP_HOSTS {
            bail!("more than {MAX_JUMP_HOSTS} jump hosts");
        }
        if hops.len() <= index {
            hops.resize_with(index + 1, ConfigMap::new);
        }
        hops[index].insert(option.to_string(), value.clone());
    }
    for (index, hop) in hops.iter().enumerate() {
        if !hop.contains_key("hostname") {
            bail!("jump host {index} names no host");
        }
    }
    Ok(hops)
}

/// Takes the hosts `config` is reached through out of it, as [`jump_hosts`]
/// reads them.
pub fn take_jump_hosts(config: &mut ConfigMap) -> anyhow::Result<Vec<ConfigMap>> {
    let hops = jump_hosts(config)?;
    config.retain(|key, _| !key.starts_with(PREFIX));
    Ok(hops)
}

/// Whether `config` says the host is reached through others, either way:
/// with hops already put in, or with a `proxyjump` still to resolve.
pub fn has_jump_hosts(config: &ConfigMap) -> bool {
    config.keys().any(|key| key.starts_with(PREFIX))
        || config
            .get("proxyjump")
            .is_some_and(|value| !value.trim().eq_ignore_ascii_case("none"))
}

/// What a host's hop is to go with: what it does not set itself about how
/// to connect is as the host's.
pub(crate) fn inherit(host: &ConfigMap, hop: &mut ConfigMap) {
    for key in INHERITED {
        if let Some(value) = host.get(*key) {
            hop.entry(key.to_string()).or_insert_with(|| value.clone());
        }
    }
}

/// The hops of a `config` that may not have had them resolved: those it
/// holds, else those its `proxyjump` names, resolved with the default
/// configuration files.
pub(crate) fn hops_for(config: &ConfigMap) -> anyhow::Result<Vec<ConfigMap>> {
    let hops = jump_hosts(config)?;
    if !hops.is_empty() || !has_jump_hosts(config) {
        return Ok(hops);
    }
    let mut resolver = Config::new();
    resolver.add_default_config_files();
    resolver.jump_chain(config)
}

type Identity = (String, String, String);

fn identity(config: &ConfigMap) -> Identity {
    let get = |key: &str| config.get(key).cloned().unwrap_or_default();
    (get("user"), get("hostname"), get("port"))
}

impl Config {
    /// Resolves `target`'s `proxyjump` option, if it has one, into the
    /// configurations of the hosts it names, each as `for_host` gives it,
    /// and puts them into `target` (see [`set_jump_hosts`]). The first hop
    /// is reached as its own configuration says, through any hosts its own
    /// `proxyjump` names; those after it through the one before, as
    /// `ssh -J` does. Hops already put in are left as they are.
    pub fn resolve_jump_hosts(&self, target: &mut ConfigMap) -> anyhow::Result<()> {
        if !jump_hosts(target)?.is_empty() {
            return Ok(());
        }
        let hops = self.jump_chain(target)?;
        set_jump_hosts(target, &hops);
        Ok(())
    }

    fn jump_chain(&self, target: &ConfigMap) -> anyhow::Result<Vec<ConfigMap>> {
        let mut chain = vec![];
        self.extend_jump_chain(target, &mut chain, &mut vec![identity(target)])?;
        if chain.len() > MAX_JUMP_HOSTS {
            bail!("ProxyJump goes through more than {MAX_JUMP_HOSTS} hosts");
        }
        Ok(chain)
    }

    /// Puts the hops `host`'s `proxyjump` names onto `chain`, after those
    /// the first of them is itself reached through. `way` holds the hosts
    /// on the way being worked out, each to be reached through the next: a
    /// first hop already among them would be reached through itself. A
    /// host may well come up twice otherwise, as `ssh -J` lets it.
    fn extend_jump_chain(
        &self,
        host: &ConfigMap,
        chain: &mut Vec<ConfigMap>,
        way: &mut Vec<Identity>,
    ) -> anyhow::Result<()> {
        let Some(value) = host.get("proxyjump") else {
            return Ok(());
        };
        let specs = parse_proxy_jump(value)?;
        for (index, spec) in specs.iter().enumerate() {
            // Those on the chain, those on `way` still to go on it (all but
            // the host it starts from), and this one.
            if chain.len() + way.len() > MAX_JUMP_HOSTS {
                bail!("ProxyJump goes through more than {MAX_JUMP_HOSTS} hosts");
            }
            let mut hop = self.hop_config(spec);
            inherit(host, &mut hop);
            if index == 0 {
                let id = identity(&hop);
                if way.contains(&id) {
                    bail!("ProxyJump leads back to {}@{}:{}", id.0, id.1, id.2);
                }
                way.push(id);
                let before = chain.len();
                self.extend_jump_chain(&hop, chain, way)?;
                way.pop();
                if chain.len() > before {
                    hop.remove("proxycommand");
                }
            } else {
                hop.remove("proxycommand");
            }
            hop.remove("proxyjump");
            chain.push(hop);
        }
        Ok(())
    }

    /// The configuration of the host `spec` names, with its user and port
    /// in place before the options that name them (`%p` in a ProxyCommand)
    /// are expanded, as they would be on ssh's command line.
    fn hop_config(&self, spec: &JumpSpec) -> ConfigMap {
        // What was set for the host the way leads to is not for its hops.
        let mut config = self.for_another_host();
        if let Some(user) = &spec.user {
            config.set_option("user", user);
        }
        if let Some(port) = spec.port {
            config.set_option("port", port.to_string());
        }
        config.for_host(&spec.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(user: Option<&str>, host: &str, port: Option<u16>) -> JumpSpec {
        JumpSpec {
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
        }
    }

    #[test]
    fn proxy_jump_values_parse_as_ssh_reads_them() {
        assert_eq!(parse_proxy_jump("none").unwrap(), vec![]);
        assert_eq!(parse_proxy_jump(" NONE ").unwrap(), vec![]);
        assert_eq!(
            parse_proxy_jump("bastion").unwrap(),
            vec![spec(None, "bastion", None)]
        );
        assert_eq!(
            parse_proxy_jump("me@bastion:2222, relay").unwrap(),
            vec![
                spec(Some("me"), "bastion", Some(2222)),
                spec(None, "relay", None)
            ]
        );
        assert_eq!(
            parse_proxy_jump("ssh://me@bastion:2222/").unwrap(),
            vec![spec(Some("me"), "bastion", Some(2222))]
        );
        assert_eq!(
            parse_proxy_jump("me@[fd00::1]:2200").unwrap(),
            vec![spec(Some("me"), "fd00::1", Some(2200))]
        );
        assert_eq!(
            parse_proxy_jump("fd00::1").unwrap(),
            vec![spec(None, "fd00::1", None)]
        );
        assert_eq!(
            parse_proxy_jump("a@b@bastion").unwrap(),
            vec![spec(Some("a@b"), "bastion", None)]
        );
        for bad in [
            "",
            "me@",
            "@bastion",
            "bastion:",
            "bastion:0",
            "bastion:99999",
            "[::1",
            "a,,b",
            "two words",
        ] {
            assert!(parse_proxy_jump(bad).is_err(), "{:?} parsed", bad);
        }
    }

    #[test]
    fn a_spec_shows_as_it_would_be_written() {
        for text in [
            "bastion",
            "me@bastion:2222",
            "me@[fd00::1]:2200",
            "[fd00::1]",
        ] {
            let parsed = parse_proxy_jump(text).unwrap();
            assert_eq!(parsed[0].to_string(), text);
        }
    }

    fn resolver() -> Config {
        let mut config = Config::new();
        config.add_config_string(
            r#"
Host target
    HostName target.example
    ProxyJump hop1,me@hop2:2202

Host hop1
    HostName hop1.example
    User hopper
    ProxyCommand nc %h %p

Host hop2
    ProxyCommand nc %h %p
    ProxyJump never-used

Host chained
    ProxyJump hop3

Host hop3
    ProxyJump hop1

Host loop-a
    ProxyJump loop-b

Host loop-b
    ProxyJump loop-a

Host twice
    ProxyJump repeat-a,repeat-b

Host repeat-a
    ProxyJump repeat-b
"#,
        );
        config
    }

    #[test]
    fn hops_resolve_with_their_own_configuration() {
        let config = resolver();
        let mut target = config.for_host("target");
        target.insert("wezterm_ssh_backend".into(), "ssh2".into());
        config.resolve_jump_hosts(&mut target).unwrap();
        let hops = jump_hosts(&target).unwrap();
        assert_eq!(hops.len(), 2);

        assert_eq!(hops[0]["hostname"], "hop1.example");
        assert_eq!(hops[0]["user"], "hopper");
        assert_eq!(hops[0]["port"], "22");
        // The first hop is still reached as its configuration says.
        assert_eq!(hops[0]["proxycommand"], "nc hop1.example 22");
        // ... and goes as the host it leads to.
        assert_eq!(hops[0]["wezterm_ssh_backend"], "ssh2");

        assert_eq!(hops[1]["hostname"], "hop2");
        assert_eq!(hops[1]["user"], "me");
        assert_eq!(hops[1]["port"], "2202");
        // The second is reached through the first, whatever it says.
        assert!(!hops[1].contains_key("proxycommand"));
        assert!(!hops[1].contains_key("proxyjump"));
    }

    #[test]
    fn the_first_hop_brings_the_hosts_it_is_reached_through() {
        let config = resolver();
        let mut target = config.for_host("chained");
        config.resolve_jump_hosts(&mut target).unwrap();
        let hops = jump_hosts(&target).unwrap();
        let names: Vec<_> = hops.iter().map(|hop| hop["hostname"].as_str()).collect();
        assert_eq!(names, ["hop1.example", "hop3"]);
        // hop3 is reached through hop1, not by a command of its own.
        assert!(!hops[1].contains_key("proxycommand"));
    }

    #[test]
    fn a_proxy_jump_that_leads_back_to_itself_is_refused() {
        let config = resolver();
        let mut target = config.for_host("loop-a");
        let err = config.resolve_jump_hosts(&mut target).unwrap_err();
        assert!(format!("{:#}", err).contains("ProxyJump"), "{:#}", err);
    }

    #[test]
    fn a_host_may_come_up_twice_on_the_way_without_leading_back() {
        let config = resolver();
        let mut target = config.for_host("twice");
        config.resolve_jump_hosts(&mut target).unwrap();
        let names: Vec<_> = jump_hosts(&target)
            .unwrap()
            .iter()
            .map(|hop| hop["hostname"].clone())
            .collect();
        // repeat-a is reached through repeat-b, then repeat-b through it.
        assert_eq!(names, ["repeat-b", "repeat-a", "repeat-b"]);
    }

    #[test]
    fn of_proxy_command_and_proxy_jump_the_first_obtained_is_used() {
        let mut config = Config::new();
        config.add_config_string(
            r#"
Host ssm-*
    ProxyCommand aws ssm start-session --target %h

Host bastion
    ProxyCommand none

Host *
    ProxyJump bastion
"#,
        );
        // Its own ProxyCommand comes before `Host *`'s ProxyJump.
        let ssm = config.for_host("ssm-1234");
        assert_eq!(
            ssm["proxycommand"],
            "aws ssm start-session --target ssm-1234"
        );
        assert!(!ssm.contains_key("proxyjump"));
        // The bastion is reached directly, not through itself.
        let mut bastion = config.for_host("bastion");
        assert_eq!(bastion["proxycommand"], "none");
        assert!(!has_jump_hosts(&bastion));
        config.resolve_jump_hosts(&mut bastion).unwrap();
        assert!(jump_hosts(&bastion).unwrap().is_empty());
        // And everything else goes through it.
        let mut other = config.for_host("elsewhere");
        config.resolve_jump_hosts(&mut other).unwrap();
        let hops = jump_hosts(&other).unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0]["hostname"], "bastion");
        assert_eq!(hops[0]["proxycommand"], "none");
    }

    #[test]
    fn a_hop_s_proxy_command_reaches_the_port_proxy_jump_names() {
        let config = resolver();
        let mut target = config.for_host("target");
        target.insert("proxyjump".into(), "hop1:2222".into());
        config.resolve_jump_hosts(&mut target).unwrap();
        let hops = jump_hosts(&target).unwrap();
        assert_eq!(hops[0]["port"], "2222");
        assert_eq!(hops[0]["proxycommand"], "nc hop1.example 2222");
    }

    #[test]
    fn a_hop_signs_in_with_the_agent_the_host_does_unless_it_has_its_own() {
        let mut host = ConfigMap::new();
        host.insert("identityagent".into(), "/relay/agent.sock".into());
        let mut hop = ConfigMap::new();
        inherit(&host, &mut hop);
        assert_eq!(hop["identityagent"], "/relay/agent.sock");
        let mut own = ConfigMap::new();
        own.insert("identityagent".into(), "/own/agent.sock".into());
        inherit(&host, &mut own);
        assert_eq!(own["identityagent"], "/own/agent.sock");
    }

    #[test]
    fn in_one_stanza_the_first_of_proxy_jump_and_proxy_command_counts() {
        let mut config = Config::new();
        config.add_config_string(
            "Host jumps-first\n    ProxyJump gateway\n    ProxyCommand none\n\
             Host command-first\n    ProxyCommand none\n    ProxyJump gateway\n",
        );
        let jumps = config.for_host("jumps-first");
        assert_eq!(jumps["proxyjump"], "gateway");
        assert!(!jumps.contains_key("proxycommand"));
        let command = config.for_host("command-first");
        assert_eq!(command["proxycommand"], "none");
        assert!(!command.contains_key("proxyjump"));
    }

    #[test]
    fn a_host_ruled_out_by_a_negation_is_reached_directly() {
        let mut config = Config::new();
        config.add_config_string("Host * !gateway\n    ProxyJump gateway\n");
        let mut gateway = config.for_host("gateway");
        assert!(!gateway.contains_key("proxyjump"));
        config.resolve_jump_hosts(&mut gateway).unwrap();
        assert!(jump_hosts(&gateway).unwrap().is_empty());
        let mut inside = config.for_host("inside");
        config.resolve_jump_hosts(&mut inside).unwrap();
        let hops = jump_hosts(&inside).unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0]["hostname"], "gateway");
    }

    #[test]
    fn a_hop_expands_its_options_with_its_own_user_and_not_the_hosts_overrides() {
        let mut config = Config::new();
        config.add_config_string(
            "Host gateway\n    IdentityFile /keys/%r\n    ProxyCommand nc %h %p %r\n",
        );
        // Set for the destination, as on ssh's command line.
        config.set_option("proxyjump", "relay@gateway:2200");
        config.set_option("identityfile", "/keys/destination");
        let mut target = config.for_host("inside");
        config.resolve_jump_hosts(&mut target).unwrap();
        let hops = jump_hosts(&target).unwrap();
        assert_eq!(hops.len(), 1, "the gateway is not reached through itself");
        assert_eq!(hops[0]["user"], "relay");
        assert_eq!(hops[0]["identityfile"], "/keys/relay");
        assert_eq!(hops[0]["proxycommand"], "nc gateway 2200 relay");
    }

    #[test]
    fn a_way_too_long_to_follow_is_refused_however_it_is_nested() {
        let mut text = String::new();
        for i in 0..=MAX_JUMP_HOSTS {
            text.push_str(&format!("Host h{i}\n    ProxyJump h{}\n", i + 1));
        }
        let mut config = Config::new();
        config.add_config_string(&text);
        let mut target = config.for_host("h0");
        let err = config.resolve_jump_hosts(&mut target).unwrap_err();
        assert!(format!("{:#}", err).contains("more than"), "{:#}", err);
    }

    #[test]
    fn hops_survive_the_round_trip_through_a_config_map() {
        let mut first = ConfigMap::new();
        first.insert("hostname".into(), "one".into());
        first.insert("proxycommand".into(), "nc one 22".into());
        first.insert(JUMP_TAG.into(), "0".into());
        let mut second = ConfigMap::new();
        second.insert("hostname".into(), "two".into());
        second.insert("proxycommand".into(), "never".into());
        second.insert("proxyjump".into(), "never".into());

        let mut target = ConfigMap::new();
        target.insert("hostname".into(), "target".into());
        target.insert("wezterm_jump.7.hostname".into(), "stale".into());
        set_jump_hosts(&mut target, &[first.clone(), second]);

        let hops = jump_hosts(&target).unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0], first);
        assert_eq!(hops[1]["hostname"], "two");
        assert!(!hops[1].contains_key("proxycommand"));
        assert!(!hops[1].contains_key("proxyjump"));
        assert!(has_jump_hosts(&target));

        let taken = take_jump_hosts(&mut target).unwrap();
        assert_eq!(taken.len(), 2);
        assert!(!has_jump_hosts(&target));
        assert_eq!(target["hostname"], "target");
    }

    #[test]
    fn a_hop_with_no_host_is_refused() {
        let mut target = ConfigMap::new();
        target.insert("wezterm_jump.0.user".into(), "me".into());
        assert!(jump_hosts(&target).is_err());
    }

    #[test]
    fn hops_name_themselves_for_prompts() {
        let mut hop = ConfigMap::new();
        hop.insert("hostname".into(), "bastion".into());
        hop.insert("user".into(), "me".into());
        hop.insert("port".into(), "2222".into());
        hop.insert(JUMP_TAG.into(), "1".into());
        let named = JumpHop::new(3, &hop);
        assert_eq!(named.to_string(), "me@bastion:2222");
        assert_eq!(named.index, 3);
        assert_eq!(named.tag.as_deref(), Some("1"));
    }
}
