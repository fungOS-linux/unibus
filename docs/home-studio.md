# Home studio notes

Written 2026-09-30. Covers monitoring, clocking and MIDI, the patchbay map,
the pedal/reamp path, and the planned patch-matrix recording console. Items
marked **Check** are assumptions that have not been confirmed.

## Verified live state (2026-09-30)

- The Mac Studio and Pi are connected over Ethernet (`192.168.2.3` and
  `192.168.2.74`). A 100-packet test measured 0% loss and 0.54 ms average RTT.
- The Launchkey Mini is connected to the Mac Studio and sends MIDI through
  Unibus to FluidSynth on the Pi.
- The KeyStep mk2 is connected locally to the Pi and is subscribed directly to
  the same FluidSynth input. Both keyboards can play it simultaneously.
- The minimum MIDI note velocity is 1 (of 127).
- Wired MIDI testing delivered 96 notes in order. The raw timestamp envelope
  was 69 ms median, 94 ms p95, and 114 ms maximum, but includes approximately
  67 ms of clock offset between hosts. Typical network transport was therefore
  about 2 ms. Unlike Wi-Fi, Ethernet showed no 250-1000 ms stalls.
- **Check:** the notes below identify the Pi's shared playback device as the
  2i2, while the current piano target has also been described as the 18i20.
  Enumerate ALSA devices and record their USB serials before relying on either
  statement.

## 1. Gear inventory (as stated)

### Interfaces and mixing

- Scarlett 18i20 (4th Gen), the main interface
- Scarlett 2i2 (on the Pi), Scarlett 4i4 (spare, could come back)
- Mackie ProFX12v3
- 48-point TRS patchbay (top row 1-24, bottom row 25-48)
- Pi 4, which every USB interface is now plugged into

### Monitors

- KRK Rokit 4 pair (main)
- Kenwood KR-6400 receiver + 3-way passive speakers (second opinion / A-B)
- Subwoofer on the Mackie control-room output
- ATH-M50x headphones (on the 2i2)

### Outboard

- Warm Audio WA-2A clone (mono)
- Symetrix 525 (stereo)
- GL-100 (mono in, stereo out; previously called DL-100, same box)
- Fiedelstein Lunchbox: 8 slots, XLR in/out per slot, no modules yet
- Reamp box, passive DI (D0D-260), Vox 10W practice amp

### Synths and controllers

- Moog DFAM, Behringer Neutron, modular (mono out)
- Roland TR-6S (USB audio to the Pi)
- Pam's Pro Workout (master clock), DrummerTime module
- Arturia KeyStep 32 Mk2, Novation Launchkey Mini, Numark Mixtrack Platinum

### Mics and other

- 2x Shure SM57, AT2020, kick mic, Blue Snowball (USB)
- Field recorder (not part of this system)
- Pedals, guitar, bass, banjo

## 2. Monitoring chain

```text
18i20 OUT 1-2 -> Mackie -> main out -> KRKs
Mackie Sub mix out -> Kenwood
Mackie control room -> subwoofer
```

- Levers: main level (KRKs), Sub bus level (Kenwood), control room knob (sub),
  and the Kenwood's own volume.
- The monitor path stays off the patchbay (fixed wiring).
- Two volume controls are in series (18i20 monitor knob and Mackie). Leave the
  18i20 knob at a fixed point and control listening level at the Mackie so
  level-matched A/B settings stay put.
- Level-match A/B with pink noise and an SPL app, within about 0.5 dB, and note
  both knob positions.
- Sub: start the low-pass around 80 Hz, adjust by ear in 10 Hz steps (60-100
  Hz), flip polarity at the crossover, and level-match last. Use the sub's
  normal line input, not an LFE/bypass input.
- Mackie XLR outs back into the 18i20 work, but they are line level into a mic
  preamp. Keep gain at minimum and watch the clip light, or use an XLR-to-TRS
  cable into a combo input set to line.
- **Check:** the 3rd-gen guide identifies OUT 3-4 as the ALT monitor pair. If
  the 4th-gen unit has them configured as alt speakers, they follow the monitor
  knob. Move the 525 sends to OUT 5-6 if so.

## 3. Clock and MIDI

- Pam's is the master clock.
- Pam's MIDI out (TRS) -> TRS star splitter -> DrummerTime MIDI/clock in and
  18i20 MIDI in (TRS-to-DIN adapter).
- The 18i20 MIDI out is not a thru; it sends only what the computer sends.
- Clock and notes are separate signals. The Neutron needs notes, not clock:
  KeyStep MIDI out (DIN) -> Neutron MIDI in. KeyStep USB also goes to the host.
- The Launchkey Mini plays soft synths through Unibus MIDI transport.
- TR-6S clock: use a third splitter leg to its MIDI in (DIN), or forward it
  from the DAW over USB (with potentially more jitter).
- **Check:** TRS MIDI Type A vs Type B on the adapters. Swap if there is no
  data. Passive splitting beyond 2-3 loads may drop out; a 1-in/4-out MIDI thru
  box is the clean fix.
- **Check:** DrummerTime's jack type and whether the same jack accepts analog
  clock.

