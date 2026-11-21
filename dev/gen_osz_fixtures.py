#!/usr/bin/env python3
"""Generate the committed synthetic .osz fixtures for the phase-3 import
validation script (dev/validate-phase3-import.sh).

Provenance (I-03 posture, mirrors dev/gen_assets.py): every byte here is
INVENTED — synthetic .osu metadata, a hand-built 8-bit PCM WAV of zeros,
placeholder image bytes. No copyrighted content, no real beatmap data.
The generator runs where python3 exists (it does NOT run on the dev
machine — the devshell has no python3; that is exactly why the fixtures
are committed binaries, byte-pinned by sha256 in the validation script).

Deterministic: fixed entry order, fixed mtime, STORED method, fixed
attrs — re-running this script reproduces byte-identical fixtures.

  submitted.osz   set 20261115: 2 diffs + audio + bg kept (4), video/.osb/
                  skin/.txt skipped (4, off-allowlist), 1 dir entry skipped
  unsubmitted.osz BeatmapSetID -1: 1 diff + audio — routes to
                  songs/unsubmitted/<hash8>/ (D-43)
  oversize.osz    honest .osu + huge.wav whose CENTRAL directory declares
                  600 MiB (cap: 512 MiB, D-44) — must fail BEFORE any write
  timeline.osz    set 20261119 (Δ-J, the visualizer fixture): Normal +
                  Hard + tone.wav. Normal is the hand-computed corrected-
                  §7.3 case: circles t=0/500, slider t=2000 length 300
                  slides 2 (SliderMultiplier 2.0, beatLength 500,
                  inherited -50 at t=1000 → SV 2.0 → 300/(100·2·2)·500 =
                  375 ms/slide, 750 ms total → 2000..2750), spinner
                  3000..4200, kiai 4000..8000 clamped to the span →
                  objects=4 circles=2 sliders=1 spinners=1 kiai=1 span
                  0..4200. Hard (invented the same way): circles
                  t=0/250/500/750, slider t=1500 length 300 slides 1 →
                  1500..1875, spinner 2500..3700, kiai 3000..8000 →
                  clamped (3000, 3700) → objects=6 circles=4 sliders=1
                  spinners=1 kiai=1 span 0..3700. Entry names carry the
                  sort digit because the manifest (and so the loader's
                  "first difficulty") is ENTRY-NAME order — Normal must be
                  first so `--diff Hard` exercises the exact-match path.
                  tone.wav = tiny_wav(128) (distinct byte count from the
                  other fixtures' audio: no cross-set canonical dedup).
"""

import os
import struct
import sys
import zipfile

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")
ZIP_DATE = (2026, 11, 15, 0, 0, 0)  # fixed for determinism
SET_ID = 20261115
TIMELINE_SET_ID = 20261119  # Δ-J: invented (I-03), dated like the others


def osu_text(diff: str, set_id: int) -> bytes:
    return (
        "osu file format v14\n"
        "\n"
        "[Metadata]\n"
        f"BeatmapSetID: {set_id}\n"
        "Title: Fixture Title\n"
        "Artist: Fixture Artist\n"
        "Creator: hyprbeat\n"
        f"Version: {diff}\n"
        "\n"
        "[Difficulty]\n"
        "HPDrainRate:5\n"
        "CircleSize:3\n"
        "OverallDifficulty:6\n"
        "ApproachRate:8\n"
    ).encode()


