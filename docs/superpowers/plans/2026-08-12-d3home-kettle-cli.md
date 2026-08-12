# d3home — план реализации

> **Для агентов:** обязательный под-skill — `superpowers:subagent-driven-development`
> (рекомендуется) или `superpowers:executing-plans`. Шаги отмечены `- [ ]`.

**Цель:** консольная тулза `d3home` для управления чайником Polaris PWK 1725CGLD по
локальному UDP, с архитектурой под будущие устройства.

**Архитектура:** воркспейс из двух крейтов. `syncleo` — библиотека протокола, внутри
жёстко разделены чистый `codec`, чистый автомат `session` и трейты ввода-вывода
`transport`/`discovery`. `d3home` — CLI поверх: конфиг, алиасы, вывод, коды возврата.
Соединение одноразовое: каждая команда поднимает сессию и закрывает её.

**Стек:** Rust 2024, `aes` + `cbc` + `cipher` + `sha2` + `x25519-dalek` (крипта),
`mdns-sd` (дискавери), `clap` (CLI), `serde` + `toml`, `thiserror`, `proptest`,
`assert_cmd`.

Спека: `docs/superpowers/specs/2026-08-12-d3home-kettle-cli-design.md`.

## Глобальные ограничения

- Rust edition 2024. Версии зависимостей проверены на резолв: `aes 0.9`, `cbc 0.2`,
  `sha2 0.11`, `x25519-dalek 3.0` (фича `static_secrets`), `mdns-sd 0.21`, `clap 4.6`
  (фича `derive`), `toml 1.1`, `thiserror 2.0`, `proptest 1.11`, `assert_cmd 2.2`.
- Код, имена и сообщения коммитов — по-английски. Документация и README — по-русски.
- `syncleo` не зависит от `clap`, `toml` и чего-либо про конфиги и терминал.
- Модули `codec` и `session` не содержат ни сокетов, ни обращений к системным часам.
  Время в них передаётся аргументом.
- Реальный токен устройства не попадает ни в один файл репозитория. В тестах —
  только фиктивные значения.
- На каждый найденный баг сначала пишется воспроизводящий тест, потом чинится.
- Каждая задача заканчивается зелёным `cargo test` и коммитом.

## Golden-векторы

Сняты с Python-референса `gch1p/polaris_pwk_1725cgld` и используются во всём плане.
Константы (все — фиктивные):

```
INKEY  = 000102030405060708090a0b0c0d0e0f
OUTKEY = 101112131415161718191a1b1c1d1e1f
TOKEN  = a0a1a2a3a4a5a6a7a8a9aaabacadaeaf
```

## Файловая структура

```
Cargo.toml                        воркспейс
crates/syncleo/
  src/lib.rs                      реэкспорты
  src/error.rs                    Error, CodecError
  src/codec/mod.rs
  src/codec/frame.rs              FrameType, FrameHead, Frame
  src/codec/keys.rs               rotl, derive, public_wire, SessionKeys
  src/codec/crypt.rs              encrypt_frame, decrypt_frame
  src/codec/command.rs            PowerMode, Command, Event
  src/codec/handshake.rs          handshake_payload
  src/session.rs                  Session, Input, Action, Millis
  src/transport.rs                Transport, UdpTransport, MemoryTransport
  src/discovery.rs                Discovery, Found, MdnsDiscovery
  src/client.rs                   Client, DeviceState
  src/simulator.rs                KettleSimulator (device side, под фичей `simulator`)
  tests/vectors.rs                golden-векторы
  tests/session.rs                автомат на виртуальных часах
  tests/loopback.rs               сквозняк против симулятора
crates/d3home/
  src/main.rs                     точка входа, коды возврата
  src/config.rs                   Config, Device, Cached, ConfigError
  src/cli.rs                      разбор аргументов
  src/commands/kettle.rs          status, start, off, watch
  src/commands/registry.rs        discover, devices, alias
  src/output.rs                   человеческий вывод и --json
  tests/cli.rs                    сквозные тесты CLI против симулятора
```

---

### Task 1: Воркспейс и `rotl`

**Файлы:** создать `Cargo.toml`, `crates/syncleo/Cargo.toml`, `crates/syncleo/src/lib.rs`,
`crates/syncleo/src/codec/mod.rs`, `crates/syncleo/src/codec/keys.rs`

**Интерфейсы:**
- Производит: `pub fn rotl(key: &[u8; 16], n: u8) -> [u8; 16]`

Циклический сдвиг влево на `n` байт — основа схемы шифрования. `n` берётся из полубайта,
то есть всегда 0–15, но функция обязана вести себя корректно и при `n >= 16`.

- [ ] **Шаг 1: Каркас воркспейса**

`Cargo.toml` в корне:

```toml
[workspace]
members = ["crates/syncleo", "crates/d3home"]
resolver = "3"
```

`crates/syncleo/Cargo.toml`:

```toml
[package]
name = "syncleo"
version = "0.1.0"
edition = "2024"

[dependencies]
```

`crates/syncleo/src/lib.rs`:

```rust
pub mod codec;
```

`crates/syncleo/src/codec/mod.rs`:

```rust
pub mod keys;
```

Крейт `d3home` появится в Task 10, до тех пор временно уберите его из `members`.

- [ ] **Шаг 2: Написать падающий тест**

В конец `crates/syncleo/src/codec/keys.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotl_by_zero_is_identity() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        assert_eq!(rotl(&key, 0), key);
    }

    #[test]
    fn rotl_moves_bytes_to_the_front() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let got = rotl(&key, 3);
        assert_eq!(got[0], 3, "byte at n must become the first byte");
        assert_eq!(got[12], 15, "last original byte lands at len-n-1");
        assert_eq!(got[13], 0, "wrapped bytes follow");
        assert_eq!(got[15], 2);
    }

    #[test]
    fn rotl_wraps_past_the_key_length() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        assert_eq!(rotl(&key, 19), rotl(&key, 3));
    }
}
```

- [ ] **Шаг 3: Убедиться, что тест падает**

Запустить: `cargo test -p syncleo`
Ожидается: ошибка компиляции, `cannot find function 'rotl'`.

- [ ] **Шаг 4: Минимальная реализация**

В начало `crates/syncleo/src/codec/keys.rs`:

```rust
/// Rotate a key left by `n` bytes. The Syncleo framing derives both the AES key
/// and the IV this way, using the two nibbles of the frame sequence number.
pub fn rotl(key: &[u8; 16], n: u8) -> [u8; 16] {
    let n = (n as usize) % 16;
    core::array::from_fn(|i| key[(i + n) % 16])
}
```

- [ ] **Шаг 5: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: 3 passed.

- [ ] **Шаг 6: Коммит**

```bash
git add Cargo.toml crates/
git commit -m "Add workspace skeleton and key rotation helper"
```

---

### Task 2: Кадр

**Файлы:** создать `crates/syncleo/src/codec/frame.rs`, `crates/syncleo/src/error.rs`;
изменить `crates/syncleo/src/codec/mod.rs`, `crates/syncleo/src/lib.rs`

**Интерфейсы:**
- Производит:
  - `pub enum FrameType { Ack, Cmd, Aux, Nak }`, `FrameType::from_u8(u8) -> Option<FrameType>`, `FrameType::as_u8(self) -> u8`
  - `pub struct FrameHead { pub seq: u8, pub ty: FrameType, pub len: u16 }`
  - `pub struct Frame { pub head: FrameHead, pub payload: Vec<u8> }`
  - `Frame::to_bytes(&self) -> Vec<u8>`, `Frame::parse(buf: &[u8]) -> Result<Frame, CodecError>`
  - `pub enum CodecError` с вариантами `ShortFrame`, `LengthMismatch { declared: u16, actual: usize }`, `UnknownFrameType(u8)`, `SeqMismatch { head: u8, body: u8 }`, `BadPadding`, `EmptyBody`, `BadCommandLength { ty: u8, len: usize }`

Заголовок: `seq: u8, ty: u8, len: u16 little-endian`, дальше `len` байт тела.

- [ ] **Шаг 1: Написать падающие тесты**

