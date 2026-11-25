# Native WireGuard

**Status:** Researched, not started

**Decision:** Parked (2026-11-26)

## Summary

ThinkTerm can already reach machines through its embedded tailnet. This note
looks at a second kind of network beside it: a plain WireGuard tunnel built
from a standard `.conf` file, with no control server, no NAT traversal and no
relay. ThinkTerm would run the tunnel itself, in user space, for its own
connections only. It would not turn on a system VPN.

The conclusion of the research:

- It can be built from crates that are already in the dependency tree. The
  WireGuard state machine (`ts_tunnel`), the routing and filtering around it
  (`ts_dataplane`) and the user-space TCP/IP stack (`ts_netstack_smoltcp`)
  work without the control plane, DERP or the runtime that ties them to it.
- The SSH, terminal and phone connection code can stay as it is. A new
  engine hands back the same stream type the tailnet hands back today.
- The estimate is about three weeks for desktop, Android and iOS together.
- Making networks into plugins is not worth it: phones cannot run plugins,
  and the plugin channel does not carry byte streams.

Everything under "What the crates provide" was checked against the source of
the versions in `Cargo.lock` (tailscale-rs 0.7.1). Things that only a running
test can confirm are listed under "Not yet verified".

## Goal

- Import a WireGuard configuration: `[Interface]` with `PrivateKey`,
  `Address` and optionally `DNS`, and one or more `[Peer]` sections with
  `PublicKey`, `Endpoint`, `AllowedIPs`, `PersistentKeepalive` and
  `PresharedKey`.
- Reach SSH hosts (and, on the desktop, whatever else ThinkTerm dials) through
  that tunnel, on the desktop, Android and iOS.
- Keep the tailnet working beside it.

Not goals for a first version:

- A system-wide VPN, or carrying traffic for other apps.
- Acting as a WireGuard server, or accepting connections through the tunnel.
- NAT traversal or relays. Those are what the tailnet is for.

## What the crates provide

The vendored, patched copies of tailscale-rs live in `deps/tailscale-rs`
(`tailscale`, `ts_runtime`, `ts_control`, `ts_netstack_smoltcp`, `ts_derp`,
`ts_netmon`; see `PATCHES.txt`). Everything else (`ts_tunnel`,
`ts_dataplane`, `ts_packetfilter`, `ts_keys`, ...) comes unmodified from
crates.io.

### `ts_tunnel`: the WireGuard protocol

- Standard WireGuard: Noise_IKpsk2 with the WireGuard prologue
  (`handshake.rs`, `PROLOGUE`). The official Tailscale clients talk to the
  tailnet node through this same code, so its handshake and transport
  messages already interoperate with the reference implementations.
- Sans-I/O: an `Endpoint` takes packets in and hands back what to send where.
  It opens no sockets and has no timers of its own; the caller asks for the
  next event and dispatches it.
- Each peer has a pre-shared key: `PeerConfig::new(id, public_key, psk)`
  (`config.rs`). An all-zero key is WireGuard's "no pre-shared key".
- It implements mac1/mac2, cookie replies, replay protection, handshake
  retries (every 5 s, giving up after 90 s), the passive keepalive (10 s),
  and session lifetimes (fresh for 120 s, dead after 180 s).
- It queues at most 32 packets per peer while a handshake is in progress.
- `Endpoint::upsert_peer` panics with "nodekey collision" when a second peer
  has the same public key (`endpoint.rs`). The importer must reject a
  configuration that lists one public key twice.
- Its README says it is not a complete WireGuard. It leaves three things to
  the caller: cryptokey routing (choosing a peer by destination address and
  checking the source address of what arrives), padding, and tracking each
  peer's underlay address (roaming).

### `ts_dataplane`: routing and filtering around the tunnel

`ts_dataplane::DataPlane` supplies two of the three missing pieces, and its
fields are public, so it can be filled without `ts_runtime`:

