# d3home

A command line for the smart devices in your home. The first of them is a
Polaris PWK 1725CGLD kettle.

```
$ d3home kettle start 80
heating to 80 °C

$ d3home kettle status

  kettle  PWK 1725CGLD  [syncleo]   ● custom

  74 °C  ●●●●●●●●●●●●●●●●●●·······◉  80 °C

  Current temperature  74 °C
  Target temperature   80 °C
  Child lock           no
  Error                no
```

The tool talks to the kettle directly over the local network, in the device's
own UDP protocol. No cloud, no vendor account, no internet: unplug the router's
uplink and it keeps working. **It never changes the device's configuration**, so
the vendor's Polaris IQ Home app keeps working exactly as before — we are simply
another client on the same network, and the kettle never learns otherwise.

## Status

Working and in daily use, but young, and honest about its limits:

- **One device.** The `syncleo` driver covers Polaris IQ Home kettles. It has
  been exercised against a **PWK 1725CGLD, firmware 2.27.0, MCU 1.1.4**. Other
  models in the range speak the same protocol and may well work; nobody has
  tried.
- **Linux.** It uses `termios`, `ioctl(TIOCGWINSZ)` and `if_nametoindex`, so it
  is Unix-shaped; only Linux has been tested.
- **The same network as the device.** There is no remote access, by design.
- The protocol was reverse-engineered. It is pinned by golden vectors and
  verified against real hardware, but it is not a vendor-supported interface and
  a firmware update could change it.

## Installing

Rust 1.85 or newer (the crates use edition 2024).

```
git clone <this repository>
cd d3-home
cargo install --path crates/d3home
```

That puts `d3home` in `~/.cargo/bin`. To build without installing:

```
cargo build --release        # binary at target/release/d3home
```

## Quick start

```
d3home add                   # finds the kettle, asks for a name and a token
d3home kettle status
d3home kettle start 80
```