`crates/syncleo/src/codec/frame.rs`, блок тестов:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_frame() {
        // seq=0x35, type=1 (Cmd), len=0x0010 little-endian, then 16 bytes
        let mut buf = vec![0x35, 0x01, 0x10, 0x00];
        buf.extend_from_slice(&[0xAA; 16]);

        let frame = Frame::parse(&buf).expect("valid frame");

        assert_eq!(frame.head.seq, 0x35);
        assert_eq!(frame.head.ty, FrameType::Cmd);
        assert_eq!(frame.head.len, 16);
        assert_eq!(frame.payload, vec![0xAA; 16]);
    }

    #[test]
    fn round_trips_through_bytes() {
        let mut buf = vec![0xC2, 0x00, 0x02, 0x00, 0xDE, 0xAD];
        let frame = Frame::parse(&buf).unwrap();
        assert_eq!(frame.to_bytes(), buf);

        buf.truncate(4);
        assert!(Frame::parse(&buf).is_err(), "declared length must be honoured");
    }

    #[test]
    fn rejects_a_frame_shorter_than_its_header() {
        assert!(matches!(Frame::parse(&[0x01, 0x02, 0x03]), Err(CodecError::ShortFrame)));
    }

    #[test]
    fn rejects_a_length_that_disagrees_with_the_buffer() {
        let buf = vec![0x00, 0x01, 0xFF, 0x00, 0x01, 0x02];
        assert!(matches!(
            Frame::parse(&buf),
            Err(CodecError::LengthMismatch { declared: 255, actual: 2 })
        ));
    }

    #[test]
    fn rejects_an_unknown_frame_type() {
        let buf = vec![0x00, 0x09, 0x00, 0x00];
        assert!(matches!(Frame::parse(&buf), Err(CodecError::UnknownFrameType(9))));
    }
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p syncleo`
Ожидается: ошибки компиляции — нет `Frame`, `FrameType`, `CodecError`.

- [ ] **Шаг 3: Реализация**

`crates/syncleo/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("frame is shorter than its 4-byte header")]
    ShortFrame,
    #[error("declared payload length {declared} does not match {actual} bytes present")]
    LengthMismatch { declared: u16, actual: usize },
    #[error("unknown frame type {0}")]
    UnknownFrameType(u8),
    #[error("decrypted sequence {body} does not match header sequence {head}")]
    SeqMismatch { head: u8, body: u8 },
    #[error("invalid PKCS7 padding")]
    BadPadding,
    #[error("decrypted body is empty")]
    EmptyBody,
    #[error("command {ty} carries {len} bytes, which is not a valid length")]
    BadCommandLength { ty: u8, len: usize },
}
```

`crates/syncleo/src/codec/frame.rs`:

```rust
use crate::error::CodecError;

