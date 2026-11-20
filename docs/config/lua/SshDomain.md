# SshDomain

The `SshDomain` struct specifies information about an individual
[SSH Domain](../../multiplexing.md#ssh-domains).

It is a lua object with the following fields:

```lua
config.ssh_domains = {
  {
    -- The name of this specific domain.  Must be unique amongst
    -- all types of domain in the configuration file.
    name = 'my.server',

    -- identifies the host:port pair of the remote server
    -- Can be a DNS name or an IP address with an optional
    -- ":port" on the end.
    remote_address = '192.168.1.1',

    -- Whether agent auth should be disabled.
    -- Set to true to disable it.
    -- no_agent_auth = false,

    -- The username to use for authenticating with the remote host
    username = 'yourusername',

    -- If true, connect to this domain automatically at startup
    -- connect_automatically = true,

    -- Specify an alternative read timeout
    -- timeout = 60,

    -- The path to the wezterm binary on the remote host.
    -- Primarily useful if it isn't installed in the $PATH
    -- that is configure for ssh.
    -- remote_wezterm_path = "/home/yourusername/bin/wezterm"
  },
}
```

{{since('20220101-133340-7edc5b5a')}}

You may now specify a table with ssh config overrides:

```lua
config.ssh_domains = {
  {
    name = 'my.server',
    remote_address = '192.168.1.1',
    ssh_option = {
      identityfile = '/path/to/id_rsa.pub',
    },
  },
}
```

{{since('20220319-142410-0fcdea07')}}

You may now specify the type of `multiplexing` used by an ssh domain.
The following values are possible:

* `"WezTerm"` - this is the default; use wezterm's multiplexing client.
  Having wezterm installed on the server is required to use this mode.
* `"None"` - don't use any multiplexing. The connection is an ssh connection
  using the same mechanism as is used by `wezterm ssh`; losing connectivity
  will lose any panes/tabs.  This mode of operation is convenient when using
  SSH to connect automatically into eg: a locally hosted WSL instance, together
  with the [default_domain](config/default_domain.md) option.

A new `assume_shell` option, when coupled with `multiplexing = "None"`, allows
wezterm to assume that the remote host uses a specific shell command language
dialect, which in turn allows wezterm to respect the
current working directory as set by [OSC 7 / Shell
Integration](../../shell-integration.md) on the remote host when spawning new
panes and tabs.  The following values are recognized for `assume_shell`:

* `"Unknown"` - this is the default. We can't make any assumptions about the
  remote shell.
* `"Posix"` - the remote host uses a POSIX/Bourne Shell compatible environment
  that allows the syntax `env -c DIR ENV1=VAL1 ENV2=VAL2 CMD` and
  `env -c DIR ENV1=VAL1 ENV2=VAL2 $SHELL`.

```lua
config.ssh_domains = {
  {
    name = 'my.server',
    remote_address = '192.168.1.1',
    multiplexing = 'None',

    -- When multiplexing == "None", default_prog can be used
    -- to specify the default program to run in new tabs/panes.
    -- Due to the way that ssh works, you cannot specify default_cwd,
    -- but you could instead change your default_prog to put you
    -- in a specific directory.
    default_prog = { 'fish' },

    -- assume that we can use syntax like:
    -- "env -C /some/where $SHELL"
    -- using whatever the default command shell is on this
    -- remote host, so that shell integration will respect
    -- the current directory on the remote host.
    assume_shell = 'Posix',
  },
}

config.default_domain = 'my.server'
```

You may now specify the round-trip latency threshold for enabling predictive
local echo using `local_echo_threshold_ms`. If the measured round-trip latency
between the wezterm client and the server exceeds the specified threshold, the
client will attempt to predict the server's response to key events and echo the
result of that prediction locally without waiting, hence hiding latency to the
user. This option only applies when `multiplexing = "WezTerm"`.

```lua
config.ssh_domains = {
  {
    name = 'my.server',
    remote_address = '192.168.1.1',
    local_echo_threshold_ms = 10,
  },
}
```

{{since('20221119-145034-49b9839f')}}

The lag indicator now defaults to disabled. It is recommended to display
the lag information in your status bar using [this
example](pane/get_metadata.md).

If you prefer to have the information overlaid on the content area, then
you can set `overlay_lag_indicator = true`, but note that I'd like to
remove that functionality in the future.

{{since('nightly')}}

### Reaching a host through a jump host

A host that can only be reached through another one, such as a bastion or a
VPS in front of a private network, can be reached the way `ssh -J` reaches
it. The connection goes to the jump host first, which opens a TCP connection
on to `remote_address` and only passes the bytes along. The SSH session with
the host itself runs end to end through it, so the jump host never sees what
goes over it, and it needs nothing but an `sshd` that allows TCP forwarding
(OpenSSH's `AllowTcpForwarding`, on by default). Everything an SSH domain
does then works as if the host were reached directly: multiplexing, the Files
panel, installing or updating ThinkTerm on the host.

`ProxyJump` in your `~/.ssh/config` is followed, and so is `proxyjump` in
`ssh_option`:

```lua
config.ssh_domains = {
  {
    name = 'inside',
    -- The host's address as the jump host reaches it
    remote_address = '10.0.0.5',
    username = 'me',
    ssh_option = {
      -- One or more [user@]host[:port], separated by commas and gone
      -- through in order. Each is looked up in ~/.ssh/config like any host.
      proxyjump = 'me@bastion.example.com:22',
    },
  },
}
```

`jump_hosts` says the same, with each jump host's own settings:

```lua
config.ssh_domains = {
  {
    name = 'inside',
    remote_address = '10.0.0.5',
    username = 'me',
    jump_hosts = {
      {
        remote_address = 'bastion.example.com:22',
        username = 'me',
        ssh_option = { identityfile = '/home/me/.ssh/bastion_ed25519' },
      },
    },
  },
}
```

* Each host is signed in to in turn. Host key questions and password prompts
  name the jump host they come from, and a password stored for one host is
  never sent to another.
* With `jump_hosts`, or `proxyjump`, the host's own `proxycommand` is not
  used; the first jump host's own `proxycommand` or `ProxyJump` still is.
* On the Remote Hosts page, a saved host's **Jump host** setting does this
  with another saved host.
* Mosh cannot go through a jump host: it reaches the host itself over UDP.

### Replacing the command run on the remote host

`override_proxy_command` replaces the `thinkterm cli --prefer-mux proxy`
command that a multiplexing SSH domain runs on the remote host to reach its
mux server. It is run by the remote host's shell, without a pty, and has to
pass the mux protocol through its standard input and output.

One use is a host that only the remote host can sign in to: here
`relay.example.com` holds a key for `10.0.0.5`, and this machine needs none.

```lua
config.ssh_domains = {
  {
    name = 'inside-via-relay',
    remote_address = 'relay.example.com',
    username = 'me',
    override_proxy_command = "ssh -T -o BatchMode=yes me@10.0.0.5 "
      .. "'~/.local/bin/thinkterm cli --prefer-mux proxy'",
  },
}
```

Only the multiplexing connection goes on to `10.0.0.5` that way: the Files
panel and installing ThinkTerm work with `relay.example.com` itself. Where
this machine can sign in to the host, a jump host (above) is the better fit.
