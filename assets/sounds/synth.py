#!/usr/bin/env python3
"""Generate ThinkTerm's notification sounds.

These are synthesised rather than sourced so their provenance is unambiguous:
nothing here is licensed from anyone. Run this script to regenerate the WAVs
after changing a pitch, an envelope, or a timbre.

    python3 assets/sounds/synth.py

Standard library only, by design — nobody should need a toolchain to retune a
notification. Output is 44100 Hz / 16-bit / mono, peaking at -3 dBFS.

Three things decide whether a prompt feels gentle or sharp, and they are
independent: how high the partials reach, how fast the onset is, and how much
energy survives above a few kHz. Meaning, separately, comes from the interval —
rising and consonant reads as finished, a repeat reads as a question.
"""
import math
import os
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
    # Finished: a rising fifth resolves, so it reads as landing somewhere.
    ("done.wav", [(0.0, C5, 1.0), (0.075, G5, 0.92)], WOOD, 0.28, 0.70, 0.012, 3800),
    # Needs input: the same note twice asks rather than answers, and sits low
    # enough not to startle.
    ("needs-input.wav", [(0.0, E5, 1.0), (0.15, E5, 0.88)], BELL, 0.30, 0.85, 0.018, 3200),
]

if __name__ == "__main__":
    for args in SOUNDS:
        path, seconds = render(*args)
        print(f"{os.path.basename(path):16} {os.path.getsize(path) / 1024:5.0f} KB  {seconds:.2f}s")
