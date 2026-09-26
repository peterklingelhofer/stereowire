# stereowire

A high-fidelity, low-latency screen and system-audio link from a Mac to a Mac
or Windows PC.

Built for watching someone work in a digital audio workstation over the
internet: you see their screen at native resolution and hear exactly what they
hear, in full lossless audio quality.

## Why not just use Zoom

Conferencing tools are built to survive bad networks with many participants, so
they aggressively compress audio and cap it around 32-48 kHz mono at a low
bitrate. That is fine for speech and useless for judging a mix.

With only two people the constraint disappears. Uncompressed 48 kHz stereo float
is about 3 Mbit/s, which is nothing on a modern connection, so `stereowire` does
not compress audio at all. What arrives is bit-identical to what left.

## How it works

    ScreenCaptureKit ─┬─ video ─ VideoToolbox HEVC ─┐
                      │                             ├─ UDP ─ Tailscale ─┐
                      └─ audio ─ uncompressed PCM ──┘                   │
                                                                        │
       AVSampleBufferDisplayLayer ◀─ reassembly ◀───────────────────────┤
                    CoreAudio ◀──── jitter buffer ◀─────────────────────┘

The diagram is the Mac receiver's path. A Windows receiver decodes the same
H.264 or HEVC stream through Media Foundation and renders it with Direct3D 11.
Audio plays back through WASAPI in shared mode.

- **Video** is captured at the display's native pixel size and encoded with the
  hardware HEVC encoder in low-latency mode, or H.264 with `--codec h264`.
  B-frames are disabled, so no frame ever waits on a later one.
- **Audio** is captured from the system mix by ScreenCaptureKit, so Logic's
  output is picked up directly with no virtual audio device (no BlackHole).
  It is sent as raw 48 kHz stereo float.
- **Transport** is plain UDP inside Tailscale. WireGuard already provides
  encryption, authentication, and NAT traversal, so there is no TLS or QUIC
  handshake in the path.
- **Loss** is handled differently for each stream, because they degrade
  differently. Each audio packet carries a copy of the previous packet, so any
  isolated loss is reconstructed exactly. Video frames are far larger, so each
  group of ten fragments carries parity that rebuilds the lost ones. A frame
  lost outright still triggers a keyframe request.
- **Reordering** is absorbed on both streams. Long paths deliver packets out
  of order, so audio and video that arrive early are held briefly and released
  in sequence. Video must be decoded in order, and
  showing a frame that arrived early would strand its predecessor and force a
  resync, which costs a frozen picture until the next keyframe.
- **Congestion** is handled by the receiver, which holds every useful signal and
  asks the sender for a bitrate. It reacts to rising round-trip time as well as
  to loss, because a filling queue shows up as delay first. On an uncongested
  link it simply asks for the maximum and never moves.
- **Clock drift** between the two machines is absorbed by playing back a
  fraction of a percent fast or slow. Without this the audio buffer slowly fills
  or empties until it glitches, which at typical crystal error happens well
  within a working session.

## Security

`stereowire` carries no encryption or authentication of its own. It runs inside
Tailscale, so every packet is already encrypted and authenticated by WireGuard
before it leaves the machine. A second layer would add latency and a second
implementation to get wrong.

That makes the tunnel a requirement. Outside it, the screen and system audio go
out in the clear and the port accepts packets from anyone who finds it, so
`stereowire send` refuses a destination that is not on the tailnet:

    Error: 8.8.8.8 is not a Tailscale address.

Tailscale addresses come from 100.64.0.0/10 and fd7a::/16. Loopback is exempt,
so the self-test and local experiments work. `--allow-untunnelled` overrides the
check for a path that is already encrypted by other means, such as a VPN or an
SSH tunnel, and warns on every run.

## Setup