pub const HEAD_LEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Ack,
    Cmd,
    Aux,
    Nak,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ack),
            1 => Some(Self::Cmd),
            2 => Some(Self::Aux),
            3 => Some(Self::Nak),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Ack => 0,
            Self::Cmd => 1,
            Self::Aux => 2,
            Self::Nak => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHead {
    pub seq: u8,
    pub ty: FrameType,
    pub len: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub head: FrameHead,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(seq: u8, ty: FrameType, payload: Vec<u8>) -> Self {
        let len = payload.len() as u16;
        Self { head: FrameHead { seq, ty, len }, payload }
    }

    pub fn parse(buf: &[u8]) -> Result<Self, CodecError> {
        if buf.len() < HEAD_LEN {
            return Err(CodecError::ShortFrame);
        }
        let seq = buf[0];
        let ty = FrameType::from_u8(buf[1]).ok_or(CodecError::UnknownFrameType(buf[1]))?;
        let len = u16::from_le_bytes([buf[2], buf[3]]);
        let payload = &buf[HEAD_LEN..];
        if payload.len() != len as usize {
            return Err(CodecError::LengthMismatch { declared: len, actual: payload.len() });
        }
        Ok(Self { head: FrameHead { seq, ty, len }, payload: payload.to_vec() })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEAD_LEN + self.payload.len());
        out.push(self.head.seq);
        out.push(self.head.ty.as_u8());
        out.extend_from_slice(&self.head.len.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}
```

Добавить `thiserror` в зависимости, `pub mod error;` в `lib.rs`, `pub mod frame;` в
`codec/mod.rs`.

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: все тесты passed.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Add frame header parsing and serialisation"
```

---

### Task 3: Вывод ключей

**Файлы:** изменить `crates/syncleo/src/codec/keys.rs`

**Интерфейсы:**
- Потребляет: `rotl` из Task 1
- Производит:
  - `pub struct SessionKeys { pub inkey: [u8; 16], pub outkey: [u8; 16] }`
  - `pub fn public_wire(our_private: &[u8; 32]) -> [u8; 32]`
  - `pub fn derive(our_private: &[u8; 32], device_public_wire: &[u8; 32]) -> SessionKeys`

Три места, где байты разворачиваются: наш публичный ключ перед отправкой, публичный ключ
устройства перед импортом, общий секрет перед хешированием. Все три покрыты вектором.

- [ ] **Шаг 1: Написать падающий тест**

Добавить в блок тестов `keys.rs`:

```rust
    // Golden vector produced by the Python reference implementation
    // (gch1p/polaris_pwk_1725cgld) with a fixed private key.
    const OUR_PRIVATE: [u8; 32] = [7; 32];
    const DEVICE_PUBLIC_WIRE: &str =
        "21d4043d930c3d75140c158c3406257204670512254e6e145eae239f354bdb57";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn our_public_key_goes_on_the_wire_reversed() {
        assert_eq!(
            public_wire(&OUR_PRIVATE).to_vec(),
            unhex("6d7be97f7ff374c67e2228812774d1811872009cfc5833fdc704f2eaea4fbe13")
        );
    }

    #[test]
    fn derives_the_reference_session_keys() {
        let device: [u8; 32] = unhex(DEVICE_PUBLIC_WIRE).try_into().unwrap();
        let keys = derive(&OUR_PRIVATE, &device);

        assert_eq!(keys.inkey.to_vec(), unhex("3ec02e08f3f3bea4dacc8179f46f493d"));
        assert_eq!(keys.outkey.to_vec(), unhex("8c6715bf7555d1e7e0032db0a7d3da76"));
    }
```

- [ ] **Шаг 2: Убедиться, что тест падает**

Запустить: `cargo test -p syncleo derives_the_reference`
Ожидается: ошибка компиляции, нет `derive` и `public_wire`.

- [ ] **Шаг 3: Реализация**

Зависимости: `cargo add -p syncleo sha2 x25519-dalek --features x25519-dalek/static_secrets`

```rust
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKeys {
    pub inkey: [u8; 16],
    pub outkey: [u8; 16],
}

fn reversed(bytes: [u8; 32]) -> [u8; 32] {
    let mut out = bytes;
    out.reverse();
    out
}

/// Our X25519 public key in the byte order the firmware expects: reversed.
pub fn public_wire(our_private: &[u8; 32]) -> [u8; 32] {
    let secret = StaticSecret::from(*our_private);
    reversed(PublicKey::from(&secret).to_bytes())
}

/// Derive the two AES-128 half-keys from the X25519 shared secret.
///
/// The firmware reverses byte order in three places: our public key on the wire,
/// the device public key before import, and the shared secret before hashing.
pub fn derive(our_private: &[u8; 32], device_public_wire: &[u8; 32]) -> SessionKeys {
    let secret = StaticSecret::from(*our_private);
    let device = PublicKey::from(reversed(*device_public_wire));
    let shared = reversed(secret.diffie_hellman(&device).to_bytes());

    let digest = Sha256::digest(shared);
    SessionKeys {
        inkey: digest[..16].try_into().expect("sha256 is 32 bytes"),
        outkey: digest[16..].try_into().expect("sha256 is 32 bytes"),
    }
}
```

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: все passed. Если ключи не совпали — виноват один из трёх разворотов, проверяйте
их по очереди, а не переписывайте всё.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Derive session keys from the X25519 shared secret"
```

---

### Task 4: Шифрование и расшифровка кадра

**Файлы:** создать `crates/syncleo/src/codec/crypt.rs`, `crates/syncleo/tests/vectors.rs`;
изменить `crates/syncleo/src/codec/mod.rs`

**Интерфейсы:**
- Потребляет: `Frame`, `FrameType`, `CodecError` (Task 2), `SessionKeys`, `rotl` (Task 1, 3)
- Производит:
  - `pub fn encrypt_frame(keys: &SessionKeys, seq: u8, ty: FrameType, body: &[u8]) -> Frame`
  - `pub fn decrypt_frame(keys: &SessionKeys, frame: &Frame) -> Result<Vec<u8>, CodecError>`

`body` — это `[cmd_type, data...]` для `Cmd` и пустой срез для `Ack`/`Nak`. Перед
шифрованием спереди дописывается `seq`, потом PKCS7. `decrypt_frame` возвращает тело
**без** ведущего `seq`, предварительно сверив его с заголовком.

Направления используют ключи крест-накрест:
- исходящие: `key = rotl(outkey, seq & 0x0F)`, `iv = rotl(inkey, seq >> 4)`
- входящие: `key = rotl(inkey, seq & 0x0F)`, `iv = rotl(outkey, seq >> 4)`

- [ ] **Шаг 1: Написать падающие тесты**

`crates/syncleo/tests/vectors.rs`:

```rust
use syncleo::codec::crypt::{decrypt_frame, encrypt_frame};
use syncleo::codec::frame::{Frame, FrameType};
use syncleo::codec::keys::SessionKeys;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn keys() -> SessionKeys {
    SessionKeys {
        inkey: unhex("000102030405060708090a0b0c0d0e0f").try_into().unwrap(),
        outkey: unhex("101112131415161718191a1b1c1d1e1f").try_into().unwrap(),
    }
}

// Every expected value below was produced by the Python reference
// implementation (gch1p/polaris_pwk_1725cgld) with the keys above.

#[test]
fn encrypts_mode_commands_byte_for_byte() {
    // command 1 (mode), payload 1 = on
    let frame = encrypt_frame(&keys(), 0x35, FrameType::Cmd, &[0x01, 0x01]);
    assert_eq!(frame.to_bytes(), unhex("35011000c6ed01166ba072d2baa600574cc0bcc8"));

    // payload 0 = off, at sequence 0 (no rotation at all)
    let frame = encrypt_frame(&keys(), 0x00, FrameType::Cmd, &[0x01, 0x00]);
    assert_eq!(frame.to_bytes(), unhex("000110004c4f50e332b954b25aabec5d9b69b46a"));

    // payload 3 = custom, at sequence 0xFE (both nibbles rotate)
    let frame = encrypt_frame(&keys(), 0xFE, FrameType::Cmd, &[0x01, 0x03]);
    assert_eq!(frame.to_bytes(), unhex("fe011000f184650ca412db228c17f105b574f0bc"));
}

#[test]
fn encrypts_a_target_temperature_command() {
    // command 2 (target temperature), 80 whole degrees, 0 hundredths
    let frame = encrypt_frame(&keys(), 0xC2, FrameType::Cmd, &[0x02, 80, 0]);
    assert_eq!(frame.to_bytes(), unhex("c20110003c1e25f8bcd62e14e9cad115d182ceae"));
}

#[test]
fn decrypts_a_frame_sent_by_the_device() {
    // The device encrypts with the key roles swapped, so this is what we receive.
    let raw = unhex("35011000ac0017868ee3c024d65b3fb78602638a");
    let frame = Frame::parse(&raw).unwrap();

    let body = decrypt_frame(&keys(), &frame).expect("device frame decrypts");

    assert_eq!(body, vec![0x01, 0x01], "mode command, value on");
}

#[test]
fn round_trips_every_body_length_through_a_padding_boundary() {
    // 14 bytes of body plus the sequence byte exactly fills one AES block,
    // which is where PKCS7 bugs hide.
    let device_view = SessionKeys { inkey: keys().outkey, outkey: keys().inkey };

    for len in 0..40usize {
        let body: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let frame = encrypt_frame(&keys(), 0x7B, FrameType::Cmd, &body);
        let back = decrypt_frame(&device_view, &frame).expect("round trip");
        assert_eq!(back, body, "body of length {len} survived the round trip");
    }
}

#[test]
fn rejects_a_frame_whose_sequence_was_tampered_with() {
    let raw = unhex("35011000ac0017868ee3c024d65b3fb78602638a");
    let mut frame = Frame::parse(&raw).unwrap();
    frame.head.seq = 0x36;

    assert!(decrypt_frame(&keys(), &frame).is_err(), "sequence mismatch must be caught");
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p syncleo --test vectors`
Ожидается: ошибка компиляции, нет модуля `crypt`.

- [ ] **Шаг 3: Реализация**

Зависимости: `cargo add -p syncleo aes cbc cipher`

```rust
use aes::Aes128;
use cbc::{Decryptor, Encryptor};
use cipher::block_padding::Pkcs7;
use cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};

use super::frame::{Frame, FrameType};
use super::keys::{SessionKeys, rotl};
use crate::error::CodecError;

const BLOCK: usize = 16;

fn outgoing_key_iv(keys: &SessionKeys, seq: u8) -> ([u8; 16], [u8; 16]) {
    (rotl(&keys.outkey, seq & 0x0F), rotl(&keys.inkey, seq >> 4))
}

fn incoming_key_iv(keys: &SessionKeys, seq: u8) -> ([u8; 16], [u8; 16]) {
    (rotl(&keys.inkey, seq & 0x0F), rotl(&keys.outkey, seq >> 4))
}

/// Encrypt one frame. `body` is `[command_type, data..]` for `Cmd` frames and
/// empty for `Ack`/`Nak`, which carry nothing but their sequence number.
pub fn encrypt_frame(keys: &SessionKeys, seq: u8, ty: FrameType, body: &[u8]) -> Frame {
    let mut plain = Vec::with_capacity(1 + body.len());
    plain.push(seq);
    plain.extend_from_slice(body);

    let (key, iv) = outgoing_key_iv(keys, seq);
    let padded_len = (plain.len() / BLOCK + 1) * BLOCK;
    let mut buf = vec![0u8; padded_len];
    buf[..plain.len()].copy_from_slice(&plain);

    let ciphertext = Encryptor::<Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
        .expect("buffer sized for PKCS7")
        .to_vec();

    Frame::new(seq, ty, ciphertext)
}

/// Decrypt one frame, returning the body without its leading sequence byte.
pub fn decrypt_frame(keys: &SessionKeys, frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let seq = frame.head.seq;
    let (key, iv) = incoming_key_iv(keys, seq);

    let mut buf = frame.payload.clone();
    let plain = Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut buf)
        .map_err(|_| CodecError::BadPadding)?;

    let (&first, rest) = plain.split_first().ok_or(CodecError::EmptyBody)?;
    if first != seq {
        return Err(CodecError::SeqMismatch { head: seq, body: first });
    }
    Ok(rest.to_vec())
}
```

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: все passed, включая пять тестов в `vectors.rs`.

- [ ] **Шаг 5: Property-тест на кругорейс**

Добавить в `crates/syncleo/tests/vectors.rs`, зависимость `cargo add -p syncleo --dev proptest`:

```rust
proptest::proptest! {
    #[test]
    fn any_body_at_any_sequence_survives_a_round_trip(
        seq in proptest::prelude::any::<u8>(),
        body in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
    ) {
        let device_view = SessionKeys { inkey: keys().outkey, outkey: keys().inkey };
        let frame = encrypt_frame(&keys(), seq, FrameType::Cmd, &body);

        let parsed = Frame::parse(&frame.to_bytes()).unwrap();
        let back = decrypt_frame(&device_view, &parsed).unwrap();

        proptest::prop_assert_eq!(back, body);
    }
}
```

Запустить: `cargo test -p syncleo --test vectors`
Ожидается: passed.

- [ ] **Шаг 6: Коммит**

```bash
git add crates/
git commit -m "Encrypt and decrypt frames against reference vectors"
```

---

### Task 5: Команды и события

**Файлы:** создать `crates/syncleo/src/codec/command.rs`; изменить `crates/syncleo/src/codec/mod.rs`

**Интерфейсы:**
- Потребляет: `CodecError` (Task 2)
- Производит:
  - `pub enum PowerMode { Off, On, Custom }` + `from_u8`/`as_u8`
  - `pub enum Command { Mode(PowerMode), TargetTemperature(u8), Ping }` + `Command::encode(&self) -> Vec<u8>`
  - `pub enum Event { HandshakeResponse { protocol: u16, fw_major: u8, fw_minor: u8, mode: u8 }, Mode(PowerMode), TargetTemperature(u8), CurrentTemperature(u8), WaterPresent(bool), Error(bool), Backlight(bool), ChildLock(bool), AccessControl(bool), Hardware([u8; 3]), Diagnostic(Vec<u8>), Ping, Unknown { ty: u8, data: Vec<u8> } }`
  - `Event::decode(body: &[u8]) -> Result<Event, CodecError>`

Коды команд: 0 рукопожатие, 1 режим, 2 целевая температура, 7 ошибка, 9 вода,
20 текущая температура, 28 подсветка, 30 блокировка от детей, 133 контроль доступа,
143 железо, 145 диагностика, 255 пинг.

Температура приходит парой байт: целые и сотые. Сотые отбрасываем — прошивка шлёт там
ноль, а целых градусов достаточно.

Неизвестный код команды **не ошибка**: возвращается `Event::Unknown`. Прошивка шлёт
сообщения, которых нет в референсе, и ронять из-за них сессию нельзя.

- [ ] **Шаг 1: Написать падающие тесты**

Блок тестов в `command.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_outgoing_commands() {
        assert_eq!(Command::Mode(PowerMode::On).encode(), vec![1, 1]);
        assert_eq!(Command::Mode(PowerMode::Off).encode(), vec![1, 0]);
        assert_eq!(Command::Mode(PowerMode::Custom).encode(), vec![1, 3]);
        assert_eq!(Command::TargetTemperature(80).encode(), vec![2, 80, 0]);
        assert_eq!(Command::Ping.encode(), vec![255]);
    }

    #[test]
    fn decodes_temperatures_discarding_hundredths() {
        assert_eq!(Event::decode(&[20, 93, 50]).unwrap(), Event::CurrentTemperature(93));
        assert_eq!(Event::decode(&[2, 80, 0]).unwrap(), Event::TargetTemperature(80));
    }

    #[test]
    fn decodes_boolean_state() {
        assert_eq!(Event::decode(&[9, 1]).unwrap(), Event::WaterPresent(true));
        assert_eq!(Event::decode(&[7, 0]).unwrap(), Event::Error(false));
        assert_eq!(Event::decode(&[28, 1]).unwrap(), Event::Backlight(true));
        assert_eq!(Event::decode(&[30, 1]).unwrap(), Event::ChildLock(true));
        assert_eq!(Event::decode(&[133, 0]).unwrap(), Event::AccessControl(false));
    }

    #[test]
    fn decodes_a_handshake_response() {
        // protocol 2 little-endian, firmware 1.4, mode 0, then an echoed token
        let body = [0u8, 0x02, 0x00, 0x01, 0x04, 0x00, 0xAA, 0xBB];
        assert_eq!(
            Event::decode(&body).unwrap(),
            Event::HandshakeResponse { protocol: 2, fw_major: 1, fw_minor: 4, mode: 0 }
        );
    }

    #[test]
    fn decodes_hardware_and_diagnostics() {
        assert_eq!(Event::decode(&[143, 1, 1, 1]).unwrap(), Event::Hardware([1, 1, 1]));
        assert_eq!(Event::decode(&[145, 9, 9]).unwrap(), Event::Diagnostic(vec![9, 9]));
        assert_eq!(Event::decode(&[255]).unwrap(), Event::Ping);
    }

    #[test]
    fn keeps_unknown_commands_instead_of_failing() {
        // The firmware sends messages the reference never identified. Surviving
        // them matters more than understanding them.
        assert_eq!(
            Event::decode(&[77, 1, 2, 3]).unwrap(),
            Event::Unknown { ty: 77, data: vec![1, 2, 3] }
        );
    }

    #[test]
    fn rejects_a_known_command_with_the_wrong_length() {
        assert!(matches!(
            Event::decode(&[20, 93]),
            Err(CodecError::BadCommandLength { ty: 20, len: 1 })
        ));
        assert!(matches!(Event::decode(&[]), Err(CodecError::EmptyBody)));
    }
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p syncleo`
Ожидается: ошибки компиляции.

- [ ] **Шаг 3: Реализация**

```rust
use crate::error::CodecError;

