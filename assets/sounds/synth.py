#!/usr/bin/env python3
"""Generate ThinkTerm's notification sounds.

These are synthesised rather than sourced so their provenance is unambiguous:
nothing here is licensed from anyone. Run this script to regenerate the WAVs
after changing a pitch, an envelope, or a timbre.

    python3 assets/sounds/synth.py

Standard library only, by design — nobody should need a toolchain to retune a
notification. Output is 44100 Hz / 16-bit: needs-input.wav mono, peaking at
-3 dBFS; done.wav stereo, peaking at -4.4 dBFS, its voices spread across the
field and given a little room.

Three things decide whether a prompt feels gentle or sharp, and they are
independent: how high the partials reach, how fast the onset is, and how much
energy survives above a few kHz. Meaning, separately, comes from the interval —
rising and consonant reads as finished, a repeat reads as a question.
"""
import math
import os
import random
import struct
import wave

SR = 44100
OUT_DIR = os.path.dirname(os.path.abspath(__file__))

# (frequency ratio, amplitude, decay multiplier). Upper partials die first,
# which is what makes a struck object sound struck rather than blown.
WOOD = [(1.0, 1.0, 1.0), (3.9, 0.13, 3.5)]
BELL = [(1.0, 1.0, 1.0), (2.0, 0.20, 2.6), (3.0, 0.06, 4.5)]

C5, E5, G5 = 523.25, 659.25, 784.0


def strike(buf, start, freq, decay, timbre, gain, attack):
    """Mix one struck note into `buf`, `start` seconds in."""
    origin = int(start * SR)
    for i in range(int(min(decay * 5.0, 2.0) * SR)):
        if origin + i >= len(buf):
            break
        t = i / SR
        # Raised-cosine onset. An instant one clicks, and even a few
        # milliseconds too fast reads as a tick rather than a strike.
        envelope = 0.5 - 0.5 * math.cos(math.pi * min(t / attack, 1.0))
        value = 0.0
        for ratio, amp, decay_mul in timbre:
            value += amp * math.exp(-t / (decay / decay_mul)) * math.sin(
                2.0 * math.pi * freq * ratio * t
            )
        buf[origin + i] += value * envelope * gain


def lowpass(buf, cutoff):
    """One-pole rolloff, to round off whatever edge the partials leave."""
    alpha = 1.0 - math.exp(-2.0 * math.pi * cutoff / SR)
    carry = 0.0
    for i, sample in enumerate(buf):
        carry += alpha * (sample - carry)
        buf[i] = carry


