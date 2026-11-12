#!/usr/bin/env python3
"""Generate hyprbeat's original audio assets (CONTEXT.md I-03, D-34).

Everything here is SYNTHESIZED from first principles (sines, filtered noise,
envelopes) with a fixed seed — zero third-party content, therefore CC0 by
provenance: the only author is this script, which is committed alongside its
outputs. `git ls-files assets/` is the I-03 audit.

Outputs (16-bit PCM mono 44100 Hz WAV):
  assets/hitsounds/normal.wav    soft short tick   (~60 ms)
  assets/hitsounds/clap.wav      noise burst       (~90 ms)
  assets/hitsounds/whistle.wav   sweeping sine     (~250 ms)
  assets/hitsounds/finish.wav    cymbal-ish tail   (~400 ms)
  assets/test-tone.wav           12 s 440 Hz tone with 2 Hz amplitude beats
                                  (phase-2 validation: audible + visibly
                                  pulsing wallpaper while it plays)

Usage: python3 dev/gen_assets.py   (from the repo root)
"""

import math
import os
import struct
import wave

RATE = 44100
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)


# deterministic LCG noise (no random module — seed is fixed, output is stable)
class Lcg:
    """x_{n+1} = (a*x_n + c) mod 2^32 — enough for click/cymbal textures."""

    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFF

    def next_f32(self) -> float:
        # constants from Numerical Recipes
        self.state = (1664525 * self.state + 1013904223) & 0xFFFFFFFF
        return (self.state / 0xFFFFFFFF) * 2.0 - 1.0


def one_pole_lowpass(samples: list[float], alpha: float) -> list[float]:
    out = []
    y = 0.0
    for x in samples:
        y += alpha * (x - y)
        out.append(y)
    return out


def env_exp(n: int, tau_ms: float) -> list[float]:
    tau = tau_ms * RATE / 1000.0
    return [math.exp(-i / tau) for i in range(n)]


def env_attack_decay(n: int, attack_ms: float, tau_ms: float) -> list[float]:
    a = max(1, int(attack_ms * RATE / 1000.0))
    e = []
    for i in range(n):
        if i < a:
            e.append(i / a)
        else:
            e.append(math.exp(-(i - a) / (tau_ms * RATE / 1000.0)))
    return e


def make_normal() -> list[float]:
    # short soft "tick": 1050 Hz sine under a fast exponential decay, with a
    # 2 ms noise transient on top so the attack is audible
    n = int(0.060 * RATE)
    rng = Lcg(0x48595270)
    noise = one_pole_lowpass([rng.next_f32() for _ in range(n)], 0.4)
    body = [math.sin(2.0 * math.pi * 1050.0 * i / RATE) for i in range(n)]
    e = env_exp(n, 12.0)
    out = []
    for i in range(n):
        click = noise[i] * (1.0 if i < int(0.002 * RATE) else 0.0)
        out.append(0.55 * body[i] * e[i] + 0.35 * click * e[i])
    return out


def make_clap() -> list[float]:
    # three quick noise bursts (0 / 12 / 24 ms) over one decay
    n = int(0.090 * RATE)
    rng = Lcg(0x636C6170)
    bursts = [rng.next_f32() for _ in range(n)]
    hi = one_pole_lowpass(bursts, 0.8)   # keep the snap
    lo = one_pole_lowpass(bursts, 0.15)  # body
    e = env_exp(n, 26.0)
    out = []
    for i in range(n):
        on = (
            i < int(0.012 * RATE)
            or int(0.024 * RATE) <= i < int(0.036 * RATE)
            or int(0.048 * RATE) <= i
        )
        out.append((0.5 * hi[i] + 0.6 * lo[i]) * e[i] * (1.0 if on else 0.25))
    return out


def make_whistle() -> list[float]:
    # sine sweep 2600 -> 1800 Hz with 6 Hz vibrato, fade in/out
    n = int(0.250 * RATE)
    out = []
    phase = 0.0
    for i in range(n):
        t = i / RATE
        f = 2600.0 - (800.0 * t / 0.250) + 40.0 * math.sin(2.0 * math.pi * 6.0 * t)
        phase += 2.0 * math.pi * f / RATE
        fade = min(i / (0.005 * RATE), 1.0, (n - i) / (0.010 * RATE))
        out.append(0.55 * math.sin(phase) * fade)
    return out


def make_finish() -> list[float]:
    # cymbal-ish: bright noise + 1200 Hz sine tail, long decay
    n = int(0.400 * RATE)
    rng = Lcg(0x46694E69)
    noise = one_pole_lowpass([rng.next_f32() for _ in range(n)], 0.9)
    body = [math.sin(2.0 * math.pi * 1200.0 * i / RATE) for i in range(n)]
    e = env_attack_decay(n, 1.0, 150.0)
    out = []
    for i in range(n):
        out.append((0.45 * noise[i] + 0.5 * body[i]) * e[i])
    return out


def make_test_tone() -> list[float]:
    # 12 s of 440 Hz at low amplitude; every 500 ms a 60 ms "beat" swells to
    # ~3x so the envelope is both audible and visible in the wallpaper.
    n = int(12.0 * RATE)
    out = []
    for i in range(n):
        t = i / RATE
        s = math.sin(2.0 * math.pi * 440.0 * t)
        phase = t % 0.5
        swell = 1.0
        if phase < 0.060:
            # smooth bump: sin^2 over the 60 ms window
            x = phase / 0.060
            swell = 1.0 + 2.0 * math.sin(math.pi * x) ** 2
        out.append(0.14 * s * swell)
    return out


def write_wav(path: str, samples: list[float], peak: float) -> None:
    # normalize to the requested peak, clip, dither-free 16-bit
    m = max(1e-9, max(abs(s) for s in samples))
    k = peak / m
    frames = b"".join(
        struct.pack("<h", int(max(-32768, min(32767, round(s * k * 32767.0)))))
        for s in samples
    )
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(frames)
    print(f"wrote {os.path.relpath(path, REPO)} "
          f"({len(samples) / RATE * 1000:.0f} ms, {os.path.getsize(path)} bytes)")


def main() -> None:
    sounds = {
        "normal": (make_normal(), 0.25),
        "clap": (make_clap(), 0.30),
        "whistle": (make_whistle(), 0.22),
        "finish": (make_finish(), 0.30),
    }
    for name, (samples, peak) in sounds.items():
        write_wav(os.path.join(REPO, "assets", "hitsounds", f"{name}.wav"), samples, peak)
    write_wav(os.path.join(REPO, "assets", "test-tone.wav"), make_test_tone(), 0.55)


if __name__ == "__main__":
    main()
