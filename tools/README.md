# Host tools: capture and analyze

Two binaries for the always-on Linux server that sits next to the sniffer:

| Binary | Job |
|---|---|
| `sniffer-capture` | long-running daemon: one TCP connection to the sniffer, one log file per local day, wall-clock stamps, a control socket for notes, and optional log compaction |
| `sniffer-analyze` | reads those logs back (raw, compact or both) and writes a markdown protocol map |

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

The unit runs with `--compact --deadband 0x001b:1,0x001e:5,0x0024:15`, so it
writes both `capture-YYYY-MM-DD.log` and `compact-YYYY-MM-DD.log` and keeps 14
days of gzipped raw log. See [Compaction](#compaction) for what that does and
when to add `--no-raw`.

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

## Compaction

The heat pump controller re-reads the same two register blocks every second, so
a raw day is about 346k lines / 108 MiB of which almost all is a byte-identical
repeat. `--compact` writes a second daily file, `compact-YYYY-MM-DD.log`, in
**exactly the same line format**, so every tool here (and every `grep` you
already have) keeps working on it.

```bash
sniffer-capture run --dir /var/lib/modbus-sniffer \
    --compact --deadband 0x001b:1,0x001e:5,0x0024:15
```

What always survives into the compact log:

- `CAPTURE ...` and `NOTE ...` daemon lines and every `# ` device line (hello,
  bus, note, status, ok/err) - they are rare, and each one is a fact about the
  capture;
- every frame that is not part of a recognised poll: writes (0x05, 0x06, 0x0f,
  0x10) and their acks, exceptions, other function codes, reads of other
  ranges, and read requests that never got an answer;
- `BAD_CRC` and `SHORT` lines, `[DROPPED n ...]` markers, and any line the
  parser cannot read at all.

A **recognised poll** is a read request (function 0x03 or 0x04, body exactly
`start(2) + count(2)`, CRC OK) followed within `--max-latency` by a response on
the same slave and function whose byte count is `2 * count`. Its identity is the
key `(slave, func, start, count)`. Anything between the request and its
response (a `# status` line, a noise frame) makes the compactor keep that pair
verbatim rather than guess.

A recognised poll is **dropped** when its response equals the last response
*written* for that key. A register listed in `--deadband` counts as equal while
`|new - last_written| <= band`, compared as signed 16-bit. Comparing against the
last written value, not the previous one, is what makes slow drift visible: the
deviation accumulates until it crosses the band and the pair is written. When a
pair is written, **both** lines go out, byte for byte as they arrived.

### The COMPACT lines

Four bookkeeping lines keep the folded-away traffic visible. They are ordinary
stamped log lines, and `sniffer-analyze` understands all four:

```text
<ts> COMPACT begin deadband=0x001b:1,0x001e:5,0x0024:15 snapshot=300s gap=10s window=1s
<ts> COMPACT snapshot keys=2
<ts> COMPACT poll slave=0x01 func=0x03 start=0x0000 count=63 n=300 suppressed=299
<ts> COMPACT gap slave=0x01 func=0x03 start=0x0000 count=63 len_ms=42500
```

- **begin** - first line of the output and of every daily file after a
  rotation; records the settings that produced the file.
- **snapshot** - every `--snapshot` interval (default `300s`, `0s` disables),
  followed by the latest request+response pair in full, regardless of change,
  for every key polled since the previous snapshot. So the whole bus state can
  be reconstructed from any point in the file. A key that has stopped being
  polled is not resurrected, and a pair already in the output just above is not
  repeated - a duplicated frame line would be counted twice by every reader.
- **poll** - the liveness counter: `n` polls of this key since the previous
  `poll` line for it, of which `suppressed` were dropped. One is written
  whenever a key's pair is written, once per key per snapshot, and once per key
  at end of input, so no poll goes uncounted.
- **gap** - written when a key that had been polling resumes after more than
  `--poll-gap` (default `10s`) of silence. The pair after a gap is always
  written in full, because a value after a silence is never a safe "unchanged".

### Options

| Option | Default | Meaning |
|---|---|---|
| `--compact` | off | also write `compact-YYYY-MM-DD.log` |
| `--no-raw` | off | skip the raw log (only allowed with `--compact`) |
| `--deadband <reg>:<band>,...` | none | repeatable; `0x` prefix for hex, decimal otherwise |
| `--snapshot <dur>` | `300s` | full restatement interval, `0s` to disable |
| `--poll-gap <dur>` | `10s` | silence per key that gets a `COMPACT gap` line |
| `--max-latency <dur>` | `1s` | pairing window for request -> response |
| `--raw-keep-days <n>` | `14` | days of gzipped raw log to keep |

### Raw log retention

At startup and at every midnight rotation the daemon gzips the `capture-*.log`
files of days before today (about 35x: 108 MiB -> ~3 MiB) and deletes
`capture-*.log.gz` older than `--raw-keep-days`. The compression runs on a
blocking thread, so it never stalls the capture loop, and **compact logs are
never compressed or deleted** - they are the long-term record.

### Compacting an existing log

```bash
sniffer-capture compact /var/lib/modbus-sniffer/capture-2026-10-03.log \
    -o /tmp/compact-2026-10-03.log \
    --deadband 0x001b:1,0x001e:5,0x0024:15
```

`.gz` input is unpacked on the fly and `-` reads standard input; without `-o`
the result goes to standard output. The summary goes to stderr:

```text
sniffer-capture: 40287 lines / 12396466 bytes in, 8802 lines / 1653044 bytes out (21.8% of the lines, 13.3% of the bytes)
sniffer-capture: 18460 polls, 16719 suppressed, 301 unanswered read requests, 32 snapshots, 0 gaps, 0 unparsed lines kept
```

Measured on the first four hours of real bus traffic with the deadbands above:
the settings block (0x003f, 67 registers) collapsed from 9230 polls to 19
written pairs (54 request lines once the snapshot restatements are counted), the
status block (0x0000, 63 registers) from 9229 polls to 1721 pairs (1752). The
2389 `SHORT` noise frames and 213 `BAD_CRC` frames in that log are all kept, and
they are what dominates the remaining line count. The next registers worth a
deadband, if the files are still too big, are the temperatures around
0x0010-0x001a (tenths of a degree, so `:1` each).

## The report

```bash
sniffer-analyze /var/lib/modbus-sniffer/capture-*.log --window 10s --out map.md
```

Options: `--window` (half-width of the note correlation window, default `10s`),
`--max-latency` (how long a request stays open waiting for its response, default
`1s`), `--gap` (bus silence worth reporting, default `30s`).

Raw logs, compact logs and a mixture of both can be fed in together; files are
sorted by time across the whole input. Section 1 says which kind it got.

Sections:

1. **Overview** - time span, CRC-OK / BAD_CRC / SHORT counts, drop markers, bus
   settings seen, reboots, silent gaps. On a compact log it also reports the
   compactor's settings, the polls it counted and how many it dropped, and the
   `COMPACT gap` markers; the `COMPACT poll` / `COMPACT snapshot` lines count as
   bus activity, because the traffic they stand for really was there.
2. **Roles** - how requests were paired with responses, per-slave counts, and
   which addresses the master polls. The sniffer taps one wire and cannot see
   direction, so roles come from PDU shape plus order.
3. **Polling loop** - every `(slave, function, start address, count)` read
   request with its occurrence count, request period (min/median/max) and
   response latency (min/median/max). This is the table you reproduce when your
   own device takes over as master. On a compact log `Requests` is what survived
   in the file, `Polls`/`Suppressed` come from the `COMPACT poll` counters, and
   the period is their mean over the span those counters cover.
4. **Register map** - every address seen, split into *changing*, *free-running*
   and *constant*. Values are shown as hex, u16, s16 and `/10`, because a Tuya
   heat-pump controller usually stores a temperature as tenths of a degree.
   Writes (0x05, 0x06, 0x0F, 0x10) are listed separately with timestamps. On a
   compact log the values come from the responses that were kept, so a
   deadbanded register loses its intermediate wobble: the same addresses, the
   same first and last values, fewer samples and a slightly smaller *distinct*
   count.
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

   Days whose raw log has already been gzipped and expired live on in the
   compact logs, so for a long history use those (or both - the analyzer sorts
   across files and marks the input as compact):

   ```bash
   sniffer-analyze /var/lib/modbus-sniffer/compact-*.log --out map.md
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

Compaction has its own unit tests: dedup of identical polls, deadband behaviour
including drift that accumulates past the band relative to the last written
value, signed comparison across zero, snapshot cadence and contents, the poll
counters adding up to the polls seen, gap detection, "writes, notes and damaged
frames are always kept", request and response written together, and the gzip /
retention plan against an injected date and a temp dir. Two end-to-end tests run
the daemon with `--compact` and with `--no-raw` against the fake device and feed
the result back through the analyzer.
