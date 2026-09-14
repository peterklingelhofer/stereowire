# Remote music collaboration: what works at what distance

Notes on choosing an approach for playing music with someone in another city,
and where the thresholds actually fall. Written alongside `stereowire`, which
solves a different problem: watching and listening while someone else works.

## The one number that matters

**One-way delay**, ear to ear. Bandwidth is irrelevant at these rates, and
jitter matters only because it decides how much you have to buffer, which feeds
back into the delay.

The useful intuition: sound travels about **1.1 feet per millisecond**. So any
one-way delay converts directly into "how far apart are we standing".

| one-way | like standing | in practice |
|---------|---------------|-------------|
| 5 ms    | 5 feet        | same room, unnoticeable |
| 10 ms   | 11 feet       | comfortable, normal band spacing |
| 20 ms   | 22 feet       | workable; wide stage |
| 25 ms   | 28 feet       | the practical threshold |
| 35 ms   | 39 feet       | tempo drags, playing fights you |
| 50 ms   | 55 feet       | real-time ensemble is not happening |

The ~25 ms figure comes out of ensemble studies (Chafe and colleagues at
Stanford, and follow-on work), which consistently find that pairs playing
together stay in time up to roughly this point, and past it **tempo
progressively decelerates** because each player is unconsciously waiting on the
other. Below about 10 ms, ensembles tend to slightly *accelerate*. Both effects
are control-loop properties, so practice does not train them away.

## Estimating delay from distance

Light in fiber travels about 200,000 km/s. That gives a floor:

    round trip (ms) ≈ distance (km) / 100

Real routes wander and every hop adds a little, so **expect 1.5x to 2.5x that
figure**, and measure your own.

| great-circle | theoretical RTT | realistic RTT | realistic one-way | verdict |
|--------------|-----------------|---------------|-------------------|---------|
| 300 km       | 3 ms            | 8-15 ms       | 4-8 ms            | real-time, easy |
| 1,200 km     | 12 ms           | 25-40 ms      | 12-20 ms          | real-time, good |
| 2,000 km     | 20 ms           | 35-55 ms      | 18-28 ms          | real-time, marginal |
| 4,000 km     | 40 ms           | 70-90 ms      | 35-45 ms          | interval-based |
| 8,000 km     | 80 ms           | 140-180 ms    | 70-90 ms          | interval or async |

### Two example routes

| route | distance | est. one-way | approach |
|-------|----------|--------------|----------|
| New York to Chicago | ~1,150 km | 12-20 ms | real-time is realistic |
| New York to Seattle | ~3,900 km | 35-45 ms | **interval-based (NINJAM)** |

The coast-to-coast route is over three times the first, which puts it past the
real-time threshold for good; no software fixes distance. NINJAM is the right
call there, and plenty of records have been made by overdubbing to a loop.

## The network is only part of the budget

Delay accumulates at every stage, and the network is often the smaller share:

| source | typical | notes |
|--------|---------|-------|
| Audio interface in + out | 5-12 ms | buffer size dependent; 64 samples at 48 kHz is 1.3 ms per buffer |
| Network one-way | varies | the part you cannot change |
| **Bluetooth headphones** | **100-300 ms** | **disqualifying; use wired headphones** |
| Wi-Fi instead of Ethernet | +2-10 ms, plus jitter | use Ethernet both ends |
| DAW plugins with lookahead | 0-50 ms | limiters and linear-phase EQ are the usual culprits |

So a 35 ms network path realistically lands near **45-50 ms** ear to ear. Budget
accordingly: once the network alone passes 25 ms, the question is answered.

Bypassing the DAW matters. JackTrip and SonoBus talk to the audio interface
directly, which is why they beat routing through Logic for this purpose.

## The three approaches

### 1. Real-time (under ~25 ms one-way)

Everyone plays together as normal, accepting a small fixed delay.

- **[SonoBus](https://sonobus.net)**: free and open source, peer-to-peer.
  Easiest to get working, since it handles NAT traversal and its quality is
  configurable up to uncompressed. **Start here.** It reaches nearly the same
  result as JackTrip for much less setup.
- **[JackTrip](https://jacktrip.org)**: free and open source. Uncompressed
  audio, the smallest buffers the hardware allows, the lowest latency
  available. More setup, and it wants JACK. Its paid Virtual Studio service is
  an optional convenience; the software alone does the job.
- **[Jamulus](https://jamulus.io)**: free and open source, server-based, Opus
  compressed. Scales to larger groups. The codec adds a few ms, so it loses to
  the above for a duo.

### 2. Interval-based (any distance)

**[NINJAM](https://www.cockos.com/ninjam/)** sidesteps latency. Everyone agrees
on a tempo and an interval length, commonly 8 or 16 beats, and plays to a click.
What you hear from everyone else is what they played during the **previous
interval**, delayed to land exactly on the bar.

The delay is real and *musically aligned*, so everything lands on the beat. It
works identically at 5 ms or 500 ms, so distance stops mattering.

- **Good for:** grooves, riffs, improvisation, layering ideas, songwriting,
  long-form jamming. You build a piece by responding to the last loop.
- **Bad for:** arranged material with stops, hits, and tight call-and-response
  inside a bar. You cannot trade fours in real time.
- **Adjusting to it** takes a session or two. The trick is to play *to* what
  you hear, as if overdubbing, and let it sit a loop behind.

**[JamTaba](https://jamtaba.com)** is the client to use: free, open source, a
modern NINJAM interface, and it runs as a **VST/AU plugin inside Logic**, which
keeps the whole session inside the DAW.

**Self-hosting the server:** NINJAM's server (`ninjamsrv`) is free and runs on
any machine. Run it on either Mac and connect both clients over Tailscale, which
keeps it private and account-free and reuses the setup `stereowire` already
needs.

### 3. Asynchronous

For arranged material, trading Logic sessions or stems often beats either of the
above, since latency stops existing once nobody is playing simultaneously. It
suits a two-person project well: track separately, then use `stereowire` to
review takes together and decide what to keep.

## Where `stereowire` fits

`stereowire` shares a screen and system audio at full fidelity in one direction,
so one person can watch the other work in Logic and hear exactly what they hear.

Use it for the parts that are about *listening and deciding*: mix decisions,
arrangement review, "does this take work", walking through a session. Its audio
is uncompressed, so you can make mix judgements against it, which the codec in a
conferencing tool rules out.

Run NINJAM/JamTaba for playing and `stereowire` for everything else. They cover
different halves of the work and can run at the same time.

## Measuring the actual link

Do this before deciding anything. The tables above are starting points; your
route is the only figure that counts.

    tailscale status              # must say "direct", not "relay"
    tailscale ping <machine>      # direct vs relayed, and RTT
    ping <their-tailscale-ip>     # steady-state RTT and jitter

`stereowire receive` also shows live round trip and jitter in its title bar,
which is the easiest way to watch a route over a few minutes.

Two things to check beyond the average:

1. **Is the path direct?** If Tailscale reports *relay*, traffic is going
   through a DERP server and latency can be far worse than distance implies.
2. **How much is it moving?** A steady 80 ms is more playable than something
   swinging between 50 and 130, because the buffer has to cover the worst case.
   Check at the hour you would actually play; evening congestion is real.
