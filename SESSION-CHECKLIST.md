# First real session checklist

Everything measured so far is loopback on one machine, with loss and reordering
simulated. These are the numbers only a real link between two machines can give,
and the order to collect them in.

## Before starting

On both machines:

    stereowire doctor

Then confirm the path is direct. A relayed link adds latency the distance does
not explain, and is the single most likely reason a session feels worse than it
should:

    tailscale status        # must say "direct", not "relay"
    tailscale ping <peer>

Watch `ping <peer>` for a minute at the hour you would actually work. Note the
average **and** the spread: a steady 90 ms is far easier to work against than
something wandering between 50 and 130.

## Starting the link

Watcher:

    stereowire receive

Sharer:

    stereowire send --to <peer>

If the watcher is on Windows:

- The sender adds `--codec h264`, since that decodes with no extra software.
- The watcher runs `stereowire.exe receive`, or double-clicks `stereowire.exe`.
- Allow the Windows Firewall prompt on first run.
- Check the console afterwards for the interface rate note: if the output
  device is not at the stream's rate, Windows resamples and says so.

## What to read, and what it means

**Sender**, every five seconds:

    [   5s] video 57 fps, 8.0 Mbit/s | audio 750 pkt/s, peak -6.0 dBFS

- `peak silent` while Logic is playing means the capture is not picking up your
  interface. Fix that before anything else.
- `video bitrate now N Mbit/s` means congestion control reduced the rate. If it
  settles well below the maximum, the link cannot carry what you asked for.

**Receiver**, every five seconds:

    link: ... 0 frames dropped out of sync, 0 keyframe requests, in sync, video 40 Mbit/s
    latency: 78 ms round trip (about 39 ms each way), jitter 3 ms

- Steady `keyframe requests` mean frames are being lost outright.
- `audio: N recovered, M concealed` distinguishes repaired loss from audible
  loss. Recovered costs nothing. Concealed is a real gap.
- `audio: correcting clock drift by N ppm` is expected on two machines and is
  the point of the feature. Anything under a few hundred ppm is normal.
- `a/v offset` only appears with playback on, and is the first reading of
  audio/video alignment this project has been able to take on real hardware.

## Numbers worth writing down

| reading | where | why it matters |
|---------|-------|----------------|
| round trip and jitter | receiver, or the title bar | sets `--buffer-ms`, and decides whether real-time playing is even possible |
| direct or relay | `tailscale status` | a relay invalidates every latency conclusion |
| settled video bitrate | sender | whether the link carries the quality you asked for |
| concealed audio blocks | receiver | the audio number that marks a real gap in playback |
| clock drift ppm | receiver | confirms drift compensation is doing something on real clocks |
| a/v offset | receiver | never yet measured between two machines |
| frame pacing on Windows | receiver console, `video frames (N shown)` | whether software decoding keeps up with the sender's fps, the untested part of that path |

## If it goes wrong

- **Nothing arrives:** check `stereowire doctor` on both ends and that the
  sender used the peer's Tailscale name or 100.x address.
- **Picture freezes, audio fine:** the expected failure shape, since video is
  the fragile stream. Check keyframe requests and whether the bitrate settled
  low.
- **Audio gaps:** raise `--buffer-ms` to 50 or more: jitter is exceeding the
  buffer.
- **Everything lags progressively:** the link cannot carry the bitrate. Lower
  `--mbps`.