pub mod ty {
    pub const HANDSHAKE: u8 = 0;
    pub const MODE: u8 = 1;
    pub const TARGET_TEMPERATURE: u8 = 2;
    pub const ERROR: u8 = 7;
    pub const WATER: u8 = 9;
    pub const CURRENT_TEMPERATURE: u8 = 20;
    pub const BACKLIGHT: u8 = 28;
    pub const CHILD_LOCK: u8 = 30;
    pub const ACCESS_CONTROL: u8 = 133;
    pub const HARDWARE: u8 = 143;
    pub const DIAGNOSTIC: u8 = 145;
    pub const PING: u8 = 255;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    Off,
    On,
    Custom,
}

impl PowerMode {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::Custom => 3,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            3 => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Mode(PowerMode),
    TargetTemperature(u8),
    Ping,
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Mode(m) => vec![ty::MODE, m.as_u8()],
            Self::TargetTemperature(t) => vec![ty::TARGET_TEMPERATURE, *t, 0],
            Self::Ping => vec![ty::PING],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    HandshakeResponse { protocol: u16, fw_major: u8, fw_minor: u8, mode: u8 },
    Mode(PowerMode),
    TargetTemperature(u8),
    CurrentTemperature(u8),
    WaterPresent(bool),
    Error(bool),
    Backlight(bool),
    ChildLock(bool),
    AccessControl(bool),
    Hardware([u8; 3]),
    Diagnostic(Vec<u8>),
    Ping,
    Unknown { ty: u8, data: Vec<u8> },
}

fn expect_len(ty: u8, data: &[u8], want: usize) -> Result<(), CodecError> {
    if data.len() == want {
        Ok(())
    } else {
        Err(CodecError::BadCommandLength { ty, len: data.len() })
    }
}

impl Event {
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let (&cmd, data) = body.split_first().ok_or(CodecError::EmptyBody)?;
        let flag = |d: &[u8]| -> Result<bool, CodecError> {
            expect_len(cmd, d, 1)?;
            Ok(d[0] == 1)
        };

        Ok(match cmd {
            ty::HANDSHAKE => {
                if data.len() < 5 {
                    return Err(CodecError::BadCommandLength { ty: cmd, len: data.len() });
                }
                Self::HandshakeResponse {
                    protocol: u16::from_le_bytes([data[0], data[1]]),
                    fw_major: data[2],
                    fw_minor: data[3],
                    mode: data[4],
                }
            }
            ty::MODE => {
                expect_len(cmd, data, 1)?;
                match PowerMode::from_u8(data[0]) {
                    Some(m) => Self::Mode(m),
                    None => Self::Unknown { ty: cmd, data: data.to_vec() },
                }
            }
            ty::TARGET_TEMPERATURE => {
                expect_len(cmd, data, 2)?;
                Self::TargetTemperature(data[0])
            }
            ty::CURRENT_TEMPERATURE => {
                expect_len(cmd, data, 2)?;
                Self::CurrentTemperature(data[0])
            }
            ty::ERROR => Self::Error(flag(data)?),
            ty::WATER => Self::WaterPresent(flag(data)?),
            ty::BACKLIGHT => Self::Backlight(flag(data)?),
            ty::CHILD_LOCK => Self::ChildLock(flag(data)?),
            ty::ACCESS_CONTROL => Self::AccessControl(flag(data)?),
            ty::HARDWARE => {
                expect_len(cmd, data, 3)?;
                Self::Hardware([data[0], data[1], data[2]])
            }
            ty::DIAGNOSTIC => Self::Diagnostic(data.to_vec()),
            ty::PING => {
                expect_len(cmd, data, 0)?;
                Self::Ping
            }
            other => Self::Unknown { ty: other, data: data.to_vec() },
        })
    }
}
```

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: все passed.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Encode commands and decode device events"
```

---

### Task 6: Рукопожатие

**Файлы:** создать `crates/syncleo/src/codec/handshake.rs`; изменить
`crates/syncleo/src/codec/mod.rs`, `crates/syncleo/tests/vectors.rs`

**Интерфейсы:**
- Потребляет: `SessionKeys` (Task 3), `Frame`, `FrameType` (Task 2)
- Производит: `pub fn handshake_frame(keys: &SessionKeys, seq: u8, our_public_wire: &[u8; 32], token: &[u8; 16]) -> Frame`

Особый случай: тело собирается вручную и общей схемой шифрования **не** обрабатывается.

```
payload = 0x00 || our_public_wire(32) || AES-128-CBC(key=outkey, iv=inkey, token)
```

Токен ровно 16 байт — один блок AES, дополнение не нужно и не добавляется.

- [ ] **Шаг 1: Написать падающий тест**

В `crates/syncleo/tests/vectors.rs`:

