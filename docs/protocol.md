# The Syncleo protocol, as spoken by a Polaris PWK 1725CGLD

Everything here was either read out of the Python reference implementation at
[gch1p/polaris_pwk_1725cgld](https://github.com/gch1p/polaris_pwk_1725cgld)
(BSD-3-Clause) or observed on a real device. Where the two disagree, the device
wins and the disagreement is written down.

Byte-for-byte confirmation lives in `crates/syncleo/tests/vectors.rs`: golden
vectors captured by running the reference implementation with fixed keys. Key
derivation, frame encryption, the handshake, acknowledgements and pings are all
compared against it.

## Discovery

mDNS, service type `_syncleo._udp.local.`. The instance name begins with the
device's MAC.

- TXT `public` — the device's public key, hex
- TXT `curve` — curve type; only `29` is supported
- TXT `protocol` — protocol version; only `2` is supported
- the port comes from SRV, addresses from A/AAAA; `169.254.0.0/16` is discarded

A device may advertise only a link-local IPv6 address. Connecting to one
requires a scope, so the interface has to be carried alongside the address.

## Key exchange

1. Generate an X25519 keypair.
2. Our public key goes on the wire **with its bytes reversed**.
3. The device's public key from TXT is likewise reversed before import.
4. `shared = reverse(x25519(our_priv, device_pub))`.
5. `h = sha256(shared)`; `inkey = h[0..16]`, `outkey = h[16..32]`.

The three reversals are not a mistake in the reading — they are how the firmware
behaves. Each one is covered by a golden vector, because getting any of them
wrong produces keys that are silently useless rather than obviously broken.

**The device rotates its keypair on every power loss.** For a kettle that means
every time it is lifted off its base. A cached public key therefore goes stale
routinely; see "Failure modes" below for what that looks like.

## Frame

```
seq: u8, type: u8, len: u16 little-endian, payload: [u8; len]
```

`type`: `0` ACK, `1` CMD, `2` AUX, `3` NAK. AUX is not implemented — it is
logged and ignored on receipt.

## Frame encryption

AES-128-CBC. The key and IV are `inkey`/`outkey` rotated left by the nibbles of
the sequence number:

- outgoing: `key = rotl(outkey, seq & 0x0F)`, `iv = rotl(inkey, (seq >> 4) & 0x0F)`
- incoming: `key = rotl(inkey, seq & 0x0F)`, `iv = rotl(outkey, (seq >> 4) & 0x0F)`

The plaintext before encryption is `[seq, cmd_type, ...data]`, padded with PKCS7.
On decryption the first byte must equal the `seq` from the header; if it does
not, the frame is corrupt.

## Handshake

A special case: the body is assembled by hand and is not encrypted by the scheme
above.

```
payload = 0x00 || our_pubkey_reversed(32) || AES-128-CBC(key=outkey, iv=inkey, token)
```

The token is 16 bytes (32 hex characters) — exactly one AES block, so no padding
is involved. The device authorises us by decrypting it successfully.

The response (command type `0`) is
`protocol: u16le, fw_major: u8, fw_minor: u8, mode: u8, token: [u8]`. The device
may send an ACK first and the response after, or the response straight away;
both orderings occur.

## Commands

| Code | Meaning | Data |
|------|---------|------|
| 0 | handshake | see above |
| 1 | mode | `u8`: 0 off, 1 on (boil to 100), 3 custom target |
| 2 | target temperature | `u8` whole, `u8` hundredths |
| 7 | error | `u8` bool |
| 9 | `volume` | `u8`, meaning unestablished — see below |
| 20 | current temperature | `u8` whole, `u8` hundredths |
| 28 | backlight | `u8` bool |
| 30 | child lock | `u8` bool |
| 133 | access control | `u8` bool |
| 143 | hardware version | 3 × `u8` |
| 145 | vendor telemetry | structured, see below |
| 255 | ping | empty |

Mode `1` also moves the target to 100: a bare start after an earlier
`start 45` reports a target of 100.

The vendor app offers targets from 30 to 100 in steps of 5. The step is not
enforced here — the protocol carries a raw byte, there is no evidence the device
refuses intermediate values, and a refusal would surface as a device NAK anyway.

Unknown command codes do not drop the session. They are surfaced with their raw
bytes and acknowledged like anything else — the firmware sends codes the
reference never identified, and refusing to continue over one would be a bug.

### Command 9, `volume`

The reference decodes this byte as `== 1` and calls it `volume`, which is easy
to read as "water present". It is not. Measured on a real device:

| Kettle state | Value |
|---|---|
| empty | 0 |
| a full litre of water | 0 |
| immediately after boiling to 98 °C | 0 |
| after the water was changed | 0 |

The likeliest explanation is that the protocol is shared across the Polaris IQ
Home range and this model has no sensor behind that byte. The value is kept and
reported as a raw number, and left out of the human-readable `status` — a
permanently-zero row under a name nobody can justify is noise.

### Command 145, vendor telemetry

Not opaque bytes, as the reference assumed: a 20-byte header followed by
repeated pairs of a 4-byte ASCII tag and a 4-byte little-endian value. Decoded
from three real captures — each exactly 52 bytes, exactly 20 + four pairs, every
tag printable ASCII (NUL-padded to four bytes).

Tags seen: `udps`, `mdns`, `IDLE`, `Tmr `, `rtT`, `ppT`, `tiT`. The set varies
between sessions — four pairs every time, but a shifting subset of tags — which
is itself the evidence that these are RTOS task names with accumulated runtime
rather than a fixed record layout. `IDLE` is usually the largest, and `mdns` is
the task answering our own discovery.

```
diagnostic: udps=27415433 ppT=486289 tiT=265550 Tmr =305513
```

Note the trailing space in `Tmr ` — it is significant and must not be trimmed.

The device also sends a one-byte diagnostic, `[0]`, which does not fit the shape
at all. `syncleo::codec::command::decode_diagnostic` is a pure function over the
payload that returns `None` for anything that does not fit — wrong length, a
trailing partial pair, a non-printable tag — so such payloads are shown as raw
bytes rather than guessed at.

This telemetry is destined for the vendor. It is acknowledged and discarded.

### Still unidentified

| Code | Data | What is known |
|------|------|---------------|
| 50 | 1 byte | always `0` in every observation |
| 66 | 4 bytes | always `0 0 0 0` in every observation |

The theory that 66 marks a water change was tested — the water was changed
twice — and disproved.

## Timing

A ping every 3 seconds, up to 5 send attempts per frame, and the connection is
considered lost after 15 seconds of silence. Pings are acknowledgeable: the
reference sends them as `WrappedMessage(PingMessage(), ack=True)`. An
acknowledgement carries the sequence number of the frame it answers.

The outgoing socket binds to a random port.

## Observed device behaviour

Taken from a real PWK 1725CGLD running firmware **2.27.0**, MCU **1.1.4**. The
reference implementation dates from 2022, and its checks (`protocol == 2`,
`curve == 29`) still pass — the protocol has not drifted in four years.

**The port is not fixed.** `41122` was observed, not a round number. Take it
from the SRV record and nothing else.

**Advertised addresses vary between runs.** Sometimes only a link-local IPv6,
sometimes a DHCP address as well. A client that ignores IPv6 will occasionally
find nothing at all.

**Lifting the kettle off its base cuts its power.** That is not a dropped
connection but a device switching off, and it is by far the commonest reason a
kettle cannot be found — likelier than any network fault.

**Temperature is reported once per degree.** A full boil looks like a steady
ladder, `45 → 46 → … → 98`, followed by `mode: off`.

**`start N` is two commands**, and the order matters. Sending the target first
and the mode second means a partial failure leaves a new target on a kettle that
is still off — inert. The other order can leave it heating to its *previous*
target while the caller is told the command failed.

**The vendor app is unaffected.** Everything above was gathered while the
Polaris IQ Home app continued to work normally on the same device; nothing here
changes the device's configuration.

## Failure modes worth knowing

**An acknowledgement is not agreement.** It means the frame arrived, nothing
more. A kettle with no water in it, or one lifted off its base, acknowledges
perfectly well and then does nothing. Any command that changes something has to
read the state back to find out what actually happened.

**A stale public key looks like silence.** After the device rotates its keypair,
our derived keys no longer match, it cannot decrypt our handshake, and it simply
does not answer. The symptom is a timeout, not a rejection — which is worth
knowing, because it means a *rejected* handshake really does mean a wrong token,
and is not worth retrying.

**The kettle stops at 98 °C**, not 100, when told to boil.