The token comes from the vendor app — see [Registering a device](#registering-a-device).
If nothing is found, the usual culprit is a firewall dropping mDNS; see
[When mDNS does not work](#when-mdns-does-not-work).

## Configuration

`~/.config/d3home/devices.toml`, mode `0600`. It holds the device token — the
key to the kettle — so the file belongs neither in a repository nor in a backup
somebody else can read. If it does end up readable by more than its owner
(restored from a backup, copied with `cp` and no `-p`), `d3home` notices and
warns on stderr the next time it loads the config. It still works: the warning
is a nudge, not a refusal.

```toml
[[devices]]
name    = "kettle"
aliases = ["k", "чайник"]
driver  = "syncleo"
model   = "PWK 1725CGLD"
mac     = "deadbeefdead"
token   = "..."

# filled in automatically after the first successful discovery
[devices.cached]
address    = "192.168.1.42"
port       = 41122
public_key = "..."
# interface = "enp8s0"   # only needed when address is a link-local IPv6 (fe80::/10)
```

The kettle may advertise itself over IPv4 or as a link-local IPv6 address of the
`fe80::…` form. You cannot connect to one of those without naming an interface,
so the cache carries the interface name too. The name, not the index: interface
indices are reassigned across a reboot, names are not.

### Registering a device

The config creates itself on the first `add`. You never have to hand-write TOML.

In the Polaris IQ Home app, share the device. You get a link of this shape:

```
https://l.polaris-iot.com/device-share/polaris/57/deadbeefdead?token=deadbeefdeadbeefdeadbeefdeadbeef&name=PWK%201725CGLD
```

From there, three ways in — whichever suits the moment:

```
d3home add "<link>" --name kettle    # from the link: mac and token come out of it
d3home add                           # in a terminal: lists what it found on the
                                     # network, asks for a name and a token
d3home add --name kettle --mac deadbeefdead --token <token>
```

Interactive mode only engages when there is somebody there to answer: in a pipe
or a script it asks nothing and instead says precisely what it is missing. The
token is read **without echoing** — it reaches neither the screen nor your shell
history, unlike the flag form.

Case and separators (`:`, `-`) in `mac` do not matter. `d3home` lowercases it and
strips them when loading the config, so whatever your router's DHCP table shows
(usually uppercase, colon-separated) can be pasted as it is.

### Aliases

The first word after `d3home` is a device name, or any alias you gave it — not a
hardcoded type. So `d3home k start 80` and `d3home чайник start 80` are the same
command; aliases are not restricted to ASCII.

```
d3home alias add k kettle
d3home alias rm k
d3home alias                  # list them
```

An alias that collides with a built-in command (`discover`, `devices`, `alias`,
`help`), with another device's name, or with another device's alias is rejected
when the config loads, with an error naming the collision — rather than resolved
silently and arbitrarily. The same goes for an empty alias, and for one starting
with `-`: no argument parser can read that as a word, so such an alias could
never be typed.

A colon is allowed, which makes grouping by room a matter of naming:
`kitchen:kettle`, `bath:heater`. It works identically everywhere — as the command
word, behind `--device`, and in completion.

## Commands

```
d3home kettle status          # mode, current and target temperature, error flag
d3home kettle start           # boil to 100 (synonym: on)
d3home kettle start 80        # heat to 80
d3home kettle set 80          # set the target without starting
d3home kettle off             # stop (synonym: stop)
d3home kettle watch           # what is happening, until q or Ctrl-C; reconnects itself
d3home kettle trace           # the whole event stream with protocol codes

d3home add                    # register a device
d3home completions fish       # print a completion script
d3home discover               # what is visible on the network
d3home devices                # what is configured, with model, driver and MAC
```

Global flags: `--device <name>`, `--json`, `--config <path>`, `--help`,
`--version`.

They are accepted **anywhere on the command line** — before the device word,
after the action, after the action's own arguments. `d3home kettle status --json`
behaves exactly like `d3home --json kettle status`. If a device name or alias
begins with a dash, separate it with a double dash: `d3home -- --strange status`.

`start`, `set` and `off` report what they and the kettle actually agreed on — and
not on the strength of a delivery receipt. **An acknowledgement means the frame
arrived, not that the device agreed to act on it.** A kettle with no water in it,
or one lifted off its base, acknowledges perfectly well and then does nothing. So
the state is read back after sending, and the message describes what really
happened. If the kettle did not start, you get exit code 6 and an explanation
rather than a cheerful "heating to 100 °C".

### `set` versus `start`

`start 80` begins heating to 80. `set 80` only records the target: the kettle
stays off, and if it is already heating the target changes underneath it.

### `watch` and a kettle lifted off its base

A kettle spends plenty of its life off its base — lifted, filled, put back. When
it is lifted it loses power **completely**: this is not a dropped connection but a
device switching off, and the whole session goes with it — keys, counters, the
fact that we were ever authorised. Resuming is impossible in principle.

So `watch` does not exit when the connection dies. It runs the full cycle again:
finds the kettle (the address may have changed on a new DHCP lease), performs the
handshake with the same token from the config, opens a new session. Attempts back
off — briefly at first, then twice as long each time, capped at five seconds, so
it does not hammer the network while the kettle sits on a table with tea in it.
The moment it returns is marked in the event stream, because the device replays
its entire state on every connection and an unmarked repeat block would read as a
glitch rather than as a new session.

`watch` waits only on connectivity: a timeout, silence, an unacknowledged
command — everything indistinguishable from "the kettle has no power right now".
A rejected token or a broken config are not cured by waiting, and still return
their exit code immediately, exactly as before.

`watch` expects its output to be piped somewhere (`| jq`, a log, a notifier). If
the reading end closes first (`| head -1`, a notifier that died), `watch` exits
quietly with code 0 rather than crashing.

Single-shot commands (`status`, `start`, `off`) are unchanged: they wait what they
waited before and fail if the kettle is absent. Waiting forever in a command whose
job is to run and exit would be worse, not better.

## How the kettle is found

On first use, an mDNS search for `_syncleo._udp.local`. The address, port and
public key that come back are written to `[devices.cached]`, and later commands go
straight to the known address — no second of searching, and no dependence on
whether mDNS works on that network at all.

When the cached address goes quiet — the router handed out a different IP, or the
kettle rotated its keys — the tool falls back to discovery on its own and updates
the cache. A search for one known device stops as soon as that device answers
rather than running out its window: against the real kettle that is the difference
between 4.4 and 8.8 seconds. It does linger briefly after the answer, because a
device can resolve on several interfaces at once and a global address is worth
more than a link-local one.

The same fallback covers an address that cannot be turned into a socket at all — a
cached link-local IPv6 whose interface has since been renamed, disabled or
replaced (which is exactly why the index is not the thing stored). That is a stale
cache, not an internal error.

A kettle that answers but rejects the handshake is treated differently: no
fallback. A wrong token is not cured by searching again, and hiding that behind a
timeout would be a lie.

### When mDNS does not work

The usual cause on Linux is a firewall dropping inbound UDP 5353:

```
sudo firewall-cmd --add-service=mdns              # until reboot
sudo firewall-cmd --permanent --add-service=mdns  # for good
```

If mDNS is unavailable on the network as a matter of principle,
`[devices.cached]` can be filled in by hand — printing the public key is exactly
what `d3home discover` is for.

### `watch` versus `trace`

The kettle repeats the same values endlessly. `watch` shows not the reports but
the **changes** — the moment something happened:

```
17:22:52  connected — 45 °C, idle
17:23:04  heating to 100 °C
17:26:31  reached 98 °C, switched off
```

The bar sits **above** the log, separated by a blank line. The bar, that blank
line and the last fifteen log lines are one block, redrawn in place: the cursor
moves back to its top, everything below is wiped, and it is printed again. No
terminal setting is touched, so there is nothing to restore afterwards.

```
  71 °C  ●●●●●●●●●●●●●●●●●·······◉  100 °C

17:22:52  connected — 45 °C, idle
17:23:04  heating to 100 °C
```

Diagnostics, hardware versions, access control and the unidentified command codes
stay out of the log: watching a kettle boil and taking a protocol apart are
different jobs.

`trace` is the second job. Everything, with millisecond timestamps and the
protocol code beside the decoded meaning:

```
17:22:31.660  145  diagnostic: udps=61062937 IDLE=49428231 ppT=1201672 tiT=466229
17:22:31.692    1  mode: off
17:22:31.719    2  target temperature: 0 °C
```

On one and the same burst from the device, `watch` prints one line and `trace`
prints thirteen. `trace` is what the two unidentified command codes will
eventually be identified from.

Under `--json` the two are identical — the whole event stream, unfiltered. That is
a stream for a program, not for eyes.

## Completion

```
d3home completions fish > ~/.config/fish/completions/d3home.fish
d3home completions bash > ~/.local/share/bash-completion/completions/d3home
d3home completions zsh  > ~/.zsh/completions/_d3home    # directory must be on fpath
```

It completes not only the built-in commands but **your own device names and
aliases** — read from the config at the moment you press Tab, not baked into the
script. So `d3home alias add k kettle` makes `k` completable immediately, with
nothing to regenerate. Actions are suggested per the device's driver, so a new
kind of device brings its own.

If the config is missing or broken, completion still offers the built-ins: a
broken config should not feel like a broken shell.

## Output

On a terminal the output is laid out: `status` prints a block with the values
picked out, and `watch` keeps a live line that appears the moment it starts,
updates after **every** event, and never disappears between readings. During a
heat that line is a progress bar rather than sixty near-identical lines — the
kettle sends one per degree.

The bar's scale is **absolute, 0–100 °C**: one cell is four degrees, and it does
not rescale when the target changes. So 40 °C looks the same whether the kettle is
heading for 60 or for boiling, and the eye never has to recalibrate.

The target is marked with a ring, `◉`. Colour carries the state: heating is green,
idle is white. An idle kettle that still remembers a target is therefore never
mistaken for one in progress.

```
  71 °C  ●●●●●●●●●●●●●●●●●●·······◉  100 °C
```

The same bar appears in `status`, under the heading. That heading names the
device, its model and its driver, so a registry holding several devices stays
legible.

In a pipe the block is not drawn at all: there every line matters and nothing can
be overwritten, so the log runs straight down like any other log.

`watch` exits on **`q`** or Ctrl-C. Reading a single keypress puts the terminal
into character-at-a-time mode for the duration — and restores it on every exit
path, including a signal, where a handler puts the settings back before the
process dies. Ctrl-C keeps behaving exactly as it always did; the tool does not
break the habit.

In a pipe, in a file and under `--json` all of this switches itself off. Escape
sequences in the middle of data are corruption, not decoration. `NO_COLOR` and
`TERM=dumb` are honoured.

```
d3home kettle status        # a block, in colour
d3home kettle status | cat  # plain key: value lines
NO_COLOR=1 d3home kettle status
```

## Listing devices

```
$ d3home devices

  kettle  (k, чайник)
  model     PWK 1725CGLD
  driver    syncleo
  mac       de:ad:be:ef:de:ad
  endpoint  192.168.1.42:41122
```

The identifier shown is the **MAC, not a slice of the token**. The MAC is already
broadcast over mDNS, so it is in no sense secret, and it is what the router's
admin page and the vendor's share link both display — which is what ties a line in
this list to an object on a worktop. Printing part of a secret is a habit worth
not forming.

## Exit codes

| Code | What happened |
|------|---------------|
| 0 | success |
| 1 | internal error |
| 2 | bad arguments or bad config |
| 3 | device not found on the network |
| 4 | handshake rejected — wrong token |
| 5 | timeout, device not answering |
| 6 | device refused, or reported an error of its own |

Under `--json`, **failures are machine-readable too**. The shape changes, the
stream does not: failures go to stderr so that stdout carries only the answer and
a redirect to a file is never polluted.

```
$ d3home --json teapot status
{"error":{"exit_code":2,"kind":"usage","message":"no device matches 'teapot'"}}
```

`kind` is what a script may rely on: the wording of a message can be improved at
any time, the slug and the exit code cannot. The values are `usage`, `not_found`,
`bad_token`, `timeout`, `device`, `internal`.

The promise covers the whole stream: under `--json` **every** line on stderr is
JSON, warnings included. A promise kept only for errors is worse than none at
all — a parser would trip on precisely the warning that was overlooked.

A command counts as successful only once the kettle has confirmed it. When there
was no confirmation the exit code says so; success is never printed for a command
the device did not take.

## How it is put together

Two crates:

- **`syncleo`** — the protocol. Inside it, the pure part (`codec`, `session`: no
  sockets, no system clocks, time arrives as an argument) is kept strictly apart
  from the part that does I/O (`transport`, `client`, `discovery`). That is why
  the session state machine is tested on a virtual clock — deterministically and
  instantly.
- **`d3home`** — the CLI: the device registry, argument parsing, output, exit
  codes.

The protocol was rewritten from scratch against the Python implementation at
[gch1p/polaris_pwk_1725cgld](https://github.com/gch1p/polaris_pwk_1725cgld)
(BSD-3c), which is known to drive the real device. The cryptography is pinned by
golden vectors captured from that implementation: key derivation, frame
encryption, the handshake, acknowledgements and pings are all compared byte for
byte.

The crate carries a kettle simulator (feature `simulator`, off by default, never
compiled into the shipped binary). It implements the device side of the protocol,
which buys end-to-end tests over real UDP and makes development possible when the
kettle is not at hand.

```
cargo test --workspace
```

One test is marked `#[ignore]` — it needs a real network with working multicast.

## The protocol

[`docs/protocol.md`](docs/protocol.md) documents the wire protocol as the device
actually speaks it: discovery, the key exchange and its three byte reversals,
framing, encryption, the command table, and the places where a real kettle
disagrees with the reference implementation.

## Not done yet

- A background daemon and "it boiled" notifications. The session layer already
  hands events outward as a stream and `watch` merely prints them, so a notifier
  needs the same stream and a long-lived process — the protocol does not stand in
  the way.
- Roborock vacuums and Alice BT remotes. The driver and the CLI are separated for
  exactly this.
- Working from outside the home network.

## Contributing

Issues and patches are welcome, particularly:

- **Another Polaris model.** If `d3home discover` sees your device and `status`
  reads it, say so — that is the cheapest way to widen the tested range. If it
  does not, `d3home <device> trace` is the output worth attaching.
- **The two unidentified command codes**, 50 and 66. See
  [`docs/protocol.md`](docs/protocol.md) for what has been ruled out already.

`cargo test --workspace` and `cargo clippy --all-targets --workspace -- -D warnings`
should both be clean. New behaviour comes with a test; the simulator in
`crates/syncleo/src/simulator.rs` means you do not need a kettle to write one.

## Credit

The protocol was worked out from
[gch1p/polaris_pwk_1725cgld](https://github.com/gch1p/polaris_pwk_1725cgld) by
Evgeny Zinoviev (BSD-3-Clause) — a Python implementation known to drive the real
device. This is an independent implementation in Rust, but without that work it
would have been a great deal of packet-staring.

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)),
at your option.

Unless you state otherwise, any contribution you submit for inclusion shall be
dual-licensed as above, with no additional terms.