| Field | What it does |
| --- | --- |
| `wireguard` | The `ts_tunnel::Endpoint`. |
| `or_out` | Outbound routing: destination IP to peer, i.e. `AllowedIPs`. |
| `src_filter_in` | Inbound source check: a packet from a peer must come from one of its `AllowedIPs`. |
| `or_in` | Inbound delivery to the overlay (the netstack). |
| `ur_out` | Underlay routing: peer to a UDP transport and remote address. |
| `packet_filter` | A stateless filter on protocol, source, destination and destination port. It starts as `DropAllFilter`. |

- `process_outbound` pads packets to 16 bytes, routes them by destination,
  encrypts them and picks the underlay. `process_inbound` decrypts, strips
  the padding, checks the source address and applies the packet filter.
  `next_event` and `process_events` drive the timers.
- The `async_tokio` module is a ready driver. It provides
  `new_underlay_transport`, `new_overlay_transport`, `send_underlay`, `run`,
  `step` and `inner`.
- The "peer GC" in `process_events` only prunes the activity map used for
  path discovery. It does not remove WireGuard peers.
- The packet filter must be replaced: an allow-all filter for configured
  peers is enough, since the netstack only answers connections it opened.
- What is left for us is roaming. A client does not need it, with one
  exception: when `Endpoint` is a host name whose address changes, the
  engine has to resolve it again, for example when handshakes keep failing.

### `ts_netstack_smoltcp`: the user-space TCP/IP stack

- It does not depend on any other `ts_*` crate.
- `piped(Config)` returns a netstack and the pipe its IP packets go in and
  out of. `Config::default()` uses an MTU of 1280.
- `set_ips` gives it the interface addresses (`Address` from the
  configuration). The `CreateSocket` trait has `tcp_connect`, `tcp_listen`
  and `udp_bind`.
- `tailscale::netstack::TcpStream` is a re-export of
  `ts_netstack_smoltcp::netsock::TcpStream`. The tailnet engine's `bridge`
  (`thinkterm-tailnet-engine/src/node.rs`) can therefore carry a WireGuard
  connection unchanged.

### Why not `ts_runtime`

`ts_runtime` is the actor system that connects the control client, the
data plane and the netstack. It cannot carry a plain WireGuard peer without
patches:

- `route_updater.rs` (`build_peer_routes`) routes a peer by its best direct
  path found by disco, else through DERP, else not at all. A peer that only
  has a static `Endpoint` gets no route.
- `dataplane.rs` adds every peer with an all-zero pre-shared key.
- The peers come from `ts_control` updates; there is no way to hand it a
  static list.

Patching the control runner, the route updater, the pre-shared key and the
`Device` constructor would add four more patches to carry through
`merge-upstream.sh`. A small engine of our own, on `ts_dataplane`, needs none.

### If `ts_tunnel` falls short

Another user-space WireGuard crate (for example boringtun) could replace
`ts_tunnel` if it fails to interoperate. That costs a second WireGuard and
crypto stack in the binaries, and the routing, filtering and netstack glue
is still needed. This was not evaluated in code; it is a fallback only.

## The proposed engine

A crate of its own (for example `thinkterm-wireguard-engine`) beside
`thinkterm-tailnet-engine`:

```text
SSH / dial ──> netstack (ts_netstack_smoltcp) ──IP packets──> DataPlane
                                                               │ or_out: AllowedIPs → peer
                                                               │ wireguard: encrypt
                                                               │ ur_out: peer → UDP + Endpoint
                                                               ▼
                                                        tokio UdpSocket ──> server
```

1. Parse and validate the configuration (see below).
2. Create a `DataPlane` with the interface's private key. For each peer:
   `wireguard.upsert_peer(PeerConfig::new(id, key, psk))`, its `AllowedIPs`
   into `or_out` and `src_filter_in`, and its `Endpoint` into `ur_out` as a
   UDP destination. Set an allow-all packet filter.
3. Create the netstack with `piped`, set its addresses, and attach it as the
   overlay transport. Bind one UDP socket as the underlay transport.
4. Run the driver. Resolve `Endpoint` host names when starting, and again
   when handshakes keep failing.
5. `PersistentKeepalive`: every N seconds, hand an empty packet straight to
   `wireguard.send` and route the result through `ur_out`. An empty packet is
   WireGuard's keepalive, and sending one starts a handshake when there is
   no session. It cannot go through `or_out`, which routes by the IP header
   that an empty packet does not have.