```rust
#[test]
fn builds_the_reference_handshake_frame() {
    use syncleo::codec::handshake::handshake_frame;

    let our_public: [u8; 32] = core::array::from_fn(|i| i as u8);
    let token: [u8; 16] = unhex("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf").try_into().unwrap();

    let frame = handshake_frame(&keys(), 0x01, &our_public, &token);

    assert_eq!(
        frame.to_bytes(),
        unhex(concat!(
            "01013100",
            "00",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "3a5eb8fc7284be1035c8c6dc6c30aafa",
        )),
    );
}
```

- [ ] **Шаг 2: Убедиться, что тест падает**

Запустить: `cargo test -p syncleo --test vectors builds_the_reference_handshake`
Ожидается: ошибка компиляции.

- [ ] **Шаг 3: Реализация**

```rust
use aes::Aes128;
use cbc::Encryptor;
use cipher::{BlockModeEncrypt, KeyIvInit};

use super::frame::{Frame, FrameType};
use super::keys::SessionKeys;

/// Build the opening frame of a session.
///
/// The handshake body is assembled by hand and deliberately bypasses the frame
/// encryption used everywhere else: the token is a single AES block encrypted
/// with the unrotated keys, and no padding is applied. The device authenticates
/// us by decrypting it successfully.
pub fn handshake_frame(
    keys: &SessionKeys,
    seq: u8,
    our_public_wire: &[u8; 32],
    token: &[u8; 16],
) -> Frame {
    let mut block = *token;
    Encryptor::<Aes128>::new(&keys.outkey.into(), &keys.inkey.into())
        .encrypt_blocks(core::slice::from_mut((&mut block).into()));

    let mut payload = Vec::with_capacity(1 + 32 + 16);
    payload.push(0x00);
    payload.extend_from_slice(our_public_wire);
    payload.extend_from_slice(&block);

    Frame::new(seq, FrameType::Cmd, payload)
}
```

Если `encrypt_blocks` не подойдёт по типам, используйте
`encrypt_padded::<cipher::block_padding::NoPadding>` на буфере ровно в 16 байт —
результат обязан совпасть с вектором побайтово.

- [ ] **Шаг 4: Тест зелёный**

Запустить: `cargo test -p syncleo`
Ожидается: все passed.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Build the handshake frame"
```

---

### Task 7: Автомат сессии

**Файлы:** создать `crates/syncleo/src/session.rs`, `crates/syncleo/tests/session.rs`;
изменить `crates/syncleo/src/lib.rs`

**Интерфейсы:**
- Потребляет: всё из `codec`
- Производит:
  - `pub struct Millis(pub u64)` — монотонное время, передаётся снаружи
  - `pub enum Input { Packet(Vec<u8>), Tick }`
  - `pub enum Action { Send(Vec<u8>), Emit(Event), Acked(u8), Connected, Lost(LostReason) }`
  - `pub enum LostReason { Silence, HandshakeRejected, Unacknowledged }`
  - `pub struct Session`
  - `Session::new(our_private: [u8; 32], device_public_wire: [u8; 32], token: [u8; 16], now: Millis) -> (Session, Vec<Action>)` — сразу возвращает `Action::Send` с рукопожатием
  - `Session::step(&mut self, input: Input, now: Millis) -> Vec<Action>`
  - `Session::request(&mut self, cmd: Command, now: Millis) -> Vec<Action>`
  - `Session::is_connected(&self) -> bool`

Ни сокетов, ни `Instant::now()`: всё время приходит аргументом, поэтому тесты
детерминированы и мгновенны.

Правила: пинг раз в 3000 мс; неподтверждённый кадр переотправляется раз в 1000 мс до
5 попыток; 15000 мс без единого входящего пакета — `Lost(Silence)`. На любой входящий
`Cmd` отвечаем `Ack` с тем же `seq`. Номер исходящей последовательности инкрементируется
по модулю 256. Пришёл `Nak` на рукопожатие — `Lost(HandshakeRejected)`.

- [ ] **Шаг 1: Написать падающие тесты**

`crates/syncleo/tests/session.rs`:

```rust
use syncleo::codec::command::{Command, Event, PowerMode};
use syncleo::codec::crypt::{decrypt_frame, encrypt_frame};
use syncleo::codec::frame::{Frame, FrameType};
use syncleo::codec::keys::{SessionKeys, derive};
use syncleo::session::{Action, Input, LostReason, Millis, Session};

const OUR_PRIVATE: [u8; 32] = [7; 32];
const DEVICE_PRIVATE: [u8; 32] = [9; 32];
const TOKEN: [u8; 16] = [0xA0; 16];

/// Keys as the device sees them: same secret, roles swapped.
fn device_keys() -> SessionKeys {
    let k = derive(&OUR_PRIVATE, &syncleo::codec::keys::public_wire(&DEVICE_PRIVATE));
    SessionKeys { inkey: k.outkey, outkey: k.inkey }
}

fn start() -> (Session, Vec<Action>) {
    Session::new(
        OUR_PRIVATE,
        syncleo::codec::keys::public_wire(&DEVICE_PRIVATE),
        TOKEN,
        Millis(0),
    )
}

fn sent(actions: &[Action]) -> Vec<Vec<u8>> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send(bytes) => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

/// Build a packet as the device would send it.
fn from_device(seq: u8, ty: FrameType, body: &[u8]) -> Vec<u8> {
    encrypt_frame(&device_keys(), seq, ty, body).to_bytes()
}

#[test]
fn opens_with_a_handshake() {
    let (_session, actions) = start();
    let packets = sent(&actions);

    assert_eq!(packets.len(), 1, "exactly one packet on open");
    let frame = Frame::parse(&packets[0]).unwrap();
    assert_eq!(frame.head.ty, FrameType::Cmd);
    assert_eq!(frame.payload[0], 0x00, "handshake command");
    assert_eq!(frame.payload.len(), 1 + 32 + 16);
}

#[test]
fn reports_connected_once_the_device_answers() {
    let (mut session, _) = start();

    let response = from_device(0, FrameType::Cmd, &[0, 0x02, 0x00, 1, 4, 0]);
    let actions = session.step(Input::Packet(response), Millis(50));

    assert!(session.is_connected());
    assert!(actions.iter().any(|a| matches!(a, Action::Connected)));
    assert!(!sent(&actions).is_empty(), "device commands must be acknowledged");
}

#[test]
fn acknowledges_incoming_commands_with_the_same_sequence() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(10));

    let actions = session.step(
        Input::Packet(from_device(0x42, FrameType::Cmd, &[20, 93, 0])),
        Millis(20),
    );

    assert!(
        actions.iter().any(|a| matches!(a, Action::Emit(Event::CurrentTemperature(93)))),
        "temperature must be emitted"
    );
    let ack = Frame::parse(&sent(&actions)[0]).unwrap();
    assert_eq!(ack.head.ty, FrameType::Ack);
    assert_eq!(ack.head.seq, 0x42, "ack carries the sequence it answers");
}

#[test]
fn survives_an_unknown_command() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(10));

    let actions = session.step(Input::Packet(from_device(1, FrameType::Cmd, &[77, 9])), Millis(20));

    assert!(session.is_connected(), "an unknown command must not drop the session");
    assert!(actions.iter().any(|a| matches!(a, Action::Emit(Event::Unknown { ty: 77, .. }))));
}

#[test]
fn resends_an_unacknowledged_command_then_gives_up() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(10));

    let first = session.request(Command::Mode(PowerMode::On), Millis(100));
    assert_eq!(sent(&first).len(), 1);

    let mut resends = 0;
    let mut now = 100u64;
    for _ in 0..8 {
        now += 1000;
        resends += sent(&session.step(Input::Tick, Millis(now))).len();
    }

    assert_eq!(resends, 4, "five attempts total means four resends");
}

#[test]
fn pings_every_three_seconds() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(0));

    assert!(sent(&session.step(Input::Tick, Millis(2_999))).is_empty());

    let actions = session.step(Input::Tick, Millis(3_000));
    let frame = Frame::parse(&sent(&actions)[0]).unwrap();
    let body = decrypt_frame(&device_keys(), &frame).unwrap();
    assert_eq!(body, vec![255], "ping command");
}

#[test]
fn declares_the_connection_lost_after_fifteen_silent_seconds() {
    let (mut session, _) = start();
    session.step(Input::Packet(from_device(0, FrameType::Cmd, &[0, 2, 0, 1, 4, 0])), Millis(0));

    assert!(session.step(Input::Tick, Millis(14_999)).iter().all(|a| !matches!(a, Action::Lost(_))));

    let actions = session.step(Input::Tick, Millis(15_000));
    assert!(actions.iter().any(|a| matches!(a, Action::Lost(LostReason::Silence))));
    assert!(!session.is_connected());
}