def timeline_osu(version: str, beatmap_id: int, timing: str, objects: str) -> bytes:
    """A full visualizer-fixture difficulty (unlike `osu_text`'s metadata
    stubs): General/Metadata/Difficulty/TimingPoints/HitObjects, modeled on
    assets/golden/v14-standard.osu. `SliderMultiplier: 2.0` + the caller's
    timing/objects make every duration hand-computable via the corrected
    §7.3 formula (length / (100 · SM · SV) · beatLength per slide)."""
    return (
        "osu file format v14\n"
        "\n"
        "[General]\n"
        "AudioFilename: tone.wav\n"
        "AudioLeadIn: 0\n"
        "Mode: 0\n"
        "\n"
        "[Metadata]\n"
        "Title: Timeline Fixture\n"
        "Artist: Fixture Artist\n"
        "Creator: hyprbeat\n"
        f"Version: {version}\n"
        f"BeatmapID: {beatmap_id}\n"
        f"BeatmapSetID: {TIMELINE_SET_ID}\n"
        "\n"
        "[Difficulty]\n"
        "HPDrainRate: 5\n"
        "CircleSize: 4\n"
        "OverallDifficulty: 8\n"
        "ApproachRate: 9\n"
        "SliderMultiplier: 2.0\n"
        "SliderTickRate: 1\n"
        "\n"
        "[TimingPoints]\n"
        f"{timing}\n"
        "\n"
        "[HitObjects]\n"
        f"{objects}\n"
    ).encode()


def tiny_wav(num_samples: int) -> bytes:
    """Minimal valid RIFF/WAVE: 8-bit mono 8000 Hz, all zeros."""
    data = b"\x80" * num_samples  # 0x80 = silence in 8-bit unsigned PCM
    header = struct.pack(
        "<4sI4s4sIHHIIHH4sI",
        b"RIFF", 36 + len(data), b"WAVE",
        b"fmt ", 16,
        1,          # PCM
        1,          # mono
        8000,       # rate
        8000,       # byte rate
        1,          # block align
        8,          # bits
        b"data", len(data),
    )
    return header + data


def write_zip(path: str, entries: list[tuple[str, bytes]]) -> None:
    with open(path, "wb") as fh:
        with zipfile.ZipFile(fh, "w", compression=zipfile.ZIP_STORED) as zf:
            for name, data in entries:
                zi = zipfile.ZipInfo(name, date_time=ZIP_DATE)
                zi.create_system = 3  # unix
                if name.endswith("/"):
                    # directory entry: unix dir mode + DOS directory bit
                    zi.external_attr = (0o40755 << 16) | 0x10
                    data = b""
                else:
                    zi.external_attr = 0o644 << 16
                zf.writestr(zi, data, compress_type=zipfile.ZIP_STORED)


