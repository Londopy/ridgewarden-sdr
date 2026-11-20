# ridgewarden-sdr

[![CI](https://github.com/Londopy/ridgewarden-sdr/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-sdr/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-sdr)](https://github.com/Londopy/ridgewarden-sdr/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-sdr/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-sdr)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Find every SDR on the mesh, and use them together to find a transmitter.**

A ridgewarden app for AREDN meshes. Each site describes its receivers in a config file and publishes them as signed events; every site builds the same directory and map from the log, so it works with the internet down. Only amateur-band OpenWebRX+ profiles are offered over RF.

When something is interfering, one request makes every site measure the same frequency at the same moment. Any SDR gives signal strength, a KrakenSDR adds its bearing, and an RTL-SDR that can record IQ adds time differences of arrival, lined up on a reference transmitter. The page combines whatever came back into one fix with a 95% error ellipse. A weekly test against a beacon at a surveyed spot publishes how far the fixes really miss.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box. Spec sheet 01 of the AREDN app suite.

## What it does

- **Directory and map.** Each `[[receiver]]` in `sdr.toml` is published as a signed `sdr.receiver` event: name, site, position (from the config or the node's own sysinfo), tuning range, antenna, the mesh-facing OpenWebRX+ link and the profiles it offers. It is published again whenever it changes and once a day. Every few minutes the site also logs `sdr.status` telemetry: whether the web UI answered, and when the receiver last measured something. The map is inline SVG drawn on the server, with other mesh nodes for context; there are no map tiles, so nothing comes from the internet. The top of the page counts receivers and sites heard in the last day.
- **Part 97 band check.** Every profile of the mesh-facing OpenWebRX+ instance is checked against the US amateur bands. If any profile falls outside, or listeners may move the center frequency, the link is withheld from the directory and the page says why. Every site repeats the check on what other sites publish and hides a link that fails it.
- **`check-owrx`** reads OpenWebRX+'s `settings.json`, reports which profiles fall outside the amateur bands, and can write an amateur-only copy for a mesh-facing instance.
- **Captures.** "Measure 146.52 MHz, 16 kHz wide, for 10 seconds at 18:00" goes out as a signed `sdr.capture` event, from the command line or a form on the page. Sites pick it up a few seconds early and start together. Each receiver that covers it logs an `sdr.reading`: level, noise floor, peak, gain and calibration. The page ranks the sites, loudest first, and maps them.
- **KrakenSDR bearings.** A site with a KrakenSDR reads its bearings during the capture and logs the average as `sdr.bearing`, with its spread.
- **Time differences of arrival.** If the request names a reference transmitter, receivers with an `iq_command` record 2 s of IQ with `rtl_sdr` and share a cut-down snippet through the core's blob store. A site with `tdoa.solve` fetches the snippets, lines the clocks up on the reference, and logs the time differences as `sdr.tdoa`.
- **One fix.** Every site combines the signal strengths, bearings and time differences for a capture into one position with a 95% error ellipse, on the map with the bearing lines. It says when the fix is outside the ring of receivers, when another spot fits as well, when nothing cross-checks it, and when the measurements disagree with each other. A fix with an ellipse under 1 km is marked as narrowed to a neighborhood.
- **Weekly accuracy test.** One site asks every week for a capture of a beacon whose position was surveyed, and publishes the miss distance as a signed `sdr.beacon` event. The accuracy page charts every result, shows how many misses fell inside their own ellipse, and compares the last four tests with the four before.
- `/api/receivers.geojson` and `/api/capture?id=` return the same data as JSON.

### How loud is "loud"

Receivers are not calibrated against each other, and the page says so. `rtl_power` reports power in dB relative to each dongle's ADC, and that number moves with the gain setting. So:

- when every reading comes from a receiver with a measured `calibration_db`, results are ranked in dBm at the antenna;
- otherwise they are ranked by how far the signal stands above each receiver's own noise floor (the median bin either side of the band, scaled to the same bandwidth). That cancels gain settings, but not antennas, feedline loss or local noise;
- if some reading has no floor, raw levels are used and labelled uncalibrated.

Raw levels, floors, gain and calibration are always shown next to the ranking. In a fix, signal strength counts the least of the three methods, and less still when uncalibrated.

### How good is a fix

Honestly: rough with signal strength alone, several degrees per KrakenSDR bearing (worse with multipath off ridges), and with RTL-SDRs lined up on a reference transmitter, 0.5 to 2 km inside the ring of receivers at best and much worse outside it. Better than that needs receivers with real 10 MHz and PPS inputs, which this version doesn't use.

The ellipse comes from the geometry and each measurement's stated error (`tdoa.sigma_us`, each KrakenSDR's `sigma_deg`, `fix.level_sigma_db`), widened when redundant measurements disagree by more than those errors allow. The weekly beacon test is the check: about 95% of its misses should land inside their ellipse. If they don't, raise the stated errors.

Time differences need something to time. Data bursts, digital voice (DMR, P25, YSF, D-STAR) and key-ups correlate well; a plain carrier, or one carrying only a CTCSS tone, has nothing to correlate, and the page says so instead of showing a fix built on noise. A correlation peak has to stand 8 times the spread of the rest before a pair is used, and one that implies a path difference longer than the sites are apart is refused as multipath.

## Part 97 and what goes over RF

The mesh is amateur RF, and 97.113(c) does not allow retransmitting signals from other services over it. This app keeps to that in four places.

**Web receivers.** Run two OpenWebRX+ instances:

- a LAN instance with every profile you like (NOAA weather, airband, broadcast), reachable only from the site's own LAN or the internet, never advertised on the mesh;
- a mesh-facing instance with amateur profiles only. This is the one in `openwebrx_url`.

OpenWebRX+ reads its config from `/etc/openwebrx/openwebrx.conf` unless started with `-c`, and keeps `settings.json` in `[core] data_directory` (default `/var/lib/openwebrx`). For the second instance make a config with its own data directory and port, for example `/etc/openwebrx/openwebrx-rf.conf`:

```ini
[core]
data_directory = /var/lib/openwebrx-rf

[web]
port = 8074
```

then build its settings from the LAN instance's and start it with `openwebrx -c /etc/openwebrx/openwebrx-rf.conf`:

```sh
ridgewarden-sdr check-owrx /var/lib/openwebrx/settings.json
ridgewarden-sdr check-owrx /var/lib/openwebrx/settings.json \
    --write /var/lib/openwebrx-rf/settings.json --recenter
```

The copy drops profiles whose waterfall (`center_freq` plus and minus half of `samp_rate`) isn't wholly inside one amateur band, drops devices left with no profiles, and turns `allow_center_freq_changes` off at every level (OpenWebRX+ reads it per profile, per device and globally, and takes any true value). `--recenter` moves a profile that only spills over a band edge back inside it instead; OpenWebRX+'s own default 2 m profile (145 MHz, 2.048 MS/s) runs 24 kHz below 144 MHz and is moved to 145.024 MHz. 219-220 MHz is never offered: 97.303(l) keeps it to fixed point-to-point digital message forwarding. Each instance needs its own SDR, since only one program can hold a dongle. If the node's LAN is in direct mode, also block the LAN instance's port from mesh addresses on the Pi.

This app doesn't proxy OpenWebRX+'s traffic; it links to the mesh-facing instance and checks its `settings.json` on every publish. Edit that file by hand and the link is withheld until it passes again.

**Signal-strength readings and bearings.** A site measures only frequencies whose whole requested band is inside an amateur band, unless its own `capture.local_bands` lists a non-amateur range it may measure. A reading is a handful of numbers (levels in dB, gain, position) and a bearing is one angle, never audio or a waterfall, so they are measurement data rather than a retransmitted signal. Still, they cross RF, so measuring outside the amateur bands is off by default; ask your club and the AREDN forum before turning it on. A site that may not measure a request logs a "refused" reading saying why. A request can only be made from a site that may measure it itself. Noise floor bins come only from spectrum the site may measure.

**IQ snippets.** A snippet is a recording, so both of its channels must be amateur: the target and the reference. That rules out the NOAA Weather Radio and ATSC references the spec mentions; a reference has to be an amateur transmitter at a surveyed spot, such as a club beacon sending data or a busy digipeater. A site records a snippet only when both channels, with 10 kHz either side for the channel filter's skirts, are inside amateur bands, whatever `local_bands` says, and otherwise measures signal strength only. Config validation refuses a non-amateur `[[reference]]`.

**The capture form key** crosses the mesh in the clear like everything else on amateur RF (no encryption under 97.113(a)(4)), so it keeps out casual misuse rather than a determined listener. Use `form_allow` to take form posts only from the site's LAN.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-sdr/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-sdr/releases/latest): `sudo apt install ./ridgewarden-sdr_<version>_arm64.deb` (`_amd64` on a PC). Either way the download is checked against the release key your core already trusts, `/etc/ridgewarden/sdr.toml` is written if missing, and the service starts once the core is set up. The page is on port 8798.

For captures, install rtl-sdr (`sudo apt install rtl-sdr` on Raspberry Pi OS and Debian), which provides `rtl_power` and `rtl_sdr`, then unplug and replug the dongle. Its udev rules give the `plugdev` group access to the dongles, and the service runs with that group. For bearings, run [krakensdr_doa](https://github.com/krakenrf/krakensdr_doa) with its output format set to "Kraken App" and tune its VFO to the frequency you're hunting; this app reads its bearings but doesn't retune it.

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-sdr`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-sdr ridgewarden-sdr --config /etc/ridgewarden/sdr.toml <command>`.

## Usage

```sh
ridgewarden-sdr init --config /etc/ridgewarden/sdr.toml
ridgewarden-sdr run  --config /etc/ridgewarden/sdr.toml
```

The page is on `http://<pi>:8798/`: receivers and the map, captures and fixes, the accuracy record, and the band plan.

Ask every site to measure, starting in ten minutes (or at a UTC time with `--at 2026-11-20T18:00:00Z`), with time differences lined up on a reference from `sdr.toml`:

```sh
ridgewarden-sdr measure 144.6M 16k --secs 10 --at 10 --ref "TST digipeater" --note "kerchunker on the input"
ridgewarden-sdr result 3f9c2a1b
```

Leave a few minutes between asking and the start time. The core syncs every two minutes by default, and a site that gets the request more than a minute after its start time skips it rather than report a measurement from a different moment. Start times and the time-difference search rely on each site's clock being within `tdoa.max_skew_ms` (250 ms): a Pi without a real-time clock needs NTP or GPS time.

The target and the reference share one 2 MHz recording, so they must be 100 kHz to 1.7 MHz apart, and each channel at most 40 kHz wide.

## Configuration

`/etc/ridgewarden/sdr.toml` (`init` writes a commented one):

```toml
listen = "0.0.0.0:8798"
status_interval_s = 300      # how often live status is published

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # lets this app publish via the core
imports_dir = "/var/lib/ridgewarden/imports"      # where IQ snippets are handed to the core

[capture]
form_key = ""                # passphrase for the page's capture form, 12+ characters; empty turns it off
form_allow = ["10.54.12.0/29"]  # only these addresses may post the form; empty means any
# local_bands = [[162.4, 162.55]]   # non-amateur MHz ranges this site may measure

[site_names]
"tst-cuesta" = "Cuesta Peak"

[[receiver]]
name = "rx1"                 # lowercase letters, digits, hyphens
antenna = "discone, 30 ft above the roof"
min_mhz = 24
max_mhz = 1766
# lat = 35.33                # leave out to use the node's own position
# lon = -120.62
openwebrx_url = "http://tst-pi.local.mesh:8074/"
openwebrx_settings = "/var/lib/openwebrx-rf/settings.json"

[receiver.capture]
command = ["rtl_power", "-d", "1"]   # the program and any leading arguments
iq_command = ["rtl_sdr", "-d", "1"]  # for time differences; leave out if it can't
gain_db = 30                 # leave out for automatic gain
ppm = 0
# calibration_db = -97.5     # add to the measured dB for dBm at the antenna

[[kraken]]
name = "kraken"
url = "http://10.54.12.9:8081/DOA_value.html"
heading_deg = 0              # which way the array's zero points; leave out to use what it reports
sigma_deg = 5                # the least bearing error to claim

[[reference]]                # read by the site asking; its details go into the request
name = "TST digipeater"
freq_mhz = 144.39
bw_khz = 16
lat = 35.4044                # surveyed
lon = -120.5981

[tdoa]
solve = false                # true at one or two sites
max_skew_ms = 250
sigma_us = 3.0               # timing error of a clean pair

[fix]
path_loss_exp = 3.0          # 2 in free space, 3 to 4 over hills
level_sigma_db = 8.0

[beacon]                     # the same at every site, schedule = true at exactly one
name = "TST beacon"
freq_mhz = 144.285
lat = 35.3611                # surveyed
lon = -120.6522
weekday = "sun"
hour_utc = 18
minute = 30
schedule = false
reference = "TST digipeater" # optional: include time differences in the test
```

A receiver without `[receiver.capture]` is listed but never measures. Signal-strength captures run `command` followed by `-f low:high:bin -i <secs> -1 -c 20%` and `-g`/`-p` when set, and read the CSV it prints; the last value of each line, which rtl_power prints twice, is dropped, and bins that might reach past the requested band aren't counted. The span is six times the requested bandwidth with the signal left of center, so the dongle's DC spike falls outside it; the bins either side give the noise floor. Time-difference captures run `iq_command` followed by `-f <center> -s 2048000 -n <samples> [-g] [-p] -`, read unsigned 8-bit I/Q from it, and measure the level from the recording's own spectrum, on its own dB scale: rankings that mix the two use height above each receiver's floor, and `calibration_db` applies only to rtl_power readings. `rtl_power` and `rtl_sdr` need a dongle no other program holds: use a dongle of their own, or expect captures to fail with their error while OpenWebRX+ has the device open. A failed capture is logged with that error, and one capture at a time runs on each receiver; the next waits its turn until a minute past its start time.

To get a `calibration_db`, feed a signal generator of known level into the receiver's feedline at the antenna end, run a capture on it, and take the dBm you fed in minus the level the page shows. Keep the gain fixed afterwards.

KrakenSDR bearings are the "Kraken App" output's relative bearing plus the heading, as krakensdr_doa's own map output works them out. Set the array's heading in its station settings, or `heading_deg` here, and check it against a transmitter at a known spot before trusting it.

The capture form signs requests under this site's callsign, so it takes a passphrase of at least 12 characters, refuses posts from other sites' pages and from addresses outside `form_allow`, takes one attempt every 10 seconds per address, and stops checking keys for a while after 20 wrong ones from anywhere in 10 minutes. Wrong keys are logged with the address they came from. The command line needs only access to the core's token.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`. Once a station's core has polled its own node, its receivers, readings, bearings and snippets have to name that node, so it can't claim to be measuring from somewhere else; until then they are taken as they come. A site's own position comes only from its own core's polls of its own node. One station can list at most 16 receivers, keep at most 50 requests within a day of each other, and log at most 16 readings and 16 snippets per capture; a capture keeps at most 200 readings, and each site starts at most four captures per requesting station at a time. A snippet blob larger than a snippet can be, or published by a different station than its note, is never fetched. Receivers not heard from for 30 days drop off the directory. Readings, bearings and time differences are telemetry, so the core prunes them after 30 days; capture requests and beacon results are kept. Snippets are dropped from the blob store after a week.

### Testing without hardware

Three stand-ins take the real programs' arguments, so the app runs them exactly as it runs the real ones:

- `ridgewarden-sdr sim-rtl-power` prints made-up spectrum in rtl_power's CSV format after waiting the integration time. `--signal FREQ:DB[:WIDTH]` adds a carrier, and `-g` moves every level by the gain above 30 dB, as a real dongle would.
- `ridgewarden-sdr sim-rtl-sdr` writes rtl_sdr's raw 8-bit I/Q. `--site LAT:LON` is where the receiver is, `--tx FREQ:LAT:LON[:DEVIATION]` adds a transmitter sending 4-level FSK (deviation 0 for a plain carrier), delayed by its distance from the site, and `--crystal-ppm` puts the dongle's clock and tuning off.
- `ridgewarden-sdr sim-kraken --listen ADDR --site LAT:LON --tx FREQ:LAT:LON --heading DEG --error DEG` serves a `DOA_value.html` pointing at the transmitter.

```toml
[receiver.capture]
command = ["ridgewarden-sdr", "sim-rtl-power", "--signal", "144.6M:-25:12k"]
iq_command = ["ridgewarden-sdr", "sim-rtl-sdr", "--site", "35.33:-120.62",
              "--tx", "144.6M:35.42:-120.63", "--tx", "144.39M:35.30:-120.75"]
gain_db = 30
```

## Limits

- Real hardware hasn't been through it yet: rtl_power's and rtl_sdr's formats come from their source code and the stand-ins follow them, but no capture has been run on a real dongle, a real KrakenSDR or a real reference transmitter. The 0.5 to 2 km figure is the spec's realistic target, not a measured result; the weekly beacon test is how to measure it.
- Time differences use RTL-SDRs aligned on a reference, not GPS-disciplined receivers with 10 MHz and PPS inputs; the 500 m stretch target needs those and isn't attempted.
- NOAA Weather Radio and ATSC stations can't be references here, because the snippets cross amateur RF (see Part 97 above).
- Snippets travel as blobs through the core and use its rate limit and time window; a site can solve only once they have arrived.
- KrakenSDRs must be tuned by hand to the frequency being hunted; the app reads bearings and doesn't change krakensdr_doa's settings.
- The live fox-hunt demo and a write-up with a university direction-finding group are not software and aren't part of this repo.
- Waterfalls of non-amateur bands over the mesh stay off; whether they would be acceptable as measurement data is a question for the club and the AREDN forum, not something the app decides.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-sdr
cd ridgewarden-sdr
cargo build --release
cargo test
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