1. Install [Tailscale](https://tailscale.com/download) on both machines and
   sign in to the same account. Both machines get a stable address and talk
   directly.
2. Build, or use a notarized `StereoWire.app` (see Distribution).

       cargo build --release

3. Grant Screen Recording to whichever binary is *sending*, in
   System Settings > Privacy & Security > Screen Recording. macOS prompts on
   first run.

Run `stereowire doctor` to check all of the above and print how to fix anything
that is missing. `send` and `receive` run the same checks at startup, so a
missing dependency is explained at startup with the command that fixes it.

    stereowire doctor                   check Tailscale and permissions
    stereowire doctor --peer <peer>     also check that peer is reachable and direct

### Watching from Windows

Sending is Mac-only, but the watcher can be on Windows 10 or 11 x64:

1. Install [Tailscale for Windows](https://tailscale.com/download/windows) and
   sign in to the same account as the sender.
2. Get `stereowire.exe`: download the `stereowire-windows-x64` artifact from
   the latest CI run (see Continuous integration), or build it yourself (see
   Distribution).
3. Run it: `stereowire.exe receive` from a terminal, or double-click it, which
   receives with no arguments needed. The first run triggers a Windows
   Firewall prompt, since the receiver listens on UDP 9000: allow it on
   private networks. The .exe is unsigned, so SmartScreen shows "Windows
   protected your PC": click `More info`, then `Run anyway`.

## Usage

On the machine sharing its screen:

    stereowire send --to <peer-tailscale-name-or-ip>

On the machine watching:

    stereowire receive

Only one direction runs at a time. Swapping who runs which command needs two
Macs, since a Windows machine can only run `receive`.

When the watcher is on Windows, add `--codec h264` on the sender:

    stereowire send --to <peer> --codec h264

Windows decodes H.264 with no extra software. HEVC needs the "HEVC Video
Extensions" app from the Microsoft Store, and the receiver says so if it's
missing.

On the Windows machine, watching is the same command, run as
`stereowire.exe receive` (see Setup for the firewall prompt and SmartScreen).

Talk over a separate channel (a phone call, Discord on your phones) so the
conversation never competes with the audio you are judging.

Useful flags:

    stereowire send --mbps 40           video bitrate (default 40)
    stereowire send --max-width 2560    cap capture width (default 2560)
    stereowire send --fps 60            frame rate (default 60)
    stereowire send --codec h264        default hevc, use h264 for a Windows watcher
    stereowire send --no-fec            halve audio bandwidth, lose loss recovery
    stereowire send --min-mbps 8        floor congestion control may not go below
    stereowire send --no-congestion-control   hold the bitrate fixed
    stereowire receive --max-mbps 40    highest bitrate to ask the sender for
    stereowire receive --buffer-ms 50   more audio buffering on a jittery link
    stereowire receive --no-audio       watch silently
    stereowire receive --record out.wav record the received audio, uncompressed
    stereowire receive --hide-latency   start with the title-bar readout off

In the receiver window, `View > Show Latency in Title` (Command-L) toggles the
readout, `View > Enter Full Screen` fills the display, and Command-Q quits. On
Windows, Ctrl+L toggles the same readout, and closing the window quits.

## What gets captured

ScreenCaptureKit taps the system audio mix, which has some useful properties.
Each one below was measured by playing a known stereo tone and analysing the
recording per channel:

- **Stereo is preserved intact.** A 440 Hz left / 880 Hz right probe came back
  with 113 dB and 106 dB of channel separation, at exactly unity gain, each tone
  still in the channel it started in.
- **The tap is pre-volume.** Captured at full scale with the system volume at
  zero, so you can work muted or on headphones and the far end still receives
  full-level audio. Your volume knob does not affect what they hear.
- **It is the whole system mix.** An app with no window at all is still
  captured, as is one on a display you are not sharing.
- **It taps ahead of the output device.** A 48 kHz tone captured cleanly while
  the output device ran at 44.1 kHz, so the destination device does not colour
  the capture.
- **The app's own playback is excluded**, so running both ends on one Mac
  cannot form a feedback loop.

### Sample rate

The wire format carries whatever rate the capture is actually running at, and
the receiver opens its output to match. That rate is read from the capture
stream itself, because **ScreenCaptureKit does not honour every rate you ask
it for**: request 44.1 kHz on macOS 15 and it still delivers 48 kHz. Trusting
the request would pitch everything down by 8.8%, about a tone and a half flat,
which is measurable but easy to miss by ear if you are not listening for it.

`--sample-rate` asks for a rate, defaulting to the output device's own. If the
system overrides it, `stereowire send` says so:

    audio: capturing at 48000 Hz (asked for 44100; macOS chose the rate)

In practice this means a 44.1 kHz project is resampled to 48 kHz once, by
CoreAudio, on the way into the capture. That conversion is not avoidable from
here, and it is a single high-quality resampling pass, so it costs nothing you
can hear. Everything downstream of it is bit-exact.

On the receiving Mac, opening output at the sender's rate switches the
physical output device to that rate, and macOS does not switch it back when
playback stops (Audio MIDI Setup restores it). WASAPI shared mode, which is
what the Windows receiver uses, cannot switch the device's rate at all: if the
output device is set to something else, Windows resamples on the way out, and
the receiver says so:

    audio: the output device runs at 44100 Hz, so Windows resamples this 48000 Hz stream. For a lossless path set the device to 48000 Hz in Settings > System > Sound, under the device's Format

Lossless playback on Windows needs:

- the device's Format set to the stream's rate, in Settings > System > Sound
  under the device
- the device volume at 100%
- audio enhancements and spatial sound off

`--record` writes the received samples before playback, so it is lossless on
either platform regardless of device settings.

**Check your interface before a session.** If Logic outputs to an audio
interface rather than the built-in device, confirm capture with the level meter
`stereowire send` prints: a real `peak -N dBFS` means it is working, `peak
silent` means it is not.

## Repairing lost video

Each group of ten video fragments carries parity blocks, and the number adapts
to the link.

The first is a plain XOR, which rebuilds any single missing fragment in the
group. That is the cheap case and covers most loss. The second is a weighted sum
over GF(256), which makes **any pair** of losses in a group recoverable: two
plain XORs of the same fragments would carry no more information than one, so
the weights are what make the two equations independently solvable. This is the
P+Q scheme RAID-6 uses.

Each block costs about 10%, so the second is only worth carrying when packets
are actually going missing. The receiver decides, since it is the side that can
see the loss, and asks for one block on a clean link and two once loss appears.
Parity rises quickly on a burst and settles back slowly, so it does not flap
report to report.

At 5% loss with heavy reordering, the second block roughly doubles the fragments
rebuilt (47 against 24) and delivers 80 frames rather than 70.

Three or more losses in one group are still beyond it, and the frame is
dropped: inventing a plausible-looking fragment would corrupt the picture
silently, which is worse than losing it outright.

## Adapting to the link

Video bitrate is chosen by the receiver and requested of the sender, which
clamps it to its own configured range. Two signals drive it:

- **Loss** above 5% is unambiguous congestion and backs the rate off sharply.
  Below 1% the link counts as clean, because real networks lose the occasional
  packet without being congested and reacting to that would cost quality for
  nothing.
- **Round-trip time rising above its own quietest measurement** is the earlier
  warning. A queue filling somewhere on the path shows up as delay before it
  shows up as loss, so this backs off before anything is actually dropped.

Recovery is deliberately slower than backoff, and two consecutive clean reports
are required before probing upward, so one quiet moment during congestion does
not restart the climb. **On a link with no loss and steady latency the rate sits
at the configured maximum and never moves**, so a good connection costs nothing.
`--no-congestion-control` pins it if you would rather it never adapt.

## Clock drift

The two machines' sample clocks are independent and differ by tens of parts per
million. Left alone the audio buffer fills or empties until it glitches: at 50
ppm a 30 ms buffer runs out in about ten minutes.

`stereowire` holds the buffer at its target by playing back slightly fast or
slow. The correction is capped at 0.1%, roughly 1.7 cents, which is far below
audibility and about twenty times more authority than real drift needs.

Inside a deadband around the target the samples are passed through untouched, so
ordinary playback is bit-exact. The correction only engages once the clocks have
actually pulled the buffer off target. When it is active the receiver says so:

    audio: correcting clock drift by 240 ppm

## Reading the latency

The receiver's title bar shows the live state of the link:

    stereowire  ·  rtt 78 ms  ±3  ·  audio buffer 30 ms

- **rtt** is the measured round trip. The receiver stamps a probe with its own
  clock twice a second and the sender echoes it back untouched, so the figure
  is a measured round trip and needs no clock synchronisation between the two
  machines.
- **±** is jitter, how much the round trip is moving around. This matters more
  than the round trip itself for deciding `--buffer-ms`: a steady 90 ms link is
  far easier to work against than one wandering between 50 and 130.
- **audio buffer** is how much audio is queued for playback, which adds to what
  you actually hear on top of the network delay.

The same figures print to the console every five seconds, including a rough
one-way estimate. One-way is reported as half the round trip. Real routes can be
asymmetric, so treat that figure as approximate.

The title updates once a second. Doing it per frame would cost real work for no
benefit, since nobody reads a number changing 60 times a second.

## Verifying it works

`stereowire selftest` runs the whole media path locally with no peer, no
display, and no permissions: synthetic frames go through the real encoder, real
packet fragmentation, real UDP sockets, the real reassembler, and a real
hardware decoder, and the result is measured against the source. The framing and
depacketizing it exercises are the same code `send` and `receive` use, so a bug
in either is caught here.

`cargo test` covers the transport logic directly: packet framing, fragment
reassembly, keyframe resync, audio FEC recovery, jitter buffering, RTT
smoothing, the WAV writer, and socket round trips.

    stereowire selftest                        clean link
    stereowire selftest --loss 2               2% packet loss
    stereowire selftest --loss 1 --reorder 3   loss plus jitter-induced reordering
    stereowire selftest --loss 5 --fec-parity 2   stronger video repair

Measured on an M-series Mac:

| link                | video                        | audio                   |
|---------------------|------------------------------|-------------------------|
| clean               | 120/120 frames, 50.4 dB PSNR | bit-exact               |
| 1% loss, reorder 3  | 112/120 frames, 1 resync     | bit-exact (178 repairs) |
| 2% loss, reorder 5  | 97/120 frames, 2 resyncs     | bit-exact (238 repairs) |
| 5% loss, reorder 8  | 67/120 frames, 7 resyncs     | 99.87% exact            |

`--reorder N` delivers datagrams up to N positions out of order, which is what
jitter over a long path does and is harsher here than a real route: these runs
shuffle roughly a tenth of all packets.

Windows has no encoder, so its self-test replays a dump instead of recording
one. `--dump` writes every encoded video datagram to a file, and `--replay`
feeds a file back through the same delivery-and-verify path on either
platform, with no encoder involved:

    stereowire selftest --codec h264 --dump pattern-h264.swd        # on the Mac
    stereowire selftest --replay pattern-h264.swd                   # on either machine
    stereowire selftest --replay pattern-h264.swd --loss 2 --reorder 5

A 640x360 H.264 dump made this way scores PSNR mean 47.9 dB on the Mac's
VideoToolbox decoder and 47.4 dB on Windows' Media Foundation decoder, audio
bit-exact on both.

Producing a Windows build needs no Windows machine:

    rustup target add x86_64-pc-windows-gnu
    brew install mingw-w64
    cargo build --release --target x86_64-pc-windows-gnu -p stereowire

which writes `target/x86_64-pc-windows-gnu/release/stereowire.exe`. Its unit
tests run under Wine with
`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine cargo test --target
x86_64-pc-windows-gnu`, and the whole receiver runs under CrossOver, which
supplies Media Foundation backed by GStreamer, and Direct3D on Metal.

Every frame that is displayed is clean, and loss costs a brief resync. **Audio
degrades far more gracefully than video**, which is the intended priority: it
stays bit-exact through 2% loss combined with heavy reordering.

Reordering used to be the dominant failure. Releasing frames in sequence instead
of on arrival took the 1% case from 31 frames and 20 resyncs to 112 frames and a
single resync, because a resync is expensive: it freezes the picture until a new
keyframe arrives, so waiting up to three frames for a straggler is far cheaper
than giving up on it.

## Continuous integration

`.github/workflows/ci.yml` runs on every push. The protocol crate is portable
Rust, so its tests run on a Linux runner, which starts faster and costs less.

The `app` job builds and tests the sender and receiver together on macOS,
since building the sender needs Apple frameworks. Its self-test runs
unattended: no display and no Screen Recording permission are needed, so the
encoder, sockets, reassembly and decoder are all exercised on every push. It
also dumps an H.264 pattern for the next job, since Windows has no encoder to
make its own.

A `windows` job then builds and tests the receiver on `windows-latest`,
replaying that dump through the Media Foundation decoder clean and with loss
and reordering, and uploads the resulting `stereowire.exe` as the
`stereowire-windows-x64` artifact, so a build can be handed to a Windows
tester without a Windows machine.

## Distribution

    ./scripts/bundle.sh --notarize

Builds `dist/StereoWire.app`, signs it with your Developer ID, notarizes it, and
staples the ticket, so a Mac receiver opens it by double-clicking with no
Gatekeeper warnings.

The app is universal: the script builds `aarch64-apple-darwin` and
`x86_64-apple-darwin` and merges them with `lipo`, so one download runs on
Apple Silicon and on Intel. Both targets have to be installed, and a missing
one is a hard error, so a single-architecture build cannot slip through and fail
once it reaches someone else's Mac:

    rustup target add aarch64-apple-darwin x86_64-apple-darwin

Sending needs a hardware HEVC encoder for low-latency rate control. Apple
Silicon and T2 Macs have one. Where there is none the encoder says so and falls
back to the default rate controller, which costs a little latency but still
works. Receiving has no such requirement.

Double-clicking `StereoWire.app` with no arguments starts it in receive mode,
which is all the viewer ever needs. Notarization credentials are covered under
Releasing below.

There is no equivalent bundle for Windows. A Windows build is a single
unsigned `stereowire.exe`, either the `stereowire-windows-x64` CI artifact
(see Continuous integration) or cross-built (see Verifying it works). Nothing
signs or notarizes it: SmartScreen warns once on first run, and there is no
Gatekeeper equivalent to satisfy.

### Releasing

Releases are built, signed and notarized on your own machine, so the Developer
ID key never leaves it.

Notarizing needs a stored credential, created once. If you have never done it,
or `bundle.sh` reports `No Keychain password item found for profile:
stereowire`:

```
xcrun notarytool store-credentials stereowire \
  --apple-id you@example.com --team-id <team-id> --password <app-specific-password>
```

The app-specific password comes from appleid.apple.com under Sign-In and
Security. It is a separate 16-character token with its own revocation. Then cut
a release:

```
./scripts/bundle.sh --notarize          # build, sign, notarize, staple, re-zip
gh release create v0.1.0 dist/StereoWire.zip --title v0.1.0 --generate-notes
```

Keep the tag and the workspace version in `Cargo.toml` in step, because the
bundle takes its `CFBundleVersion` from `Cargo.toml`.

Before sending the zip to anyone, confirm the ticket actually stapled, since a
signed but unnotarized build is refused by Gatekeeper on a machine that has
never seen it:

```
spctl --assess --type execute --verbose=2 dist/StereoWire.app   # accepted, Notarized Developer ID
xcrun stapler validate dist/StereoWire.app
```

#### Signing in CI instead (dormant)

`.github/workflows/release.yml` can do all of the above on a tag. It is
deliberately inert: it has no `push` trigger and runs only when started by hand,
so nothing fails for want of secrets that were never added. Enabling it needs
five repository secrets:

| secret | what it is |
|--------|------------|
| `MACOS_CERT_P12` | Developer ID Application certificate and key, exported from Keychain Access as .p12, then base64 encoded |
| `MACOS_CERT_PASSWORD` | the password set when exporting that .p12 |
| `NOTARY_KEY_P8` | App Store Connect API key (.p8), base64 encoded |
| `NOTARY_KEY_ID` | that key's ID |
| `NOTARY_ISSUER` | the issuer ID from App Store Connect |

It would use an App Store Connect API key rather than an Apple ID and
app-specific password, because that key is revocable on its own and is not tied
to anyone's Apple ID, and it pins `actions/checkout` to a commit rather than a
tag, since a tag can be moved to point at different code and that job would hold
the signing key.

Weigh it up before enabling. Uploading a Developer ID certificate means anyone
who can run a workflow here can sign code as you, and a compromised Developer ID
is not a small problem: Apple can revoke it, which invalidates everything you
have ever signed with it. Local notarizing takes about three minutes, which for
a two-person tool is cheaper than the exposure.

## Bandwidth

Video is whatever you set with `--mbps`, but only when the screen is changing.
A static Logic window costs almost nothing. Audio is a constant ~6.3 Mbit/s
with FEC on, or ~3.1 Mbit/s with `--no-fec`.

## Limits

- Sender: macOS 13+ only. Receiver: macOS 13+ or Windows 10/11 x64. The
  capture and codec layers are Apple-specific. The protocol crate
  (`stereowire-proto`) is portable Rust with no platform dependencies.
- No Windows sender. Screen and system-audio capture are ScreenCaptureKit and
  CoreAudio, both macOS-only, so `stereowire send` on a Windows build errors
  immediately: `sending is macOS only for now; this build can receive`.
- Windows decoding is software only, through Media Foundation's own H.264 and
  HEVC decoders. No hardware path is used yet. 1440p60 needs a capable CPU,
  and `--max-width 1920` on the sender is the lever if the picture stutters.
- HEVC on Windows needs the "HEVC Video Extensions" app from the Microsoft
  Store. Without it, the receiver asks for `--codec h264` instead.
- Decoder latency on Windows has not been measured on real hardware, only
  under CrossOver on a Mac.
- No encryption of its own: the tunnel provides it. See Security above.
- Audio and video are timestamped on one capture clock and their offset is
  measured and reported, but nothing actively aligns them. Video is shown as
  soon as it decodes while audio waits in its jitter buffer, so audio is
  expected to lag by roughly the buffer depth, well inside the tolerance for
  a picture and its sound. The offset can only be measured between two
  machines: on one, playback feeds back into capture, and disabling playback
  drains the buffer as fast as it fills.
- One direction at a time, by design.
- Captures the main display. Window and multi-display selection is not wired up.
- Video recovers at most two lost fragments per group of ten, so a link losing
  more than a few percent will visibly stutter while audio keeps working.
- The link has never been run between two machines on the real internet. Every
  measurement here is loopback, with loss and reordering simulated.

## Playing together

This tool is for *watching and listening*. Run it for a few minutes and read
the title bar before deciding whether a given route is playable at all.

Rough guide for one-way delay, where sound travels about 1.1 feet per
millisecond, so the figures map onto how far apart you would be standing:

| one-way | feels like  | verdict                                  |
|---------|-------------|------------------------------------------|
| < 10 ms | same room   | comfortable                              |
| 10-20ms | 10-20 feet  | workable for most material               |
| 20-30ms | 20-35 feet  | difficult, tempo tends to drag           |
| > 30 ms | 35+ feet    | tight rhythmic playing breaks down       |

Beyond roughly 25-30 ms each way, ensembles progressively slow down, because
each player is waiting on the other. That threshold holds regardless of
practice.

Before assuming a route is fine, also check `tailscale status` says **direct**.
If it says *relay*, traffic is bouncing through a Tailscale DERP server and the
latency can be far worse than the distance suggests.

For actually jamming, pick the tool that matches the route you measured:

- **Under roughly 25-30 ms one-way: [JackTrip](https://jacktrip.org).** It
  sends uncompressed audio over UDP with the smallest buffers the hardware
  allows, so everyone hears everyone in something close to real time and plays
  as they normally would.
- **Above that: [NINJAM](https://www.cockos.com/ninjam/)** (or
  [JamTaba](https://jamtaba-music-web-site.appspot.com) as a friendlier
  client). It gives up on real time and instead delays everyone by a whole
  musical interval, so you play along to the previous bar rather than to each
  other. It only works on looping, chord-based material, but it stays in time
  on links where JackTrip would be unusable.

Measure before choosing. A route that reads well under load takes JackTrip. One
that sits above 30 ms, or that `tailscale status` reports as *relay*, takes
NINJAM.
