# Host tools: capture and analyze

Two binaries for the always-on Linux server that sits next to the sniffer:

| Binary | Job |
|---|---|
| `sniffer-capture` | long-running daemon: one TCP connection to the sniffer, one log file per local day, wall-clock stamps, plus a control socket for notes |
| `sniffer-analyze` | reads those logs back and writes a markdown protocol map |

A third binary, `fake-device`, speaks the device protocol over TCP so both of the
above can be exercised with no hardware attached.

The point of all this is not pretty logs. It is to map a heat pump's Modbus
protocol well enough to **replace the wired controller** with your own device:
which addresses the controller polls, how often, what each register means, and
which register changes when you press a button on the controller.

## Install

```bash
cargo install --path tools
```

That puts `sniffer-capture`, `sniffer-analyze` and `fake-device` in
`~/.cargo/bin`. For the systemd unit below, copy `sniffer-capture` to
`/usr/local/bin`:

```bash
sudo install -m 0755 ~/.cargo/bin/sniffer-capture /usr/local/bin/
```

## Quick trial, no hardware

Three terminals:

```bash
# 1. a fake sniffer, polling 4 holding registers every 250 ms and writing one
#    register every 8th cycle
fake-device --listen 127.0.0.1:4000 --period 250ms --write-every 8

# 2. the capture daemon, into a scratch directory
mkdir -p /tmp/cap
sniffer-capture run --device 127.0.0.1:4000 --dir /tmp/cap --control /tmp/cap/ctl.sock

# 3. drop a note, then look at what it correlates with
sniffer-capture note --control /tmp/cap/ctl.sock "setpoint 22.0 -> 23.0"
sniffer-analyze /tmp/cap/capture-*.log --window 10s --out /tmp/cap/report.md
```

Note that a unix socket path has a hard length limit (around 100 bytes), so keep
the `--control` path short.

## systemd setup

The unit is `tools/systemd/modbus-sniffer-capture.service`. It runs as a static
system user so that a human can use the control socket.

```bash
# 1. the service account and the group that gets to use the control socket
sudo groupadd --system modbus-sniffer
sudo useradd --system --gid modbus-sniffer --no-create-home \
     --shell /usr/sbin/nologin modbus-sniffer

# 2. let yourself talk to the daemon (log out and back in afterwards)
sudo usermod -aG modbus-sniffer "$USER"

# 3. install and start
sudo install -m 0644 tools/systemd/modbus-sniffer-capture.service \
     /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now modbus-sniffer-capture
```

Edit `--device` in the unit if your sniffer is not reachable as
`modbus-sniffer:4000`.

`StateDirectory=modbus-sniffer` gives the logs `/var/lib/modbus-sniffer`, and
`RuntimeDirectory=modbus-sniffer` gives the socket `/run/modbus-sniffer/ctl.sock`
- which are the defaults the CLI uses, so after this the short forms work:

```bash
sniffer-capture status
sniffer-capture note "setpoint 26 -> 27"
```

### Why not DynamicUser

`DynamicUser=yes` would be tidier, but the dynamic group only exists while the
service runs, so you cannot add yourself to it, and the control socket would be
root-only in practice. The unit keeps a commented note about this. If you never
need the CLI as a normal user, switch to `DynamicUser=yes` and run the CLI with
`sudo`.

## The control CLI

Every subcommand other than `run` talks to a running daemon over the unix socket
and prints the device's own one-line answer. Exit status is non-zero when the
answer starts with `# err`.

```bash
sniffer-capture note "turned DHW boost on"   # mark the log
sniffer-capture bus 19200 8N1                # retune the sniffer's UART
sniffer-capture status                       # ask the device how it is doing
```

`--control <path>` overrides the socket path for all three.

A note is the one thing in this system that cannot be reconstructed afterwards,
so if the device is unreachable the daemon writes the note straight into the log
as `NOTE <text>` and tells you it did:

```text
# err the device is not connected - note written to the capture log instead
```

The analyzer treats a `NOTE` line and a device-echoed `# note` line the same way.

## The capture log

One file per local day, `capture-YYYY-MM-DD.log`, rotated at local midnight:

```text
2026-10-03T11:50:08.437+02:00 CAPTURE connected to 127.0.0.1:45123
2026-10-03T11:50:08.437+02:00 # hello sniffer-esp32c6 0.1.0 uptime_ms=3601001 bus=9600 8E1 replay=3
2026-10-03T11:50:08.437+02:00 CAPTURE uptime offset_ms=1791017407436 firmware=sniffer-esp32c6 version=0.1.0
2026-10-03T11:50:07.687+02:00 [3600.251] RX slave=0x01 func=0x03 (Read Holding Registers) data=00 00 00 04 crc=0x0944 computed=0x0944 OK
2026-10-03T11:50:08.454+02:00 [3601.018] RX slave=0x01 func=0x03 (Read Holding Registers) data=08 00 dc 00 01 00 01 00 00 crc=0x1ae4 computed=0x1ae4 OK
```