#[test]
fn treats_a_rejected_handshake_as_a_bad_token() {
    let (mut session, _) = start();

    let actions = session.step(Input::Packet(from_device(0, FrameType::Nak, &[])), Millis(10));

    assert!(actions.iter().any(|a| matches!(a, Action::Lost(LostReason::HandshakeRejected))));
}

#[test]
fn ignores_a_packet_it_cannot_decrypt() {
    let (mut session, _) = start();

    let actions = session.step(Input::Packet(vec![0x00, 0x01, 0x04, 0x00, 1, 2, 3, 4]), Millis(10));

    assert!(actions.is_empty(), "garbage on the wire is dropped silently");
    assert!(!session.is_connected());
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p syncleo --test session`
Ожидается: ошибка компиляции, нет модуля `session`.

- [ ] **Шаг 3: Реализация**

Написать `session.rs` по интерфейсу выше. Ключевые детали, которые тесты проверяют явно:

- `Session::new` сразу кладёт рукопожатие в очередь неподтверждённых и возвращает
  `Action::Send`.
- `step(Input::Packet(..))` парсит кадр, расшифровывает; при ошибке — пустой список
  действий, без паники и без смены состояния.
- `HandshakeResponse` переводит в подключённое состояние и даёт `Action::Connected`.
- Любой входящий `Cmd` порождает `Ack` тем же `seq` **и** `Action::Emit`.
- Входящий `Ack` снимает кадр с этим `seq` с переотправки.
- `Nak` до подключения — `Lost(HandshakeRejected)`.
- Таймеры считаются от `now`, переданного в `step`: последний входящий пакет, последний
  пинг, время и счётчик попыток по каждому неподтверждённому кадру.

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: все passed, 9 тестов в `session.rs`.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Add the session state machine with a virtual clock"
```

---

### Task 8: Транспорт, симулятор и сквозняк по loopback

**Файлы:** создать `crates/syncleo/src/transport.rs`, `crates/syncleo/src/simulator.rs`,
`crates/syncleo/src/client.rs`, `crates/syncleo/tests/loopback.rs`; изменить `lib.rs`

**Интерфейсы:**
- Потребляет: `Session`, `Action`, `Input`, `Command`, `Event`
- Производит:
  - `pub trait Transport { fn send(&mut self, bytes: &[u8]) -> std::io::Result<()>; fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<Vec<u8>>>; }`
  - `pub struct UdpTransport` + `UdpTransport::connect(addr: SocketAddr) -> std::io::Result<Self>`
  - `pub struct KettleSimulator` + `KettleSimulator::spawn(token: [u8; 16]) -> std::io::Result<KettleHandle>`
  - `pub struct KettleHandle { pub addr: SocketAddr, pub public_wire: [u8; 32] }` + `KettleHandle::state(&self) -> SimulatedState` + `KettleHandle::shutdown(self)`
  - `pub struct SimulatedState { pub mode: PowerMode, pub target: u8, pub current: u8, pub water: bool }`
  - `pub struct DeviceState { pub current_temperature: Option<u8>, pub target_temperature: Option<u8>, pub mode: Option<PowerMode>, pub water_present: Option<bool>, pub error: Option<bool>, pub child_lock: Option<bool> }`
  - `pub struct Client` + `Client::connect(transport: Box<dyn Transport>, our_private: [u8; 32], device_public_wire: [u8; 32], token: [u8; 16], timeout: Duration) -> Result<Client, Error>`
  - `Client::send(&mut self, cmd: Command) -> Result<(), Error>`
  - `Client::collect_state(&mut self, window: Duration) -> Result<DeviceState, Error>`
  - `Client::watch(&mut self, on_event: impl FnMut(Event) -> std::ops::ControlFlow<()>) -> Result<(), Error>`

Симулятор — это устройство-сторона протокола: слушает UDP, отвечает на рукопожатие,
подтверждает команды, шлёт состояние. Он нужен не только тестам — без него разработку
нельзя вести, когда чайника нет под рукой.

`Client` — тонкая обёртка: крутит цикл «recv → session.step → выполнить Action», настоящие
часы живут только здесь.

`Client::watch` отдаёт события наружу через колбэк, а не печатает их сам. Это то
требование из спеки, которое оставляет дорогу будущим уведомлениям.

- [ ] **Шаг 1: Написать падающие тесты**

`crates/syncleo/tests/loopback.rs`:

```rust
use std::time::Duration;
use syncleo::client::Client;
use syncleo::codec::command::{Command, PowerMode};
use syncleo::simulator::KettleSimulator;
use syncleo::transport::UdpTransport;

const OUR_PRIVATE: [u8; 32] = [11; 32];
const TOKEN: [u8; 16] = [0xA0; 16];

fn connect(handle: &syncleo::simulator::KettleHandle, token: [u8; 16]) -> Result<Client, syncleo::Error> {
    let transport = UdpTransport::connect(handle.addr).unwrap();
    Client::connect(
        Box::new(transport),
        OUR_PRIVATE,
        handle.public_wire,
        token,
        Duration::from_secs(3),
    )
}

#[test]
fn drives_the_simulated_kettle_over_real_udp() {
    let handle = KettleSimulator::spawn(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).expect("handshake succeeds");

    client.send(Command::Mode(PowerMode::Custom)).unwrap();
    client.send(Command::TargetTemperature(80)).unwrap();

    let state = handle.state();
    assert_eq!(state.mode, PowerMode::Custom);
    assert_eq!(state.target, 80);

    handle.shutdown();
}

#[test]
fn reads_state_back_from_the_device() {
    let handle = KettleSimulator::spawn(TOKEN).unwrap();
    let mut client = connect(&handle, TOKEN).unwrap();

    let state = client.collect_state(Duration::from_millis(500)).unwrap();

    assert!(state.current_temperature.is_some(), "device reports its temperature");
    assert!(state.water_present.is_some(), "device reports whether it holds water");

    handle.shutdown();
}

#[test]
fn a_wrong_token_is_rejected() {
    let handle = KettleSimulator::spawn(TOKEN).unwrap();

    let err = connect(&handle, [0xFF; 16]).expect_err("the device must refuse a bad token");

    assert!(
        matches!(err, syncleo::Error::HandshakeRejected),
        "a bad token must be distinguishable from a timeout, got {err:?}"
    );

    handle.shutdown();
}

#[test]
fn an_unreachable_device_times_out() {
    // Port 1 on loopback: nothing listens there.
    let transport = UdpTransport::connect("127.0.0.1:1".parse().unwrap()).unwrap();
    let err = Client::connect(
        Box::new(transport),
        OUR_PRIVATE,
        [0; 32],
        TOKEN,
        Duration::from_millis(300),
    )
    .expect_err("must not hang");

    assert!(matches!(err, syncleo::Error::Timeout), "got {err:?}");
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p syncleo --test loopback`
Ожидается: ошибка компиляции.

- [ ] **Шаг 3: Реализация**

Добавить в `crates/syncleo/src/error.rs`:

```rust
#[derive(Debug, Error)]
pub enum Error {
    #[error("device did not respond in time")]
    Timeout,
    #[error("device rejected the handshake; the token is probably wrong")]
    HandshakeRejected,
    #[error("connection lost: no traffic from the device")]
    Silence,
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

Реализовать `transport.rs`, `simulator.rs`, `client.rs` по интерфейсам выше. Симулятор
держит своё состояние за `Arc<Mutex<..>>`, чтобы `handle.state()` читал его из теста, и
живёт в отдельном потоке до `shutdown()`.

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: все passed, 4 теста в `loopback.rs`.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Add transport, kettle simulator and blocking client"
```

---

### Task 9: Дискавери по mDNS

**Файлы:** создать `crates/syncleo/src/discovery.rs`; изменить `lib.rs`

**Интерфейсы:**
- Производит:
  - `pub struct Found { pub mac: String, pub address: IpAddr, pub port: u16, pub public_wire: [u8; 32], pub curve: u8, pub protocol: u16 }`
  - `pub trait Discovery { fn find_all(&self, timeout: Duration) -> Result<Vec<Found>, Error>; fn find(&self, mac: &str, timeout: Duration) -> Result<Option<Found>, Error> }`
  - `pub struct MdnsDiscovery` + `MdnsDiscovery::new() -> Result<Self, Error>`
  - `pub fn parse_service(name: &str, addresses: &[IpAddr], port: u16, txt: &[(String, String)]) -> Result<Found, Error>` — чистая, тестируется без сети

Тип сервиса `_syncleo._udp.local.`; имя инстанса начинается с MAC; TXT-записи `public`
(hex), `curve` (обязано быть 29), `protocol` (обязано быть 2). Адреса из `169.254.0.0/16`
отбрасываются.

- [ ] **Шаг 1: Написать падающие тесты**

Блок тестов в `discovery.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn txt(public: &str, curve: &str, protocol: &str) -> Vec<(String, String)> {
        vec![
            ("public".into(), public.into()),
            ("curve".into(), curve.into()),
            ("protocol".into(), protocol.into()),
        ]
    }

    const PUBLIC: &str = "21d4043d930c3d75140c158c3406257204670512254e6e145eae239f354bdb57";

    #[test]
    fn reads_a_well_formed_service_record() {
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap();

        assert_eq!(found.mac, "aabbccddeeff");
        assert_eq!(found.port, 8888);
        assert_eq!(found.address, Ipv4Addr::new(192, 168, 1, 42).into());
        assert_eq!(found.public_wire.len(), 32);
    }

    #[test]
    fn skips_link_local_addresses() {
        let found = parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(169, 254, 3, 4).into(), Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .unwrap();

        assert_eq!(found.address, Ipv4Addr::new(192, 168, 1, 42).into(), "169.254/16 is useless here");
    }

    #[test]
    fn refuses_protocol_versions_it_was_not_written_for() {
        // Guessing at an unknown protocol version would be worse than saying so.
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "29", "3"),
        )
        .is_err());

        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt(PUBLIC, "30", "2"),
        )
        .is_err());
    }

    #[test]
    fn refuses_a_record_with_no_usable_address() {
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(169, 254, 3, 4).into()],
            8888,
            &txt(PUBLIC, "29", "2"),
        )
        .is_err());
    }

    #[test]
    fn refuses_a_malformed_public_key() {
        assert!(parse_service(
            "aabbccddeeff._syncleo._udp.local.",
            &[Ipv4Addr::new(192, 168, 1, 42).into()],
            8888,
            &txt("abcd", "29", "2"),
        )
        .is_err());
    }
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p syncleo discovery`
Ожидается: ошибка компиляции.

- [ ] **Шаг 3: Реализация**

`cargo add -p syncleo mdns-sd`. Расширить `Error` вариантами
`UnsupportedProtocol { curve: u8, protocol: u16 }`, `NoUsableAddress`, `BadServiceRecord(String)`.
`MdnsDiscovery::find_all` собирает события `mdns-sd` до истечения таймаута и прогоняет
каждое через `parse_service`, пропуская записи с ошибкой разбора.

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test -p syncleo`
Ожидается: все passed.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Discover devices over mDNS"
```

---

### Task 10: Конфиг и алиасы

**Файлы:** создать `crates/d3home/Cargo.toml`, `crates/d3home/src/main.rs`,
`crates/d3home/src/config.rs`; изменить корневой `Cargo.toml`

**Интерфейсы:**
- Производит:
  - `pub struct Config { pub devices: Vec<Device> }`
  - `pub struct Device { pub name: String, pub aliases: Vec<String>, pub driver: String, pub model: Option<String>, pub mac: String, pub token: String, pub cached: Option<Cached> }`
  - `pub struct Cached { pub address: IpAddr, pub port: u16, pub public_key: String }`
  - `pub const RESERVED: &[&str] = &["discover", "devices", "alias", "help"];`
  - `Config::load(path: &Path) -> Result<Config, ConfigError>` — вызывает `validate`
  - `Config::validate(&self) -> Result<(), ConfigError>`
  - `Config::resolve(&self, name: &str) -> Option<&Device>`
  - `Config::save(&self, path: &Path) -> Result<(), ConfigError>` — права 0600
  - `Config::default_path() -> PathBuf` — `$XDG_CONFIG_HOME/d3home/devices.toml`, иначе `~/.config/d3home/devices.toml`
  - `Device::token_bytes(&self) -> Result<[u8; 16], ConfigError>`
  - `pub enum ConfigError { Io, Parse, ReservedAlias { alias, device }, DuplicateAlias { alias, first, second }, AliasShadowsDevice { alias, device }, BadToken { device }, UnknownDevice { name } }`

- [ ] **Шаг 1: Написать падающие тесты**

Блок тестов в `config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    const KETTLE: &str = r#"
[[devices]]
name = "kettle"
aliases = ["k", "чайник"]
driver = "syncleo"
mac = "aabbccddeeff"
token = "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"
"#;

    #[test]
    fn resolves_a_device_by_name_or_alias() {
        let config = parse(KETTLE).unwrap();

        assert_eq!(config.resolve("kettle").unwrap().name, "kettle");
        assert_eq!(config.resolve("k").unwrap().name, "kettle");
        assert_eq!(config.resolve("чайник").unwrap().name, "kettle");
        assert!(config.resolve("teapot").is_none());
    }

    #[test]
    fn reads_the_cached_endpoint_when_present() {
        let config = parse(&format!(
            "{KETTLE}\n[devices.cached]\naddress = \"192.168.1.42\"\nport = 8888\npublic_key = \"ab\"\n"
        ))
        .unwrap();

        let cached = config.resolve("kettle").unwrap().cached.as_ref().unwrap();
        assert_eq!(cached.port, 8888);
    }

    #[test]
    fn refuses_an_alias_that_shadows_a_builtin_command() {
        // Silently losing `d3home discover` to an alias would be a nasty surprise.
        let toml = KETTLE.replace(r#"["k", "чайник"]"#, r#"["discover"]"#);
        assert!(matches!(parse(&toml), Err(ConfigError::ReservedAlias { .. })));
    }

    #[test]
    fn refuses_the_same_alias_on_two_devices() {
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"other\"\naliases = [\"k\"]\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"a0a1a2a3a4a5a6a7a8a9aaabacadaeaf\"\n"
        );
        assert!(matches!(parse(&toml), Err(ConfigError::DuplicateAlias { .. })));
    }

    #[test]
    fn refuses_an_alias_that_shadows_another_device_name() {
        let toml = format!(
            "{KETTLE}\n[[devices]]\nname = \"other\"\naliases = [\"kettle\"]\ndriver = \"syncleo\"\nmac = \"aa\"\ntoken = \"a0a1a2a3a4a5a6a7a8a9aaabacadaeaf\"\n"
        );
        assert!(matches!(parse(&toml), Err(ConfigError::AliasShadowsDevice { .. })));
    }

    #[test]
    fn parses_the_token_into_sixteen_bytes() {
        let config = parse(KETTLE).unwrap();
        assert_eq!(config.resolve("k").unwrap().token_bytes().unwrap(), [
            0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
            0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
        ]);

        let bad = KETTLE.replace("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf", "nothex");
        assert!(matches!(parse(&bad).unwrap().resolve("k").unwrap().token_bytes(), Err(_)));
    }

    #[test]
    fn saves_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("d3home-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");

        parse(KETTLE).unwrap().save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the file holds a device secret");

        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p d3home`
Ожидается: ошибка компиляции.

- [ ] **Шаг 3: Реализация**

Вернуть `crates/d3home` в `members` корневого `Cargo.toml`. Зависимости:
`serde` (фича `derive`), `toml`, `thiserror`, `syncleo`.

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test`
Ожидается: все passed.

- [ ] **Шаг 5: Коммит**

```bash
git add Cargo.toml crates/
git commit -m "Add the device registry with user-defined aliases"
```

---

### Task 11: CLI

**Файлы:** создать `crates/d3home/src/cli.rs`, `crates/d3home/src/output.rs`,
`crates/d3home/src/commands/kettle.rs`, `crates/d3home/src/commands/registry.rs`,
`crates/d3home/tests/cli.rs`; изменить `crates/d3home/src/main.rs`

**Интерфейсы:**
- Потребляет: `Config` (Task 10), `Client`, `KettleSimulator`, `MdnsDiscovery` (Task 8, 9)
- Производит:
  - `pub enum ExitCode { Ok = 0, Internal = 1, Usage = 2, NotFound = 3, BadToken = 4, Timeout = 5, DeviceError = 6 }`
  - `pub enum Parsed { Builtin(Builtin), Device { device: String, action: Vec<String> } }`
  - `pub fn parse(args: &[String]) -> Result<Parsed, UsageError>`

Первое слово — встроенная команда либо имя устройства или алиас. Встроенные приоритетнее,
что уже гарантировано проверкой конфига из Task 10.

`start` без аргумента шлёт `Mode(On)`. `start 80` шлёт `Mode(Custom)`, затем
`TargetTemperature(80)`. Порядок этих двух команд проверяется на живом устройстве в
Task 12; если чайник его не принимает, поменять местами и завести тест.

Температура валидируется в диапазоне 35–100 °C, выход за границы — `ExitCode::Usage`.

- [ ] **Шаг 1: Написать падающие тесты**

`crates/d3home/tests/cli.rs`, против симулятора и временного конфига:

```rust
use assert_cmd::Command;
use predicates::prelude::*;

mod support {
    use std::path::PathBuf;

    /// Write a config pointing at a simulator, with the endpoint pre-cached so
    /// the CLI never touches mDNS during tests.
    pub fn config_with(addr: std::net::SocketAddr, public_key: &str, token: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("d3home-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[[devices]]
name = "kettle"
aliases = ["k"]
driver = "syncleo"
mac = "aabbccddeeff"
token = "{token}"

[devices.cached]
address = "{}"
port = {}
public_key = "{public_key}"
"#,
                addr.ip(),
                addr.port()
            ),
        )
        .unwrap();
        path
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const TOKEN: [u8; 16] = [0xA0; 16];

#[test]
fn starts_the_kettle_at_a_chosen_temperature() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "start", "80"])
        .assert()
        .success();

    assert_eq!(handle.state().target, 80);
    handle.shutdown();
}