6. Expose `tcp_connect(addr)` returning the netstack's `TcpStream`, and a
   status: last handshake, bytes in and out, current endpoint address.

## The configuration file

| Key | First version |
| --- | --- |
| `PrivateKey` | Required. |
| `Address` | Required; IPv4 and IPv6. |
| `DNS` | Not used: the workspace has no DNS client library. Hosts are given as addresses, or resolved by the system and then matched against `AllowedIPs`. |
| `MTU` | Used for the netstack, default 1280. |
| `ListenPort` | Ignored; a random local port is used. |
| `PublicKey`, `Endpoint`, `AllowedIPs`, `PresharedKey`, `PersistentKeepalive` | Used. |
| `PreUp`, `PostUp`, `PreDown`, `PostDown`, `Table`, `FwMark`, `SaveConfig` | Never run or applied. The importer says so. |
| Extensions of modified protocols (for example AmneziaWG's `Jc`, `Jmin`, `S1`, `H1` ...) | Rejected: the server would not understand a standard handshake. |

The importer also rejects:

- two `[Peer]` sections with the same `PublicKey` (this would panic
  `ts_tunnel`);
- a peer without an `Endpoint` (we cannot accept incoming handshakes, so it
  could never be reached);
- keys that are not 32 bytes of base64.

## Where it plugs in

### Desktop

- `wezterm-ssh/src/through.rs` picks the network per host with the SSH
  option `thinkterm_network`: `system` (the default) or `tailnet`. A host
  that selects a network never falls back to the system's. WireGuard would
  become a third value, for example `thinkterm_network = wireguard`, with the
  same rule that `BindAddress` and `ProxyCommand` cannot be combined with it.
- There is one global dialer today (`set_dialer`, registered by
  `thinkterm_tailnet::use_for_ssh` and `use_for_ssh_in_server`). It needs a
  second slot, or a network argument.
- The tailnet's node runs in the holder process and is reached through its
  proxy socket (`thinkterm-tailnet/src/holder.rs`, `proxy.rs`). The keeper in
  the mux server (`keeper.rs`) restarts it. Both only run while the tailnet
  is enabled. The holder would host the WireGuard engine too, and both would
  run while either network is on.
- A command line, `thinkterm wireguard import|status|off`, and a settings
  page to import a file and show the handshake state. Strings in all six
  languages.

### Android and iOS

- `thinkterm-mobile/src/tailnet.rs` `dial(host, port)` is asked first for
  every host, from both the jump host and the target in `ssh.rs`. An address
  outside the tailnet's ranges (100.64.0.0/10, fd7a:115c:a1e0::/48) goes to
  the system network straight away.
- The WireGuard engine would be asked next: an address inside a peer's
  `AllowedIPs` goes through the tunnel. A configuration whose `AllowedIPs`
  is everything (0.0.0.0/0, ::/0) would only be used by hosts that select it
  explicitly, or it would capture every host.
- Phones pick the network by address; desktops pick it per host.

### Beside the tailnet

Both can run at once. Tailnet addresses and names are matched first, then
WireGuard's `AllowedIPs`, then the system network.

## Importing and storing the configuration

Import:

- A `.conf` file: the file picker on each platform, or the command line on
  the desktop.
- Pasted text.
- A QR code, which can wait until after the first version:
  - iOS needs a camera usage description. `ios/project.yml` only has the
    local network and Face ID ones. AVFoundation or VisionKit can read QR
    codes without a new dependency.
  - Android has no camera permission and no camera library. It would need
    CameraX and ZXing (open source; ML Kit would bring in Google Play
    services). Roughly 1 to 2 MB more, not measured.

Storage, as the tailnet's credentials are stored today:

- Desktop: `thinkterm-core/src/secret.rs`, AES-256-GCM with a key file
  beside the data (`secret.key` in the credential directory, mode 0600). It
  protects a copied settings file. It does not protect against someone who
  can read the whole home directory.
- Android: `EncryptedSharedPreferences` with a Keystore master key
  (`HostStore.kt`).
- iOS: the Keychain, `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`
  (`Models.swift`).

A configuration holds a private key and must never be synced between
devices: see the next section.

## Limits to tell users

- The server has to be reachable directly: a public address or a forwarded
  UDP port. Nothing punches through NAT.
- A network that blocks UDP blocks the tunnel. There is no relay to fall back
  to; the tailnet can relay over port 443 and WireGuard cannot.
- Some networks detect and slow or block WireGuard, especially across
  borders. Modified protocols are not supported.
- One configuration on two devices at once does not work. WireGuard knows a
  peer by its key, so the device that connected last takes the tunnel from
  the other. This includes the official WireGuard app on the same phone with
  the same configuration. While that app's VPN is on, ThinkTerm does not need
  its own tunnel anyway.
- No machine names: hosts are addresses, since `DNS` is not used.
- Scripts in the file (`PostUp` and the like) are never run.
- A configuration that routes everything is only used by hosts that select
  it.

## Making networks plugins

The question was whether a plugin interface for "network channels" could
carry both the tailnet and WireGuard.

- The plugin host (`thinkterm-plugin-server`) talks to clients in frames: a
  little-endian `u32` length and JSON (`thinkterm-plugin-channel`). A network
  channel needs a byte stream per connection, which the protocol does not
  have.
- The cheapest desktop form is a plugin that declares a command to run per
  connection with the stream on its standard input and output, as
  `ProxyCommand` does: about 3 to 5 days. `ProxyCommand` itself already works
  today, so an external user-space WireGuard tool can be used without any of
  this.
- Moving the built-in tailnet into a plugin is not recommended. It is tied
  into the mux (keeper), the holder process, the command line, serving ports
  and the settings pages. Phones have no plugin runtime, so the phones would
  lose the tailnet.
- Phones cannot load plugins as native code: the App Store rules out
  downloaded executable code, and Android 10 and later do not execute files
  an app wrote. The only route is an embedded WebAssembly interpreter (iOS
  allows no JIT), which would make the crypto much slower and take months.

## Testing

Only in a sandbox, never against a live mux or the live tailnet.

- On a Linux 5.15 development machine, an unprivileged
  `unshare -U -r -n` namespace can create a kernel WireGuard device, give it
  an address and bring it up. That gives a reference server to test against
  without root.
- Setting its keys needs `wg` from wireguard-tools, or about 100 lines of
  netlink code.
- Cases: handshake with and without a pre-shared key; the rekey after two
  minutes; `PersistentKeepalive`; the server restarting; the server's
  address changing; IPv6; MTU 1280 against the usual 1420; a cookie reply
  from a server under load; a configuration with a duplicate key.

## Plan and effort

1. Engine and sandbox interoperability tests: 3 to 4 days.
2. Desktop: holder and keeper, the per-host network option, the command
   line, the settings page, six languages: 4 to 5 days.
3. Phones: the engine in `thinkterm-mobile`, settings on both platforms,
   import, QR, key storage: 5 to 7 days.
4. Documentation and reviews: about 2 days.

About three weeks in total.

## Not yet verified

These need a running test:

- Interoperability with kernel WireGuard, including pre-shared keys and the
  rekey.
- The cookie path when the server is under load.
- The iOS build. iOS sources can only be syntax-checked on Linux.

## Risks

- `ts_tunnel` calls itself an incomplete WireGuard. `ts_dataplane` covers
  routing and padding; roaming is left to us (re-resolving `Endpoint`).
- Blocked UDP or deep packet inspection leaves no way through.
- `ts_dataplane` and `ts_tunnel` are 0.x crates that are not vendored. An
  upgrade of tailscale-rs can change their API under the engine.

## Side finding

The tailnet's inbound packet filter (`ts_dataplane`, `ts_packetfilter`) is
stateless: it looks at protocol, addresses and destination port only. Under a
restrictive tailnet ACL, replies to connections ThinkTerm itself opened might
be dropped. This is not verified and is not about WireGuard, but it would
affect the tailnet today.

## Open questions

- One WireGuard configuration, or several (and how a host names the one it
  uses)?
- Generate a key pair on the device and show the public key, instead of
  importing a private key from elsewhere?
- Should `DNS` be supported later, which means adding a DNS client?