## 4. Audio host and clocking

- Everything USB is on the Pi 4. The Mac is the control surface and runs soft
  synths. Soft-synth audio reaches the Pi over Isochrone.
- With every device on one host, host timestamps provide one timebase from
  which per-device drift can be measured and corrected.
- Each interface still has its own crystal. At about 10 ppm, a five-minute take
  drifts about 3 ms.
- Record each device raw at its native rate with host-time timestamps and align
  offline. Resample in real time only for monitored paths and insert returns.
- Hardware direct monitoring stays on the interface, not through the Pi.

### Isochrone state (from the repository)

- One-way RTP audio over UDP, 48 kHz stereo, 1 ms packets. ALSA capture and
  playback on Linux; CoreAudio capture on macOS.
- The ASRC servo (slow PI controller, clamped to 100 ppm, reports saturation)
  is written and tested, but correction is not applied yet. There is no
  production resampler.
- There is no CoreAudio playback yet, so Mac-side receive is unavailable.
- The Pi's 2i2 is currently shared through `hardware_dmixer`, used by JARVIS
  TTS and music. If the 2i2 becomes the insert loop, the console needs its
  output and JARVIS audio must move elsewhere.

### Pi 4 checks

- About 60 channels at 48 kHz/24-bit is roughly 9 MB/s and fits USB 2 in
  principle, but test the 18i20, 2i2, and TR-6S together. Do not plan on 96 kHz
  across all of them.
- The 2i2 and TR-6S are bus-powered; use a powered hub.
- Record to an SSD over USB 3, not the SD card.
- Give audio threads real-time priority and disable USB autosuspend.
- **Check:** every device enumerates (`arecord -l`), including the TR-6S.

## 5. TR-6S

- USB audio mode streams six mono instrument tracks (USB IN 1-6) plus the
  master. Set USB AUDIO to INDIV in the utility menu.
- Roland's Ableton guide shows the driver as 8 inputs / 4 outputs.
- Audio sent to it over USB passes through its onboard FX, like the TR-8S
  external input.
- It stays off the patchbay. For live monitoring, connect its analog outputs
  to a spare Mackie stereo line channel for zero latency.
- **Check:** the analog outputs continue working while it streams USB audio.
- To process drums through outboard, the console routes a stem through an
  output send (OUT 3-4 for the 525, OUT 7 for the WA-2A) and back in.

## 6. Interface counts (assumed)

These are based on the Focusrite 3rd-generation 18i20 guide. **Check** them
against the 4th-generation device in Focusrite Control.

- 8 analog inputs with mic preamps: IN 1-2 front combos with INST switches;
  IN 3-8 rear combos.
- 10 analog outputs: 1-2 main monitors, 3-4 ALT, and 5-10 line outputs.
- ADAT input/output (8 channels at 48 kHz), S/PDIF, MIDI input/output (5-pin
  DIN), and word-clock output.
- Inputs 1-2 are shared front/rear. The front jack wins and disables the rear
  connector. Keep bay cables in the rear and plug guitar, bass, or mic into the
  front as needed. This temporarily silences that channel's normal source (IN
  1 = modular, IN 2 = DFAM).

## 7. Patchbay map

Each column is one normalled pair: top jack *c* is wired to bottom jack *c* +
24. In half-normal mode, plugging into the top taps the signal without breaking
the normal; plugging into the bottom breaks it and substitutes another source.
Columns 17-18 are thru because nothing is wired below them. Stereo pairs are
adjacent and start on an odd-numbered input.

| Col | Top: source | Bottom: normalled to | Mode |
|---:|---|---|---|
| 1 | Modular out (mono) | 25: 18i20 IN 1 | Half |
| 2 | DFAM out | 26: IN 2 | Half |
| 3 | Neutron out | 27: IN 3 | Half |
| 4 | WA-2A out | 28: IN 4 | Half |
| 5 | 525 out L | 29: IN 5 | Half |
| 6 | 525 out R | 30: IN 6 | Half |
| 7 | GL-100 out L | 31: IN 7 | Half |
| 8 | GL-100 out R (unwired) | 32: IN 8, XLR direct, bay cable removed | n/a |
| 9 | 18i20 OUT 3 | 33: 525 in L | Half |
| 10 | OUT 4 | 34: 525 in R | Half |
| 11 | OUT 5 | 35: GL-100 in (mono) | Half |
| 12 | OUT 6 | 36: spare send | Half |
| 13 | OUT 7 | 37: WA-2A in | Half |
| 14 | OUT 8 | 38: reamp box in | Half |
| 15-16 | OUT 9/10 | 39-40: spare pair | Half |
| 17-18 | Mackie main L/R | 41-42: no default | Thru |
| 19-24 | Spare | 43-48: spare | Half |

### Patchbay notes

- Flex inputs: IN 4-8. To record a mic, patch into its bottom jack (28-32),
  breaking the normal.
- Mackie print: patch columns 17-18 (top) into 29-30 when the 525 is not in
  use, or mix in the console.
- If the GL-100 is mono, use column 7 only.
- Full-normal is useful only where tapping the top should silence the original
  destination. The reamp send (column 14) is the likely candidate.
