# Orbweaver hidraw lighting spike (2026-11-18)

Device: USB `1532:0207`, product string **Razer Orbweaver Chroma** (bcdDevice 0200, firmware 1.02). It has per-key RGB; it is not the single-colour original Orbweaver.

Access: udev rule `/etc/udev/rules.d/70-orbweaver-hidraw.rules` (`TAG+="uaccess"`) gives the seat user rw on `hidraw7`–`9`. Verified by opening each node `+<` as the user.

## Interfaces

| Node | Interface | Report descriptor |
|---|---|---|
| hidraw7 | input0 | boot keyboard, no report ID |
| hidraw8 | input1 | report IDs 1 keyboard, 2 consumer, 3 system control, 4 and 5 vendor 15-byte input |
| hidraw9 | input2 | mouse, plus a vendor 90-byte **feature report** (no report ID): the control channel |

## Protocol (measured with `razer.pl` on hidraw9)

Razer's standard 90-byte report: `status, tid, remaining(2), proto, size, class, id, args[80], crc (xor of bytes 2..87), 0`, sent with `HIDIOCSFEATURE(91)` and read back with `HIDIOCGFEATURE(91)` after ~60 ms. Any transaction id (`ff`, `3f`, `1f`) is answered. Status `02` = ok. All of this is perl-base (`ioctl`, `pack`): no extra runtime and no openrazer DKMS needed.

| Command | class/id | args | Result |
|---|---|---|---|
| firmware | 00/81 | — | `01 02` (v1.02) |
| device mode get / set | 00/84, 00/04 | `<mode> 00` | ok; normal `00`, driver `03` (read back) |
| backlight brightness get / set | 03/83, 03/03 | `01 05 <0–ff>` | ok, read back (stock `ff`) |
| profile LED get / set | 03/80, 03/00 | `01 <led> <0/1>` | ok, read back; led `0c` green, `0d` blue, `0e` red (stock: red on) |
| matrix effect static | 03/0a | `06 r g b` | ok, visible |
| matrix effect spectrum | 03/0a | `04` | ok (left set at the end of the spike) |
| custom frame row | 03/0b | `ff <row> <start col> <end col> rgb…` | ok, any row accepted (no validation) |
| apply custom frame | 03/0a | `05 01` | ok, visible |
| extended matrix (class 0f) | 0f/84 | — | **fail**: this device uses only the standard class `03` |

## Key matrix (user-observed)

The 20-key pad is lit at matrix **rows 1–4, columns 1–5**; matrix row 0, row 5 and column 0 are unused. In the bottom row, keys 17–20 are at **columns 3–6** (two columns further right than the rows above). The thumb cluster showed no colour from rows 0–5 / columns 0–6.

## Not resolved

- **Keymap/mode button:** no hidraw input report arrived on hidraw7–9 in normal mode or in driver mode (`03`) during two 3-minute listens. The user could not find the button, so this is inconclusive, not a negative.
- Key 16's matrix position, and whether the thumb cluster has its own LED zone.

## Consequence for a plan

CDE can drive the pad's lighting directly: per-key colour (custom frame), brightness and the three profile LEDs, over hidraw with the uaccess rule and no driver. A follow-up plan could map, for example, the active `input-device` layer or workspace onto the profile LEDs or a key colour.
