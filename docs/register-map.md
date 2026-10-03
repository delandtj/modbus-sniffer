# WFI-028T/035T register map

Working register map for the Modbus RTU bus between the wired controller and the
WFI-028T/035T full-inverter pool heat pump (380-415V/3N). This is the spec for
the replacement controller. Every entry says where it comes from; only
"confirmed" entries are safe to act on.

Sources, in order of trust:

1. **Capture** - sniffed traffic in /var/lib/modbus-sniffer (start of the real
   capture is marked by the note "CAPTURE START real bus", 2026-10-03 14:51).
2. **WFI sheet** - `modbus-protocol-wfi.pdf` / `.txt`, "Tri-phase" section.
   Matches the bus for 0x003f bit 0 and 0x0040, but is WRONG about boost (see
   below), so verify every entry before relying on it.
3. **Manual** - `wfi-028t-035t-manual.pdf`: user parameters P01-P05, running
   values A01-A14, error codes E04..E27.
4. **Generic protocol** - `communication-protocol-v1.3.2-eng.docx` / `.txt`.
   Written for an air-to-water heat pump; its map does NOT match this bus
   (it calls 0x003f "Reserve" and 0x0040 "Compressor frequency", read-only,
   while the controller writes both). Possibly the same PCB maker. Its extra
   areas (user parameters 0x0300, version 0x0360-0x0363, coil commands
   0x01/0x05) are untested on this unit - see "Open questions".

## Bus

| Item | Value | Source |
|---|---|---|
| Physical | RS-485 2-wire, controller cable green/yellow (red/black = 12V supply) | capture |
| Framing | Modbus RTU, 9600 baud, 8N1 | capture + WFI sheet |
| Roles | controller = master, heat pump PCB = slave 0x01 | capture + WFI sheet |
| Functions seen | 0x03 read holding, 0x10 write multiple | capture |
| Scaling | temperatures x10 per the generic doc ("255 = 25.5") - NOT yet verified on this unit; the status values below look like whole degrees | generic doc |

## Controller behaviour (what a replacement must imitate)

Steady state, one cycle per second (capture):

| Offset | Frame |
|---|---|
| +0 ms | `01 03 0000 003f` - read 63 registers 0x0000-0x003e (status block) |
| ~+150 ms | heat pump reply, 126 data bytes |
| +500 ms | `01 03 003f 0043` - read 67 registers 0x003f-0x0081 (settings block) |
| ~+660 ms | heat pump reply, 134 data bytes |

On a settings change the controller writes the WHOLE settings block
(`01 10 003f 0043 86 <134 bytes>`) three times, 500 ms apart, each acked by
the heat pump ~24 ms later with `01 10 003f 0043`. The changed value is
visible in the next poll.

Per the WFI sheet the stock controller must be unplugged when an external
master is used ("If adopt Modbus, the controller should be unplug to the PCB").

## Confirmed

