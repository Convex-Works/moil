package e2e

import (
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"math/big"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// A machine's owner pairs it with a service and approves the service's
// bundle. The service's next job runs there, and everything the script
// reports and produces reaches the service as the script sent it.
func TestAJobRunsOnAPairedMachine(t *testing.T) {
	t.Parallel()
	svc := newService(t)
	m := newMachine(t, "alice-mac")
	m.Pair(svc, "alice")
	m.Approve(svc.probe)
	m.Start()

	audio := randomBytes(t, 3<<20)
	transcript := svc.Receive()
	transcript.Method = "POST"
	transcript.Headers = map[string]string{"Content-Type": "application/json", "X-Upload-Token": "t0k3n"}
	payloads := []string{
		`{"segment": {"start": 1.25, "end": 3.5, "text": "héllo, wörld 👋"}, "speakers": ["ada", "grace"]}`,
		`[1, -2, 3.5e-7, 1e300, true, false, null, "", {}, []]`,
		`"just a string"`,
		`18446744073709551615`,
		`null`,
	}
	meta := `{"speakers": 2, "language": "en", "confidence": 0.93}`
	steps := []step{do.Progress(0.25, "loading")}
	for _, p := range payloads {
		steps = append(steps, do.Data(json.RawMessage(p)))
	}
	steps = append(steps,
		do.Junk("a stray print"),
		do.Stderr("a chatty library"),
		do.Copy("audio.ogg", "transcript.json"),
		do.Progress(1, "done"),
		do.Result(json.RawMessage(meta), "transcript.json"),
	)
	run := svc.Submit(moil.Job{
		Title:    "Weekly sync",
		Params:   probe(steps...),
		Inputs:   map[string]moil.Download{"audio.ogg": svc.Serve(audio)},
		Outputs:  map[string]moil.Upload{"transcript.json": transcript.Upload},
		Eligible: moil.OwnedBy("alice"),
	})

	// The service knows who lent it the machine, and what it is.
	if lent := svc.Machine(m.ID()); lent.Owner != "alice" || lent.Name != "alice-mac" || !lent.Online() ||
		lent.CPUs < 1 || lent.MemoryBytes < 1 || !lent.HasApproved(svc.probe.Hash()) {
		t.Errorf("the service sees the machine as %+v", lent)
	}

	res := succeed(t, run)
	if res.Machine != m.ID() || res.Attempt != 1 {
		t.Errorf("the job succeeded on attempt %d on %s, want attempt 1 on alice's machine %s", res.Attempt, res.Machine, m.ID())
	}
	if !sameJSON(res.Meta, []byte(meta)) {
		t.Errorf("the result's meta is %s, want %s", res.Meta, meta)
	}
	uploaded, uploads := transcript.Received()
	if !bytes.Equal(uploaded.body, audio) || uploads != 1 {
		t.Errorf("the service received %d uploads, the last of %d bytes; want the %d bytes of the input once", uploads, len(uploaded.body), len(audio))
	}
	if uploaded.method != "POST" || uploaded.header.Get("Content-Type") != "application/json" || uploaded.header.Get("X-Upload-Token") != "t0k3n" {
		t.Errorf("the output was uploaded with %s and the headers %v, not as the job asked", uploaded.method, uploaded.header)
	}
	sum := sha256.Sum256(audio)
	want := map[string]moil.File{"transcript.json": {SizeBytes: int64(len(audio)), SHA256: hex.EncodeToString(sum[:])}}
	if !reflect.DeepEqual(res.Files, want) {
		t.Errorf("the result describes the files %+v, want %+v", res.Files, want)
	}

	// Everything the script sent arrives in order, each event once, with
	// the runtime's phases around it. Its stray print arrives as a log;
	// its stderr stays on the machine. Data payloads arrive as the same
	// JSON values: the machine re-encodes them, so the order of keys and
	// the spelling of numbers may change, and numbers that don't fit a
	// 64-bit integer or a double lose precision.
	var got []string
	for _, ev := range history(run) {
		if ev.Kind != moil.EventQueued && (ev.Attempt != 1 || ev.Machine != m.ID()) {
			t.Errorf("event %+v isn't about attempt 1 on %s", ev, m.ID())
		}
		got = append(got, summary(ev))
	}
	wantEvents := []string{"queued", "assigned", "phase preparing", "phase downloading", "phase running", "progress 0.25 loading"}
	for _, p := range payloads {
		wantEvents = append(wantEvents, "data "+p)
	}
	wantEvents = append(wantEvents, "log info a stray print", "progress 1 done", "phase uploading")
	if !sameEvents(got, wantEvents) {
		t.Errorf("the service saw the events\n  %s\nwant\n  %s", strings.Join(got, "\n  "), strings.Join(wantEvents, "\n  "))
	}
}

// However an attempt fails, the service learns why, as the machine saw it,
// and retries it only if another attempt could succeed.
func TestFailuresReachTheService(t *testing.T) {
	t.Parallel()

	t.Run("a retryable error is retried up to the limit", func(t *testing.T) {
		t.Parallel()
		svc, m := lentMachine(t)
		run := svc.Submit(moil.Job{
			Params:      probe(do.Stderr("cuda: out of memory allocating 2 GiB"), do.Fail("out of memory", true)),
			MaxAttempts: 2,
		})
		e := failure(t, run)
		want := moil.JobError{Code: moil.CodeScriptError, Message: "out of memory", Retryable: true, Machine: m.ID(), Attempt: 2}
		if !sameError(e, want) || !strings.Contains(e.StderrTail, "cuda: out of memory allocating 2 GiB") {
			t.Errorf("the job failed with %+v, want %+v and the script's stderr", e, want)
		}
		if retried := awaitRetry(t, run, 1); retried.Code != moil.CodeScriptError {
			t.Errorf("attempt 1 failed with %s", retried.Code)
		}
		if got := assigned(run); !slices.Equal(got, []int{1, 2}) {
			t.Errorf("the service assigned attempts %v, want 1 and 2", got)
		}
	})

	t.Run("a permanent error is not retried", func(t *testing.T) {
		t.Parallel()
		svc, m := lentMachine(t)
		run := svc.Submit(moil.Job{Params: probe(do.Fail("the recording is empty", false))})
		e := failure(t, run)
		want := moil.JobError{Code: moil.CodeScriptError, Message: "the recording is empty", Machine: m.ID(), Attempt: 1}
		if !sameError(e, want) {
			t.Errorf("the job failed with %+v, want %+v", e, want)
		}
		if got := assigned(run); !slices.Equal(got, []int{1}) {
			t.Errorf("the service assigned attempts %v, want only 1", got)
		}
	})

	t.Run("exiting without a result", func(t *testing.T) {
		t.Parallel()
		svc, m := lentMachine(t)
		e := failure(t, svc.Submit(moil.Job{Params: probe(do.Count(1, 3), do.Exit(0))}))
		want := moil.JobError{Code: moil.CodeNoResult, Message: "the script exited without sending a result", Machine: m.ID(), Attempt: 1}
		if !sameError(e, want) || e.ExitCode == nil || *e.ExitCode != 0 {
			t.Errorf("the job failed with %+v, want %+v with exit code 0", e, want)
		}
	})

	t.Run("exiting with an error status", func(t *testing.T) {
		t.Parallel()
		svc, m := lentMachine(t)
		e := failure(t, svc.Submit(moil.Job{Params: probe(do.Stderr("Traceback: KeyError"), do.Exit(3))}))
		want := moil.JobError{Code: moil.CodeExit, Message: "the script exited with status 3", Machine: m.ID(), Attempt: 1}
		if !sameError(e, want) || e.ExitCode == nil || *e.ExitCode != 3 || !strings.Contains(e.StderrTail, "Traceback: KeyError") {
			t.Errorf("the job failed with %+v, want %+v with exit code 3 and the script's stderr", e, want)
		}
	})

	t.Run("running past the time limit", func(t *testing.T) {
		t.Parallel()
		svc, m := lentMachine(t)
		run := svc.Submit(moil.Job{Params: probe(do.Spawn(), do.Sleep(time.Minute)), Timeout: 4 * time.Second})
		p := awaitPids(t, run, 1)
		e := failure(t, run)
		want := moil.JobError{Code: moil.CodeTimeout, Message: "the attempt ran past its time limit of 4 s", Machine: m.ID(), Attempt: 1}
		if !sameError(e, want) {
			t.Errorf("the job failed with %+v, want %+v", e, want)
		}
		awaitGone(t, "the timed-out job's processes", p.all()...)
	})
}

// Cancelling a job stops it on the machine at once: the script, and
// whatever it started, even a process that ignores SIGTERM. The machine
// is then free for the next job.
func TestCancellingAJobStopsEverythingItStarted(t *testing.T) {
	t.Parallel()
	svc, m := lentMachine(t)
	never := newGate(t)
	run := svc.Submit(moil.Job{Params: probe(do.Spawn(), do.WaitFor(never))})
	p := awaitPids(t, run, 1)

	cancelled := time.Now()
	run.Cancel()
	if _, err := finish(t, run); !errors.Is(err, moil.ErrCancelled) {
		t.Fatalf("the cancelled job ended with %v", err)
	}
	awaitGone(t, "the cancelled job's processes", p.all()...)
	// Nothing waited out the ten seconds a script gets to exit on SIGTERM.
	if took := time.Since(cancelled); took > 5*time.Second {
		t.Errorf("stopping the job took %v", took)
	}

	if res := succeed(t, svc.Submit(moil.Job{Params: probe(do.Result(nil))})); res.Machine != m.ID() {
		t.Errorf("the next job ran on %s, not %s", res.Machine, m.ID())
	}
}

// sameError compares what a JobError says, apart from the exit code and
// the stderr tail.
func sameError(got *moil.JobError, want moil.JobError) bool {
	return got.Code == want.Code && got.Message == want.Message && got.Retryable == want.Retryable &&
		got.Machine == want.Machine && got.Attempt == want.Attempt
}

// assigned returns the attempts the service assigned so far.
func assigned(run *moil.Run) []int {
	var attempts []int
	for _, ev := range history(run) {
		if ev.Kind == moil.EventAssigned {
			attempts = append(attempts, ev.Attempt)
		}
	}
	return attempts
}

// summary describes an event in a line, for comparing sequences of events.
func summary(ev moil.Event) string {
	switch ev.Kind {
	case moil.EventPhase:
		return "phase " + ev.Phase
	case moil.EventProgress:
		fraction := "-"
		if ev.Fraction != nil {
			fraction = fmt.Sprint(*ev.Fraction)
		}
		return "progress " + fraction + " " + ev.Message
	case moil.EventLog:
		return "log " + ev.Level + " " + ev.Message
	case moil.EventData:
		return "data " + string(ev.Data)
	case moil.EventRetrying:
		return fmt.Sprintf("retrying %s", ev.Err.Code)
	}
	return string(ev.Kind)
}

// sameEvents compares event summaries, data payloads as JSON values.
func sameEvents(got, want []string) bool {
	if len(got) != len(want) {
		return false
	}
	for i := range got {
		g, gData := strings.CutPrefix(got[i], "data ")
		w, wData := strings.CutPrefix(want[i], "data ")
		if gData != wData || (gData && !sameJSON([]byte(g), []byte(w))) || (!gData && g != w) {
			return false
		}
	}
	return true
}

// sameJSON reports whether a and b are the same JSON value: objects with
// the same members in any order, and numbers of exactly the same value
// however they're written.
func sameJSON(a, b []byte) bool {
	var va, vb any
	return decodeJSON(a, &va) == nil && decodeJSON(b, &vb) == nil && sameValue(va, vb)
}

func decodeJSON(data []byte, v *any) error {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	return dec.Decode(v)
}

func sameValue(a, b any) bool {
	switch a := a.(type) {
	case json.Number:
		b, ok := b.(json.Number)
		ra, okA := new(big.Rat).SetString(string(a))
		rb, okB := new(big.Rat).SetString(string(b))
		return ok && okA && okB && ra.Cmp(rb) == 0
	case map[string]any:
		b, ok := b.(map[string]any)
		if !ok || len(a) != len(b) {
			return false
		}
		for k, v := range a {
			if w, ok := b[k]; !ok || !sameValue(v, w) {
				return false
			}
		}
		return true
	case []any:
		b, ok := b.([]any)
		if !ok || len(a) != len(b) {
			return false
		}
		for i := range a {
			if !sameValue(a[i], b[i]) {
				return false
			}
		}
		return true
	}
	return a == b
}

func randomBytes(t *testing.T, n int) []byte {
	b := make([]byte, n)
	rand.Read(b)
	return b
}
