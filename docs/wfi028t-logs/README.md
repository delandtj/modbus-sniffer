# WFI-028T capture logs (stock controller)

Bus captures of the WFI-028T/035T heat pump with its stock wired controller as
the Modbus master, taken by this sniffer (`sniffer-esp32c6`) through
`sniffer-capture` on 2026-10-03 and 2026-10-04. These are what the register
map in `../wfi028t-controller/docs/register-map.md` was built from.

They end at 2026-10-04 16:23:58, when the stock controller was replaced by the
`wfi028t-controller` firmware. Captures of that controller are logged
separately (`wfi-controller-capture.service`, `/var/lib/wfi-controller`).
The sniffer's own capture service is disabled since then.

| File | Covers | Notes |
|---|---|---|
| `capture-2026-10-03.log.gz` | 13:16 - 23:59 | raw; gaps while the setup was being changed (13:16-14:48), connect failures from 22:13 |
| `capture-2026-10-04.log.gz` | 00:00 - 16:23 | raw; connect failures until 15:09, real traffic 15:09 - 16:23 |
| `compact-2026-10-03.log` | 17:32 - 23:59 | compacted (`--compact`, deadbands 0x001b:1, 0x001e:5, 0x0024:15) |
| `compact-2026-10-04.log` | 00:00 - 16:23 | compacted, same settings |

Gap: nothing was captured from 2026-10-03 22:13 to 2026-10-04 15:09. The
daemon was moved to systemd at 22:13 and could not resolve `modbus-sniffer`
until a hosts entry was added.

What is in there, by time (CEST):

- 2026-10-03 14:01-14:50: bench SCAN and bus-settings SWEEP, marked with
  `# note` lines; ignore the traffic between start and end.
- 2026-10-03 14:51 onwards: real bus at 9600 8N1. Every user action on the
  controller has a `# note`: full power on/off, the A01-A14 readout
  (15:42-15:45), P01 changes, power off/on, a circulation-pump flow test
  (16:57-17:01). This is what the sensors and setpoints were mapped from.
- 2026-10-04 15:11 - 15:25: the breaker test (heat pump breaker off ~61 s at
  15:15:07; restart sequence, start hold, ECO and boost writes). See the
  "Observed sequences" section of the register map.

Read them back with the host tools. `sniffer-analyze` does not read `.gz`, so
unpack the raw logs first, for example:

```sh
mkdir -p /tmp/wfi && cd /tmp/wfi
zcat ~/Electronics/stm32-modbus-sniffer/docs/wfi028t-logs/capture-2026-10-03.log.gz > capture-2026-10-03.log
zcat ~/Electronics/stm32-modbus-sniffer/docs/wfi028t-logs/capture-2026-10-04.log.gz > capture-2026-10-04.log
sniffer-analyze capture-*.log --window 10s --out map.md
```
