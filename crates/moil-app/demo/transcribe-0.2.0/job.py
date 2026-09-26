# /// script
# requires-python = ">=3.12"
# dependencies = ["faster-whisper==1.1.1"]
# ///
"""Transcribe a meeting recording.

Reads the recording from the `audio.ogg` input, transcribes it with Whisper
(small) through CTranslate2, and writes transcript.json:

    {"language": "en", "duration": 1834.2,
     "segments": [{"start": 0.0, "end": 4.2, "text": "..."}]}
"""

import json
import sys
from pathlib import Path

from faster_whisper import WhisperModel


def emit(kind, **fields):
    print(json.dumps({"v": 1, "type": kind, **fields}), flush=True)


def main():
    job = json.load(sys.stdin)
    params = job["params"] or {}
    audio = job["inputs"]["audio.ogg"]
    model_dir = Path(job["assets_dir"]) / "whisper"

    emit("progress", fraction=0.0, message="loading model")
    model = WhisperModel(str(model_dir), device="auto", compute_type="int8")

    segments, info = model.transcribe(
        audio,
        language=params.get("language"),
        vad_filter=True,
    )
    emit(
        "log",
        level="info",
        message=f"language {info.language} ({info.language_probability:.0%})",
    )

    transcript = []
    for segment in segments:
        transcript.append(
            {
                "start": round(segment.start, 2),
                "end": round(segment.end, 2),
                "text": segment.text.strip(),
            }
        )
        emit(
            "progress",
            fraction=min(segment.end / info.duration, 1.0),
            message="transcribing",
        )

    out = Path(job["output_dir"]) / "transcript.json"
    out.write_text(
        json.dumps(
            {
                "language": info.language,
                "duration": round(info.duration, 2),
                "segments": transcript,
            },
            ensure_ascii=False,
        )
    )
    emit(
        "result",
        files=["transcript.json"],
        meta={"segments": len(transcript), "language": info.language},
    )


if __name__ == "__main__":
    main()