- Synth outputs are unbalanced. Use short TS cables. The DFAM and modular are
  hot, so set interface inputs to line and pad them if they clip.
- Software cannot discover physical patches. It needs a declared patch state:
  one entry per bottom jack naming what is plugged in.

## 8. Guitar, pedals, DI, and reamp

```text
Reamp:  OUT 8 -> col 14 / 38 -> reamp box --+
Live:   guitar -----------------------------+-> pedal chain -> passive DI -> XLR -> 18i20 IN 8
```

- The guitar replaces the reamp box at the front of the pedal chain. An A/B
  box avoids unplugging.
- The passive DI turns the pedal output into balanced mic level. Use its pad if
  the pedals clip the preamp.
- Do not send mic-level signals through the bay. Run the DI's XLR directly to
  the IN 8 rear combo.
- For direct guitar/bass, use front INST on IN 1-2; only those inputs have INST.
- Pedal-chain cables should be TS, not TRS. Use TRS only for balanced runs.
- Monitor with the interface's direct monitoring, not through the Pi.
- **Check:** whether the DI has a thru. With one, the DI and miked Vox can be
  recorded simultaneously, requiring a second input.

## 9. Lunchbox (open decision)

- Eight slots require eight sends and eight returns. Sixteen jacks do not fit
  the nine free bay columns or the interface's spare I/O.
- Plan: connect an 8-in/8-out ADAT converter (for example, a Behringer ADA8200)
  1:1 to the Lunchbox, off the patchbay. Use XLR where possible and TRS-to-XLR
  from converter line outputs. ADAT carries the 18i20 clock, with the 18i20 as
  master; all sixteen channels then share its clock domain.
- To use a module on another source, send a recorded lane over ADAT output *n*,
  through the module, and return it on ADAT input *n*.
- Mic-preamp modules take mic level; do not feed them directly from a line out.
- **Check:** confirm the 18i20 has ADAT input and output, then decide whether to
  add a converter.

## 10. Virtual patchbay / recording console

### It is

- A device graph and patch matrix mirroring the physical bay
- A raw multi-device recorder with timestamps and offline alignment
- A hardware-insert manager with measured round-trip offsets
- A MIDI clock/transport lane sourced from Pam's
- Playback of recorded lanes back through the rack

### It is not

- A plugin host, mixer, editor, or sequencer. Soft synths are audio sources.

### Milestones

1. Device list, patch matrix, and N-lane WAV recording from the 18i20.
2. Add the 2i2 as a second raw stream and measure real drift in ppm from
   recorded data.
3. Impulse-latency calibration for the insert loop.
4. MIDI clock lane and transport follow.
5. Real-time resampled return path for monitoring through the rack. This is
   where the Isochrone resampler is required.

Reuse from Isochrone: the ASRC governor, capture/playback traits, and ALSA
capture with xrun recovery. Still missing: the resampler, CoreAudio playback,
callback-driven capture, per-device latency calibration, and disk recording.

Existing tape-style DAWs worth trying first (neither product page states that
it records from multiple interfaces simultaneously or corrects their drift):

- GCS Model 8 v3.0: 24 tracks, macOS/Windows, $59.99 introductory price,
  10-day trial
- TAPE 16: 16 tracks, macOS/Windows/Linux, $29, 7-day trial

## 11. Open items

- Confirm the 18i20's real channel counts and whether OUT 3-4 are ALT.
- Decide whether to add the ADAT converter or keep the Lunchbox off the bay
  with fewer live channels.
- Decide where JARVIS audio goes if the 2i2 becomes the insert loop.
- Check whether the DI has a thru and whether the Vox or only the pedals belong
  in the loop.
- Verify Pam's, DrummerTime, and the splitter jack types.
- Enumerate every Pi audio device, including serials, and test aggregate USB
  bandwidth with all devices active.
- Scaffold console milestone 1 or design the patch-matrix data model first.

## Sources

- [Roland TR-6S review (MusicRadar)](https://www.musicradar.com/reviews/roland-tr-6s)
- [TR-8S/TR-6S individual-track recording (Roland Support)](https://support.roland.com/hc/en-us/articles/360050746551-TR-8S-TR-6S-Can-I-record-each-INST-to-an-individual-audio-track-in-my-DAW-software)
- [TR-6S Ableton Live guide (Roland)](https://rolandcorp.com.au/blog/tr-6s-ableton-live-guide%E2%80%8B)
- [Scarlett 18i20 4th Gen input 1-2 priority (Focusrite Support)](https://support.focusrite.com/hc/en-gb/articles/19846429547922-Scarlett-18i20-4th-Gen-Input-1-2-Connection-Priority)
- [18i20 hardware features, 3rd Gen (Focusrite)](https://userguides.focusrite.com/hc/en-gb/articles/23031280130706-18i20-hardware-features)
- [GCS Model 8 v3.0 (Bedroom Producers Blog)](https://bedroomproducersblog.com/2026/06/12/gcs-model-8-tape-daw/)
- [TAPE 16 (EMR Music Group)](https://emrmusicgroup.com/tape16/index.html)
