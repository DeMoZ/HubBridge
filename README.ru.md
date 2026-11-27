# HubBridge

**Android-смартфон с USB-C хабом превращается в мост по Wi-Fi/LAN: USB-устройства хаба (а
дальше и дисплеи, и карты захвата) становятся доступны на компьютере с macOS или Windows.**

Android-агент (Kotlin + ядро на Rust) · десктоп-хост для macOS, Windows, Linux (Rust) · MIT

[English version](README.md)

> Состояние: **шаг 1 из 4** — поиск в сети, сопряжение по PIN, шифрованные сессии, список
> USB-устройств с дескрипторами, события подключения/отключения и RTT по Wi-Fi/LAN и через
> `adb forward`. Дальше — проброс USB-данных, видео и приложение в трее
> ([план](docs/ROADMAP.md)).

## Сначала прочитайте

**[Что работает и что нет](docs/LIMITATIONS.md)**. Главное: у телефона с одним USB-C портом
этот порт либо хост для хаба, либо устройство для ПК — одновременно нельзя. Практичная
проводная связь — USB-Ethernet адаптер в хабе. Видео *в* телефон по USB-C не принимается
(DP Alt Mode у телефонов только на выход) — нужна UVC-карта захвата HDMI→USB.

Подробнее: [архитектура](docs/ARCHITECTURE.md) · [протокол](docs/PROTOCOL.md).

## Попробовать без телефона

```bash
cargo run -p hubbridge-agent-sim -- --pin 123456        # терминал 1: фейковый агент
cargo run -p hubbridge-host -- discover                  # терминал 2
cargo run -p hubbridge-host -- pair 127.0.0.1 --pin 123456
cargo run -p hubbridge-host -- devices
cargo run -p hubbridge-host -- watch
```

## С телефоном

1. `cd android && ./gradlew installDebug` (нужны Android SDK, JDK 17+, `cargo-ndk` и
   Android-таргеты Rust — см. README.md).
2. На телефоне: HubBridge → **Start: Wi-Fi/LAN + ADB** → **Pair a new computer**.
3. На компьютере: `hubbridge discover`, `hubbridge pair <имя или IP> --pin <PIN>`,
   `hubbridge devices`, `hubbridge watch`. По кабелю: `hubbridge pair adb --pin <PIN>`.