def crc32_ieee(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0xEDB88320 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def hand_rolled_oversize(path: str) -> None:
    """A zip zipfile cannot write: honest STORED .osu + an entry whose
    CENTRAL directory declares 600 MiB while the local data is 10 bytes.
    The D-44 pass-1 declared-size check must abort before any read."""
    out = bytearray()
    name1 = b"Fixture Artist - Fixture Title (hyprbeat) [Easy].osu"
    data1 = osu_text("Easy", SET_ID)
    crc1 = crc32_ieee(data1)
    name2 = b"huge.wav"
    data2 = b"0123456789"
    crc2 = crc32_ieee(data2)
    lie = 600 * 1024 * 1024

    def local_header(name: bytes, size: int, crc: int) -> bytes:
        return struct.pack(
            "<4s5H3I2H", b"PK\x03\x04", 20, 0, 0, 0, 0,
            crc, size, size, len(name), 0,
        )

    def central(name: bytes, size: int, crc: int, offset: int) -> bytes:
        return struct.pack(
            "<4s6H3I5H2I", b"PK\x01\x02", 20, 20, 0, 0, 0, 0,
            crc, size, size, len(name), 0, 0, 0, 0, 0o644 << 16, offset,
        ) + name

    off1 = len(out)
    out += local_header(name1, len(data1), crc1) + name1 + data1
    off2 = len(out)
    # local header stays honest; only the central directory lies
    out += local_header(name2, len(data2), crc2) + name2 + data2
    cd_offset = len(out)
    cd = central(name1, len(data1), crc1, off1)
    cd += central(name2, lie, crc2, off2)
    out += cd
    out += struct.pack(
        "<4s4H2IH", b"PK\x05\x06", 0, 0, 2, 2, len(cd), cd_offset, 0
    )
    with open(path, "wb") as fh:
        fh.write(out)


def main() -> int:
    os.makedirs(FIXTURES, exist_ok=True)

    submitted = os.path.join(FIXTURES, "submitted.osz")
    write_zip(submitted, [
        ("Fixture Artist - Fixture Title (hyprbeat) [Easy].osu",
         osu_text("Easy", SET_ID)),
        ("Fixture Artist - Fixture Title (hyprbeat) [Normal].osu",
         osu_text("Normal", SET_ID)),
        ("audio.wav", tiny_wav(64)),
        ("bg.jpg", b"synthetic-jpeg-fixture-bytes-hyprbeat"),
        ("video.mp4", b"synthetic-video-bytes-hyprbeat"),
        ("storyboard.osb", b"synthetic-osb-bytes-hyprbeat"),
        ("skin.ini", b"synthetic-skin-bytes-hyprbeat"),
        ("notes.txt", b"synthetic-notes-bytes-hyprbeat"),
        ("subdir/", b""),
    ])

    write_zip(os.path.join(FIXTURES, "unsubmitted.osz"), [
        ("Unsubmitted Fixture (hyprbeat) [Oni].osu", osu_text("Oni", -1)),
        # different byte count than submitted.osz's audio: the two fixtures
        # must not dedup into each other's canonical copies across sets
        ("audio.wav", tiny_wav(96)),
    ])

    # Δ-J: the visualizer validation fixture (docstring above). Timing:
    # t=0 uninherited beatLength 500; t=1000 inherited -50 → SV 2.0 (in
    # force for both sliders); kiai on at 4000 (Normal) / 3000 (Hard),
    # off at 8000 — past the span end, so the window clamps to it.
    normal_timing = (
        "0,500,4,2,0,60,1,0\n"
        "1000,-50,4,2,0,60,0,0\n"
        "4000,-50,4,2,0,60,0,1\n"
        "8000,-50,4,2,0,60,0,0"
    )
    # circles t=0/500; slider t=2000, L 176→476 (= 300 px = the length
    # field), slides 2 → 375 ms/slide → 2000..2750; spinner 3000..4200.
    normal_objects = (
        "256,192,0,1,0,0:0:0:0:\n"
        "256,192,500,1,0,0:0:0:0:\n"
        "176,192,2000,2,0,L|476:192,2,300\n"
        "256,192,3000,12,0,4200,0:0:0:0:"
    )
    hard_timing = (
        "0,500,4,2,0,60,1,0\n"
        "1000,-50,4,2,0,60,0,0\n"
        "3000,-50,4,2,0,60,0,1\n"
        "8000,-50,4,2,0,60,0,0"
    )
    # circles t=0/250/500/750; slider t=1500 slides 1 → 1500..1875;
    # spinner 2500..3700.
    hard_objects = (
        "256,192,0,1,0,0:0:0:0:\n"
        "256,192,250,1,0,0:0:0:0:\n"
        "256,192,500,1,0,0:0:0:0:\n"
        "256,192,750,1,0,0:0:0:0:\n"
        "176,192,1500,2,0,L|476:192,1,300\n"
        "256,192,2500,12,0,3700,0:0:0:0:"
    )
    write_zip(os.path.join(FIXTURES, "timeline.osz"), [
        ("Fixture Artist - Timeline Fixture 1 (hyprbeat) [Normal].osu",
         timeline_osu("Normal", 20261121, normal_timing, normal_objects)),
        ("Fixture Artist - Timeline Fixture 2 (hyprbeat) [Hard].osu",
         timeline_osu("Hard", 20261122, hard_timing, hard_objects)),
        ("tone.wav", tiny_wav(128)),
    ])

    hand_rolled_oversize(os.path.join(FIXTURES, "oversize.osz"))

    import hashlib
    for fname in sorted(os.listdir(FIXTURES)):
        full = os.path.join(FIXTURES, fname)
        with open(full, "rb") as fh:
            print(f"{hashlib.sha256(fh.read()).hexdigest()}  {fname}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