#[test]
fn an_alias_works_exactly_like_the_device_name() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "k", "start"])
        .assert()
        .success();

    assert_eq!(handle.state().mode, syncleo::codec::command::PowerMode::On);
    handle.shutdown();
}

#[test]
fn status_reports_machine_readable_state() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    let out = Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "--json", "kettle", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&out).expect("stdout is valid json");
    assert!(value.get("current_temperature").is_some());

    handle.shutdown();
}

#[test]
fn a_temperature_outside_the_supported_range_is_a_usage_error() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "start", "250"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("35"));

    handle.shutdown();
}

#[test]
fn a_wrong_token_exits_with_its_own_code() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&[0xFF; 16]));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "kettle", "status"])
        .assert()
        .code(4);

    handle.shutdown();
}

#[test]
fn an_unknown_device_name_is_a_usage_error() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "teapot", "status"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("teapot"));

    handle.shutdown();
}

#[test]
fn alias_add_and_remove_survive_a_reload() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));
    let path = config.to_str().unwrap();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "alias", "add", "чай", "kettle"]).assert().success();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "чай", "start"]).assert().success();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "alias", "rm", "чай"]).assert().success();

    Command::cargo_bin("d3home").unwrap()
        .args(["--config", path, "чай", "start"]).assert().code(2);

    handle.shutdown();
}

