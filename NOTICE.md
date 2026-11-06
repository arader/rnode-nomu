# Notices

rnode-nomu is free software: you can redistribute it and/or modify it under the terms of the GNU
General Public License as published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version. [LICENSE](LICENSE) has the full text.

Parts of rnode-nomu are translations into Rust of
[RNode_Firmware](https://github.com/markqvist/RNode_Firmware) v1.86 (commit `56775c5`). They keep
the notices of the files they were translated from, below.

## RNode_Firmware

`crates/rnode-core` and its tests translate the firmware's host protocol, TX queue, split framing,
CSMA/CA, airtime and PHY timing, EEPROM image, external power amplifier tables and main loop from
`RNode_Firmware.ino`, `Utilities.h`, `Config.h`, `Framing.h` and `Boards.h`, which carry this
notice:

```text
Copyright (C) 2024, Mark Qvist

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <https://www.gnu.org/licenses/>.
```

## RNode_Firmware's SX126x driver

`crates/rnode-sx126x` translates the firmware's SX126x modem driver, `sx126x.cpp`, which is under
the MIT license. The crate is distributed as part of rnode-nomu under GPL-3.0-or-later, so its
`license` field is `GPL-3.0-or-later AND MIT`.

```text
Copyright Sandeep Mistry, Mark Qvist and Jacob Eva.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Referenced, not included

The daemon (`crates/rnode-nomu`), the simulated radio (`crates/rnode-sim`), the end-to-end
harness (`crates/xtask`, `tests/e2e`) and the hardware test tools (`tools/hwtest`) cite
RNode_Firmware and Reticulum for the behaviour they match, but are not translations of them.

[Reticulum](https://github.com/markqvist/Reticulum) is not part of this repository: the
end-to-end test downloads a pinned commit when it runs, and the hardware test tools import it as an
installed package. It is under the Reticulum License.
