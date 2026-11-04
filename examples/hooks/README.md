# Example hooks

A hook is your own executable that moil runs after a job of one bundle
succeeds on your machine, with the job's outputs ([spec/machine.md
§2](../../spec/machine.md#2-hooks); `moil hooks --help` has the whole
contract). It lives at `~/.moil/services/<service>/hooks/<bundle>`, with
one extension if you like. To install the example as tide's `transcribe`
hook:

```sh
cp meeting-notes.py ~/.moil/services/<tide's ID>/hooks/transcribe.py
chmod +x ~/.moil/services/<tide's ID>/hooks/transcribe.py
```

| Hook | For | What it does |
|---|---|---|
| [`meeting-notes.py`](meeting-notes.py) | tide's `transcribe` | Asks Codex for notes on the meeting and writes them into an Obsidian vault |

To try a hook without waiting for a job, hand it a context yourself, and
run it in an empty directory, as moil does:

```sh
mkdir -p /tmp/out && echo "Ada: let's ship on Friday." > /tmp/out/transcript.txt
echo '{"v":1,"service":{"id":"tide.example.com","name":"tide","base_url":"https://tide.example.com/moil"},"bundle":{"name":"transcribe","version":"0.3.0","hash":"'$(printf 0%.0s {1..64})'"},"job_id":"recording-1","attempt":1,"title":"Weekly sync","params":null,"ended_at":"2026-09-27T22:41:07Z","output_dir":"/tmp/out","outputs":{"transcript.txt":"/tmp/out/transcript.txt"}}' \
  | (cd "$(mktemp -d)" && TMPDIR="$PWD" /path/to/hook)
```
