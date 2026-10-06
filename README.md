# nrxp-smoltcp

[![License](https://img.shields.io/badge/license-0BSD-blue.svg)](LICENSE-0BSD.txt)
[![Rust](https://img.shields.io/badge/rust-1.91%2B-orange.svg)](https://www.rust-lang.org)
[![based on smoltcp](https://img.shields.io/badge/based%20on-smoltcp-informational.svg)](https://github.com/smoltcp-rs/smoltcp)

_nrxp-smoltcp_ is a fork of [smoltcp](https://github.com/smoltcp-rs/smoltcp), the standalone,
event-driven TCP/IP stack originally built for bare-metal, real-time systems. This fork keeps
the upstream protocol implementation and its no-heap, no-macro-magic design philosophy, and adds
a set of changes aimed at running smoltcp's TCP socket as the transport core of a **userspace
tunnel/VPN client**: a real BBR congestion controller, buffers that resize themselves to the
path's bandwidth-delay product at runtime, active queue management for latency-sensitive
UDP/ICMP traffic, and a bounded async-friendly virtual device for bridging a tokio-style I/O
loop into smoltcp's synchronous `poll()`.

[docs (upstream)]: https://docs.rs/smoltcp/

## Why this fork exists

Upstream smoltcp is tuned for embedded targets: fixed-size buffers sized at compile time, and a
choice between Cubic and Reno for congestion control. Running it as the TCP/IP core of a
userspace tunnel client surfaces different problems — the effective RTT and bandwidth of a
tunneled path vary a lot and aren't known ahead of time, so fixed buffer sizes are either wasteful
or too small, and Cubic/Reno's loss-based congestion control reacts poorly to the latency spikes
typical of tunneled links. This fork addresses that.

## What's different from upstream

### BBR congestion control (`socket-tcp-bbr` feature)

A from-scratch BBRv1 implementation in [`src/socket/tcp/congestion/bbr.rs`](src/socket/tcp/congestion/bbr.rs):
the full Startup → Drain → ProbeBW → ProbeRTT state machine, delivery-rate sampling per ACK,
pacing-gain cycling, and `Minmax` sliding-window filters for bottleneck bandwidth and RTprop.

It went through several rounds of correctness fixes after testing against real traffic traces,
including:

* the RTprop min-filter was using a 1-round window (i.e. effectively no filtering) — widened to
  a proper 10-round sliding window per the BBR spec;
* `ProbeRTT` duration was computed as `RTT + 200ms` instead of the spec's fixed `200ms`;
* round-trip counting (`round_count`) wasn't advancing correctly, which broke the Startup
  bandwidth-growth check used to decide when to exit Startup and enter Drain.

The `Controller` trait (`src/socket/tcp/congestion.rs`) was extended with `on_ack_with_rate`,
`pacing_rate`, `get_estimated_bdp`, and `pre_transmit`/`post_transmit` hooks so a controller can
act on delivery-rate samples instead of just cumulative ACK counts — this is what BBR needs and
what plain Reno/Cubic didn't require.

### Dynamic, BDP-sized socket buffers

`DynamicSocketBuffer` (in [`src/socket/tcp.rs`](src/socket/tcp.rs)) replaces the fixed-capacity
ring buffer for TCP sockets. Buffers can grow or shrink at runtime via `Socket::resize`, driven
by the congestion controller's `estimated_bdp()` — a socket can start small and grow toward the
path's real bandwidth-delay product as BBR characterizes it, instead of the buffer size being a
compile-time guess.

### Active Queue Management (AQM) for real-time traffic

`PacketBuffer` ([`src/storage/packet_buffer.rs`](src/storage/packet_buffer.rs)) now timestamps
each enqueued packet and, on dequeue, can drop entries older than a configurable age instead of
delivering stale data. It's wired into UDP, ICMP, and raw sockets — useful for tunneled real-time
traffic (VoIP, games, control channels) where a late packet is worse than a dropped one.

### RTT/timing API for tunnel-aware sockets

`Socket::smoothed_rtt()`, `Socket::latest_rtt()`, and `Socket::set_tunnel_rtt()` expose the TCP
RTT estimator and let an outer layer feed in an independently measured RTT — needed when
smoltcp's TCP socket runs *inside* an encrypted tunnel and the RTT that matters for congestion
control is measured one layer down, not by smoltcp's own ACK clock.

### `phy::ChannelDevice` — a virtual device for async bridging

[`src/phy/channel_device.rs`](src/phy/channel_device.rs) is a `Device` implementation backed by
two bounded queues, meant to bridge an async I/O source (e.g. a TUN file descriptor read from a
tokio task) into smoltcp's synchronous `Interface::poll()` loop. Backpressure is explicit: RX
pushes fail closed when the queue is full instead of growing without bound, and TX is drained by
the caller after every poll.

### Status

This fork is used to run a userspace VPN/tunnel client's TCP/IP stack and is not published to
crates.io. It tracks upstream `smoltcp` with the changes above layered on top; the base protocol
implementation (IPv4/IPv6/ARP/ICMPv4/ICMPv6/6LoWPAN/DHCP/DNS) is inherited from upstream and
documented below, unchanged.

---

## Features

_smoltcp_ is missing many widely deployed features, usually because no one implemented them yet.
To set expectations right, both implemented and omitted features are listed.

### Media layer

There are 3 supported mediums.

* Ethernet
  * Regular Ethernet II frames are supported.
  * Unicast, broadcast and multicast packets are supported.
  * ARP packets (including gratuitous requests and replies) are supported.
  * ARP requests are sent at a rate not exceeding one per second.
  * Cached ARP entries expire after one minute.
  * 802.3 frames and 802.1Q are **not** supported.
  * Jumbo frames are **not** supported.
* IP
  * Unicast, broadcast and multicast packets are supported.
* IEEE 802.15.4
  * Only support for data frames.

### IP layer

#### IPv4

  * IPv4 header checksum is generated and validated.
  * IPv4 time-to-live value is configurable per socket, set to 64 by default.
  * IPv4 default gateway is supported.
  * Routing outgoing IPv4 packets is supported, through a default gateway or a CIDR route table.
  * IPv4 fragmentation and reassembly is supported.
  * IPv4 options are **not** supported and are silently ignored.

#### IPv6

  * IPv6 hop-limit value is configurable per socket, set to 64 by default.
  * Routing outgoing IPv6 packets is supported, through a default gateway or a CIDR route table.
  * IPv6 hop-by-hop header is supported.
  * ICMPv6 parameter problem message is generated in response to an unrecognized IPv6 next header.
  * ICMPv6 parameter problem message is **not** generated in response to an unknown IPv6
    hop-by-hop option.

#### 6LoWPAN

  * Implementation of [RFC6282](https://tools.ietf.org/rfc/rfc6282.txt).
  * Fragmentation is supported, as defined in [RFC4944](https://tools.ietf.org/rfc/rfc4944.txt).
  * UDP header compression/decompression is supported.
  * Extension header compression/decompression is supported.
  * Uncompressed IPv6 Extension Headers are **not** supported.

### IP multicast

#### IGMP

The IGMPv1 and IGMPv2 protocols are supported, and IPv4 multicast is available.

  * Membership reports are sent in response to membership queries at
    equal intervals equal to the maximum response time divided by the
    number of groups to be reported.

### ICMP layer

#### ICMPv4

The ICMPv4 protocol is supported, and ICMP sockets are available.

  * ICMPv4 header checksum is supported.
  * ICMPv4 echo replies are generated in response to echo requests by default.
  * ICMP sockets can listen to ICMPv4 Port Unreachable messages, or any ICMPv4 messages with
    a given IPv4 identifier field.
  * ICMPv4 protocol unreachable messages are **not** passed to higher layers when received.
  * ICMPv4 parameter problem messages are **not** generated.

#### ICMPv6

The ICMPv6 protocol is supported, and ICMP sockets are available.

  * ICMPv6 header checksum is supported.
  * ICMPv6 echo replies are generated in response to echo requests by default.
  * ICMPv6 protocol unreachable messages are **not** passed to higher layers when received.

#### NDISC

  * Neighbor Advertisement messages are generated in response to Neighbor Solicitations.
  * Router Advertisement messages are read, but **not** generated.
  * Router Solicitation messages are generated, but **not** read.
  * Redirected Header messages are **not** generated or read.

### UDP layer

The UDP protocol is supported over IPv4 and IPv6, and UDP sockets are available.

  * Header checksum is always generated and validated.
  * In response to a packet arriving at a port without a listening socket,
    an ICMP destination unreachable message is generated.
  * Stale queued packets can be dropped on dequeue via AQM (fork addition, see above).

### TCP layer

The TCP protocol is supported over IPv4 and IPv6, and server and client TCP sockets are available.

  * Header checksum is generated and validated.
  * Maximum segment size is negotiated.
  * Window scaling is negotiated.
  * Multiple packets are transmitted without waiting for an acknowledgement.
  * Reassembly of out-of-order segments is supported, with no more than 4 or 32 gaps in sequence space.
  * Keep-alive packets may be sent at a configurable interval.
  * Retransmission timeout starts at at an estimate of RTT, and doubles every time.
  * Time-wait timeout has a fixed interval of 10 s.
  * User timeout has a configurable interval.
  * Delayed acknowledgements are supported, with configurable delay.
  * Nagle's algorithm is implemented.
  * Selective acknowledgements are supported.
  * Silly window syndrome avoidance is **not** implemented.
  * Congestion control is optional: `CUBIC`, `Reno`, and `BBR` (fork addition) are implemented.
  * Socket buffers can be fixed-size (upstream) or dynamically resized at runtime toward the
    estimated bandwidth-delay product (fork addition, see above).
  * Timestamping is **not** supported.
  * Urgent pointer is **ignored**.
  * Probing Zero Windows is implemented.
  * Packetization Layer Path MTU Discovery [PLPMTU](https://tools.ietf.org/rfc/rfc4821.txt) is **not** implemented.

## Installation

This fork is not published to crates.io. Depend on it directly from git:

```toml
[dependencies]
smoltcp = { package = "nrxp-smoltcp", git = "https://github.com/nineAp/nrxp-smoltcp", default-features = false, features = ["log"] }
```

The package is named `nrxp-smoltcp`, but its library target keeps the name `smoltcp`.
Renaming the dependency back to `smoltcp` as above leaves existing `use smoltcp::...`
paths untouched.

Pin to a commit or tag for reproducible builds.

If you just need upstream smoltcp without the fork-specific changes, use the
[official crate](https://crates.io/crates/smoltcp) instead.

## Feature flags

### Feature `std`

The `std` feature enables use of objects and slices owned by the networking stack through a
dependency on `std::boxed::Box` and `std::vec::Vec`.

This feature is enabled by default.

### Feature `alloc`

The `alloc` feature enables use of objects owned by the networking stack through a dependency
on collections from the `alloc` crate. This only works on nightly rustc.

This feature is disabled by default.

### Feature `log`

The `log` feature enables logging of events within the networking stack through
the [log crate][log]. Normal events (e.g. buffer level or TCP state changes) are emitted with
the TRACE log level. Exceptional events (e.g. malformed packets) are emitted with
the DEBUG log level.

[log]: https://crates.io/crates/log

This feature is enabled by default.

### Feature `defmt`

The `defmt` feature enables logging of events with the [defmt crate][defmt].

[defmt]: https://crates.io/crates/defmt

This feature is disabled by default, and cannot be used at the same time as `log`.

### Feature `verbose`

The `verbose` feature enables logging of events where the logging itself may incur very high
overhead. For example, emitting a log line every time an application reads or writes as little
as 1 octet from a socket is likely to overwhelm the application logic unless a `BufReader`
or `BufWriter` is used, which are of course not available on heap-less systems.

This feature is disabled by default.

### Features `phy-raw_socket` and `phy-tuntap_interface`

Enable `smoltcp::phy::RawSocket` and `smoltcp::phy::TunTapInterface`, respectively.

These features are enabled by default.

### Features `socket-raw`, `socket-udp`, `socket-tcp`, `socket-icmp`, `socket-dhcpv4`, `socket-dns`

Enable the corresponding socket type.

These features are enabled by default.

### Features `socket-tcp-cubic`, `socket-tcp-reno`, `socket-tcp-bbr`

Enable the corresponding TCP congestion control algorithm. `CUBIC` is used by default when
enabled; `BBR` (fork addition) must be selected explicitly, either via this feature or at
run-time through `Socket::set_congestion_control()`.

### Features `proto-ipv4`, `proto-ipv6` and `proto-sixlowpan`

Enable [IPv4], [IPv6] and [6LoWPAN] respectively.

[IPv4]: https://tools.ietf.org/rfc/rfc791.txt
[IPv6]: https://tools.ietf.org/rfc/rfc8200.txt
[6LoWPAN]: https://tools.ietf.org/rfc/rfc6282.txt

## Configuration

_smoltcp_ has some configuration settings that are set at compile time, affecting sizes
and counts of buffers.

They can be set in two ways:

- Via Cargo features: enable a feature like `<name>-<value>`. `name` must be in lowercase and
use dashes instead of underscores. For example. `iface-max-addr-count-3`. Only a selection of values
is available, check `Cargo.toml` for the list.
- Via environment variables at build time: set the variable named `SMOLTCP_<value>`. For example
`SMOLTCP_IFACE_MAX_ADDR_COUNT=3 cargo build`. You can also set them in the `[env]` section of `.cargo/config.toml`.
Any value can be set, unlike with Cargo features.

Environment variables take precedence over Cargo features. If two Cargo features are enabled for the same setting
with different values, compilation fails.

### `IFACE_MAX_ADDR_COUNT`

Max amount of IP addresses that can be assigned to one interface (counting both IPv4 and IPv6 addresses). Default: 2.

### `IFACE_MAX_MULTICAST_GROUP_COUNT`

Max amount of multicast groups that can be joined by one interface. Default: 4.

### `IFACE_MAX_SIXLOWPAN_ADDRESS_CONTEXT_COUNT`

Max amount of 6LoWPAN address contexts that can be assigned to one interface. Default: 4.

### `IFACE_NEIGHBOR_CACHE_COUNT`

Amount of "IP address -> hardware address" entries the neighbor cache (also known as the "ARP cache" or the "ARP table") holds. Default: 4.

### `IFACE_MAX_ROUTE_COUNT`

Max amount of routes that can be added to one interface. Includes the default route. Includes both IPv4 and IPv6. Default: 2.

### `IFACE_MAX_PREFIX_COUNT`

Max amount of IPv6 prefixes that can be added to one interface via SLAAC.
Should be lower or equal to `IFACE_MAX_ADDR_COUNT`.

### `FRAGMENTATION_BUFFER_SIZE`

Size of the buffer used for fragmenting outgoing packets larger than the MTU. Packets larger than this setting will be dropped instead of fragmented. Default: 1500.

### `ASSEMBLER_MAX_SEGMENT_COUNT`

Maximum number of non-contiguous segments the assembler can hold. Used for both packet reassembly and TCP stream reassembly. Default: 4.

### `REASSEMBLY_BUFFER_SIZE`

Size of the buffer used for reassembling (de-fragmenting) incoming packets. If the reassembled packet is larger than this setting, it will be dropped instead of reassembled. Default: 1500.

### `REASSEMBLY_BUFFER_COUNT`

Number of reassembly buffers, i.e how many different incoming packets can be reassembled at the same time. Default: 1.

### `DNS_MAX_RESULT_COUNT`

Maximum amount of address results for a given DNS query that will be kept. For example, if this is set to 2 and the queried name has 4 `A` records, only the first 2 will be returned. Default: 1.

### `DNS_MAX_SERVER_COUNT`

Maximum amount of DNS servers that can be configured in one DNS socket. Default: 1.

### `DNS_MAX_NAME_SIZE`

Maximum length of DNS names that can be queried. Default: 255.

### IPV6_HBH_MAX_OPTIONS

The maximum amount of parsed options the IPv6 Hop-by-Hop header can hold. Default: 4.

## Hosted usage examples

_smoltcp_, being a freestanding networking stack, needs to be able to transmit and receive
raw frames. For testing purposes, we will use a regular OS, and run _smoltcp_ in
a userspace process. Only Linux is supported (right now).

On \*nix OSes, transmitting and receiving raw frames normally requires superuser privileges, but
on Linux it is possible to create a _persistent tap interface_ that can be manipulated by
a specific user:

```sh
sudo ip tuntap add name tap0 mode tap user $USER
sudo ip link set tap0 up
sudo ip addr add 192.168.69.100/24 dev tap0
sudo ip -6 addr add fe80::100/64 dev tap0
sudo ip -6 addr add fdaa::100/64 dev tap0
sudo ip -6 route add fe80::/64 dev tap0
sudo ip -6 route add fdaa::/64 dev tap0
```

It's possible to let _smoltcp_ access Internet by enabling routing for the tap interface:

```sh
sudo iptables -t nat -A POSTROUTING -s 192.168.69.0/24 -j MASQUERADE
sudo sysctl net.ipv4.ip_forward=1
sudo ip6tables -t nat -A POSTROUTING -s fdaa::/64 -j MASQUERADE
sudo sysctl -w net.ipv6.conf.all.forwarding=1

# Some distros have a default policy of DROP. This allows the traffic.
sudo iptables -A FORWARD -i tap0 -s 192.168.69.0/24 -j ACCEPT
sudo iptables -A FORWARD -o tap0 -d 192.168.69.0/24 -j ACCEPT
```

### Bridged connection

Instead of the routed connection above, you may also set up a bridged (switched)
connection. This will make smoltcp speak directly to your LAN, with real ARP, etc.
It is needed to run the DHCP example.

NOTE: In this case, the examples' IP configuration must match your LAN's!

NOTE: this ONLY works with actual wired Ethernet connections. It
will NOT work on a WiFi connection.

```sh
# Replace with your wired Ethernet interface name
ETH=enp0s20f0u1u1

sudo modprobe bridge
sudo modprobe br_netfilter

sudo sysctl -w net.bridge.bridge-nf-call-arptables=0
sudo sysctl -w net.bridge.bridge-nf-call-ip6tables=0
sudo sysctl -w net.bridge.bridge-nf-call-iptables=0

sudo ip tuntap add name tap0 mode tap user $USER
sudo brctl addbr br0
sudo brctl addif br0 tap0
sudo brctl addif br0 $ETH
sudo ip link set tap0 up
sudo ip link set $ETH up
sudo ip link set br0 up

# This connects your host system to the internet, so you can use it
# at the same time you run the examples.
sudo dhcpcd br0
```

To tear down:

```
sudo killall dhcpcd
sudo ip link set br0 down
sudo brctl delbr br0
```

### Fault injection

In order to demonstrate the response of _smoltcp_ to adverse network conditions, all examples
implement fault injection, available through command-line options:

  * The `--drop-chance` option randomly drops packets, with given probability in percents.
  * The `--corrupt-chance` option randomly mutates one octet in a packet, with given
    probability in percents.
  * The `--size-limit` option drops packets larger than specified size.
  * The `--tx-rate-limit` and `--rx-rate-limit` options set the amount of tokens for
    a token bucket rate limiter, in packets per bucket.
  * The `--shaping-interval` option sets the refill interval of a token bucket rate limiter,
    in milliseconds.

A good starting value for `--drop-chance` and `--corrupt-chance` is 15%. A good starting
value for `--?x-rate-limit` is 4 and `--shaping-interval` is 50 ms.

Note that packets dropped by the fault injector still get traced;
the  `rx: randomly dropping a packet` message indicates that the packet *above* it got dropped,
and the `tx: randomly dropping a packet` message indicates that the packet *below* it was.

### Packet dumps

All examples provide a `--pcap` option that writes a [libpcap] file containing a view of every
packet as it is seen by _smoltcp_.

[libpcap]: https://wiki.wireshark.org/Development/LibpcapFileFormat

### examples/tcpdump.rs

_examples/tcpdump.rs_ is a tiny clone of the _tcpdump_ utility.

Unlike the rest of the examples, it uses raw sockets, and so it can be used on regular interfaces,
e.g. `eth0` or `wlan0`, as well as the `tap0` interface we've created above.

Read its [source code](/examples/tcpdump.rs), then run it as:

```sh
cargo build --example tcpdump
sudo ./target/debug/examples/tcpdump eth0
```

### examples/httpclient.rs

_examples/httpclient.rs_ emulates a network host that can initiate HTTP requests.

The host is assigned the hardware address `02-00-00-00-00-02`, IPv4 address `192.168.69.1`, and IPv6 address `fdaa::1`.

Read its [source code](/examples/httpclient.rs), then run it as:

```sh
cargo run --example httpclient -- --tap tap0 ADDRESS URL
```

For example:

```sh
cargo run --example httpclient -- --tap tap0 93.184.216.34 http://example.org/
```

or:

```sh
cargo run --example httpclient -- --tap tap0 2606:2800:220:1:248:1893:25c8:1946 http://example.org/
```

It connects to the given address (not a hostname) and URL, and prints any returned response data.
The TCP socket buffers are limited to 1024 bytes to make packet traces more interesting.

### examples/ping.rs

_examples/ping.rs_ implements a minimal version of the `ping` utility using raw sockets.

The host is assigned the hardware address `02-00-00-00-00-02` and IPv4 address `192.168.69.1`.

Read its [source code](/examples/ping.rs), then run it as:

```sh
cargo run --example ping -- --tap tap0 ADDRESS
```

It sends a series of 4 ICMP ECHO\_REQUEST packets to the given address at one second intervals and
prints out a status line on each valid ECHO\_RESPONSE received.

The first ECHO\_REQUEST packet is expected to be lost since arp\_cache is empty after startup;
the ECHO\_REQUEST packet is dropped and an ARP request is sent instead.

Currently, netmasks are not implemented, and so the only address this example can reach
is the other endpoint of the tap interface, `192.168.69.100`. It cannot reach itself because
packets entering a tap interface do not loop back.

### examples/server.rs

_examples/server.rs_ emulates a network host that can respond to basic requests.

The host is assigned the hardware address `02-00-00-00-00-01` and IPv4 address `192.168.69.1`.

Read its [source code](/examples/server.rs), then run it as:

```sh
cargo run --example server -- --tap tap0
```

It responds to:

  * pings (`ping 192.168.69.1`);
  * UDP packets on port 6969 (`socat stdio udp4-connect:192.168.69.1:6969 <<<"abcdefg"`),
    where it will respond with reversed chunks of the input indefinitely;
  * TCP connections on port 6969 (`socat stdio tcp4-connect:192.168.69.1:6969`),
    where it will respond "hello" to any incoming connection and immediately close it;
  * TCP connections on port 6970 (`socat stdio tcp4-connect:192.168.69.1:6970 <<<"abcdefg"`),
    where it will respond with reversed chunks of the input indefinitely.
  * TCP connections on port 6971 (`socat stdio tcp4-connect:192.168.69.1:6971 </dev/urandom`),
    which will sink data. Also, keep-alive packets (every 1 s) and a user timeout (at 2 s)
    are enabled on this port; try to trigger them using fault injection.
  * TCP connections on port 6972 (`socat stdio tcp4-connect:192.168.69.1:6972 >/dev/null`),
    which will source data.

Except for the socket on port 6971. the buffers are only 64 bytes long, for convenience
of testing resource exhaustion conditions.

### examples/client.rs

_examples/client.rs_ emulates a network host that can initiate basic requests.

The host is assigned the hardware address `02-00-00-00-00-02` and IPv4 address `192.168.69.2`.

Read its [source code](/examples/client.rs), then run it as:

```sh
cargo run --example client -- --tap tap0 ADDRESS PORT
```

It connects to the given address (not a hostname) and port (e.g. `socat stdio tcp4-listen:1234`),
and will respond with reversed chunks of the input indefinitely.

### examples/benchmark.rs

_examples/benchmark.rs_ implements a simple throughput benchmark.

Read its [source code](/examples/benchmark.rs), then run it as:

```sh
cargo run --release --example benchmark -- --tap tap0 [reader|writer]
```

It establishes a connection to itself from a different thread and reads or writes a large amount
of data in one direction.

## Bare-metal usage examples

Examples that use no services from the host OS are necessarily less illustrative than examples
that do. Because of this, only one such example is provided.

### examples/loopback.rs

_examples/loopback.rs_ sets up _smoltcp_ to talk with itself via a loopback interface.
Although it does not require `std`, this example still requires the `alloc` feature to run, as well as `log`, `proto-ipv4` and `socket-tcp`.

Read its [source code](/examples/loopback.rs), then run it without `std`:

```sh
cargo run --example loopback --no-default-features --features="log proto-ipv4 socket-tcp alloc"
```

... or with `std` (in this case the features don't have to be explicitly listed):

```sh
cargo run --example loopback -- --pcap loopback.pcap
```

It opens a server and a client TCP socket, and transfers a chunk of data. You can examine
the packet exchange by opening `loopback.pcap` in [Wireshark].

If the `std` feature is enabled, it will print logs and packet dumps, and fault injection
is possible; otherwise, nothing at all will be displayed and no options are accepted.

[wireshark]: https://wireshark.org

### examples/loopback\_benchmark.rs

_examples/loopback_benchmark.rs_ is another simple throughput benchmark.

Read its [source code](/examples/loopback_benchmark.rs), then run it as:

```sh
cargo run --release --example loopback_benchmark
```

It establishes a connection to itself via a loopback interface and transfers a large amount
of data in one direction.

Note: Although the loopback interface can be used in bare-metal environments,
this benchmark _does_ rely on `std` to be able to measure the time cost.

## About the author / this fork

The changes in this fork — BBR, dynamic BDP-aware buffers, AQM, the RTT/tunnel API, and
`ChannelDevice` — were written to take smoltcp's TCP socket out of the embedded/bare-metal box
and run it as the transport core of a real userspace tunnel/VPN client, where path RTT and
bandwidth are unknown ahead of time and vary during a session. That meant working at the level
of a real TCP/IP stack: implementing a congestion controller against the BBR spec, finding and
fixing the filtering and state-machine bugs that made the first pass mis-time ProbeRTT and
under-count rounds, and reworking buffer sizing and packet aging around real traffic traces
rather than synthetic tests.

* GitHub: [@nineAp](https://github.com/nineAp)
* LinkedIn: [Kirill Trapeznikov](https://www.linkedin.com/in/kirill-trapeznikov-57b450374/)
* Telegram: [@ktr2102](https://t.me/ktr2102)

## License

_smoltcp_ is distributed under the terms of 0-clause BSD license.

See [LICENSE-0BSD](LICENSE-0BSD.txt) for details.

This fork is a derivative work of [smoltcp-rs/smoltcp](https://github.com/smoltcp-rs/smoltcp),
released under the same license.
