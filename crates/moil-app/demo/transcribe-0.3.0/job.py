# /// script
# requires-python = ">=3.12"
# dependencies = [
#     # 1.2.1 drifts timestamps after long silences; the fix isn't released.
#     "faster-whisper @ git+https://github.com/SYSTRAN/faster-whisper@ed9a06cd89a93e47838f564998a6c09b655d7f43",
#     "sherpa-onnx==1.13.8",
# ]
# ///
"""Transcribe a meeting recording with speaker labels.

Reads the recording from the `audio.ogg` input, finds who spoke when with a
pyannote segmentation model and TitaNet speaker embeddings (sherpa-onnx),
transcribes it with Whisper (small) through CTranslate2, and writes
transcript.json:

    {"language": "en", "duration": 1834.2, "speakers": 3,
     "segments": [{"start": 0.0, "end": 4.2, "speaker": 0, "text": "..."}]}
"""

import json
import sys
from pathlib import Path

import sherpa_onnx
from faster_whisper import WhisperModel, decode_audio

SAMPLE_RATE = 16_000


def emit(kind, **fields):
    print(json.dumps({"v": 1, "type": kind, **fields}), flush=True)


def diarize(samples, assets, speakers):
    """Speaker turns as (start, end, speaker), in order."""
    config = sherpa_onnx.OfflineSpeakerDiarizationConfig(
        segmentation=sherpa_onnx.OfflineSpeakerSegmentationModelConfig(
            pyannote=sherpa_onnx.OfflineSpeakerSegmentationPyannoteModelConfig(
                model=assets["diarization/segmentation.onnx"],
            ),
        ),
        embedding=sherpa_onnx.SpeakerEmbeddingExtractorConfig(
            model=assets["diarization/embedding.onnx"],
        ),
        clustering=sherpa_onnx.FastClusteringConfig(
            num_clusters=speakers or -1,
            threshold=0.5,
        ),
        min_duration_on=0.3,
        min_duration_off=0.5,
    )
    diarization = sherpa_onnx.OfflineSpeakerDiarization(config)

    def on_progress(done, total):
        emit("progress", fraction=0.6 + 0.35 * done / total, message="diarizing")
        return 0

    turns = diarization.process(samples, callback=on_progress).sort_by_start_time()
    return [(turn.start, turn.end, turn.speaker) for turn in turns]


def speaker_of(segment, turns):
    """The speaker who talks the most during `segment`, if anyone does."""
    overlap = {}
    for start, end, speaker in turns:
        shared = min(end, segment["end"]) - max(start, segment["start"])
        if shared > 0:
            overlap[speaker] = overlap.get(speaker, 0.0) + shared
    return max(overlap, key=overlap.get) if overlap else None


def main():
    job = json.load(sys.stdin)
    params = job["params"] or {}
    assets = job["assets"]
    samples = decode_audio(job["inputs"]["audio.ogg"], sampling_rate=SAMPLE_RATE)

    emit("progress", fraction=0.0, message="loading model")
    model = WhisperModel(
        str(Path(job["assets_dir"]) / "whisper"),
        device="auto",
        compute_type="int8",
    )
    segments, info = model.transcribe(
        samples,
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
            fraction=0.6 * min(segment.end / info.duration, 1.0),
            message="transcribing",
        )

    turns = diarize(samples, assets, params.get("speakers"))
    for segment in transcript:
        segment["speaker"] = speaker_of(segment, turns)
    speakers = len({speaker for _, _, speaker in turns})

    out = Path(job["output_dir"]) / "transcript.json"
    out.write_text(
        json.dumps(
            {
                "language": info.language,
                "duration": round(info.duration, 2),
                "speakers": speakers,
                "segments": transcript,
            },
            ensure_ascii=False,
        )
    )
    emit(
        "result",
        files=["transcript.json"],
        meta={
            "segments": len(transcript),
            "speakers": speakers,
            "language": info.language,
        },
    )


if __name__ == "__main__":
    main()