Each line is `<RFC3339 local time with milliseconds> <the device's line, verbatim>`.

The device only knows its own uptime, so on every `# hello` the daemon computes
`offset = wall_clock_now - uptime_ms` and stamps every later line that carries a
`[secs.ms]` with `offset + uptime`. That is why the replayed lines above are
stamped *before* the hello that delivered them: they really did happen earlier,
while nothing was connected. Lines with no stamp of their own (drop markers,
status lines, daemon events) get their arrival time.

A reconnect learns a new offset. A reboot shows up as a hello with a lower
uptime than anything seen before, and is logged:

```text
2026-10-03T12:10:02.118+02:00 CAPTURE device rebooted (uptime_ms 3721004 -> 512)
```

Daemon events are `CAPTURE <text>`; user notes written locally are `NOTE <text>`.
Both are ordinary log lines, so a single `grep` over several days still works.

The file a line lands in is chosen by *arrival* time, which right after a
reconnect can differ from the line's own stamp by the depth of the device's
replay buffer. Feed the analyzer every log file you have and it sorts by time
across files, so this never matters in practice.

Writes are flushed per line: a multi-day capture that loses its tail to a power
cut is a worse outcome than one write syscall per frame at 9600 baud.

## The report

```bash
sniffer-analyze /var/lib/modbus-sniffer/capture-*.log --window 10s --out map.md
```

Options: `--window` (half-width of the note correlation window, default `10s`),
`--max-latency` (how long a request stays open waiting for its response, default
`1s`), `--gap` (bus silence worth reporting, default `30s`).

Sections:

1. **Overview** - time span, CRC-OK / BAD_CRC / SHORT counts, drop markers, bus
   settings seen, reboots, silent gaps.
2. **Roles** - how requests were paired with responses, per-slave counts, and
   which addresses the master polls. The sniffer taps one wire and cannot see
   direction, so roles come from PDU shape plus order.
3. **Polling loop** - every `(slave, function, start address, count)` read
   request with its occurrence count, request period (min/median/max) and
   response latency (min/median/max). This is the table you reproduce when your
   own device takes over as master.
4. **Register map** - every address seen, split into *changing*, *free-running*
   and *constant*. Values are shown as hex, u16, s16 and `/10`, because a Tuya
   heat-pump controller usually stores a temperature as tenths of a degree.
   Writes (0x05, 0x06, 0x0F, 0x10) are listed separately with timestamps.
5. **Notes correlation** - the useful one. For each note, every write and every
   register transition inside the window, old -> new. Registers that move on more
   than half the polls (counters, uptimes, live temperatures) are summarised
   rather than listed, because "it changed near your note" tells you nothing
   about them.
6. **Exceptions and anomalies** - exception responses with their decoded codes,
   requests that never got an answer, frames of unrecognised shape, and the
   line-level damage counts.

Frames with a bad CRC are counted but never decoded, so a noisy bus cannot
invent registers.

## The workflow

1. Start the capture and leave it running for days. It reconnects on its own
   with a backoff capped at 30 s, so a device reboot or a Wi-Fi hiccup costs you
   only the frames the device could not buffer.

2. Whenever you touch the controller, say so **as you do it**:

   ```bash
   sniffer-capture note "setpoint 26 -> 27"
   sniffer-capture note "DHW boost on"
   sniffer-capture note "mode: heating -> cooling"
   ```

   One note per change, and name the old and the new value. Section 5 is only as
   good as these notes.

3. Prod every setting the controller has, including the ones you do not care
   about - a register you can explain is a register you will not misread later.

4. Analyze over everything you have:

   ```bash
   sniffer-analyze /var/lib/modbus-sniffer/capture-*.log --out map.md
   ```

5. Read section 5 first: it names the register behind each setting. Then section
   3, which is the poll loop your replacement device has to imitate. Section 4
   tells you which registers are worth reading at all.

A couple of days of idle capture plus one session of deliberate button-pressing
usually maps more of the protocol than a week of staring at raw frames.

## Testing without hardware

`fake-device` builds its frame lines with `modbus-sniffer-core`'s own
`build_frame` + `format_frame`, so the bytes it puts on the wire cannot drift
from what the firmware emits.

```bash
fake-device --listen 127.0.0.1:4000 \
    --uptime-ms 3600000 \   # pretend to have been up an hour
    --replay 3 \            # replay 3 buffered lines on connect
    --period 250ms \
    --write-every 8 \       # a Write Single Register exchange every 8th cycle
    --bad-crc-every 17      # corrupt every 17th response
```

Restart it with a lower `--uptime-ms` to make the capture daemon see a reboot.

The test suite drives the daemon against this fake:

```bash
cargo test -p modbus-sniffer-tools
```

It covers the uptime-to-wall-clock mapping (wall-clock deltas must reproduce
uptime deltas exactly, and replayed lines must be stamped before the hello that
carried them), note forwarding, the local fallback when the device is gone,
reconnect after the device disappears, reboot detection, midnight rotation with
an injected clock, and request/response pairing, register tracking and note
correlation on synthetic logs.