def render(name, notes, timbre, decay, length, attack, cutoff):
    buf = [0.0] * int(length * SR)
    for start, freq, gain in notes:
        strike(buf, start, freq, decay, timbre, gain, attack)
    lowpass(buf, cutoff)

    # The file must not end on a non-zero sample or it clicks on the way out.
    fade = int(0.03 * SR)
    for i in range(fade):
        buf[len(buf) - fade + i] *= 1.0 - i / fade

    peak = max(abs(sample) for sample in buf) or 1.0
    scale = 0.708 / peak  # -3 dBFS: a prompt must not outshout the work
    frames = b"".join(
        struct.pack("<h", int(max(-1.0, min(1.0, sample * scale)) * 32767))
        for sample in buf
    )

    path = os.path.join(OUT_DIR, name)
    with wave.open(path, "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(SR)
        out.writeframes(frames)
    return path, len(buf) / SR


SOUNDS = [
    # Needs input: the same note twice asks rather than answers, and sits low
    # enough not to startle.
    ("needs-input.wav", [(0.0, E5, 1.0), (0.15, E5, 0.88)], BELL, 0.30, 0.85, 0.018, 3200),
]


# Finished: four steps that land. Two on an F chord, two on C -- the plagal
# "amen" -- each with a plucked edge so all four are heard, over a low thump
# per chord; then the C chord holds, a soft shimmer rising out of its top.
#
# The pieces below are seeded, and each draws its random phases in the order
# `finished` asks for them: changing that order changes the file.

PITCH = {
    "C2": 65.41, "F2": 87.31, "C3": 130.81, "F3": 174.61, "C4": 261.63,
    "E4": 329.63, "G4": 392.0, "A4": 440.0, "C5": 523.25, "G5": 783.99,
    "C6": 1046.5, "D6": 1174.66,
}


def voice(freq, length, gain, attack, release, tremolo=0.0, partials=4, stop=None):
    """A warm sustained note: three slightly detuned voices, each a few
    partials falling off by 1/h^1.6 -- soft, nothing struck about it."""
    count = int(length * SR)
    out = [0.0] * count
    detunes = [0.0, -0.003, 0.003]
    for detune in detunes:
        f = freq * (1 + detune)
        for h in range(1, partials + 1):
            amp = 1 / h**1.6
            step = 2 * math.pi * f * h / SR
            phase = random.random() * 6.28
            for i in range(count):
                out[i] += amp * math.sin(phase + step * i)
    for i in range(count):
        t = i / SR
        envelope = (1 - math.exp(-t / attack)) * math.exp(-t * release)
        if stop is not None and t > stop:
            envelope *= max(0.0, 1 - (t - stop) / 0.09)
        wobble = 1 - tremolo * (0.5 + 0.5 * math.sin(2 * math.pi * 5.5 * t))
        out[i] *= gain * envelope * wobble / len(detunes)
    return out


def pluck(freq, length, gain):
    """A plucked edge (Karplus-Strong): what makes a step heard as a step."""
    random.seed(1 + int(freq))
    period = max(2, int(SR / freq))
    ring = [random.uniform(-1, 1) for _ in range(period)]
    for _ in range(3):
        ring = [(ring[i] + ring[i - 1]) / 2 for i in range(period)]
    out = []
    index = 0
    for _ in range(int(length * SR)):
        sample = ring[index]
        ring[index] = 0.985 * 0.5 * (sample + ring[(index + 1) % period])
        out.append(gain * sample)
        index = (index + 1) % period
    return out


def thump(freq, length, decay):
    """A low thump under a chord, falling onto its pitch."""
    out = []
    phase = 0.0
    for i in range(int(length * SR)):
        t = i / SR
        pitch = freq * (1 + 0.6 * math.exp(-t * 25))
        phase += 2 * math.pi * pitch / SR
        out.append(0.9 * math.sin(phase) * min(1, t / 0.006) * math.exp(-t * decay))
    return out


def shimmer(freq, length, gain, fade=2.5):
    """Airy partials that swell in and flutter, rather than ring out."""
    count = int(length * SR)
    out = [0.0] * count
    for k, (ratio, amp) in enumerate(((1, 1), (1.5, 0.5), (2, 0.35))):
        step = 2 * math.pi * freq * ratio / SR
        phase = random.random() * 6.28
        for i in range(count):
            t = i / SR
            envelope = (
                (1 - math.exp(-t / 0.25))
                * math.exp(-t * fade)
                * (0.6 + 0.4 * math.sin(2 * math.pi * (4 + k) * t))
            )
            out[i] += gain * amp * envelope * math.sin(phase + step * i)
    return out


def room(channel, mix):
    """A small room: four feedback combs beside the dry signal, then two
    allpasses to smear their echoes into a tail."""
    out = channel[:]
    for delay, feedback in ((1557, 0.78), (1617, 0.77), (1491, 0.79), (1422, 0.80)):
        comb = [0.0] * len(channel)
        for i in range(len(channel)):
            comb[i] = channel[i] + (comb[i - delay] * feedback if i >= delay else 0)
        for i in range(len(channel)):
            out[i] += comb[i] * mix / 4
    for delay, g in ((225, 0.5), (556, 0.5)):
        smeared = [0.0] * len(out)
        for i in range(len(out)):
            before = out[i - delay] if i >= delay else 0
            fed = smeared[i - delay] if i >= delay else 0
            smeared[i] = -g * out[i] + before + g * fed
        out = smeared
    return out


def finished(name):
    random.seed(11)
    length = 1.8
    # How much sooner than its first cut everything dies away: the chord
    # lands as before and lets go a little quicker.
    tail = 1.25
    step = 0.13
    left = [0.0] * int(length * SR)
    right = [0.0] * int(length * SR)

    def place(start, samples, pan=0.0):
        origin = int(start * SR)
        to_left = math.cos((pan + 1) * math.pi / 4)
        to_right = math.sin((pan + 1) * math.pi / 4)
        for i, sample in enumerate(samples):
            if origin + i < len(left):
                left[origin + i] += sample * to_left
                right[origin + i] += sample * to_right

    def held(start, freq, gain, attack, release, pan=0.0, **shape):
        place(start, voice(freq, length - start, gain, attack, release * tail, **shape), pan)

    def edge(start, freq, gain, pan=0.0):
        place(start, pluck(freq, min(0.5, length - start), gain), pan)

    place(0, thump(PITCH["F2"], length, 4.0 * tail))
    place(2 * step, thump(PITCH["C2"], length - 2 * step, 2.4 * tail))
    # One and two, on F.
    held(0, PITCH["F3"], 0.50, 0.006, 1.6, partials=6)
    edge(0, PITCH["F3"], 0.50 * 0.35)
    held(0, PITCH["C4"], 0.25, 0.01, 1.0, -0.2, partials=4, stop=2 * step)
    held(step, PITCH["A4"], 0.36, 0.006, 1.0, 0.25, partials=4, tremolo=0.1)
    edge(step, PITCH["A4"], 0.13, 0.25)
    # Three and four, landing on C.
    held(2 * step, PITCH["G4"], 0.42, 0.006, 1.7, -0.3, partials=3, tremolo=0.25)
    edge(2 * step, PITCH["G4"], 0.42 * 0.35, -0.3)
    held(2 * step, PITCH["C3"], 0.45, 0.01, 1.6, partials=6)
    held(2 * step, PITCH["E4"], 0.30, 0.02, 1.7, 0.2, partials=3, tremolo=0.2)
    held(3 * step, PITCH["C5"], 0.45, 0.006, 1.8, 0.3, partials=3, tremolo=0.3)
    edge(3 * step, PITCH["C5"], 0.45 * 0.35, 0.3)
    for start, note, gain in ((3 * step + 0.04, "G5", 0.09), (3 * step + 0.10, "C6", 0.07), (3 * step + 0.16, "D6", 0.04)):
        place(start, shimmer(PITCH[note], length - start, gain, 2.5 * tail))

    left, right = room(left, 0.25), room(right, 0.25)
    peak = max(max(abs(s) for s in left), max(abs(s) for s in right)) or 1.0
    scale = 0.6 / peak
    fade = int(0.06 * SR)
    for i in range(fade):
        left[len(left) - 1 - i] *= i / fade
        right[len(right) - 1 - i] *= i / fade
    frames = b"".join(
        struct.pack(
            "<hh",
            int(max(-1, min(1, a * scale)) * 32767),
            int(max(-1, min(1, b * scale)) * 32767),
        )
        for a, b in zip(left, right)
    )
    path = os.path.join(OUT_DIR, name)
    with wave.open(path, "wb") as out:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(SR)
        out.writeframes(frames)
    return path, length


if __name__ == "__main__":
    for path, seconds in [finished("done.wav")] + [render(*args) for args in SOUNDS]:
        print(f"{os.path.basename(path):16} {os.path.getsize(path) / 1024:5.0f} KB  {seconds:.2f}s")
