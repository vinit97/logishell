# device support

Support follows reported HID++ capabilities, without a model allowlist.
A device appearing in status does not mean every feature is supported.

| Connection | Support and limits |
| --- | --- |
| Bluetooth | Discovery, pairing, connections, BlueZ battery; settings need an accessible HID++ interface |
| Bolt | Paired devices, battery, settings, authenticated pairing/unpairing |
| Unifying | Paired devices, HID++ 1/2 battery/settings, pairing/unpairing |
| Direct USB | HID inventory and HID++ settings; changes require verifiable device identity |
| Other receivers | Per-device HID++ interfaces exposed by Linux; receiver pairing unavailable |

Charging-only cables do not create USB input connections. Gaming, LIGHTSPEED,
Nano, and older devices may expose only inventory or limited settings.

## configuration by capability

| Setting | Required capability |
| --- | --- |
| DPI | `0x2201`; range and steps reported by the sensor |
| Wheel mode / SmartShift | `0x2110` or `0x2111` |
| Scroll inversion | `0x2121` with inversion support |
| Thumb-wheel inversion and directional actions | `0x2150` version 0 |
| Fn inversion | `0x40a0`, `0x40a2`, `0x40a3`, or legacy Fn register |
| Backlight | `0x1982` version 2 or 3 |
| Haptic strength | `0x19b0`; preserves the haptic enable state |
| Button/key bindings | `0x1b04` and individually divertible controls |

Use **setup → Device details** or `config get` to inspect capabilities. Status
reads identity, connection, and battery only. Unknown battery is never shown as
0%; coarse levels remain coarse.

Bindings need the daemon and virtual-input permission. Ordinary typing keys and
primary mouse buttons are not assumed to be remappable. Haptic strength changes
include a test pulse when supported. Haptic actions, Actions Ring, macros, RGB,
application profiles, and firmware updates are unsupported.

## validation status

| Hardware | Physically checked |
| --- | --- |
| MX Master 4 over Bolt | Status/battery; user tested DPI, scroll inversion, wheel mode, SmartShift, and sensitivity changes; haptic strength write/readback verified, perceived intensity pending; binding activation/readback verified; thumb-wheel capability/settings reads verified, inversion writes and directional actions pending |
| MX Mechanical Mini over Bolt | Status/battery, firmware, Fn and backlight reads; writes unverified |
| Other devices/transports, pairing, mapped desktop actions | Simulated tests only; physical verification pending |

## protocol references

Independent implementation; no OpenLogi or Solaar application code is vendored.
Solaar links describe interoperability research, including Bolt, rather than an
official Logitech specification.

- [Logitech HID++ documentation](https://github.com/Logitech/cpg-docs), [HID++ 1.0 / Unifying](https://lekensteyn.nl/files/logitech/logitech_hidpp10_specification_for_Unifying_Receivers.pdf), and [reprogrammable controls](https://lekensteyn.nl/files/logitech/x1b04_specialkeysmsebuttons.html).
- Linux [hidraw](https://www.kernel.org/doc/html/latest/hid/hidraw.html), [uinput](https://www.kernel.org/doc/html/latest/input/uinput.html), [HID++ driver](https://github.com/torvalds/linux/blob/master/drivers/hid/hid-logitech-hidpp.c), and [receiver driver](https://github.com/torvalds/linux/blob/master/drivers/hid/hid-logitech-dj.c).
- BlueZ [devices](https://bluez.readthedocs.io/en/latest/device-api/), [adapters](https://bluez.readthedocs.io/en/latest/adapter-api/), and [pairing agents](https://bluez.readthedocs.io/en/latest/agent-api/).
- Solaar [receiver research](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/receiver.py), [registers](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/hidpp10_constants.py), and [notifications](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/notifications.py).
- Solaar [legacy registers](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/hidpp10.py) and [setting research](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/settings_templates.py).
- Keyboard control labels use [published control identifiers](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/special_keys.py) as interoperability data; they do not imply a fixed F-key position or an assigned action.
- OpenLogi [haptic protocol research](https://openlogi.org/hidpp/features/x19b0-haptic-feedback).
- Logitech [thumb-wheel events](https://logitech.github.io/hackathons/devmon/api/#divert-thumb-wheel-events) and OpenLogi [thumb-wheel protocol research](https://openlogi.org/hidpp/features/x2150-thumbwheel).
