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
"""

import os
import struct
import sys
import zipfile

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")
ZIP_DATE = (2026, 11, 15, 0, 0, 0)  # fixed for determinism
SET_ID = 20261115


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

    hand_rolled_oversize(os.path.join(FIXTURES, "oversize.osz"))

    import hashlib
    for fname in sorted(os.listdir(FIXTURES)):
        full = os.path.join(FIXTURES, fname)
        with open(full, "rb") as fh:
            print(f"{hashlib.sha256(fh.read()).hexdigest()}  {fname}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