#[test]
fn refuses_to_create_an_alias_that_shadows_a_builtin() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "alias", "add", "discover", "kettle"])
        .assert()
        .code(2);

    handle.shutdown();
}

#[test]
fn devices_lists_what_is_configured_without_leaking_the_token() {
    let handle = syncleo::simulator::KettleSimulator::spawn(TOKEN).unwrap();
    let config = support::config_with(handle.addr, &hex(&handle.public_wire), &hex(&TOKEN));

    Command::cargo_bin("d3home")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "devices"])
        .assert()
        .success()
        .stdout(predicate::str::contains("kettle").and(predicate::str::contains("k")))
        .stdout(predicate::str::contains(&hex(&TOKEN)).not());

    handle.shutdown();
}
```

- [ ] **Шаг 2: Убедиться, что тесты падают**

Запустить: `cargo test -p d3home --test cli`
Ожидается: ошибка компиляции или падение сборки бинарника.

- [ ] **Шаг 3: Реализация**

Зависимости: `clap` (фича `derive`), `serde_json`; `assert_cmd`, `predicates`,
`serde_json` в dev; `syncleo` с фичей `simulator` в dev. Флаг `--config <path>`
перекрывает `Config::default_path()` — без него тесты трогали бы настоящий конфиг.

Порядок работы команды устройства: загрузить конфиг → разрешить имя → взять кэш или
дискавери → `Client::connect` → команда → вывод → код возврата. Ошибки `syncleo::Error`
отображаются в коды возврата один в один по таблице из спеки.

- [ ] **Шаг 4: Тесты зелёные**

Запустить: `cargo test`
Ожидается: все passed.

- [ ] **Шаг 5: Коммит**

```bash
git add crates/
git commit -m "Add the d3home command line interface"
```

---

### Task 12: Проверка на живом чайнике

**Файлы:** создать `README.md`; изменить что потребуется по результатам

Всё до этой задачи проверялось против симулятора, написанного по тому же пониманию
протокола, что и клиент. Общая ошибка в этом понимании симулятором не ловится — только
живым устройством.

- [ ] **Шаг 1: Убедиться, что чайник виден**

Запустить: `cargo run -p d3home -- discover`

Ожидается: MAC `aabbccddeeff`, адрес, порт. Если пусто — проверить, что машина в одной
сети с чайником и что mDNS не режется. **Не запускать под песочницей, режущей мультикаст:
именно из-за неё скан на этапе дизайна не увидел ни одного сервиса.** Если mDNS до
устройства не доходит принципиально, вписать `address`, `port` и `public_key` в
`[devices.cached]` руками и завести задачу на команду для ручного ввода.

- [ ] **Шаг 2: Прочитать состояние**

Запустить: `cargo run -p d3home -- kettle status`
Ожидается: правдоподобная текущая температура и наличие воды.

- [ ] **Шаг 3: Проверить управление**

```bash
cargo run -p d3home -- kettle start 60
cargo run -p d3home -- kettle status     # цель 60, чайник греет
cargo run -p d3home -- kettle off
```

Здесь выясняются две вещи, заложенные в план как предположения:

- **Порядок команд в `start 80`.** Если чайник игнорирует целевую температуру, поменять
  местами `Mode(Custom)` и `TargetTemperature` и закрепить тестом в `loopback.rs`.
- **Реальные границы температуры.** Диапазон 35–100 взят с потолка. Прощупать, где
  устройство отказывает, и поправить валидацию с тестом на новые границы.

- [ ] **Шаг 4: Проверить, что приложение не сломалось**

Открыть Polaris IQ Home на телефоне, убедиться, что чайник виден и управляется. Это
жёсткое требование спеки, и проверить его можно только руками.

- [ ] **Шаг 5: На каждое расхождение — тест**

Любое поведение живого устройства, разошедшееся с симулятором, чинится в таком порядке:
тест в `loopback.rs`, воспроизводящий реальное поведение → правка симулятора → правка
клиента. Симулятор без этого расходится с железом и обесценивает все тесты.

- [ ] **Шаг 6: README**

Описать по-русски: установку, где лежит конфиг и что токен берётся из ссылки
device-share в приложении, набор команд, алиасы, коды возврата. Отдельным абзацем —
что штатное приложение продолжает работать и настройки устройства не меняются.

- [ ] **Шаг 7: Коммит**

```bash
git add -A
git commit -m "Verify against real hardware and document usage"
```

---

## Самопроверка плана

- **Покрытие спеки.** Транспорт — Task 8; дискавери — 9; обмен ключами — 3; кадр — 2;
  шифрование — 4; рукопожатие — 6; команды — 5; тайминги и автомат — 7; конфиг и
  алиасы — 10; CLI, коды возврата и `--json` — 11; golden-векторы — 4 и 6; property —
  4; симулятор и сквозняк — 8; живое железо — 12. Требование «события поднимаются из
  `session` наружу потоком, а не печатаются внутри `watch`» — `Client::watch` в Task 8.
- **Плейсхолдеры.** Нет: во всех шагах либо готовый код, либо точный список того, что
  проверяют уже написанные тесты. Два места намеренно оставлены под проверку железом
  (порядок команд в `start`, границы температуры) — оба явно названы в Task 12 с
  указанием, что делать по итогу.
- **Согласованность типов.** `SessionKeys`, `Frame`, `FrameType`, `CodecError`,
  `Command`, `Event`, `PowerMode`, `Session`, `Action`, `Input`, `Millis`, `Transport`,
  `Client`, `DeviceState`, `Found`, `Config`, `Device`, `Cached` объявлены один раз и
  используются под теми же именами дальше.
