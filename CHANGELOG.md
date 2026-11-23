# Changelog

All notable changes to ridgewarden-sdr are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- A timing page under Accuracy: each beacon test's time differences, where the correlation peaked, plotted against the delays the surveyed spot gives, with each pair's stated 95% error, how many fall inside it, the mean error with its sign (a steady offset shows there), the error per test over time, and, once six tests are in, whether `tdoa.sigma_us` needs raising. Beacon results now keep the time differences they measured; only pairs between snippets published for the capture count.
- The mesh-facing OpenWebRX+ instance is checked as it runs, through its `status.json`: its web link is withheld while it serves a profile outside the amateur bands or one its settings file doesn't have, and while it can't be asked.
- `check-owrx` warns about a profile without a `samp_rate` of its own: OpenWebRX+'s `status.json` fails on it, which withholds the directory link.

### Fixed

- The message for a time difference refused as multipath had a run of stray spaces in it.
- When the reference transmitter can't be lined up between two sites, the message now says one site's clock may be more than `tdoa.max_skew_ms` off, not only that the reference is unclear.

## [0.1.0] - 2026-11-20

### Added

- Runs as its own system user, `ridgewarden-sdr`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- A directory of every SDR on the mesh, built from signed `sdr.receiver` events, with live status (web UI answering, last capture) as telemetry and a count of receivers and sites heard today. It works with the internet down.
- A map of the receivers drawn as inline SVG, with no map tiles, and the same data as GeoJSON.
- The US amateur band plan from Part 97.301 and 97.303(h), and a check of every OpenWebRX+ profile offered over RF against it. The web link is withheld, and hidden by every other site too, while any profile is outside the bands, sits in 219-220 MHz, or listeners may move the center frequency at any level.
- `check-owrx`, which reports OpenWebRX+ profiles outside the amateur bands and writes an amateur-only `settings.json` for a mesh-facing instance, optionally moving profiles that spill over a band edge back inside it.
- Coordinated captures from the command line (`measure`, `result`) or a form on the page: every site with a suitable receiver starts together and logs its signal strength, ranked in dBm when every receiver is calibrated, otherwise by height above each receiver's own noise floor, always labelled as relative.
- KrakenSDR bearings, read from krakensdr_doa's "Kraken App" output during a capture and logged with their spread.
- Time differences of arrival: receivers with `rtl_sdr` record IQ snippets of the target and a reference transmitter, share them through the core's blob store, and a solving site lines the clocks up on the reference, removes drift, and logs the time differences. GCC-PHAT with quality gating refuses a plain carrier or CTCSS-only signal, and multipath that implies an impossible path difference.
- One fix per capture from signal strength, bearings and time differences, with a 95% error ellipse on the map, warnings outside the ring of receivers, for a second spot that fits as well, and when measurements disagree; a fix under 1 km is marked as narrowed to a neighborhood.
- The weekly beacon test: one site asks for a capture of a beacon at a surveyed spot and publishes the miss distance; the accuracy page charts every result, how many fell inside their ellipse, and the trend.
- Only amateur frequencies are measured, unless a site allows a band locally; IQ snippets only ever carry amateur channels; sites that may not measure a request say so.
- The capture form takes a passphrase of at least 12 characters, an address allow-list, per-address and site-wide limits on wrong keys, and logs failures.
- Caps on what one station can put in the directory, requests, readings and snippets, and readings pinned to the node the station's core polls.
- `sim-rtl-power`, `sim-rtl-sdr` and `sim-kraken`, stand-ins that take the real programs' arguments and print their formats, for tests and demos without hardware.

[Unreleased]: https://github.com/Londopy/ridgewarden-sdr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Londopy/ridgewarden-sdr/releases/tag/v0.1.0