| Register | Bits / values | Meaning | Evidence |
|---|---|---|---|
| 0x003f | bit 0 | 1 = unit on, 0 = off | WFI sheet; capture shows 1 while running (OFF not yet captured) |
| 0x003f | bit 6 | 1 = normal/ECO, 0 = full power (BOOST) | capture 14:52:47-14:53:53, four toggles; WFI sheet says boost is 0x0040 bit 4, which did NOT change |
| 0x0040 | value | 1 = heating, 2 = cooling, 7 = auto | WFI sheet; capture shows 1 while in heating |
| 0x0004 | bit 7 | set while full power is active (heat pump's confirmation) | capture, follows every 0x003f bit 6 toggle |

## Candidates (unverified)

- 0x0042 = 27: matches P01 (heating setpoint) factory default 27 C. Verify by
  changing the setpoint.
- Status registers that vary with time (live sensors). Map them by reading
  A01-A14 on the controller (hold the button 3 s for "Parameter checking"),
  noting the time, and matching:

| Addr | Range seen | Guess |
|---|---|---|
| 0x000f | 274..275 | ? |
| 0x0010 | 58..60 | temperature? |
| 0x0011 | 39..42 | temperature? |
| 0x0012 | 8..14 | water/ambient temperature? |
| 0x0013 | 11..22 | piping temperature? |
| 0x0014 | 59..69 | exhaust/discharge temperature? |
| 0x0015 | 72..79 | ? |
| 0x0018 | 123..152 | EEV aperture or radiator temperature? |
| 0x001a | 55..72 | compressor frequency (Hz)? |
| 0x001b | 54..72 | compressor frequency target? |
| 0x001e | 537..550 | voltage or EEV steps? |
| 0x0020 | 8..10 | compressor current (A)? |
| 0x0024 | 706..859 | fan speed (r/min)? |

## Open questions

- Setpoints P01-P05, timers, clock, auto mode: change each on the controller
  with a note, then diff the settings block.
- On/off: capture a power-off to confirm 0x003f bit 0.
- Error/fault bits: wait for a real fault or defrost (multi-day capture).
- Does the PCB also answer the generic protocol (version at 0x0360-0x0363,
  user area 0x0300, coils)? Test with a single read in a polling gap, once a
  transmitting setup exists.
- Temperature scaling: whole degrees or x10? Settle with the A01-A14 cross-check.

## Snapshot: status block 0x0000-0x003e

Last value at 2026-10-03 15:32, plus the range over 4921 polls since 14:51.

| Addr | Dec | Last hex | u16 | s16 | /10 | Range in capture | Note |
|---|---|---|---|---|---|---|---|
| 0x0000 | 0 | 0x2020 | 8224 | 8224 | 822.4 | constant |  |
| 0x0001 | 1 | 0x0404 | 1028 | 1028 | 102.8 | constant |  |
| 0x0002 | 2 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0003 | 3 | 0x000d | 13 | 13 | 1.3 | constant |  |
| 0x0004 | 4 | 0x0021 | 33 | 33 | 3.3 | 33..161 (2 values) |  |
| 0x0005 | 5 | 0x0080 | 128 | 128 | 12.8 | constant |  |
| 0x0006 | 6 | 0x0014 | 20 | 20 | 2 | constant |  |
| 0x0007 | 7 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0008 | 8 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0009 | 9 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000a | 10 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000b | 11 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000c | 12 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000d | 13 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000e | 14 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x000f | 15 | 0x0113 | 275 | 275 | 27.5 | 274..275 (2 values) |  |
| 0x0010 | 16 | 0x003b | 59 | 59 | 5.9 | 58..60 (3 values) |  |
| 0x0011 | 17 | 0x0028 | 40 | 40 | 4 | 39..42 (4 values) |  |
| 0x0012 | 18 | 0x000c | 12 | 12 | 1.2 | 8..14 (7 values) |  |
| 0x0013 | 19 | 0x000e | 14 | 14 | 1.4 | 11..22 (12 values) |  |
| 0x0014 | 20 | 0x0041 | 65 | 65 | 6.5 | 59..69 (11 values) |  |
| 0x0015 | 21 | 0x004c | 76 | 76 | 7.6 | 72..79 (8 values) |  |
| 0x0016 | 22 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0017 | 23 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0018 | 24 | 0x0082 | 130 | 130 | 13 | 123..152 (22 values) |  |
| 0x0019 | 25 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x001a | 26 | 0x0037 | 55 | 55 | 5.5 | 55..72 (18 values) |  |
| 0x001b | 27 | 0x0036 | 54 | 54 | 5.4 | 54..72 (16 values) |  |
| 0x001c | 28 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x001d | 29 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x001e | 30 | 0x021d | 541 | 541 | 54.1 | 537..550 (5 values) |  |
| 0x001f | 31 | 0x0050 | 80 | 80 | 8 | constant |  |
| 0x0020 | 32 | 0x0008 | 8 | 8 | 0.8 | 8..10 (3 values) |  |
| 0x0021 | 33 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0022 | 34 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0023 | 35 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0024 | 36 | 0x02d2 | 722 | 722 | 72.2 | 706..859 (35 values) |  |
| 0x0025 | 37 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0026 | 38 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0027 | 39 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0028 | 40 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0029 | 41 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x002a | 42 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x002b | 43 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x002c | 44 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x002d | 45 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x002e | 46 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x002f | 47 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0030 | 48 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0031 | 49 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0032 | 50 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0033 | 51 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0034 | 52 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0035 | 53 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0036 | 54 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0037 | 55 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0038 | 56 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0039 | 57 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003a | 58 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003b | 59 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003c | 60 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003d | 61 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003e | 62 | 0x77ff | 30719 | 30719 | 3071.9 | constant |  |

## Snapshot: settings block 0x003f-0x0081

Same window. The two changing entries in 0x003f are the full power toggles.

| Addr | Dec | Last hex | u16 | s16 | /10 | Range in capture | Note |
|---|---|---|---|---|---|---|---|
| 0x003f | 63 | 0x1071 | 4209 | 4209 | 420.9 | 4145..4209 (2 values) |  |
| 0x0040 | 64 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x0041 | 65 | 0x0021 | 33 | 33 | 3.3 | constant |  |
| 0x0042 | 66 | 0x001b | 27 | 27 | 2.7 | constant |  |
| 0x0043 | 67 | 0x0032 | 50 | 50 | 5 | constant |  |
| 0x0044 | 68 | 0x0096 | 150 | 150 | 15 | constant |  |
| 0x0045 | 69 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0046 | 70 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0047 | 71 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0048 | 72 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0049 | 73 | 0x01f4 | 500 | 500 | 50 | constant |  |
| 0x004a | 74 | 0x001b | 27 | 27 | 2.7 | constant |  |
| 0x004b | 75 | 0x000a | 10 | 10 | 1 | constant |  |
| 0x004c | 76 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x004d | 77 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x004e | 78 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x004f | 79 | 0xffec | 65516 | -20 | -2 | constant |  |
| 0x0050 | 80 | 0x0028 | 40 | 40 | 4 | constant |  |
| 0x0051 | 81 | 0xfffa | 65530 | -6 | -0.6 | constant |  |
| 0x0052 | 82 | 0x000b | 11 | 11 | 1.1 | constant |  |
| 0x0053 | 83 | 0x0010 | 16 | 16 | 1.6 | constant |  |
| 0x0054 | 84 | 0x0006 | 6 | 6 | 0.6 | constant |  |
| 0x0055 | 85 | 0x0011 | 17 | 17 | 1.7 | constant |  |
| 0x0056 | 86 | 0x001e | 30 | 30 | 3 | constant |  |
| 0x0057 | 87 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x0058 | 88 | 0x0058 | 88 | 88 | 8.8 | constant |  |
| 0x0059 | 89 | 0x0028 | 40 | 40 | 4 | constant |  |
| 0x005a | 90 | 0x0008 | 8 | 8 | 0.8 | constant |  |
| 0x005b | 91 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x005c | 92 | 0x0017 | 23 | 23 | 2.3 | constant |  |
| 0x005d | 93 | 0x0028 | 40 | 40 | 4 | constant |  |
| 0x005e | 94 | 0x002c | 44 | 44 | 4.4 | constant |  |
| 0x005f | 95 | 0x0030 | 48 | 48 | 4.8 | constant |  |
| 0x0060 | 96 | 0x0036 | 54 | 54 | 5.4 | constant |  |
| 0x0061 | 97 | 0x003a | 58 | 58 | 5.8 | constant |  |
| 0x0062 | 98 | 0x0040 | 64 | 64 | 6.4 | constant |  |
| 0x0063 | 99 | 0x0048 | 72 | 72 | 7.2 | constant |  |
| 0x0064 | 100 | 0x0050 | 80 | 80 | 8 | constant |  |
| 0x0065 | 101 | 0x0054 | 84 | 84 | 8.4 | constant |  |
| 0x0066 | 102 | 0x005a | 90 | 90 | 9 | constant |  |
| 0x0067 | 103 | 0x005f | 95 | 95 | 9.5 | constant |  |
| 0x0068 | 104 | 0x0064 | 100 | 100 | 10 | constant |  |
| 0x0069 | 105 | 0x0069 | 105 | 105 | 10.5 | constant |  |
| 0x006a | 106 | 0x006e | 110 | 110 | 11 | constant |  |
| 0x006b | 107 | 0x0073 | 115 | 115 | 11.5 | constant |  |
| 0x006c | 108 | 0x000c | 12 | 12 | 1.2 | constant |  |
| 0x006d | 109 | 0x000d | 13 | 13 | 1.3 | constant |  |
| 0x006e | 110 | 0x000e | 14 | 14 | 1.4 | constant |  |
| 0x006f | 111 | 0x002e | 46 | 46 | 4.6 | constant |  |
| 0x0070 | 112 | 0x0034 | 52 | 52 | 5.2 | constant |  |
| 0x0071 | 113 | 0x003a | 58 | 58 | 5.8 | constant |  |
| 0x0072 | 114 | 0x0040 | 64 | 64 | 6.4 | constant |  |
| 0x0073 | 115 | 0x0048 | 72 | 72 | 7.2 | constant |  |
| 0x0074 | 116 | 0x0055 | 85 | 85 | 8.5 | constant |  |
| 0x0075 | 117 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0076 | 118 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x0077 | 119 | 0x000c | 12 | 12 | 1.2 | constant |  |
| 0x0078 | 120 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0079 | 121 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x007a | 122 | 0x0008 | 8 | 8 | 0.8 | constant |  |
| 0x007b | 123 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x007c | 124 | 0x000c | 12 | 12 | 1.2 | constant |  |
| 0x007d | 125 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x007e | 126 | 0x000e | 14 | 14 | 1.4 | constant |  |
| 0x007f | 127 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0080 | 128 | 0x0011 | 17 | 17 | 1.7 | constant |  |
| 0x0081 | 129 | 0x0000 | 0 | 0 | 0 | constant |  |
