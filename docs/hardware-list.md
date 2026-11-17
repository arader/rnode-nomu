# Hardware list

This list shows which LoRa hardware works with Reticulum through rnode-nomu, which works without
it, and which does not work yet, and how far each has been tested.
[Setting up the LoRa hardware](hardware.md) explains how to set up a board.

Whatever the hardware, the RNode logic itself (the KISS protocol, the framing, CSMA/CA and the PHY
timing) is checked on every pull request against values derived from RNode_Firmware, and with
Reticulum on the [simulated radio](development.md#the-simulated-radio) (see
[Tests](development.md#tests)). The tables below are about the hardware.

## Reading the tables

**Needs rnode-nomu?**

- **Yes:** Linux drives the bare modem over SPI and GPIO, and rnode-nomu makes it an RNode.
- **No:** a microcontroller on the hardware drives the modem. Reticulum uses the hardware without
  rnode-nomu: when it runs RNode_Firmware, through `RNodeInterface` on its serial port.

**Tested**

- **Validated:** rnode-nomu ran on this hardware with Reticulum and exchanged packets over the air
  with a hardware RNode, and the results are written down.
- **Interoperates:** hardware that does not run rnode-nomu, but has exchanged packets with it over
  the air.
- **Untested:** going by the modem and its wiring, rnode-nomu should work, and a config or the
  steps to write one exist, but nobody has reported running it on this hardware.
  [Open an issue](https://github.com/arader/rnode-nomu/issues) with what you find, whether it
  works or not.

## Hardware rnode-nomu drives

| Hardware | Modem | Needs rnode-nomu? | Tested | Notes |
| --- | --- | --- | --- | --- |
| Clockwork uConsole CM5 with the HackerGadgets AIO V2 | SX1262 | Yes | Validated: the reference board | [`config.uconsole-aio-v2.toml`](../config.uconsole-aio-v2.toml); [setup and results](hardware.md#uconsole-cm5-with-the-hackergadgets-aio-v2) |
| Clockwork uConsole CM5 with the HackerGadgets AIO V1 | SX1262 | Yes | Untested | [`config.uconsole-aio-v1.toml`](../config.uconsole-aio-v1.toml), the V2 config without the power rail; [notes](hardware.md#uconsole-with-the-hackergadgets-aio-v1) |
| Clockwork uConsole CM4 with the AIO V1 or V2 | SX1262 | Yes | Untested | The AIO configs with `chip = "pinctrl-bcm2711"`, and SPI1 enabled as the [AIO V1 notes](hardware.md#uconsole-with-the-hackergadgets-aio-v1) describe |
| A Linux board with an SX1262 module on its SPI bus, such as a Raspberry Pi with a Waveshare Core1262 or an Ebyte E22-900M22S | SX1262 | Yes | Untested | [Mapping your own board](hardware.md#mapping-your-own-board) |
| SX1262 modules with a power amplifier, such as the Ebyte E22-900M30S, which the MeshAdv Pi Hat carries | SX1262 and a PA | Yes | Untested: no board with a PA has been tested yet ([#2](https://github.com/arader/rnode-nomu/issues/2)) | [`[radio.pa]`](configuration.md#radiopa) with the module's gain table; `rxen`, and `txen` where a host GPIO drives TXEN. The MeshAdv Pi Hat's chip select is GPIO 21, which SPI0 gets with `dtoverlay=spi0-1cs,cs0_pin=21` |
| SX1268 modules (410–493 MHz), such as the Ebyte E22-400M series | SX1268 | Yes, with the `sx1262` driver | Untested | RNode_Firmware drives its SX1268 boards with its SX1262 code |
| Flipper One with an SX1262 module of your own on its GPIO header | SX1262 | Yes | Untested: ships from July 2027 | See [Flipper One](#flipper-one) |

## Hardware with its own microcontroller

| Hardware | Modem | Needs rnode-nomu? | Tested | Notes |
| --- | --- | --- | --- | --- |
| Boards running RNode_Firmware, such as the Heltec LoRa32 V3 and V4, the LilyGO T-Beam and T3S3, the RAK4631, and Seeed's XIAO ESP32S3 with the Wio-SX1262 | SX1262, SX1268, SX1276, SX1278 or SX1280 | No | Heltec LoRa32 V3: interoperates. The others: untested with rnode-nomu | Reticulum uses them through `RNodeInterface` on their serial port; the firmware lists [the boards it supports](https://github.com/markqvist/RNode_Firmware#supported-boards--devices). A Heltec LoRa32 V3 has been the other end of rnode-nomu's tests throughout development ([How this was made](../README.md#how-this-was-made)) |
| Flipper One LoRa Module | SX1262, with an RP2350 microcontroller | Probably not | Untested: ships from July 2027 | See [Flipper One](#flipper-one) |

## Not supported yet

| Hardware | Modem | What it would take |
| --- | --- | --- |
| SX1276, SX1277 and SX1278 modules, such as the HopeRF RFM95W | SX127x | A driver following the firmware's `sx127x.cpp`; [Radio drivers](drivers.md#adding-a-new-radio-an-sx127x-driver) walks through one |
| SX1280 and SX1281 modules for 2.4 GHz, such as the Ebyte E28-2G4M20S | SX128x | A driver following the firmware's `sx128x.cpp` |
| LR1110 and LR1121 modules, such as the Ebyte E80 series and Seeed's Wio-LR1121 | LR11xx | A driver with no RNode firmware to follow: neither RNode_Firmware nor RNode_Firmware_CE supports these chips |
| USB sticks with a CH341 bridge and an SX1262, such as the MeshStick, MeshToad and RAK19714 | SX1262 | A way to the modem's reset, BUSY and DIO1, which are on the bridge's pins: Linux's `spi-ch341` driver offers SPI but no GPIO lines, so an out-of-tree kernel driver or a CH341 bus in rnode-nomu. Every modem command would then cross USB |
| LoRaWAN concentrators, such as the RAK2287, RAK5146, Seeed WM1302 and RAK5166 | SX1302 or SX1303 | A driver for a multi-channel gateway chip, which offers only 125, 250 and 500 kHz |

## Not usable

| Hardware | Why |
| --- | --- |
| LoRa modules and HATs with their own firmware behind a UART, such as Waveshare's SX1262/SX1268 LoRa HAT for the Raspberry Pi and Ebyte's E22-…T and E220 modules | The module's firmware owns the modem and sends its own packet format; rnode-nomu needs the bare modem on SPI |

## Flipper One

[Flipper One](https://www.kickstarter.com/projects/flipper-devices/flipper-one) is a Linux handheld:
a Rockchip RK3576 running Flipper OS, which is based on Debian 13, with a 20-pin GPIO header and an
M.2 slot. Flipper launched its Kickstarter campaign in November 2026 and expects to ship from July
2027. Flipper One has no LoRa radio of its own. As of November 2026 there are two ways to add one;
[#5](https://github.com/arader/rnode-nomu/issues/5) tracks both.

- **Flipper's LoRa Module** mounts on the back and connects through the GPIO header. It has its own
  microcontroller, an RP2350, with an SX1262, a switchable LNA (+12 dB) and up to +28 dBm, for
  863–928 MHz with 868 and 915 MHz band-pass filters. Flipper says it works with Reticulum,
  MeshCore and Meshtastic out of the box. Neither RNode_Firmware nor RNode_Firmware_CE runs on the
  RP2350, and Flipper has not yet published the module's firmware or how it presents itself to
  Linux, so whether rnode-nomu has a part in it is open.
- **An SX1262 module of your own on the GPIO header** is the case rnode-nomu is for: the header's
  12 SoC pins can carry SPI3 or SPI4 and the modem's control lines, all on one GPIO chip. It needs a
  device-tree overlay that routes SPI to the header, which Flipper's kernel does not have yet, and
  a module that matches the header's logic level, which Flipper's documentation does not give (1.8
  or 3.3 V). rnode-nomu's [Debian package](installing.md#debian-package) is built for arm64 Debian
  12 or newer, which includes Flipper OS.
