# Example hooks

A hook is your own executable that moil runs after a job of one bundle
succeeds on your machine, with the job's outputs ([spec/machine.md
§2](../../spec/machine.md#2-hooks); `moil hooks --help` has the whole
contract). It lives at `~/.moil/services/<service>/hooks/<bundle>`.

| Hook | For | What it does |
|---|---|---|
| [`meeting-notes.py`](meeting-notes.py) | klisi's `transcribe` | Asks Codex for notes on the meeting and writes them into an Obsidian vault |

To try a hook without waiting for a job, hand it a context yourself:

```sh
mkdir -p /tmp/out && echo "Ada: let's ship on Friday." > /tmp/out/transcript.txt
echo '{"v":1,"service":{"id":"klisi.example.com","name":"klisi","base_url":"https://klisi.example.com/moil"},"bundle":{"name":"transcribe","version":"0.3.0","hash":"'$(printf 0%.0s {1..64})'"},"job_id":"recording-1","attempt":1,"title":"Weekly sync","params":null,"ended_at":"2026-09-27T22:41:07Z","output_dir":"/tmp/out","outputs":{"transcript.txt":"/tmp/out/transcript.txt"}}' \
  | (cd /tmp/out && TMPDIR=/tmp ./path/to/hook)
```
